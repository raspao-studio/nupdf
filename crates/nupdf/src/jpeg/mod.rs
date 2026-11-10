// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from libjpeg 10 jdapimin.c, jdapistd.c, jdmaster.c, jdinput.c,
// jdcoefct.c, jddctmgr.c, jdmainct.c, jdsample.c, jdcolor.c and jutils.c,
// as bundled with MuPDF 1.28.6, Copyright (C) 1991-1998, Thomas G. Lane.
// Modified 2002-2026 by Guido Vollbeding. This software is based in part on
// the work of the Independent JPEG Group.

//! A JPEG decoder that reproduces libjpeg 10, the decoder MuPDF bundles,
//! for the way MuPDF drives it: default parameters (integer IDCT, smoothing
//! of incomplete progressive images, full-size output), one scanline at a
//! time, from a source that never suspends.
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

    /// `jpeg_calc_output_dimensions`, at libjpeg's default scale.
    fn calc_output_dimensions(&mut self) {
        let info = &mut self.info;
        self.output_width = info.image_width;
        self.output_height = info.image_height;
        let (max_h, max_v) = (info.max_h, info.max_v);
        for c in &mut info.comps {
            // Scale subsampled components up in the IDCT where an exact 2:1
            // ratio allows it.
            c.dct_h = if max_h % (c.h * 2) == 0 { 16 } else { 8 };
            c.dct_v = if max_v % (c.v * 2) == 0 { 16 } else { 8 };
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
        self.rowgroups_avail = 8;

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
            let h_in = c.h * c.dct_h / 8;
            let v_in = c.v * c.dct_v / 8;
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
