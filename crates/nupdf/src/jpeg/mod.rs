// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from libjpeg 10 jdapimin.c, jdapistd.c, jdmaster.c, jdinput.c,
// jdcoefct.c, jddctmgr.c, jdmainct.c, jdsample.c, jdcolor.c and jutils.c,
// as bundled with MuPDF 1.28.6, Copyright (C) 1991-1998, Thomas G. Lane.
// Modified 2002-2026 by Guido Vollbeding. This software is based in part on
// the work of the Independent JPEG Group.

//! A JPEG decoder that reproduces libjpeg 10, the decoder MuPDF bundles,
//! for the way MuPDF drives it: default parameters (integer IDCT, smoothing
//! of incomplete progressive images), output at full size or scaled down
//! by a power of two in the IDCT, one scanline at a time, from a source
//! that never suspends.
//!
//! libjpeg 7 and later upsample subsampled components inside the IDCT
//! (a 16x16 transform gives a 2x2-subsampled chroma block its full size),
//! so samples depend on those transforms; no general-purpose Rust decoder
//! does this, and their output differs from MuPDF's by up to a dozen
//! levels. Only 8x8 blocks are supported: libjpeg's "SmartScale" block
//! sizes, which no PDF producer writes, are reported as unsupported.

mod arith;
mod huff;
mod idct;
pub(crate) mod marker;

use arith::Arith;
use huff::Huff;
use idct::Method;
use marker::{Reader, Stop};

use crate::stream::Stream;

pub(crate) type Block = [i16; 64];

/// `jpeg_natural_order`, padded so corrupt runs past 63 stay in bounds.
pub(crate) const NATURAL_ORDER: [usize; 80] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63,
    63, 63, 63, 63, 63, 63,
];

/// Largest whole-image coefficient buffer (progressive and multi-scan
/// images); nupdf's limit, as libjpeg's is the allocator's.
const MAX_COEFFICIENT_BYTES: usize = 1 << 30;

/// Why decoding stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A libjpeg error (`ERREXIT`), with libjpeg's message.
    Jpeg(String),
    /// Something libjpeg decodes that this port does not.
    Unsupported(String),
    /// A nupdf resource limit.
    Limit(String),
}

impl Error {
    pub(crate) fn jpeg(message: impl Into<String>) -> Error {
        Error::Jpeg(message.into())
    }
}

pub(crate) type R<T> = Result<T, Error>;

/// `JHUFF_TBL`.
#[derive(Clone)]
pub(crate) struct HuffTable {
    pub bits: [u8; 17],
    pub huffval: [u8; 256],
}

impl Default for HuffTable {
    fn default() -> Self {
        HuffTable {
            bits: [0; 17],
            huffval: [0; 256],
        }
    }
}

/// `jpeg_component_info`.
#[derive(Clone)]
pub(crate) struct Component {
    pub id: i32,
    pub index: usize,
    pub h: usize,
    pub v: usize,
    pub quant_tbl_no: i32,
    pub dc_tbl_no: usize,
    pub ac_tbl_no: usize,
    pub width_in_blocks: usize,
    pub height_in_blocks: usize,
    pub dct_h: usize,
    pub dct_v: usize,
    pub needed: bool,
    pub mcu_width: usize,
    pub mcu_height: usize,
    pub mcu_blocks: usize,
    pub mcu_sample_width: usize,
    pub last_col_width: usize,
    pub last_row_height: usize,
    /// The quantization table latched at the component's first scan.
    pub quant: Option<[u16; 64]>,
    /// The IDCT multiplier table (the latched quantizers, or zeros).
    pub dct_table: [i32; 64],
}

impl Default for Component {
    fn default() -> Self {
        Component {
            id: 0,
            index: 0,
            h: 0,
            v: 0,
            quant_tbl_no: 0,
            dc_tbl_no: 0,
            ac_tbl_no: 0,
            width_in_blocks: 0,
            height_in_blocks: 0,
            dct_h: 8,
            dct_v: 8,
            needed: true,
            mcu_width: 0,
            mcu_height: 0,
            mcu_blocks: 0,
            mcu_sample_width: 0,
            last_col_width: 0,
            last_row_height: 0,
            quant: None,
            dct_table: [0; 64],
        }
    }
}

/// The parts of `jpeg_decompress_struct` the marker reader and entropy
/// decoders share.
pub(crate) struct Info<'s> {
    pub rd: Reader<'s>,
    pub quant_tbls: [Option<[u16; 64]>; 4],
    pub dc_huff: [Option<HuffTable>; 4],
    pub ac_huff: [Option<HuffTable>; 4],
    pub arith_dc_l: [u8; 16],
    pub arith_dc_u: [u8; 16],
    pub arith_ac_k: [u8; 16],
    pub restart_interval: u32,
    pub is_baseline: bool,
    pub progressive: bool,
    pub arith_code: bool,
    pub precision: i32,
    pub image_width: usize,
    pub image_height: usize,
    pub comps: Vec<Component>,
    pub saw_jfif: bool,
    pub saw_adobe: bool,
    pub adobe_transform: u8,
    /// `color_transform == JCT_SUBTRACT_GREEN` (from an LSE marker).
    pub subtract_green: bool,
    pub lse_maxtrans: i32,
    pub comps_in_scan: usize,
    pub cur_comp: [usize; 4],
    pub ss: i32,
    pub se: i32,
    pub ah: i32,
    pub al: i32,
    pub input_scan_number: i32,
    pub lim_se: usize,
    pub max_h: usize,
    pub max_v: usize,
    pub total_imcu_rows: usize,
    pub mcus_per_row: usize,
    pub blocks_in_mcu: usize,
    /// For each block of an MCU, its component's position in the scan.
    pub mcu_membership: [usize; 10],
    /// Progressive images: the successive-approximation bit last decoded
    /// for each coefficient of each component (-1 before any).
    pub coef_bits: Vec<[i32; 64]>,
}

impl<'s> Info<'s> {
    fn new(rd: Reader<'s>) -> Info<'s> {
        Info {
            rd,
            quant_tbls: [None; 4],
            dc_huff: Default::default(),
            ac_huff: Default::default(),
            arith_dc_l: [0; 16],
            arith_dc_u: [1; 16],
            arith_ac_k: [5; 16],
            restart_interval: 0,
            is_baseline: false,
            progressive: false,
            arith_code: false,
            precision: 0,
            image_width: 0,
            image_height: 0,
            comps: Vec::new(),
            saw_jfif: false,
            saw_adobe: false,
            adobe_transform: 0,
            subtract_green: false,
            lse_maxtrans: 0,
            comps_in_scan: 0,
            cur_comp: [0; 4],
            ss: 0,
            se: 0,
            ah: 0,
            al: 0,
            input_scan_number: 0,
            lim_se: 63,
            max_h: 1,
            max_v: 1,
            total_imcu_rows: 0,
            mcus_per_row: 0,
            blocks_in_mcu: 0,
            mcu_membership: [0; 10],
            coef_bits: Vec::new(),
        }
    }
}

/// The progressive parameter checks and `coef_bits` bookkeeping shared by
/// both entropy decoders' `start_pass`.
pub(crate) fn check_progression(info: &mut Info<'_>) -> R<()> {
    let (ss, se, ah, al) = (info.ss, info.se, info.ah, info.al);
    let bad = ss == 0 && se != 0
        || ss != 0 && (se < ss || se > info.lim_se as i32 || info.comps_in_scan != 1)
        || ah != 0 && ah - 1 != al
        || al > 13;
    if bad {
        return Err(Error::jpeg(format!(
            "Invalid progressive parameters Ss={ss} Se={se} Ah={ah} Al={al}"
        )));
    }
    for i in 0..info.comps_in_scan {
        let index = info.comps[info.cur_comp[i]].index;
        if let Some(bits) = info.coef_bits.get_mut(index) {
            for b in &mut bits[ss as usize..=se as usize] {
                *b = al;
            }
        }
    }
    Ok(())
}

fn div_round_up(a: usize, b: usize) -> usize {
    a.div_ceil(b.max(1))
}

fn round_up(a: usize, b: usize) -> usize {
    div_round_up(a, b) * b.max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    Unknown,
    Grayscale,
    Rgb,
    YCbCr,
    Cmyk,
    Ycck,
    BgRgb,
    BgYcc,
}

enum Entropy {
    Huff(Box<Huff>),
    Arith(Box<Arith>),
}

impl Entropy {
    fn start_pass(&mut self, info: &mut Info<'_>) -> R<()> {
        match self {
            Entropy::Huff(h) => h.start_pass(info),
            Entropy::Arith(a) => a.start_pass(info),
        }
    }

    fn decode_mcu(&mut self, info: &mut Info<'_>, blocks: &mut [Block]) {
        match self {
            Entropy::Huff(h) => h.decode_mcu(info, blocks),
            Entropy::Arith(a) => a.decode_mcu(info, blocks),
        }
    }
}

/// What `consume_input` reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Consumed {
    Sos,
    Eoi,
    Row,
    Scan,
}

#[derive(Clone, Copy)]
enum Upsample {
    Fullsize,
    H2V1,
    H2V2,
    Int { h: usize, v: usize },
}

/// `build_ycc_rgb_table` (or the big-gamut variant).
struct YccTables {
    cr_r: [i32; 256],
    cb_b: [i32; 256],
    cr_g: [i64; 256],
    cb_g: [i64; 256],
}

impl YccTables {
    fn new(big_gamut: bool) -> Box<YccTables> {
        // FIX(x) with 16 fraction bits.
        let (r, b, g1, g2) = if big_gamut {
            (183763, 232260, 93603, 45107)
        } else {
            (91881, 116130, 46802, 22553)
        };
        let mut t = Box::new(YccTables {
            cr_r: [0; 256],
            cb_b: [0; 256],
            cr_g: [0; 256],
            cb_g: [0; 256],
        });
        for i in 0..256 {
            let x = i as i64 - 128;
            t.cr_r[i] = ((r * x + (1 << 15)) >> 16) as i32;
            t.cb_b[i] = ((b * x + (1 << 15)) >> 16) as i32;
            t.cr_g[i] = -g1 * x;
            t.cb_g[i] = -g2 * x + (1 << 15);
        }
        t
    }
}

enum Convert {
    Gray,
    Ycc(Box<YccTables>),
    Rgb,
    SubtractGreen,
    Ycck(Box<YccTables>),
    Null,
}

/// `sample_range_limit`.
fn range_limit(x: i32) -> u8 {
    x.clamp(0, 255) as u8
}

/// `jpeg_decompress_struct` and the decompression modules MuPDF uses.
pub struct Decoder<'s> {
    info: Info<'s>,
    entropy: Option<Entropy>,
    // Input controller.
    inheaders: u8,
    has_multiple_scans: bool,
    eoi_reached: bool,
    consuming_data: bool,
    jpeg_cs: ColorSpace,
    out_cs: ColorSpace,
    /// `scale_num` over a `scale_denom` of 8.
    scale_num: usize,
    /// `min_DCT_h_scaled_size` and `min_DCT_v_scaled_size`, which are equal.
    min_dct: usize,
    // Output.
    output_width: usize,
    output_height: usize,
    out_components: usize,
    output_scanline: usize,
    output_scan_number: i32,
    // Coefficient controller.
    input_imcu_row: usize,
    output_imcu_row: usize,
    mcu: [Block; 10],
    mcu_ctr: usize,
    mcu_vert_offset: usize,
    mcu_rows_per_imcu_row: usize,
    whole: Vec<Vec<Block>>,
    whole_width: Vec<usize>,
    smoothing: bool,
    coef_bits_latch: Vec<[i32; 6]>,
    methods: Vec<Method>,
    // Main controller: one iMCU row of samples per component.
    planes: Vec<Vec<u8>>,
    plane_stride: Vec<usize>,
    rowgroup_ctr: usize,
    rowgroups_avail: usize,
    // Upsampler.
    upsample: Vec<Upsample>,
    rowgroup_height: Vec<usize>,
    color_buf: Vec<Vec<u8>>,
    color_stride: usize,
    fullsize_row: Vec<usize>,
    next_row_out: usize,
    rows_to_go: usize,
    convert: Convert,
}

impl std::fmt::Debug for Decoder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoder")
            .field("width", &self.output_width)
            .field("height", &self.output_height)
            .field("components", &self.out_components)
            .field("scanline", &self.output_scanline)
            .finish()
    }
}

impl<'s> Decoder<'s> {
    /// `jpeg_create_decompress`, reading from what `chain` has buffered.
    pub fn new(chain: Stream<'s>) -> Decoder<'s> {
        Decoder {
            info: Info::new(Reader::new(chain)),
            entropy: None,
            inheaders: 1,
            has_multiple_scans: false,
            eoi_reached: false,
            consuming_data: false,
            jpeg_cs: ColorSpace::Unknown,
            out_cs: ColorSpace::Unknown,
            scale_num: 8,
            min_dct: 8,
            output_width: 0,
            output_height: 0,
            out_components: 0,
            output_scanline: 0,
            output_scan_number: 0,
            input_imcu_row: 0,
            output_imcu_row: 0,
            mcu: [[0; 64]; 10],
            mcu_ctr: 0,
            mcu_vert_offset: 0,
            mcu_rows_per_imcu_row: 0,
            whole: Vec::new(),
            whole_width: Vec::new(),
            smoothing: false,
            coef_bits_latch: Vec::new(),
            methods: Vec::new(),
            planes: Vec::new(),
            plane_stride: Vec::new(),
            rowgroup_ctr: 0,
            rowgroups_avail: 8,
            upsample: Vec::new(),
            rowgroup_height: Vec::new(),
            color_buf: Vec::new(),
            color_stride: 0,
            fullsize_row: Vec::new(),
            next_row_out: 0,
            rows_to_go: 0,
            convert: Convert::Null,
        }
    }

    /// `jpeg_read_header(cinfo, TRUE)`.
    pub fn read_header(&mut self) -> R<()> {
        match self.consume_markers()? {
            Consumed::Sos => {
                self.default_decompress_parms();
                Ok(())
            }
            _ => Err(Error::jpeg("JPEG datastream contains no image")),
        }
    }

    pub fn num_components(&self) -> usize {
        self.info.comps.len()
    }

    /// The Adobe APP14 marker's transform, if the stream has the marker.
    pub fn adobe_transform(&self) -> Option<u8> {
        self.info.saw_adobe.then_some(self.info.adobe_transform)
    }

    pub fn jpeg_color_space(&self) -> ColorSpace {
        self.jpeg_cs
    }

    /// Overrides the color space the data is in (before `start`).
    pub fn set_jpeg_color_space(&mut self, cs: ColorSpace) {
        self.jpeg_cs = cs;
    }

    /// `scale_num` with a `scale_denom` of 8 (from 1 to 8): the image comes
    /// out that many eighths of its size, rounded up.
    pub fn set_scale(&mut self, num: usize) {
        self.scale_num = num.clamp(1, 8);
    }

    pub fn output_width(&self) -> usize {
        self.output_width
    }

    pub fn output_height(&self) -> usize {
        self.output_height
    }

    pub fn output_components(&self) -> usize {
        self.out_components
    }

    pub fn output_scanline(&self) -> usize {
        self.output_scanline
    }

    /// `default_decompress_parms`.
    fn default_decompress_parms(&mut self) {
        let info = &self.info;
        let ids: Vec<i32> = info.comps.iter().map(|c| c.id).collect();
        (self.jpeg_cs, self.out_cs) = match ids.len() {
            1 => (ColorSpace::Grayscale, ColorSpace::Grayscale),
            3 => {
                let cs = match ids[..] {
                    [1, 2, 3] => ColorSpace::YCbCr,
                    [1, 0x22, 0x23] => ColorSpace::BgYcc,
                    [0x52, 0x47, 0x42] => ColorSpace::Rgb,
                    [0x72, 0x67, 0x62] => ColorSpace::BgRgb,
                    _ if info.saw_jfif => ColorSpace::YCbCr,
                    _ if info.saw_adobe && info.adobe_transform == 0 => ColorSpace::Rgb,
                    _ => ColorSpace::YCbCr,
                };
                (cs, ColorSpace::Rgb)
            }
            4 => {
                let cs = match ids[..] {
                    [1, 2, 3, 4] => ColorSpace::Ycck,
                    [0x43, 0x4d, 0x59, 0x4b] => ColorSpace::Cmyk,
                    _ if info.saw_adobe && info.adobe_transform == 0 => ColorSpace::Cmyk,
                    _ if info.saw_adobe => ColorSpace::Ycck,
                    _ => ColorSpace::Cmyk,
                };
                (cs, ColorSpace::Cmyk)
            }
            _ => (ColorSpace::Unknown, ColorSpace::Unknown),
        };
    }

    // Input control (jdinput.c).

    /// `consume_markers`.
    fn consume_markers(&mut self) -> R<Consumed> {
        if self.eoi_reached {
            return Ok(Consumed::Eoi);
        }
        loop {
            match self.info.read_markers()? {
                Stop::Sos => {
                    if self.inheaders != 0 {
                        if self.inheaders == 1 {
                            self.initial_setup()?;
                        }
                        if self.info.comps_in_scan == 0 {
                            self.inheaders = 2;
                            continue;
                        }
                        self.inheaders = 0;
                    } else {
                        if !self.has_multiple_scans {
                            return Err(Error::jpeg("Didn't expect more than one scan"));
                        }
                        if self.info.comps_in_scan == 0 {
                            continue;
                        }
                        self.start_input_pass()?;
                    }
                    return Ok(Consumed::Sos);
                }
                Stop::Eoi => {
                    self.eoi_reached = true;
                    if self.inheaders != 0 {
                        if self.info.rd.saw_sof {
                            return Err(Error::jpeg(
                                "Invalid JPEG file structure: missing SOS marker",
                            ));
                        }
                    } else if self.output_scan_number > self.info.input_scan_number {
                        self.output_scan_number = self.info.input_scan_number;
                    }
                    return Ok(Consumed::Eoi);
                }
            }
        }
    }

    fn consume_input(&mut self) -> R<Consumed> {
        if self.consuming_data && self.has_multiple_scans {
            Ok(self.consume_data())
        } else if self.consuming_data {
            // `dummy_consume_data`: single-scan data is read while output.
            Ok(Consumed::Row)
        } else {
            self.consume_markers()
        }
    }

    /// `initial_setup`.
    fn initial_setup(&mut self) -> R<()> {
        let info = &mut self.info;
        if info.image_height > 65500 || info.image_width > 65500 {
            return Err(Error::jpeg(
                "Maximum supported image dimension is 65500 pixels",
            ));
        }
        if !(8..=12).contains(&info.precision) {
            return Err(Error::jpeg(format!(
                "Unsupported JPEG data precision {}",
                info.precision
            )));
        }
        if info.comps.len() > 10 {
            return Err(Error::jpeg(format!(
                "Too many color components: {}, max 10",
                info.comps.len()
            )));
        }
        info.max_h = 1;
        info.max_v = 1;
        for c in &info.comps {
            if !(1..=4).contains(&c.h) || !(1..=4).contains(&c.v) {
                return Err(Error::jpeg("Bogus sampling factors"));
            }
            info.max_h = info.max_h.max(c.h);
            info.max_v = info.max_v.max(c.v);
        }
        if !(info.is_baseline || info.progressive && info.comps_in_scan != 0) && info.se != 63 {
            let se = info.se;
            if (1..=16).any(|n| n * n - 1 == se) {
                return Err(Error::Unsupported(format!(
                    "jpeg block size {} (SmartScale)",
                    (1..=16).find(|n| n * n - 1 == se).unwrap_or(0)
                )));
            }
            return Err(Error::jpeg(format!(
                "Invalid progressive parameters Ss={} Se={} Ah={} Al={}",
                info.ss, info.se, info.ah, info.al
            )));
        }
        info.lim_se = 63;
        let (w, h, max_h, max_v) = (info.image_width, info.image_height, info.max_h, info.max_v);
        for c in &mut info.comps {
            c.dct_h = 8;
            c.dct_v = 8;
            c.width_in_blocks = div_round_up(w * c.h, max_h * 8);
            c.height_in_blocks = div_round_up(h * c.v, max_v * 8);
            c.needed = true;
            c.quant = None;
        }
        info.total_imcu_rows = div_round_up(h, max_v * 8);
        self.has_multiple_scans = info.comps_in_scan < info.comps.len() || info.progressive;
        Ok(())
    }

    /// `per_scan_setup`.
    fn per_scan_setup(&mut self) -> R<()> {
        let info = &mut self.info;
        if info.comps_in_scan == 1 {
            let c = &mut info.comps[info.cur_comp[0]];
            info.mcus_per_row = c.width_in_blocks;
            c.mcu_width = 1;
            c.mcu_height = 1;
            c.mcu_blocks = 1;
            c.mcu_sample_width = c.dct_h;
            c.last_col_width = 1;
            let t = c.height_in_blocks % c.v;
            c.last_row_height = if t == 0 { c.v } else { t };
            info.blocks_in_mcu = 1;
            info.mcu_membership[0] = 0;
        } else {
            if info.comps_in_scan == 0 || info.comps_in_scan > 4 {
                return Err(Error::jpeg(format!(
                    "Too many color components: {}, max 4",
                    info.comps_in_scan
                )));
            }
            info.mcus_per_row = div_round_up(info.image_width, info.max_h * 8);
            info.blocks_in_mcu = 0;
            for i in 0..info.comps_in_scan {
                let c = &mut info.comps[info.cur_comp[i]];
                c.mcu_width = c.h;
                c.mcu_height = c.v;
                c.mcu_blocks = c.h * c.v;
                c.mcu_sample_width = c.h * c.dct_h;
                let t = c.width_in_blocks % c.h;
                c.last_col_width = if t == 0 { c.h } else { t };
                let t = c.height_in_blocks % c.v;
                c.last_row_height = if t == 0 { c.v } else { t };
                if info.blocks_in_mcu + c.mcu_blocks > 10 {
                    return Err(Error::jpeg(
                        "Sampling factors too large for interleaved scan",
                    ));
                }
                for _ in 0..c.mcu_blocks {
                    info.mcu_membership[info.blocks_in_mcu] = i;
                    info.blocks_in_mcu += 1;
                }
            }
        }
        Ok(())
    }

    /// `latch_quant_tables`.
    fn latch_quant_tables(&mut self) -> R<()> {
        let info = &mut self.info;
        for i in 0..info.comps_in_scan {
            let c = info.cur_comp[i];
            if info.comps[c].quant.is_some() {
                continue;
            }
            let n = info.comps[c].quant_tbl_no;
            let table = usize::try_from(n)
                .ok()
                .and_then(|n| info.quant_tbls.get(n).copied().flatten());
            let Some(table) = table else {
                return Err(Error::jpeg(format!(
                    "Quantization table 0x{n:02x} was not defined"
                )));
            };
            info.comps[c].quant = Some(table);
        }
        Ok(())
    }

    /// `start_input_pass`.
    fn start_input_pass(&mut self) -> R<()> {
        self.per_scan_setup()?;
        self.latch_quant_tables()?;
        if let Some(e) = self.entropy.as_mut() {
            e.start_pass(&mut self.info)?;
        }
        self.input_imcu_row = 0;
        self.start_imcu_row();
        self.consuming_data = true;
        Ok(())
    }

    /// `finish_input_pass`.
    fn finish_input_pass(&mut self) {
        self.consuming_data = false;
    }

    // Master control (jdmaster.c, jdapistd.c).

    /// `jpeg_calc_output_dimensions` (`jpeg_core_output_dimensions` and
    /// the IDCT scaling of each component), for 8x8 blocks and a
    /// `scale_denom` of 8.
    fn calc_output_dimensions(&mut self) {
        let info = &mut self.info;
        // The smallest k with scale_num * 8 <= 8 * k.
        let k = self.scale_num;
        self.min_dct = k;
        self.output_width = div_round_up(info.image_width * k, 8);
        self.output_height = div_round_up(info.image_height * k, 8);
        let (max_h, max_v) = (info.max_h, info.max_v);
        // Subsampled components are scaled up in the IDCT where whole 2:1
        // ratios allow it (fancy upsampling being on), up to 2:1 between
        // a block's sides.
        let ssize = |max: usize, samp: usize| {
            let mut ssize = 1;
            while k * ssize <= 8 && max.is_multiple_of(samp * ssize * 2) {
                ssize *= 2;
            }
            ssize
        };
        for c in &mut info.comps {
            c.dct_h = k * ssize(max_h, c.h);
            c.dct_v = k * ssize(max_v, c.v);
            if c.dct_h > c.dct_v * 2 {
                c.dct_h = c.dct_v * 2;
            } else if c.dct_v > c.dct_h * 2 {
                c.dct_v = c.dct_h * 2;
            }
        }
        self.out_components = match self.out_cs {
            ColorSpace::Grayscale => 1,
            ColorSpace::Rgb | ColorSpace::BgRgb => 3,
            _ => info.comps.iter().filter(|c| c.needed).count(),
        };
    }

    /// `jpeg_start_decompress`.
    pub fn start(&mut self) -> R<()> {
        self.master_selection()?;
        if self.has_multiple_scans {
            while self.consume_input()? != Consumed::Eoi {}
        }
        self.output_scan_number = self.info.input_scan_number;
        self.prepare_for_output_pass()?;
        self.output_scanline = 0;
        Ok(())
    }

    /// `master_selection`.
    fn master_selection(&mut self) -> R<()> {
        if self.info.precision != 8 {
            return Err(Error::jpeg(format!(
                "Unsupported JPEG data precision {}",
                self.info.precision
            )));
        }
        self.calc_output_dimensions();
        if self.output_width == 0 || self.output_height == 0 || self.out_components == 0 {
            return Err(Error::jpeg("Empty JPEG image (DNL not supported)"));
        }
        self.init_color_deconverter()?;
        self.init_upsampler()?;

        let n = self.info.comps.len();
        self.entropy = Some(if self.info.arith_code {
            Entropy::Arith(Box::new(Arith::new()))
        } else {
            Entropy::Huff(Box::new(Huff::new()))
        });
        if self.info.progressive {
            self.info.coef_bits = vec![[-1; 64]; n];
        }

        if self.has_multiple_scans {
            let mut total = 0usize;
            for c in &self.info.comps {
                let blocks = round_up(c.width_in_blocks, c.h)
                    .saturating_mul(round_up(c.height_in_blocks, c.v));
                total = total.saturating_add(blocks.saturating_mul(128));
            }
            if total > MAX_COEFFICIENT_BYTES {
                return Err(Error::Limit(format!(
                    "jpeg coefficient buffer too large ({total} bytes)"
                )));
            }
            for c in &self.info.comps {
                let ww = round_up(c.width_in_blocks, c.h);
                let wh = round_up(c.height_in_blocks, c.v);
                self.whole.push(vec![[0; 64]; ww * wh]);
                self.whole_width.push(ww);
            }
        }

        for c in &self.info.comps {
            let stride = c.width_in_blocks * c.dct_h;
            let rows = if c.needed { c.v * c.dct_v } else { 0 };
            self.planes.push(vec![0; stride * rows]);
            self.plane_stride.push(stride);
        }
        self.rowgroups_avail = self.min_dct;

        self.start_input_pass()
    }

    /// `prepare_for_output_pass`.
    fn prepare_for_output_pass(&mut self) -> R<()> {
        // jddctmgr.c start_pass.
        self.methods.clear();
        for c in &mut self.info.comps {
            let Some(m) = Method::for_size(c.dct_h, c.dct_v) else {
                return Err(Error::jpeg(format!(
                    "DCT scaled block size {}x{} not supported",
                    c.dct_h, c.dct_v
                )));
            };
            self.methods.push(m);
            if c.needed
                && let Some(q) = c.quant
            {
                for (t, &v) in c.dct_table.iter_mut().zip(q.iter()) {
                    *t = i32::from(v);
                }
            }
        }
        // jdcoefct.c start_output_pass.
        if self.has_multiple_scans {
            self.smoothing = self.smoothing_ok();
        }
        self.output_imcu_row = 0;
        // jdsample.c start_pass_upsample.
        self.next_row_out = self.info.max_v;
        self.rows_to_go = self.output_height;
        // jdmainct.c start_pass_main.
        self.rowgroup_ctr = self.rowgroups_avail;
        Ok(())
    }

    // Coefficient control (jdcoefct.c).

    /// `start_iMCU_row`.
    fn start_imcu_row(&mut self) {
        let info = &self.info;
        self.mcu_rows_per_imcu_row = if info.comps_in_scan > 1 {
            1
        } else {
            let c = &info.comps[info.cur_comp[0]];
            if self.input_imcu_row + 1 < info.total_imcu_rows {
                c.v
            } else {
                c.last_row_height
            }
        };
        self.mcu_ctr = 0;
        self.mcu_vert_offset = 0;
    }

    /// `consume_data`: one iMCU row of a scan into the whole-image buffer.
    fn consume_data(&mut self) -> Consumed {
        let mut at = [(0usize, 0usize); 10];
        for yoffset in self.mcu_vert_offset..self.mcu_rows_per_imcu_row {
            for col in self.mcu_ctr..self.info.mcus_per_row {
                let mut b = 0;
                for i in 0..self.info.comps_in_scan {
                    let ci = self.info.cur_comp[i];
                    let c = &self.info.comps[ci];
                    let start_col = col * c.mcu_width;
                    for yindex in 0..c.mcu_height {
                        let row = self.input_imcu_row * c.v + yoffset + yindex;
                        for x in 0..c.mcu_width {
                            let idx = row * self.whole_width[ci] + start_col + x;
                            if b < 10 {
                                self.mcu[b] = self.whole[ci].get(idx).copied().unwrap_or([0; 64]);
                                at[b] = (ci, idx);
                                b += 1;
                            }
                        }
                    }
                }
                if let Some(e) = self.entropy.as_mut() {
                    e.decode_mcu(&mut self.info, &mut self.mcu[..b]);
                }
                for (blk, &(ci, idx)) in self.mcu[..b].iter().zip(&at[..b]) {
                    if let Some(dst) = self.whole[ci].get_mut(idx) {
                        *dst = *blk;
                    }
                }
            }
            self.mcu_ctr = 0;
        }
        self.input_imcu_row += 1;
        if self.input_imcu_row < self.info.total_imcu_rows {
            self.start_imcu_row();
            return Consumed::Row;
        }
        self.finish_input_pass();
        Consumed::Scan
    }

    /// Fills the sample planes with the next iMCU row (`decompress_data`
    /// and its variants).
    fn decompress(&mut self) -> R<()> {
        if !self.has_multiple_scans {
            self.decompress_onepass();
            return Ok(());
        }
        // Normally all input is consumed already; this is libjpeg's wait.
        while self.info.input_scan_number < self.output_scan_number
            || self.info.input_scan_number == self.output_scan_number
                && self.input_imcu_row <= self.output_imcu_row
        {
            if self.consume_input()? == Consumed::Eoi {
                break;
            }
        }
        if self.smoothing {
            self.decompress_smooth_data();
        } else {
            self.decompress_data();
        }
        Ok(())
    }

    /// `decompress_onepass`: decodes and transforms one iMCU row.
    fn decompress_onepass(&mut self) {
        let last_mcu_col = self.info.mcus_per_row.saturating_sub(1);
        let last_imcu_row = self.info.total_imcu_rows.saturating_sub(1);
        for yoffset in self.mcu_vert_offset..self.mcu_rows_per_imcu_row {
            for col in self.mcu_ctr..=last_mcu_col {
                let n = self.info.blocks_in_mcu.min(10);
                for b in &mut self.mcu[..n] {
                    *b = [0; 64];
                }
                if let Some(e) = self.entropy.as_mut() {
                    e.decode_mcu(&mut self.info, &mut self.mcu[..n]);
                }
                let mut b = 0;
                for i in 0..self.info.comps_in_scan {
                    let ci = self.info.cur_comp[i];
                    let c = &self.info.comps[ci];
                    if !c.needed {
                        b += c.mcu_blocks;
                        continue;
                    }
                    let useful = if col < last_mcu_col {
                        c.mcu_width
                    } else {
                        c.last_col_width
                    };
                    let start_col = col * c.mcu_sample_width;
                    let stride = self.plane_stride[ci];
                    let mut out_row = yoffset * c.dct_v;
                    for yindex in 0..c.mcu_height {
                        if self.input_imcu_row < last_imcu_row
                            || yoffset + yindex < c.last_row_height
                        {
                            let mut out_col = start_col;
                            for x in 0..useful {
                                if let Some(block) = self.mcu.get(b + x) {
                                    idct::inverse(
                                        self.methods[ci],
                                        block,
                                        &c.dct_table,
                                        &mut self.planes[ci],
                                        stride,
                                        out_row * stride + out_col,
                                    );
                                }
                                out_col += c.dct_h;
                            }
                            out_row += c.dct_v;
                        }
                        b += c.mcu_width;
                    }
                }
            }
            self.mcu_ctr = 0;
        }
        self.output_imcu_row += 1;
        self.input_imcu_row += 1;
        if self.input_imcu_row <= last_imcu_row {
            self.start_imcu_row();
        } else {
            self.finish_input_pass();
        }
    }

    /// Rows of blocks of component `c` in the current output iMCU row.
    fn block_rows(&self, c: &Component) -> usize {
        if self.output_imcu_row + 1 < self.info.total_imcu_rows {
            c.v
        } else {
            let t = c.height_in_blocks % c.v;
            if t == 0 { c.v } else { t }
        }
    }

    /// `decompress_data`: transforms one iMCU row from the whole image.
    fn decompress_data(&mut self) {
        for ci in 0..self.info.comps.len() {
            let c = &self.info.comps[ci];
            if !c.needed {
                continue;
            }
            let stride = self.plane_stride[ci];
            for br in 0..self.block_rows(c) {
                let row = self.output_imcu_row * c.v + br;
                for bn in 0..c.width_in_blocks {
                    let idx = row * self.whole_width[ci] + bn;
                    if let Some(block) = self.whole[ci].get(idx) {
                        idct::inverse(
                            self.methods[ci],
                            block,
                            &c.dct_table,
                            &mut self.planes[ci],
                            stride,
                            br * c.dct_v * stride + bn * c.dct_h,
                        );
                    }
                }
            }
        }
        self.output_imcu_row += 1;
    }

    /// `smoothing_ok`: whether some progressive coefficients are missing
    /// and every component can be smoothed.
    fn smoothing_ok(&mut self) -> bool {
        let info = &self.info;
        if !info.progressive || info.coef_bits.is_empty() {
            return false;
        }
        let mut useful = false;
        self.coef_bits_latch = vec![[0; 6]; info.comps.len()];
        for (ci, c) in info.comps.iter().enumerate() {
            let Some(q) = c.quant else {
                return false;
            };
            if [0, 1, 8, 16, 9, 2].iter().any(|&i| q[i] == 0) {
                return false;
            }
            let bits = &info.coef_bits[ci];
            if bits[0] < 0 {
                return false;
            }
            for (latch, &b) in self.coef_bits_latch[ci][1..].iter_mut().zip(&bits[1..=5]) {
                *latch = b;
                useful |= b != 0;
            }
        }
        useful
    }

    /// `decompress_smooth_data`: as `decompress_data`, predicting missing
    /// low-frequency coefficients from neighboring DC values.
    fn decompress_smooth_data(&mut self) {
        let last_imcu_row = self.info.total_imcu_rows.saturating_sub(1);
        for ci in 0..self.info.comps.len() {
            let c = &self.info.comps[ci];
            if !c.needed {
                continue;
            }
            let Some(q) = c.quant else {
                continue;
            };
            let block_rows = self.block_rows(c);
            let last_row = self.output_imcu_row >= last_imcu_row;
            let first_row = self.output_imcu_row == 0;
            let bits = self.coef_bits_latch[ci];
            let q = |i: usize| i64::from(q[i]);
            let (q00, q01, q10, q20, q11, q02) = (q(0), q(1), q(8), q(16), q(9), q(2));
            let stride = self.plane_stride[ci];
            let ww = self.whole_width[ci];
            let whole = &self.whole[ci];
            let dc = |row: usize, col: usize| -> i64 {
                whole.get(row * ww + col).map_or(0, |b| i64::from(b[0]))
            };
            for br in 0..block_rows {
                let cur = self.output_imcu_row * c.v + br;
                let prev = if first_row && br == 0 { cur } else { cur - 1 };
                let next = if last_row && br == block_rows - 1 {
                    cur
                } else {
                    cur + 1
                };
                let (mut dc1, mut dc2, mut dc3) = (dc(prev, 0), dc(prev, 0), dc(prev, 0));
                let (mut dc4, mut dc5, mut dc6) = (dc(cur, 0), dc(cur, 0), dc(cur, 0));
                let (mut dc7, mut dc8, mut dc9) = (dc(next, 0), dc(next, 0), dc(next, 0));
                let last_col = c.width_in_blocks.saturating_sub(1);
                for bn in 0..=last_col {
                    let mut ws = whole.get(cur * ww + bn).copied().unwrap_or([0; 64]);
                    if bn < last_col {
                        dc3 = dc(prev, bn + 1);
                        dc6 = dc(cur, bn + 1);
                        dc9 = dc(next, bn + 1);
                    }
                    let predict = |al: i32, num: i64, qv: i64| -> i16 {
                        let mut pred = if num >= 0 {
                            ((qv << 7) + num) / (qv << 8)
                        } else {
                            ((qv << 7) - num) / (qv << 8)
                        } as i32;
                        if al > 0 && pred >= 1 << al {
                            pred = (1 << al) - 1;
                        }
                        if num < 0 {
                            pred = -pred;
                        }
                        pred as i16
                    };
                    if bits[1] != 0 && ws[1] == 0 {
                        ws[1] = predict(bits[1], 36 * q00 * (dc4 - dc6), q01);
                    }
                    if bits[2] != 0 && ws[8] == 0 {
                        ws[8] = predict(bits[2], 36 * q00 * (dc2 - dc8), q10);
                    }
                    if bits[3] != 0 && ws[16] == 0 {
                        ws[16] = predict(bits[3], 9 * q00 * (dc2 + dc8 - 2 * dc5), q20);
                    }
                    if bits[4] != 0 && ws[9] == 0 {
                        ws[9] = predict(bits[4], 5 * q00 * (dc1 - dc3 - dc7 + dc9), q11);
                    }
                    if bits[5] != 0 && ws[2] == 0 {
                        ws[2] = predict(bits[5], 9 * q00 * (dc4 + dc6 - 2 * dc5), q02);
                    }
                    idct::inverse(
                        self.methods[ci],
                        &ws,
                        &c.dct_table,
                        &mut self.planes[ci],
                        stride,
                        br * c.dct_v * stride + bn * c.dct_h,
                    );
                    (dc1, dc2) = (dc2, dc3);
                    (dc4, dc5) = (dc5, dc6);
                    (dc7, dc8) = (dc8, dc9);
                }
            }
        }
        self.output_imcu_row += 1;
    }

    // Upsampling (jdsample.c).

    /// `jinit_upsampler`.
    fn init_upsampler(&mut self) -> R<()> {
        let (max_h, max_v) = (self.info.max_h, self.info.max_v);
        self.color_stride = round_up(self.output_width, max_h);
        for c in &self.info.comps {
            let h_in = c.h * c.dct_h / self.min_dct;
            let v_in = c.v * c.dct_v / self.min_dct;
            self.rowgroup_height.push(v_in);
            self.fullsize_row.push(0);
            let up = if !c.needed || h_in == max_h && v_in == max_v {
                Upsample::Fullsize
            } else if h_in * 2 == max_h && v_in == max_v {
                Upsample::H2V1
            } else if h_in * 2 == max_h && v_in * 2 == max_v {
                Upsample::H2V2
            } else if max_h % h_in == 0 && max_v % v_in == 0 {
                Upsample::Int {
                    h: max_h / h_in,
                    v: max_v / v_in,
                }
            } else {
                return Err(Error::jpeg("Fractional sampling not implemented yet"));
            };
            let buf = match up {
                Upsample::Fullsize => Vec::new(),
                _ => vec![0; self.color_stride * max_v],
            };
            self.upsample.push(up);
            self.color_buf.push(buf);
        }
        Ok(())
    }

    /// Upsamples row group `rowgroup_ctr` of component `ci`.
    fn upsample(&mut self, ci: usize) {
        let base = self.rowgroup_ctr * self.rowgroup_height[ci];
        let stride = self.plane_stride[ci];
        let (w, max_v, cs) = (self.output_width, self.info.max_v, self.color_stride);
        let plane = &self.planes[ci];
        let out = &mut self.color_buf[ci];
        let input = |r: usize| plane.get((base + r) * stride..(base + r + 1) * stride);
        let expand = |out: &mut Vec<u8>, row: usize, src: &[u8], h: usize| {
            if let Some(dst) = out.get_mut(row * cs..(row + 1) * cs) {
                for (chunk, &v) in dst[..round_up(w, h).min(cs)].chunks_mut(h).zip(src) {
                    chunk.fill(v);
                }
            }
        };
        let copy_row = |out: &mut Vec<u8>, from: usize, to: usize| {
            out.copy_within(from * cs..(from + 1) * cs, to * cs);
        };
        match self.upsample[ci] {
            Upsample::Fullsize => self.fullsize_row[ci] = base,
            Upsample::H2V1 => {
                for r in 0..max_v {
                    if let Some(src) = input(r) {
                        expand(out, r, src, 2);
                    }
                }
            }
            Upsample::H2V2 => {
                for r in (0..max_v).step_by(2) {
                    if let Some(src) = input(r / 2) {
                        expand(out, r, src, 2);
                    }
                    if r + 1 < max_v {
                        copy_row(out, r, r + 1);
                    }
                }
            }
            Upsample::Int { h, v } => {
                for (i, r) in (0..max_v).step_by(v).enumerate() {
                    if let Some(src) = input(i) {
                        expand(out, r, src, h);
                    }
                    for k in 1..v {
                        if r + k < max_v {
                            copy_row(out, r, r + k);
                        }
                    }
                }
            }
        }
    }

    /// Row `r` of component `ci`'s upsampled row group.
    fn comp_row(&self, ci: usize, r: usize) -> &[u8] {
        let w = self.output_width;
        let row = match self.upsample[ci] {
            Upsample::Fullsize => {
                let stride = self.plane_stride[ci];
                let at = (self.fullsize_row[ci] + r) * stride;
                self.planes[ci].get(at..at + w)
            }
            _ => {
                let at = r * self.color_stride;
                self.color_buf[ci].get(at..at + w)
            }
        };
        row.unwrap_or(&[])
    }

    // Color conversion (jdcolor.c).

    /// `jinit_color_deconverter`.
    fn init_color_deconverter(&mut self) -> R<()> {
        let n = self.info.comps.len();
        let ok = match self.jpeg_cs {
            ColorSpace::Grayscale => n == 1,
            ColorSpace::Rgb | ColorSpace::YCbCr | ColorSpace::BgRgb | ColorSpace::BgYcc => n == 3,
            ColorSpace::Cmyk | ColorSpace::Ycck => n == 4,
            ColorSpace::Unknown => n >= 1,
        };
        if !ok {
            return Err(Error::jpeg("Bogus JPEG colorspace"));
        }
        let notimpl = || Err(Error::jpeg("Unsupported color conversion request"));
        if self.info.subtract_green
            && (self.info.lse_maxtrans != 255
                || !matches!(self.jpeg_cs, ColorSpace::Rgb | ColorSpace::BgRgb))
        {
            return notimpl();
        }
        let green = self.info.subtract_green;
        self.convert = match (self.out_cs, self.jpeg_cs) {
            (ColorSpace::Grayscale, ColorSpace::Grayscale) => Convert::Gray,
            (ColorSpace::Grayscale, ColorSpace::YCbCr | ColorSpace::BgYcc) => {
                for c in &mut self.info.comps[1..] {
                    c.needed = false;
                }
                Convert::Gray
            }
            (ColorSpace::Rgb, ColorSpace::YCbCr) => Convert::Ycc(YccTables::new(false)),
            (ColorSpace::Rgb, ColorSpace::BgYcc) => Convert::Ycc(YccTables::new(true)),
            (ColorSpace::Rgb, ColorSpace::Rgb) | (ColorSpace::BgRgb, ColorSpace::BgRgb) => {
                if green {
                    Convert::SubtractGreen
                } else {
                    Convert::Rgb
                }
            }
            (ColorSpace::Grayscale | ColorSpace::Rgb | ColorSpace::BgRgb, _) => {
                return notimpl();
            }
            (ColorSpace::Cmyk, ColorSpace::Ycck) => Convert::Ycck(YccTables::new(false)),
            (out, jpeg) => {
                if out != jpeg {
                    return notimpl();
                }
                Convert::Null
            }
        };
        self.out_components = match self.out_cs {
            ColorSpace::Grayscale => 1,
            ColorSpace::Rgb | ColorSpace::BgRgb => 3,
            ColorSpace::Cmyk if matches!(self.convert, Convert::Ycck(_)) => 4,
            _ => self.info.comps.iter().filter(|c| c.needed).count(),
        };
        Ok(())
    }

    /// Converts the current row of the upsampled components into `out`.
    fn color_convert(&self, out: &mut [u8]) {
        let r = self.next_row_out;
        let w = self.output_width;
        match &self.convert {
            Convert::Gray => {
                let src = self.comp_row(0, r);
                let n = src.len().min(out.len());
                out[..n].copy_from_slice(&src[..n]);
            }
            Convert::Ycc(t) => {
                let (y, cb, cr) = (
                    self.comp_row(0, r),
                    self.comp_row(1, r),
                    self.comp_row(2, r),
                );
                for (((px, &y), &cb), &cr) in
                    out.as_chunks_mut::<3>().0.iter_mut().zip(y).zip(cb).zip(cr)
                {
                    let (y, cb, cr) = (i32::from(y), usize::from(cb), usize::from(cr));
                    px[0] = range_limit(y + t.cr_r[cr]);
                    px[1] = range_limit(y + ((t.cb_g[cb] + t.cr_g[cr]) >> 16) as i32);
                    px[2] = range_limit(y + t.cb_b[cb]);
                }
            }
            Convert::Rgb => {
                let (a, b, c) = (
                    self.comp_row(0, r),
                    self.comp_row(1, r),
                    self.comp_row(2, r),
                );
                for (((px, &a), &b), &c) in
                    out.as_chunks_mut::<3>().0.iter_mut().zip(a).zip(b).zip(c)
                {
                    px.copy_from_slice(&[a, b, c]);
                }
            }
            Convert::SubtractGreen => {
                let (a, b, c) = (
                    self.comp_row(0, r),
                    self.comp_row(1, r),
                    self.comp_row(2, r),
                );
                for (((px, &r), &g), &b) in
                    out.as_chunks_mut::<3>().0.iter_mut().zip(a).zip(b).zip(c)
                {
                    px[0] = r.wrapping_add(g).wrapping_sub(128);
                    px[1] = g;
                    px[2] = b.wrapping_add(g).wrapping_sub(128);
                }
            }
            Convert::Ycck(t) => {
                let (y, cb, cr, k) = (
                    self.comp_row(0, r),
                    self.comp_row(1, r),
                    self.comp_row(2, r),
                    self.comp_row(3, r),
                );
                for ((((px, &y), &cb), &cr), &k) in out
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(y)
                    .zip(cb)
                    .zip(cr)
                    .zip(k)
                {
                    let (y, cb, cr) = (i32::from(y), usize::from(cb), usize::from(cr));
                    px[0] = range_limit(255 - (y + t.cr_r[cr]));
                    px[1] = range_limit(255 - (y + ((t.cb_g[cb] + t.cr_g[cr]) >> 16) as i32));
                    px[2] = range_limit(255 - (y + t.cb_b[cb]));
                    px[3] = k;
                }
            }
            Convert::Null => {
                let n = self.out_components;
                let needed = self.info.comps.iter().enumerate().filter(|(_, c)| c.needed);
                for (pos, (ci, _)) in needed.enumerate() {
                    let src = self.comp_row(ci, r);
                    for (x, &v) in src.iter().enumerate().take(w) {
                        if let Some(d) = out.get_mut(x * n + pos) {
                            *d = v;
                        }
                    }
                }
            }
        }
    }

    /// `jpeg_read_scanlines` for one line: `out` holds `output_width *
    /// output_components` samples.
    pub fn read_scanline(&mut self, out: &mut [u8]) -> R<()> {
        if self.output_scanline >= self.output_height {
            return Ok(());
        }
        // process_data_simple_main
        if self.rowgroup_ctr >= self.rowgroups_avail {
            self.decompress()?;
            self.rowgroup_ctr = 0;
        }
        // sep_upsample
        if self.next_row_out >= self.info.max_v {
            for ci in 0..self.info.comps.len() {
                if self.info.comps[ci].needed {
                    self.upsample(ci);
                }
            }
            self.next_row_out = 0;
        }
        self.color_convert(out);
        self.rows_to_go -= 1;
        self.next_row_out += 1;
        if self.next_row_out >= self.info.max_v {
            self.rowgroup_ctr += 1;
        }
        self.output_scanline += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::Diagnostics;

    fn decode(data: &[u8]) -> (R<Vec<u8>>, Vec<String>) {
        let diag = Diagnostics::new();
        let result = (|| {
            let mut d = Decoder::new(Stream::memory(data, &diag));
            d.read_header()?;
            d.start()?;
            let stride = d.output_width() * d.output_components();
            let mut out = vec![0; stride * d.output_height()];
            for row in out.chunks_mut(stride.max(1)) {
                d.read_scanline(row)?;
            }
            Ok(out)
        })();
        let warnings = diag.snapshot().into_iter().map(|d| d.message).collect();
        (result, warnings)
    }

    /// A 16x16 baseline grayscale JPEG with every block's DC difference
    /// zero: Huffman tables with one code each, all blocks mid-gray.
    fn flat_gray() -> Vec<u8> {
        let mut j = vec![0xff, 0xd8];
        // DQT: table 0, all ones.
        j.extend([0xff, 0xdb, 0, 67, 0]);
        j.extend([1u8; 64]);
        // SOF0: 8-bit, 16x16, one component, 1x1, table 0.
        j.extend([0xff, 0xc0, 0, 11, 8, 0, 16, 0, 16, 1, 1, 0x11, 0]);
        // DHT: DC table 0 with one 1-bit code for category 0; AC table 0
        // with one 1-bit code for EOB.
        j.extend([0xff, 0xc4, 0, 20, 0x00, 1]);
        j.extend([0u8; 15]);
        j.push(0);
        j.extend([0xff, 0xc4, 0, 20, 0x10, 1]);
        j.extend([0u8; 15]);
        j.push(0);
        // SOS: one component, tables 0/0, full spectral range.
        j.extend([0xff, 0xda, 0, 8, 1, 1, 0x00, 0, 63, 0]);
        // Four blocks, each "DC 0, EOB": eight zero bits.
        j.push(0x00);
        j.extend([0xff, 0xd9]);
        j
    }

    #[test]
    fn decodes_a_flat_baseline_image() {
        let (out, warnings) = decode(&flat_gray());
        let out = out.unwrap_or_default();
        assert_eq!(out.len(), 256);
        assert!(out.iter().all(|&v| v == 128));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn truncated_data_warns_and_pads() {
        let mut j = flat_gray();
        j.truncate(j.len() - 3);
        let (out, warnings) = decode(&j);
        assert_eq!(out.map(|o| o.len()), Ok(256));
        assert!(
            warnings
                .iter()
                .any(|w| w == "premature end of file in jpeg")
        );
    }

    #[test]
    fn rejects_non_jpeg_data() {
        let (out, _) = decode(b"\x89PNG");
        assert_eq!(
            out,
            Err(Error::jpeg("Not a JPEG file: starts with 0x89 0x50"))
        );
    }

    #[test]
    fn missing_tables_are_libjpeg_errors() {
        let mut j = flat_gray();
        // Drop the DQT segment.
        j.drain(2..71);
        let (out, _) = decode(&j);
        assert_eq!(
            out,
            Err(Error::jpeg("Quantization table 0x00 was not defined"))
        );
    }

    /// Decodes as MuPDF's DCTDecode filter does (`dct::open`), returning
    /// the output's length and SHA-256, and the warnings.
    fn mupdf_decode(data: &[u8], color_transform: i32) -> (usize, String, Vec<String>) {
        use sha2::Digest;
        let diag = Diagnostics::new();
        let mut s = crate::dct::open(Stream::memory(data, &diag), color_transform);
        let out = s.read_best(data.len(), 0, usize::MAX).unwrap_or_default();
        let hash = sha2::Sha256::digest(&out)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let warnings = diag.snapshot().into_iter().map(|d| d.message).collect();
        (out.len(), hash, warnings)
    }

    macro_rules! fixture {
        ($name:literal) => {
            include_bytes!(concat!("../../tests/data/jpeg/", $name, ".jpg")).as_slice()
        };
    }

    /// The fixtures' 37x29 picture at quality 75 with 2x2 chroma, as MuPDF
    /// decodes it. Every lossless re-encoding of it (progressive,
    /// arithmetic, restart intervals, optimized tables, separate scans)
    /// must give these same samples.
    const Q75_420: &str = "2bca641120d3dfc7f3ea2964674e0b649986e0697044bcfad098ad3cb1239dac";

    #[test]
    fn every_entropy_coding_of_one_picture_decodes_as_in_mupdf() {
        let cases = [
            ("base420", fixture!("base420")),
            ("opt", fixture!("opt")),
            ("restart", fixture!("restart")),
            ("multiscan", fixture!("multiscan")),
            ("prog", fixture!("prog")),
            ("prog_restart", fixture!("prog_restart")),
            ("arith", fixture!("arith")),
            ("arith_restart", fixture!("arith_restart")),
            ("arith_prog", fixture!("arith_prog")),
            ("arith_prog_restart", fixture!("arith_prog_restart")),
        ];
        for (name, data) in cases {
            let (len, hash, warnings) = mupdf_decode(data, -1);
            assert_eq!((len, hash.as_str()), (37 * 29 * 3, Q75_420), "{name}");
            assert!(warnings.is_empty(), "{name}: {warnings:?}");
        }
    }

    #[test]
    fn sampling_factors_and_color_spaces_decode_as_in_mupdf() {
        let cases = [
            (
                "base422",
                fixture!("base422"),
                3219,
                "78a35d715eaf4ff2e3d62205c7e083eb01a00ca7de5e425addf6fadd24689991",
            ),
            (
                "base440",
                fixture!("base440"),
                3219,
                "ab1286e5040107d67f0009f867419094b6af46028cdfcad5317b270b266211eb",
            ),
            (
                "base444",
                fixture!("base444"),
                3219,
                "7e7036df9b5c69d83c593633eab95de496b2070b8198db42cf6336031730bc79",
            ),
            (
                "base411",
                fixture!("base411"),
                3219,
                "5e5dfdff742191a73ece8aa2f6b65bff07cec5b18e3b606072b6aae8ebabb842",
            ),
            (
                "base32",
                fixture!("base32"),
                3219,
                "af1e39069ce498e873655b5b153e799f918bf7f7ed0820e8dccc7bee46d9d079",
            ),
            (
                "mixed",
                fixture!("mixed"),
                3219,
                "99050daf8ff503d1e2ff86fde32802c9ae79b16c516570f925ef4c6ce6ad04b2",
            ),
            (
                "gray",
                fixture!("gray"),
                1073,
                "eec2c8e7d7122de348f0b341e53ea61ae6805bf529eeebc7f030f0f4e96c963d",
            ),
            (
                "prog_gray",
                fixture!("prog_gray"),
                1073,
                "eec2c8e7d7122de348f0b341e53ea61ae6805bf529eeebc7f030f0f4e96c963d",
            ),
            (
                "rgb",
                fixture!("rgb"),
                3219,
                "307b559681e5fb99215764106ace5be82010d52d66a21aea0c40806c67dd5243",
            ),
            (
                "cmyk",
                fixture!("cmyk"),
                4292,
                "ad444b520a4750c01a3c67fad461936dc10ca5f6a46ae911db143a053a24b062",
            ),
        ];
        for (name, data, want_len, want) in cases {
            let (len, hash, warnings) = mupdf_decode(data, -1);
            assert_eq!((len, hash.as_str()), (want_len, want), "{name}");
            assert!(warnings.is_empty(), "{name}: {warnings:?}");
        }
    }

    /// The marker segments before the first SOS (the SOS included): each
    /// one's marker, offset and total length.
    fn segments(j: &[u8]) -> Vec<(u8, usize, usize)> {
        let mut out: Vec<(u8, usize, usize)> = Vec::new();
        let mut i = 2;
        while i + 4 <= j.len() && j[i] == 0xff && out.last().is_none_or(|s| s.0 != 0xda) {
            let n = usize::from(j[i + 2]) << 8 | usize::from(j[i + 3]);
            out.push((j[i + 1], i, n + 2));
            i += n + 2;
        }
        out
    }

    /// Offset and total length of the first `marker` segment.
    fn segment(j: &[u8], marker: u8) -> (usize, usize) {
        let s = segments(j).into_iter().find(|s| s.0 == marker);
        s.map(|s| (s.1, s.2))
            .unwrap_or_else(|| panic!("no 0x{marker:02x} segment"))
    }

    fn at(j: &[u8], marker: u8) -> usize {
        segment(j, marker).0
    }

    /// `j` with `remove` bytes at `at` replaced by `insert`.
    fn splice(j: &[u8], at: usize, remove: usize, insert: &[u8]) -> Vec<u8> {
        let mut v = j[..at].to_vec();
        v.extend_from_slice(insert);
        v.extend_from_slice(&j[at + remove..]);
        v
    }

    fn set(j: &[u8], at: usize, bytes: &[u8]) -> Vec<u8> {
        splice(j, at, bytes.len(), bytes)
    }

    fn without(j: &[u8], marker: u8) -> Vec<u8> {
        let (at, len) = segment(j, marker);
        splice(j, at, len, &[])
    }

    /// `j` with its frame and first scan's component IDs replaced.
    fn with_ids(j: &[u8], ids: &[u8]) -> Vec<u8> {
        let mut v = j.to_vec();
        let sof = segments(j)
            .into_iter()
            .find(|s| (0xc0..=0xcf).contains(&s.0) && ![0xc4, 0xc8, 0xcc].contains(&s.0))
            .map_or(0, |s| s.1);
        let sos = at(j, 0xda);
        for (i, &id) in ids.iter().enumerate() {
            v[sof + 10 + 3 * i] = id;
            if i < usize::from(j[sos + 4]) {
                v[sos + 5 + 2 * i] = id;
            }
        }
        v
    }

    /// Offset of the `k`th restart marker in the entropy-coded data.
    fn rst(j: &[u8], k: usize) -> usize {
        let from = at(j, 0xda);
        (from..j.len() - 1)
            .filter(|&i| j[i] == 0xff && (0xd0..=0xd7).contains(&j[i + 1]))
            .nth(k)
            .unwrap_or(0)
    }

    /// The `subtract green` LSE marker libjpeg's encoder writes, for the
    /// components with these IDs (G, R, B order).
    fn lse(ids: [u8; 3]) -> Vec<u8> {
        let mut m = vec![0xff, 0xf8, 0, 24, 0x0d, 0, 255, 3, ids[0], ids[1], ids[2]];
        m.extend([0x80, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0]);
        m
    }

    /// Malformed, truncated and re-labelled variants of the fixtures, with
    /// the /ColorTransform each is decoded with.
    fn variants() -> Vec<(&'static str, Vec<u8>, i32)> {
        let base = fixture!("base420");
        let sof = at(base, 0xc0);
        let sos = at(base, 0xda);
        let dht = at(base, 0xc4);
        let dqt = at(base, 0xdb);
        let arith = fixture!("arith");
        let dac = at(arith, 0xcc);
        let rgb = fixture!("rgb");
        let rgb_sof = segment(rgb, 0xc0);
        let gray = fixture!("gray");
        let gray_sof = segment(gray, 0xc0);
        let cmyk = fixture!("cmyk");
        let prog = fixture!("prog");
        let r1b = fixture!("restart1b");
        let entropy = sos + segment(base, 0xda).1;
        // The chroma quantization table, with its first `count` entries.
        let chroma = |count: usize, wide: bool| {
            let q = &base[dqt + 69 + 5..dqt + 69 + 5 + count];
            let len = 3 + count * if wide { 2 } else { 1 };
            let mut seg = vec![0xff, 0xdb, (len >> 8) as u8, len as u8];
            seg.push(if wide { 0x11 } else { 0x01 });
            for &b in q {
                if wide {
                    seg.push(0);
                }
                seg.push(b);
            }
            splice(base, dqt + 69, 69, &seg)
        };
        let mut wide_dqt = vec![0xff, 0xdb, 0, 131, 0x10];
        for &b in &base[dqt + 5..dqt + 69] {
            wide_dqt.extend([0, b]);
        }
        let mut many = vec![0xff, 0xd8, 0xff, 0xc0, 0, 41, 8, 0, 8, 0, 8, 11];
        for id in 1..=11 {
            many.extend([id, 0x11, 0]);
        }
        many.extend([0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0, 0xff, 0xd9]);
        let mut lossy_tail = base[..entropy + 40].to_vec();
        lossy_tail.extend([0xaa; 30]);
        lossy_tail.extend_from_slice(&base[entropy + 70..]);
        let noise_base = fixture!("noise_base");
        let noise_prog = fixture!("noise_prog");
        let noise_arith = fixture!("noise_arith_prog");
        let stripes = fixture!("stripes_prog");
        let stripes_arith = fixture!("stripes_arith_prog");
        let mut arith_noise = arith[..at(arith, 0xda) + 14 + 30].to_vec();
        arith_noise.extend([0x5a; 40]);
        arith_noise.extend_from_slice(&arith[at(arith, 0xda) + 14 + 70..]);
        let second_scan = splice(base, base.len() - 2, 0, &base[sos..base.len() - 2]);
        vec![
            // Marker segments.
            ("two_soi", splice(base, 2, 0, &[0xff, 0xd8]), -1),
            ("two_sof", splice(base, dht, 0, &base[sof..sof + 19]), -1),
            ("empty_image", set(base, sof + 5, &[0, 0]), -1),
            ("sof_length", set(base, sof + 3, &[0x12]), -1),
            ("sof_duplicate_id", set(base, sof + 13, &[1]), -1),
            ("sof_extended", set(base, sof + 1, &[0xc1]), -1),
            ("sof_lossless", set(base, sof + 1, &[0xc3]), -1),
            ("sos_before_sof", without(base, 0xc0), -1),
            ("sos_bad_id", set(base, sos + 5, &[9]), -1),
            ("sos_duplicate_id", set(base, sos + 7, &[1]), -1),
            ("sos_length", set(base, sos + 3, &[0x0d]), -1),
            ("sos_no_components", set(base, sos + 2, &[0, 6, 0]), -1),
            ("dac_index", set(arith, dac + 4, &[40]), -1),
            ("dac_value", set(arith, dac + 5, &[0x05]), -1),
            ("dac_length", set(arith, dac + 3, &[0x09]), -1),
            ("dht_count", set(base, dht + 5, &[0xff]), -1),
            ("dht_index", set(base, dht + 4, &[0x05]), -1),
            ("dht_length", set(base, dht + 3, &[0x20]), -1),
            ("dht_oversubscribed", set(base, dht + 5, &[3, 0]), -1),
            (
                "dht_missing",
                {
                    let mut j = base.to_vec();
                    for _ in 0..4 {
                        j = without(&j, 0xc4);
                    }
                    j
                },
                -1,
            ),
            ("dht_undefined_slot", set(base, sos + 6, &[0x22]), -1),
            ("dqt_index", set(base, dqt + 4, &[0x05]), -1),
            ("dqt_length", set(base, dqt + 3, &[0x44]), -1),
            ("dqt_16bit", splice(base, dqt, 69, &wide_dqt), -1),
            ("dqt_4", chroma(4, false), -1),
            ("dqt_9", chroma(9, false), -1),
            ("dqt_16", chroma(16, false), -1),
            ("dqt_25", chroma(25, false), -1),
            ("dqt_36", chroma(36, false), -1),
            ("dqt_49", chroma(49, false), -1),
            ("dqt_16bit_9", chroma(9, true), -1),
            (
                "dri_length",
                splice(base, sos, 0, &[0xff, 0xdd, 0, 5, 0, 1, 0]),
                -1,
            ),
            (
                "unknown_marker",
                splice(base, dqt, 0, &[0xff, 0xf0, 0, 2]),
                -1,
            ),
            (
                "ignored_markers",
                splice(
                    base,
                    dqt,
                    0,
                    &[
                        0xff, 0xd0, 0xff, 0x01, 0xff, 0xe1, 0, 6, b'E', b'x', b'i', b'f', 0xff,
                        0xfe, 0, 4, b'h', b'i', 0xff, 0xdc, 0, 4, 0, 29, 0xff, 0xe0, 0, 4, 0, 0,
                    ],
                ),
                -1,
            ),
            (
                "garbage_between_markers",
                splice(base, dqt, 0, b"ab\xff\x00cd\xff\xff"),
                -1,
            ),
            ("no_image", vec![0xff, 0xd8, 0xff, 0xd9], -1),
            (
                "missing_sos",
                splice(base, dht, base.len() - dht, &[0xff, 0xd9]),
                -1,
            ),
            ("too_wide", set(base, sof + 7, &[0xff, 0xdd]), -1),
            ("too_many_components", many, -1),
            ("bogus_sampling", set(base, sof + 11, &[0x51]), -1),
            (
                "interleave_too_large",
                set(set(base, sof + 14, &[0x22]).as_slice(), sof + 17, &[0x22]),
                -1,
            ),
            (
                "fractional_sampling",
                set(set(base, sof + 11, &[0x31]).as_slice(), sof + 14, &[0x21]),
                -1,
            ),
            (
                "bad_spectral_end",
                set(set(base, sof + 1, &[0xc1]).as_slice(), sos + 12, &[40]),
                -1,
            ),
            ("baseline_short_spectrum", set(base, sos + 12, &[48]), -1),
            ("prog_bad_params", set(prog, at(prog, 0xda) + 12, &[5]), -1),
            (
                "prog_bad_approx",
                set(prog, at(prog, 0xda) + 13, &[0x31]),
                -1,
            ),
            ("second_scan", second_scan, -1),
            // LSE (JPEG-LS color transform) markers.
            ("lse_before_sof", splice(base, 2, 0, &lse([2, 1, 3])), -1),
            (
                "lse_gray",
                splice(gray, gray_sof.0 + gray_sof.1, 0, &lse([1, 1, 1])),
                -1,
            ),
            (
                "lse_rgb",
                splice(rgb, rgb_sof.0 + rgb_sof.1, 0, &lse(*b"GRB")),
                -1,
            ),
            ("lse_ycc", splice(base, sof + 19, 0, &lse([2, 1, 3])), -1),
            (
                "lse_length",
                splice(rgb, rgb_sof.0 + rgb_sof.1, 0, &set(&lse(*b"GRB"), 3, &[25])),
                -1,
            ),
            (
                "lse_id",
                splice(
                    rgb,
                    rgb_sof.0 + rgb_sof.1,
                    0,
                    &set(&lse(*b"GRB"), 4, &[0x0e]),
                ),
                -1,
            ),
            (
                "lse_maxtrans",
                splice(
                    rgb,
                    rgb_sof.0 + rgb_sof.1,
                    0,
                    &set(&lse(*b"GRB"), 6, &[127]),
                ),
                -1,
            ),
            (
                "lse_count",
                splice(rgb, rgb_sof.0 + rgb_sof.1, 0, &set(&lse(*b"GRB"), 7, &[2])),
                -1,
            ),
            (
                "lse_order",
                splice(rgb, rgb_sof.0 + rgb_sof.1, 0, &lse(*b"RGB")),
                -1,
            ),
            (
                "lse_params",
                splice(rgb, rgb_sof.0 + rgb_sof.1, 0, &set(&lse(*b"GRB"), 12, &[1])),
                -1,
            ),
            // Component IDs, JFIF and Adobe markers, and /ColorTransform.
            ("ct0", base.to_vec(), 0),
            ("ct1_rgb_ids", with_ids(&without(rgb, 0xee), b"RGB"), 1),
            ("ids_bg_ycc", with_ids(base, &[1, 0x22, 0x23]), -1),
            ("ids_bg_rgb", with_ids(base, b"rgb"), -1),
            ("ids_rgb_jfif", with_ids(base, b"RGB"), -1),
            ("ids_other_jfif", with_ids(base, &[4, 5, 6]), -1),
            ("ids_other_adobe0", with_ids(rgb, &[4, 5, 6]), -1),
            (
                "ids_other_plain",
                with_ids(&without(base, 0xe0), &[4, 5, 6]),
                -1,
            ),
            (
                "ycck_ids",
                with_ids(&set(cmyk, 17, &[2]), &[1, 2, 3, 4]),
                -1,
            ),
            (
                "ycck_adobe2",
                with_ids(&set(cmyk, 17, &[2]), &[5, 6, 7, 8]),
                -1,
            ),
            ("cmyk_adobe1_ct", set(cmyk, 17, &[1]), -1),
            (
                "cmyk_plain_ids",
                with_ids(&without(cmyk, 0xee), &[5, 6, 7, 8]),
                1,
            ),
            (
                "cmyk_plain_ycck_ids",
                with_ids(&without(cmyk, 0xee), &[1, 2, 3, 4]),
                1,
            ),
            (
                "two_components",
                {
                    let j = set(base, sof + 3, &[14]);
                    let j = set(&j, sof + 9, &[2]);
                    let j = splice(&j, sof + 16, 3, &[]);
                    let s = at(&j, 0xda);
                    let j = set(&j, s + 3, &[10]);
                    let j = set(&j, s + 4, &[2]);
                    splice(&j, s + 9, 2, &[])
                },
                -1,
            ),
            // Truncated and damaged entropy-coded data.
            ("base_truncated", base[..entropy + 150].to_vec(), -1),
            ("base_noise", lossy_tail, -1),
            (
                "base_early_eoi",
                splice(base, entropy + 60, 0, &[0xff, 0xd9]),
                -1,
            ),
            ("arith_truncated", arith[..arith.len() * 2 / 3].to_vec(), -1),
            ("arith_noise", arith_noise, -1),
            (
                "prog_after_dc",
                prog[..at(prog, 0xda) + 14 + 26].to_vec(),
                -1,
            ),
            ("prog_half", prog[..prog.len() / 2].to_vec(), -1),
            ("prog_most", prog[..prog.len() * 4 / 5].to_vec(), -1),
            ("prog_gray_half", fixture!("prog_gray")[..300].to_vec(), -1),
            (
                "arith_prog_half",
                fixture!("arith_prog")[..400].to_vec(),
                -1,
            ),
            (
                "arith_prog_most",
                fixture!("arith_prog")[..600].to_vec(),
                -1,
            ),
            ("restart1b", r1b.to_vec(), -1),
            ("rst_skipped_ahead", set(r1b, rst(r1b, 2) + 1, &[0xd4]), -1),
            ("rst_one_ahead", set(r1b, rst(r1b, 2) + 1, &[0xd3]), -1),
            ("rst_behind", set(r1b, rst(r1b, 2) + 1, &[0xd0]), -1),
            ("rst_far", set(r1b, rst(r1b, 2) + 1, &[0xd6]), -1),
            ("rst_removed", splice(r1b, rst(r1b, 2), 2, &[]), -1),
            ("rst_low_marker", set(r1b, rst(r1b, 2) + 1, &[0x01]), -1),
            ("rst_other_marker", set(r1b, rst(r1b, 2) + 1, &[0xc4]), -1),
            (
                "arith_rst_removed",
                {
                    let a = fixture!("arith_restart");
                    splice(a, rst(a, 1), 2, &[])
                },
                -1,
            ),
            (
                "arith_rst_wrong",
                {
                    let a = fixture!("arith_restart");
                    set(a, rst(a, 1) + 1, &[0xd5])
                },
                -1,
            ),
            (
                "prog_rst_removed",
                {
                    let p = fixture!("prog_restart");
                    splice(p, rst(p, 3), 2, &[])
                },
                -1,
            ),
            ("dht_table_5", set(base, sos + 6, &[0x55]), -1),
            ("dht_overfull", set(base, dht + 5, &[1, 0]), -1),
            ("dht_dc_symbol_16", set(base, dht + 21, &[16]), -1),
            (
                "huff_fill_bytes",
                splice(base, stuffed(base), 0, &[0xff]),
                -1,
            ),
            ("huff_bad_codes", noise(base, 40, &[0xff, 0x00], 12), -1),
            (
                "arith_fill_bytes",
                splice(arith, stuffed(arith), 0, &[0xff]),
                -1,
            ),
            ("arith_zeros", noise(arith, 30, &[0x00], 40), -1),
            ("arith_ones", noise(arith, 30, &[0xff, 0x00], 40), -1),
            ("arith_random", noise(arith, 30, &RANDOM, 1), -1),
            ("arith_conditioning", set(arith, dac + 5, &[0xff]), -1),
            ("arith_ac_conditioning", set(arith, dac + 7, &[0x01]), -1),
            ("prog_in_dc_scan", prog[..254].to_vec(), -1),
            ("prog_in_ac_first", prog[..319].to_vec(), -1),
            ("prog_in_ac_refine", prog[..841].to_vec(), -1),
            ("noise_base", noise_base.to_vec(), -1),
            ("noise_prog", noise_prog.to_vec(), -1),
            ("noise_arith_prog", noise_arith.to_vec(), -1),
            ("noise_prog_random", noise(noise_prog, 200, &RANDOM, 1), -1),
            (
                "noise_arith_random",
                noise(noise_arith, 150, &RANDOM, 1),
                -1,
            ),
            (
                "noise_arith_ones",
                noise(noise_arith, 150, &[0xff, 0x00], 30),
                -1,
            ),
            (
                "noise_arith_zeros",
                noise(noise_arith, 150, &[0x00], 60),
                -1,
            ),
            // Damage found by search to reach each of the arithmetic
            // decoder's bad-code exits: a DC magnitude past 2^15, an AC one,
            // an AC run past the band, and a refinement run past it.
            (
                "arith_dc_overflow",
                noise(arith, 6, &[0xef, 0x17, 0xb2, 0xda, 0x6d, 0x11], 1),
                -1,
            ),
            (
                "arith_ac_overflow",
                noise(arith, 24, &[0xc2, 0x53, 0x96, 0x6f, 0x19, 0x1a], 1),
                -1,
            ),
            (
                "arith_band_overrun",
                noise(arith, 129, &[0x25, 0x31, 0xa6, 0x3e, 0x24, 0xcc], 1),
                -1,
            ),
            (
                "arith_refine_overrun",
                noise(
                    fixture!("arith_prog"),
                    336,
                    &[0x5f, 0x76, 0x85, 0x9f, 0x0e, 0x67],
                    1,
                ),
                -1,
            ),
            ("stripes_prog", stripes.to_vec(), -1),
            ("stripes_arith_prog", stripes_arith.to_vec(), -1),
            ("stripes_prog_random", noise(stripes, 20, &RANDOM, 1), -1),
            (
                "stripes_arith_random",
                noise(stripes_arith, 15, &RANDOM, 1),
                -1,
            ),
            ("prog_in_ac_refine_early", prog[..801].to_vec(), -1),
            ("precision_7", set(base, sof + 4, &[7]), -1),
            (
                "dqt_length_1",
                splice(base, dqt, 0, &[0xff, 0xdb, 0, 1]),
                -1,
            ),
            (
                "prog_empty_scan_in_headers",
                splice(prog, at(prog, 0xda), 0, &[0xff, 0xda, 0, 6, 0, 0, 63, 0]),
                -1,
            ),
            (
                "prog_empty_scan_later",
                splice(prog, 270, 0, &[0xff, 0xda, 0, 6, 0, 0, 0, 0]),
                -1,
            ),
            (
                "prog_zero_quantizer",
                set(prog, dqt + 6, &[0])[..450].to_vec(),
                -1,
            ),
            ("sampling_1x3", fixture!("base13").to_vec(), -1),
            (
                "sampling_4x4_separate_scans",
                fixture!("sep44").to_vec(),
                -1,
            ),
            ("prog_separate_dc", fixture!("prog_sepdc").to_vec(), -1),
            (
                "prog_only_luma_dc",
                fixture!("prog_sepdc")[..228].to_vec(),
                -1,
            ),
            (
                "prog_no_chroma_ac",
                fixture!("prog_sepdc")[..568].to_vec(),
                -1,
            ),
            ("prog_in_ac_refine_late", prog[..881].to_vec(), -1),
            (
                "noise_prog_cut",
                noise_prog[..noise_prog.len() - 60].to_vec(),
                -1,
            ),
            (
                "stripes_prog_cut",
                stripes[..stripes.len() - 30].to_vec(),
                -1,
            ),
        ]
    }

    /// Deterministic bytes for damaging entropy-coded data.
    const RANDOM: [u8; 48] = [
        0x3a, 0x91, 0x0c, 0xe7, 0x55, 0x2b, 0xd8, 0x46, 0x7f, 0x13, 0xa2, 0x69, 0xc4, 0x08, 0xbd,
        0x5e, 0x97, 0x21, 0xf3, 0x6c, 0x0a, 0x84, 0x3d, 0xe9, 0x52, 0xb7, 0x1f, 0xc8, 0x64, 0x2e,
        0x99, 0x07, 0xd3, 0x4a, 0x76, 0xa5, 0x1c, 0xef, 0x38, 0x8b, 0x60, 0xd1, 0x05, 0x9e, 0x47,
        0xba, 0x2d, 0x73,
    ];

    /// `j` with `pattern`, `times` over, written over the entropy-coded
    /// data `skip` bytes into the first scan.
    fn noise(j: &[u8], skip: usize, pattern: &[u8], times: usize) -> Vec<u8> {
        let (sos, len) = segment(j, 0xda);
        let at = sos + len + skip;
        let mut bytes = pattern.repeat(times);
        bytes.truncate(j.len() - 2 - at);
        set(j, at, &bytes)
    }

    /// Offset of the first stuffed 0xff (0xff 0x00) in the entropy-coded
    /// data.
    fn stuffed(j: &[u8]) -> usize {
        let from = at(j, 0xda);
        (from..j.len() - 1)
            .find(|&i| j[i] == 0xff && j[i + 1] == 0)
            .unwrap_or(0)
    }

    /// MuPDF 1.28.6's DCTDecode output for each variant: length, the first
    /// 16 hex digits of its SHA-256, and its distinct warnings.
    #[rustfmt::skip]
    const MUPDF_VARIANTS: &[(&str, usize, &str, &[&str])] = &[
            ("arith_ac_conditioning", 3219, "47c89455dcc29b6f", &[]),
            ("arith_ac_overflow", 3219, "6d51df4d77fa640e", &[]),
            ("arith_band_overrun", 3219, "10a401e3519370ff", &[]),
            ("arith_conditioning", 3219, "3abf0ed9106766d3", &[]),
            ("arith_dc_overflow", 3219, "989a1ca7b393df80", &[]),
            ("arith_fill_bytes", 3219, "2bca641120d3dfc7", &[]),
            ("arith_noise", 3219, "7ed57574c7e686c2", &[]),
            ("arith_ones", 3219, "11acc98c77bed87b", &[]),
            ("arith_prog_half", 0, "e3b0c44298fc1c14", &["premature end of file in jpeg", "library error: jpeg error: Invalid component ID 255 in SOS", "read error; treating as end of file"]),
            ("arith_prog_most", 3219, "6660dcb78975668e", &["premature end of file in jpeg"]),
            ("arith_random", 3219, "b8c83578f737324e", &[]),
            ("arith_refine_overrun", 3219, "f8ab933eae9275c0", &[]),
            ("arith_rst_removed", 3219, "1533e36ed944023f", &[]),
            ("arith_rst_wrong", 3219, "2bca641120d3dfc7", &[]),
            ("arith_truncated", 3219, "3e54ab2b52ac1a9f", &["premature end of file in jpeg"]),
            ("arith_zeros", 3219, "e7e191dc1740ed2a", &[]),
            ("bad_spectral_end", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Invalid progressive parameters Ss=0 Se=40 Ah=0 Al=0", "read error; treating as end of file"]),
            ("base_early_eoi", 3219, "24430017cb0f80c0", &[]),
            ("base_noise", 3219, "4d666cb911b9be05", &[]),
            ("base_truncated", 3219, "b8bfe0a6e6222856", &["premature end of file in jpeg"]),
            ("baseline_short_spectrum", 3219, "2bca641120d3dfc7", &[]),
            ("bogus_sampling", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus sampling factors", "read error; treating as end of file"]),
            ("cmyk_adobe1_ct", 4292, "ad444b520a4750c0", &[]),
            ("dac_index", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus DAC index 40", "read error; treating as end of file"]),
            ("dac_length", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus marker length", "read error; treating as end of file"]),
            ("dac_value", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus DAC value 0x5", "read error; treating as end of file"]),
            ("dht_count", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus Huffman table definition", "read error; treating as end of file"]),
            ("dht_dc_symbol_16", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus Huffman table definition", "read error; treating as end of file"]),
            ("dht_index", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus DHT index 5", "read error; treating as end of file"]),
            ("dht_length", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus marker length", "read error; treating as end of file"]),
            ("dht_missing", 3219, "2bca641120d3dfc7", &[]),
            ("dht_overfull", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus Huffman table definition", "read error; treating as end of file"]),
            ("dht_oversubscribed", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus Huffman table definition", "read error; treating as end of file"]),
            ("dht_table_5", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Huffman table 0x05 was not defined", "read error; treating as end of file"]),
            ("dht_undefined_slot", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Huffman table 0x02 was not defined", "read error; treating as end of file"]),
            ("dqt_16", 3219, "b4f8deb24b19495c", &[]),
            ("dqt_16bit", 3219, "2bca641120d3dfc7", &[]),
            ("dqt_16bit_9", 3219, "4e2d78309e3dddb6", &[]),
            ("dqt_25", 3219, "99b02cfa03aa241e", &[]),
            ("dqt_36", 3219, "44d5e1d68ec6c2de", &[]),
            ("dqt_4", 3219, "30ff28204d02368a", &[]),
            ("dqt_49", 3219, "b88a8c0610d100bc", &[]),
            ("dqt_9", 3219, "4e2d78309e3dddb6", &[]),
            ("dqt_index", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus DQT index 5", "read error; treating as end of file"]),
            ("dqt_length", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus DQT index 15", "read error; treating as end of file"]),
            ("dqt_length_1", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus marker length", "read error; treating as end of file"]),
            ("dri_length", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus marker length", "read error; treating as end of file"]),
            ("empty_image", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Empty JPEG image (DNL not supported)", "read error; treating as end of file"]),
            ("fractional_sampling", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Fractional sampling not implemented yet", "read error; treating as end of file"]),
            ("garbage_between_markers", 3219, "2bca641120d3dfc7", &[]),
            ("huff_bad_codes", 3219, "a67111c87266f5c6", &[]),
            ("huff_fill_bytes", 3219, "2bca641120d3dfc7", &[]),
            ("ids_bg_rgb", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Unsupported color conversion request", "read error; treating as end of file"]),
            ("ids_bg_ycc", 3219, "a45da09de27c6742", &[]),
            ("ids_other_adobe0", 3219, "307b559681e5fb99", &[]),
            ("ids_other_jfif", 3219, "2bca641120d3dfc7", &[]),
            ("ids_other_plain", 3219, "2bca641120d3dfc7", &[]),
            ("ids_rgb_jfif", 3219, "ee04f1749097b5d7", &[]),
            ("ignored_markers", 3219, "2bca641120d3dfc7", &[]),
            ("interleave_too_large", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Sampling factors too large for interleaved scan", "read error; treating as end of file"]),
            ("lse_before_sof", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Invalid JPEG file structure: LSE before SOF", "read error; treating as end of file"]),
            ("lse_count", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Unsupported color conversion request", "read error; treating as end of file"]),
            ("lse_gray", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Unsupported color conversion request", "read error; treating as end of file"]),
            ("lse_id", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Unsupported marker type 0xf8", "read error; treating as end of file"]),
            ("lse_length", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus marker length", "read error; treating as end of file"]),
            ("lse_maxtrans", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Unsupported color conversion request", "read error; treating as end of file"]),
            ("lse_order", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Unsupported color conversion request", "read error; treating as end of file"]),
            ("lse_params", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Unsupported color conversion request", "read error; treating as end of file"]),
            ("lse_rgb", 3219, "d17ffe949fab0469", &[]),
            ("lse_ycc", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Unsupported color conversion request", "read error; treating as end of file"]),
            ("missing_sos", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Invalid JPEG file structure: missing SOS marker", "read error; treating as end of file"]),
            ("no_image", 0, "e3b0c44298fc1c14", &["library error: jpeg error: JPEG datastream contains no image", "read error; treating as end of file"]),
            ("noise_arith_ones", 384, "a4fcace339d1dc40", &[]),
            ("noise_arith_prog", 384, "9282aae9dbaa7a81", &[]),
            ("noise_arith_random", 384, "4ee70735654cbf7c", &[]),
            ("noise_arith_zeros", 384, "1835977e66015051", &[]),
            ("noise_base", 384, "9282aae9dbaa7a81", &[]),
            ("noise_prog", 384, "9282aae9dbaa7a81", &[]),
            ("noise_prog_cut", 0, "e3b0c44298fc1c14", &["premature end of file in jpeg", "library error: jpeg error: Invalid progressive parameters Ss=1 Se=63 Ah=15 Al=15", "read error; treating as end of file"]),
            ("noise_prog_random", 384, "33011cfea747eccc", &[]),
            ("precision_7", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Unsupported JPEG data precision 7", "read error; treating as end of file"]),
            ("prog_after_dc", 3219, "db76884731ebe319", &["premature end of file in jpeg"]),
            ("prog_bad_approx", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Invalid progressive parameters Ss=0 Se=0 Ah=3 Al=1", "read error; treating as end of file"]),
            ("prog_bad_params", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Invalid progressive parameters Ss=0 Se=5 Ah=0 Al=1", "read error; treating as end of file"]),
            ("prog_empty_scan_in_headers", 3219, "2bca641120d3dfc7", &[]),
            ("prog_empty_scan_later", 3219, "2bca641120d3dfc7", &[]),
            ("prog_gray_half", 1073, "299df05da530d9f8", &["premature end of file in jpeg"]),
            ("prog_half", 3219, "c6e8b747b93804a7", &["premature end of file in jpeg"]),
            ("prog_in_ac_first", 3219, "3f4c037bda932a53", &["premature end of file in jpeg"]),
            ("prog_in_ac_refine", 3219, "636c42d0b38d4433", &["premature end of file in jpeg"]),
            ("prog_in_ac_refine_early", 3219, "36646418538b21d5", &["premature end of file in jpeg"]),
            ("prog_in_ac_refine_late", 3219, "711bcf80c0f80b71", &["premature end of file in jpeg"]),
            ("prog_in_dc_scan", 3219, "091127d34b1ca89e", &["premature end of file in jpeg"]),
            ("prog_most", 3219, "8f615851cda3aaf3", &["premature end of file in jpeg"]),
            ("prog_no_chroma_ac", 3219, "254a23502a29319c", &["premature end of file in jpeg"]),
            ("prog_only_luma_dc", 3219, "d8abbc9103917d10", &["premature end of file in jpeg"]),
            ("prog_rst_removed", 3219, "58a2444e1762b029", &[]),
            ("prog_separate_dc", 3219, "2bca641120d3dfc7", &[]),
            ("prog_zero_quantizer", 3219, "854eb4acfec2cbaf", &["premature end of file in jpeg"]),
            ("restart1b", 3219, "2a5690246073ad80", &[]),
            ("rst_behind", 3219, "702569ea9aa8e2ce", &[]),
            ("rst_far", 3219, "2a5690246073ad80", &[]),
            ("rst_low_marker", 3219, "702569ea9aa8e2ce", &[]),
            ("rst_one_ahead", 3219, "11ed9ee19ed0759f", &[]),
            ("rst_other_marker", 3219, "471e7c2453abeea0", &[]),
            ("rst_removed", 3219, "702569ea9aa8e2ce", &[]),
            ("rst_skipped_ahead", 3219, "38087aa0b3d0f7bf", &[]),
            ("sampling_1x3", 3219, "dcbc9620d3d8db84", &[]),
            ("sampling_4x4_separate_scans", 3219, "d8cfdabe4491eea6", &[]),
            ("second_scan", 3219, "2bca641120d3dfc7", &[]),
            ("sof_duplicate_id", 3219, "2bca641120d3dfc7", &[]),
            ("sof_extended", 3219, "2bca641120d3dfc7", &[]),
            ("sof_length", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus marker length", "read error; treating as end of file"]),
            ("sof_lossless", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Unsupported JPEG process: SOF type 0xc3", "read error; treating as end of file"]),
            ("sos_bad_id", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Invalid component ID 9 in SOS", "read error; treating as end of file"]),
            ("sos_before_sof", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Invalid JPEG file structure: SOS before SOF", "read error; treating as end of file"]),
            ("sos_duplicate_id", 3219, "2bca641120d3dfc7", &[]),
            ("sos_length", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus marker length", "read error; treating as end of file"]),
            ("sos_no_components", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus marker length", "read error; treating as end of file"]),
            ("stripes_arith_prog", 256, "4f749d7a3fb61862", &[]),
            ("stripes_arith_random", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Invalid progressive parameters Ss=1 Se=58 Ah=9 Al=1", "read error; treating as end of file"]),
            ("stripes_prog", 256, "4f749d7a3fb61862", &[]),
            ("stripes_prog_cut", 0, "e3b0c44298fc1c14", &["premature end of file in jpeg", "library error: jpeg error: Bogus Huffman table definition", "read error; treating as end of file"]),
            ("stripes_prog_random", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Bogus Huffman table definition", "read error; treating as end of file"]),
            ("too_many_components", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Too many color components: 11, max 10", "read error; treating as end of file"]),
            ("too_wide", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Maximum supported image dimension is 65500 pixels", "read error; treating as end of file"]),
            ("two_components", 2146, "090d827cad5d0411", &[]),
            ("two_sof", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Invalid JPEG file structure: two SOF markers", "read error; treating as end of file"]),
            ("two_soi", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Invalid JPEG file structure: two SOI markers", "read error; treating as end of file"]),
            ("unknown_marker", 0, "e3b0c44298fc1c14", &["library error: jpeg error: Unsupported marker type 0xf0", "read error; treating as end of file"]),
            ("ycck_adobe2", 4292, "38dfe3d6cf8a1d5a", &[]),
            ("ycck_ids", 4292, "38dfe3d6cf8a1d5a", &[]),
            ("ct0", 3219, "ee04f1749097b5d7", &[]),
            ("cmyk_plain_ids", 4292, "ad444b520a4750c0", &[]),
            ("cmyk_plain_ycck_ids", 4292, "38dfe3d6cf8a1d5a", &[]),
            ("ct1_rgb_ids", 3219, "307b559681e5fb99", &[]),
    ];

    #[test]
    fn damaged_and_relabelled_variants_decode_as_in_mupdf() {
        // NUPDF_DUMP_JPEG_VARIANTS=<dir> writes the variants out, to be
        // checked against MuPDF with tests/data/oracle.py (see the README).
        let dump = std::env::var_os("NUPDF_DUMP_JPEG_VARIANTS");
        let mut wrong = Vec::new();
        for (name, data, ct) in variants() {
            if let Some(dir) = &dump {
                let path = std::path::Path::new(dir).join(format!("{name}.ct{ct}.jpg"));
                std::fs::write(path, &data).unwrap_or_else(|e| panic!("{e}"));
            }
            let Some(&(_, len, hash, warnings)) = MUPDF_VARIANTS.iter().find(|v| v.0 == name)
            else {
                wrong.push(format!("{name}: no MuPDF result"));
                continue;
            };
            let (got_len, got_hash, mut got_warnings) = mupdf_decode(&data, ct);
            got_warnings.dedup();
            let mut distinct: Vec<String> = Vec::new();
            for w in got_warnings {
                if !distinct.contains(&w) {
                    distinct.push(w);
                }
            }
            if (got_len, &got_hash[..16]) != (len, hash) || distinct != warnings {
                wrong.push(format!(
                    "{name}: got {got_len} {} {distinct:?}, MuPDF {len} {hash} {warnings:?}",
                    &got_hash[..16]
                ));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn damaged_variants_decode_scaled_down_to_size() {
        // Every variant decoded at a half, a quarter and an eighth: as
        // many samples as the scaled size has, or what a full decode gives
        // when that is nothing (the error is the same).
        for (name, data, ct) in variants() {
            let (full, _, _) = mupdf_decode(&data, ct);
            for l2 in 1..=3u32 {
                let diag = Diagnostics::new();
                let mut s = crate::dct::open_scaled(Stream::memory(&data, &diag), ct, l2);
                let out = s.read_bytewise(1 << 24).unwrap_or_default();
                if full == 0 {
                    assert!(out.is_empty(), "{name} at 1/{}", 1 << l2);
                } else {
                    assert!(
                        !out.is_empty() && out.len() <= full,
                        "{name} at 1/{}: {} of {full}",
                        1 << l2,
                        out.len()
                    );
                }
            }
        }
    }

    #[test]
    fn smartscale_block_sizes_are_unsupported() {
        // An extended-sequential scan ending at Se=48 asks for 7x7 blocks.
        // libjpeg 10 (so MuPDF) decodes these; this port reports them, as
        // the module documentation says.
        let base = fixture!("base420");
        let j = set(base, at(base, 0xc0) + 1, &[0xc1]);
        let j = set(&j, at(base, 0xda) + 12, &[48]);
        let (len, _, warnings) = mupdf_decode(&j, -1);
        assert_eq!(len, 0);
        assert_eq!(
            warnings,
            [
                "unsupported error: jpeg block size 7 (SmartScale)",
                "read error; treating as end of file",
            ]
        );
        // A progressive file's component-less SOS before its first scan
        // sets the block size too: Se=0 asks for 1x1 blocks. MuPDF accepts
        // that and then fails at the first AC scan ("Invalid progressive
        // parameters Ss=1 Se=5 Ah=0 Al=2"); both decode nothing.
        let prog = fixture!("prog");
        let j = splice(prog, at(prog, 0xda), 0, &[0xff, 0xda, 0, 6, 0, 0, 0, 0]);
        let (len, _, warnings) = mupdf_decode(&j, -1);
        assert_eq!(len, 0);
        assert_eq!(
            warnings,
            [
                "unsupported error: jpeg block size 1 (SmartScale)",
                "read error; treating as end of file",
            ]
        );
    }

    /// A source handing out `chunk` bytes at a time, as a filter upstream
    /// of DCTDecode does.
    struct Chunks<'a>(&'a [u8], usize);

    impl crate::stream::Source for Chunks<'_> {
        fn next(
            &mut self,
            _: usize,
            out: &mut Vec<u8>,
            _: &Diagnostics,
        ) -> Result<crate::stream::Next, String> {
            let n = self.1.min(self.0.len());
            out.extend_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(if n == 0 {
                crate::stream::Next::Eof
            } else {
                crate::stream::Next::Data
            })
        }
    }

    #[test]
    fn long_segments_are_skipped_across_reads() {
        // A 3000-byte APP1 segment read 100 bytes at a time decodes as
        // the file without it.
        let base = fixture!("base420");
        let mut app1 = vec![0xff, 0xe1, 0x0b, 0xba];
        app1.resize(3004, 0x55);
        let j = splice(base, at(base, 0xdb), 0, &app1);
        let diag = Diagnostics::new();
        let chain = Stream::new(Chunks(&j, 100), &diag);
        let mut s = crate::dct::open(chain, -1);
        let out = s.read_best(j.len(), 0, usize::MAX).unwrap_or_default();
        let whole = mupdf_decode(base, -1);
        let hash: String = {
            use sha2::Digest;
            sha2::Sha256::digest(&out)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        let msgs: Vec<String> = diag.snapshot().into_iter().map(|d| d.message).collect();
        assert_eq!((out.len(), hash, msgs), (whole.0, whole.1, vec![]));
    }

    #[test]
    fn the_decoder_api_reports_what_it_read() {
        let diag = Diagnostics::new();
        let base = fixture!("base420");
        let mut d = Decoder::new(Stream::memory(base, &diag));
        assert_eq!(d.read_header(), Ok(()));
        assert_eq!(d.jpeg_color_space(), ColorSpace::YCbCr);
        assert_eq!(d.adobe_transform(), None);
        assert_eq!(d.start(), Ok(()));
        assert_eq!(
            format!("{d:?}"),
            "Decoder { width: 37, height: 29, components: 3, scanline: 0 }"
        );
        let mut row = [0u8; 37 * 3];
        for _ in 0..29 {
            assert_eq!(d.read_scanline(&mut row), Ok(()));
        }
        // Reading past the last scanline leaves the buffer alone.
        row.fill(7);
        assert_eq!(d.read_scanline(&mut row), Ok(()));
        assert_eq!(d.output_scanline(), 29);
        assert!(row.iter().all(|&v| v == 7));

        // A stream without an image says so every time it is asked.
        let mut d = Decoder::new(Stream::memory(&[0xff, 0xd8, 0xff, 0xd9], &diag));
        let none = Err(Error::jpeg("JPEG datastream contains no image"));
        assert_eq!(d.read_header(), none);
        assert_eq!(d.read_header(), none);
    }

    #[test]
    fn coefficient_limits_reach_dct_as_limit_errors() {
        let mut j = vec![0xff, 0xd8, 0xff, 0xc2, 0, 20, 8, 0xff, 0xdc, 0xff, 0xdc, 4];
        for id in 1..=4 {
            j.extend([id, 0x11, 0]);
        }
        j.extend([0xff, 0xdb, 0, 67, 0]);
        j.extend([1u8; 64]);
        j.extend([0xff, 0xda, 0, 8, 1, 1, 0, 0, 0, 0]);
        let (len, _, warnings) = mupdf_decode(&j, -1);
        assert_eq!(len, 0);
        assert_eq!(
            warnings,
            [
                "limit error: jpeg coefficient buffer too large (34326192128 bytes)",
                "read error; treating as end of file",
            ]
        );
    }

    #[test]
    fn twelve_bit_data_is_unsupported_as_in_mupdf() {
        let (len, _, warnings) = mupdf_decode(fixture!("p12"), -1);
        assert_eq!(len, 0);
        assert_eq!(
            warnings,
            [
                "library error: jpeg error: Unsupported JPEG data precision 12",
                "read error; treating as end of file",
            ]
        );
    }

    #[test]
    fn huge_progressive_images_hit_the_limit() {
        // A progressive 65500x65500 four-component header.
        let mut j = vec![0xff, 0xd8, 0xff, 0xc2, 0, 20, 8, 0xff, 0xdc, 0xff, 0xdc, 4];
        for id in 1..=4 {
            j.extend([id, 0x11, 0]);
        }
        j.extend([0xff, 0xdb, 0, 67, 0]);
        j.extend([1u8; 64]);
        j.extend([0xff, 0xda, 0, 8, 1, 1, 0, 0, 0, 0]);
        let (out, _) = decode(&j);
        assert!(matches!(out, Err(Error::Limit(_))), "{out:?}");
    }
}
