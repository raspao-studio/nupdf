// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-crypt.c, source/fitz/crypt-arc4.c and
// source/fitz/filter-basic.c (the RC4 and AES decoding filters),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! The standard security handler, revisions 2 to 6: computing the file key
//! from a password, and decrypting strings and streams with RC4 or AES.
//!
//! MuPDF keeps one file key and overwrites it on every authentication
//! attempt, so after a failed attempt the key is whatever the last attempt
//! computed, and objects are "decrypted" with it anyway. `Crypt` keeps that
//! behavior: `authenticate` mutates the key, and decryption never checks
//! whether a password was accepted.

use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use md5::{Digest, Md5};
use sha2::{Sha256, Sha384, Sha512};

use crate::diag::{Diagnostics, report};
use crate::object::{Dict, Object};

/// `PDF_CRYPT_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    None,
    Rc4,
    AesV2,
    AesV3,
}

impl Method {
    /// `crypt_method`: the name MuPDF prints.
    pub fn name(self) -> &'static str {
        match self {
            Method::None => "None",
            Method::Rc4 => "RC4",
            Method::AesV2 | Method::AesV3 => "AES",
        }
    }
}

/// `pdf_crypt_filter`: a method and a key length in bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CryptFilter {
    pub method: Method,
    pub length: i32,
}

/// One entry of the /CF dictionary, resolved when the handler is created.
#[derive(Debug, Clone)]
struct CfEntry {
    name: Vec<u8>,
    /// Whether the entry is a dictionary; if not, the other fields are unset.
    is_dict: bool,
    cfm: Option<Vec<u8>>,
    length: Option<i32>,
}

/// `pdf_crypt`.
#[derive(Debug, Clone)]
pub struct Crypt {
    id: Vec<u8>,
    v: i32,
    length: i32,
    /// The /CF dictionary when it is one, and its object number.
    cf: Option<Vec<CfEntry>>,
    cf_num: i32,
    stmf: CryptFilter,
    strf: CryptFilter,
    r: i32,
    o: [u8; 48],
    u: [u8; 48],
    oe: [u8; 32],
    ue: [u8; 32],
    p: i32,
    encrypt_metadata: bool,
    /// 2 = user, 4 = owner, 6 = both (`pdf_authenticate_password`).
    access: u8,
    key: [u8; 32],
}

const PADDING: [u8; 32] = [
    0x28, 0xbf, 0x4e, 0x5e, 0x4e, 0x75, 0x8a, 0x41, 0x64, 0x00, 0x4e, 0x56, 0xff, 0xfa, 0x01, 0x08,
    0x2e, 0x2e, 0x00, 0xb6, 0xd0, 0x68, 0x3e, 0x80, 0x2f, 0x0c, 0xa9, 0xfe, 0x64, 0x53, 0x69, 0x7a,
];

impl Crypt {
    /// `pdf_new_crypt`. `id` is the trailer's /ID; `resolve` follows
    /// indirect references. Errors are MuPDF's report lines.
    pub fn new(
        dict: &Dict,
        id: &Object,
        resolve: &dyn Fn(&Object) -> Object,
        diag: &Diagnostics,
    ) -> Result<Crypt, String> {
        let get = |key: &[u8]| dict.get(key).map_or(Object::Null, resolve);
        let fail = |m: String| Err(report::format(&m));

        let filter = get(b"Filter");
        match filter.as_name() {
            None => return fail("unspecified encryption handler".into()),
            Some(b"Standard") => {}
            Some(other) => {
                return fail(format!(
                    "unknown encryption handler: '{}'",
                    String::from_utf8_lossy(other)
                ));
            }
        }

        let v = get(b"V").to_int_or(0);
        if ![0, 1, 2, 4, 5].contains(&v) {
            return fail("unknown encryption version".into());
        }

        let r = match get(b"R") {
            Object::Int(i) => i as i32,
            _ if v <= 4 => {
                diag.warn(
                    "crypt.guessed_revision",
                    "encryption dictionary missing revision value, guessing...",
                );
                match v {
                    0 | 1 => 2,
                    2 => 3,
                    _ => 4,
                }
            }
            _ => {
                return fail("encryption dictionary missing version and revision value".into());
            }
        };
        if !(1..=6).contains(&r) {
            return fail(format!("unknown crypt revision {r}"));
        }

        let mut o = [0u8; 48];
        match get(b"O") {
            Object::String(s) if s.len() == 32 => o[..32].copy_from_slice(&s),
            // /O and /U should be 48 bytes for revisions 5 and 6; often longer.
            Object::String(s) if r >= 5 && s.len() >= 48 => o.copy_from_slice(&s[..48]),
            _ => return fail("encryption dictionary missing owner password".into()),
        }
        let mut u = [0u8; 48];
        match get(b"U") {
            Object::String(s) if s.len() == 32 => u[..32].copy_from_slice(&s),
            Object::String(s) if r >= 5 && s.len() >= 48 => u.copy_from_slice(&s[..48]),
            Object::String(s) if s.len() < 32 => {
                diag.warn(
                    "crypt.short_key",
                    format!("encryption password key too short ({})", s.len()),
                );
                u[..s.len()].copy_from_slice(&s);
            }
            _ => return fail("encryption dictionary missing user password".into()),
        }

        let p = match get(b"P") {
            Object::Int(i) => i as i32,
            _ => {
                diag.warn(
                    "crypt.missing_permissions",
                    "encryption dictionary missing permissions",
                );
                0xffff_fffc_u32 as i32
            }
        };

        let mut oe = [0u8; 32];
        let mut ue = [0u8; 32];
        if r == 5 || r == 6 {
            match get(b"OE") {
                Object::String(s) if s.len() == 32 => oe.copy_from_slice(&s),
                _ => return fail("encryption dictionary missing owner encryption key".into()),
            }
            match get(b"UE") {
                Object::String(s) if s.len() == 32 => ue.copy_from_slice(&s),
                _ => return fail("encryption dictionary missing user encryption key".into()),
            }
        }

        let encrypt_metadata = match get(b"EncryptMetadata") {
            Object::Bool(b) => b,
            _ => true,
        };

        let mut file_id = Vec::new();
        match resolve(id).as_array() {
            Some(ids) if ids.len() == 2 => {
                if let Object::String(s) = resolve(&ids[0]) {
                    file_id = s.to_vec();
                }
            }
            _ => diag.warn(
                "crypt.missing_id",
                "missing file identifier, may not be able to do decryption",
            ),
        }

        let mut length = 40;
        if v == 2 || v == 4 {
            length = get(b"Length").to_int_or(length);
            // Some producers give the length in bytes.
            if length < 40 {
                length = length.wrapping_mul(8);
            }
            if length % 8 != 0 || !(40..=128).contains(&length) {
                return fail("invalid encryption key length".into());
            }
        }
        if v == 5 {
            length = 256;
        }

        let mut crypt = Crypt {
            id: file_id,
            v,
            length,
            cf: None,
            cf_num: 0,
            stmf: CryptFilter {
                method: Method::Rc4,
                length,
            },
            strf: CryptFilter {
                method: Method::Rc4,
                length,
            },
            r,
            o,
            u,
            oe,
            ue,
            p,
            encrypt_metadata,
            access: 0,
            key: [0; 32],
        };

        if v == 4 || v == 5 {
            let none = CryptFilter {
                method: Method::None,
                length,
            };
            crypt.stmf = none;
            crypt.strf = none;
            let cf_ref = dict.get(b"CF");
            if let Object::Dict(cf) = cf_ref.map_or(Object::Null, resolve) {
                crypt.cf_num = match cf_ref {
                    Some(Object::Ref(r)) => r.num,
                    _ => 0,
                };
                crypt.cf = Some(
                    cf.iter()
                        .map(|(name, value)| {
                            let value = resolve(value);
                            let sub = value.as_dict();
                            CfEntry {
                                name: name.to_vec(),
                                is_dict: sub.is_some(),
                                cfm: sub
                                    .and_then(|d| d.get(b"CFM"))
                                    .map(resolve)
                                    .and_then(|n| n.as_name().map(<[u8]>::to_vec)),
                                length: sub.and_then(|d| d.get(b"Length")).and_then(|l| {
                                    let l = resolve(l);
                                    l.is_number().then(|| l.to_int())
                                }),
                            }
                        })
                        .collect(),
                );
            }
            if let Some(name) = get(b"StmF").as_name() {
                crypt.stmf = crypt.parse_filter(name, diag)?;
            }
            if let Some(name) = get(b"StrF").as_name() {
                crypt.strf = crypt.parse_filter(name, diag)?;
            }
            // In revision 4 the crypt filter decides the key length.
            if crypt.strf.method != Method::None {
                crypt.length = crypt.stmf.length;
            }
        }
        Ok(crypt)
    }

    /// `pdf_parse_crypt_filter`.
    pub fn parse_filter(&self, name: &[u8], diag: &Diagnostics) -> Result<CryptFilter, String> {
        let is_identity = name == b"Identity";
        let is_std = !is_identity && name == b"StdCF";
        if !is_identity && !is_std {
            return Err(report::format(&format!(
                "Crypt Filter not Identity or StdCF ({} 0 R)",
                self.cf_num
            )));
        }
        let mut cf = CryptFilter {
            method: Method::None,
            length: self.length,
        };
        let Some(entries) = &self.cf else {
            cf.method = if is_identity {
                Method::None
            } else {
                Method::Rc4
            };
            return Ok(cf);
        };
        // pdf_dict_get: the last duplicate key wins, as in our Dict.
        match entries.iter().rev().find(|e| e.name == name) {
            Some(entry) if entry.is_dict => {
                if let Some(cfm) = &entry.cfm {
                    match cfm.as_slice() {
                        b"None" => cf.method = Method::None,
                        b"V2" => cf.method = Method::Rc4,
                        b"AESV2" => {
                            cf.method = Method::AesV2;
                            cf.length = 128;
                        }
                        b"AESV3" => {
                            cf.method = Method::AesV3;
                            cf.length = 256;
                        }
                        other => diag.warn(
                            "crypt.unknown_method",
                            format!(
                                "unknown encryption method: {}",
                                String::from_utf8_lossy(other)
                            ),
                        ),
                    }
                }
                if let Some(l) = entry.length {
                    cf.length = l;
                }
            }
            _ if !is_identity => {
                return Err(report::format(&format!(
                    "cannot parse crypt filter ({} 0 R)",
                    self.cf_num
                )));
            }
            _ => {}
        }

        if cf.method != Method::None {
            if self.r == 4 && cf.method != Method::Rc4 && cf.method != Method::AesV2 {
                diag.warn(
                    "crypt.unexpected_method",
                    format!(
                        "unexpected encryption method for revision 4 crypto: {}",
                        self.strf.method.name()
                    ),
                );
            } else if self.r >= 5 && cf.method != Method::AesV3 {
                diag.warn(
                    "crypt.illegal_method",
                    "illegal encryption method for revision 5/6, assuming AESV3",
                );
                cf.method = Method::AesV3;
            }
        }

        // Crypt filter lengths are supposed to be in bytes.
        if cf.length < 40 {
            cf.length = cf.length.wrapping_mul(8);
        }
        if cf.length % 8 != 0 {
            return Err(report::format(&format!(
                "invalid key length: {}",
                cf.length
            )));
        }
        if (1..=4).contains(&self.r) && !(40..=128).contains(&cf.length) {
            return Err(report::format(&format!(
                "invalid key length: {}",
                cf.length
            )));
        }
        if (self.r == 5 || self.r == 6) && cf.length != 256 {
            diag.warn(
                "crypt.illegal_length",
                "illegal key length for revision 5/6, assuming 256 bits",
            );
            cf.length = 256;
        }
        Ok(cf)
    }

    pub fn version(&self) -> i32 {
        self.v
    }

    pub fn revision(&self) -> i32 {
        self.r
    }

    pub fn length(&self) -> i32 {
        self.length
    }

    pub fn permissions(&self) -> i32 {
        self.p
    }

    pub fn encrypt_metadata(&self) -> bool {
        self.encrypt_metadata
    }

    pub fn string_method(&self) -> Method {
        self.strf.method
    }

    pub fn stream_method(&self) -> Method {
        self.stmf.method
    }

    /// What the last `authenticate` granted: 2 user, 4 owner, 6 both.
    pub fn access(&self) -> u8 {
        self.access
    }

    /// MuPDF's `FZ_META_ENCRYPTION` string.
    pub fn description(&self) -> String {
        let (stm, str_) = (self.stmf.method.name(), self.strf.method.name());
        if stm == str_ {
            format!(
                "Standard V{} R{} {}-bit {}",
                self.v, self.r, self.length, str_
            )
        } else {
            format!(
                "Standard V{} R{} {}-bit streams: {} strings: {}",
                self.v, self.r, self.length, stm, str_
            )
        }
    }

    fn key_bytes(&self) -> usize {
        (self.length / 8).clamp(0, 16) as usize
    }

    /// Algorithm 2 (PDF 1.7 algorithm 3.2): the file key from a password,
    /// written to the first `key_bytes()` bytes of the key.
    fn compute_key(&mut self, password: &[u8]) {
        let n = self.key_bytes();
        let mut md5 = Md5::new();
        md5.update(padded(password));
        md5.update(&self.o[..32]);
        md5.update((self.p as u32).to_le_bytes());
        md5.update(&self.id);
        if self.r >= 4 && !self.encrypt_metadata {
            md5.update([0xff; 4]);
        }
        let mut buf: [u8; 16] = md5.finalize().into();
        if self.r >= 3 {
            for _ in 0..50 {
                buf = Md5::digest(&buf[..n]).into();
            }
        }
        self.key[..n].copy_from_slice(&buf[..n]);
    }

    /// Algorithm 2.A (revision 5): validates against /O or /U, then
    /// decrypts /OE or /UE into the key.
    fn compute_key_r5(&mut self, password: &[u8], owner: bool) -> Result<[u8; 32], String> {
        let pw = &password[..password.len().min(127)];
        let hash = |salt: &[u8]| -> [u8; 32] {
            let mut sha = Sha256::new();
            sha.update(pw);
            sha.update(salt);
            if owner {
                sha.update(self.u);
            }
            sha.finalize().into()
        };
        let (validation_salt, key_salt) = if owner {
            (&self.o[32..40], &self.o[40..48])
        } else {
            (&self.u[32..40], &self.u[40..48])
        };
        let validation = hash(validation_salt);
        let mut buffer = [0u8; 32];
        buffer.copy_from_slice(&hash(key_salt));
        let aes = Aes::new(&buffer, self.length)
            .ok_or_else(|| report::format(&format!("aes invalid key size ({})", self.length)))?;
        let mut key = if owner { self.oe } else { self.ue };
        aes.cbc_decrypt(&mut [0; 16], &mut key);
        self.key = key;
        Ok(validation)
    }

    /// Revision 6 (ISO 32000-2 algorithm 2.B).
    fn compute_key_r6(&mut self, password: &[u8], owner: bool) -> [u8; 32] {
        let pw = &password[..password.len().min(127)];
        let (salts, extra) = if owner {
            (&self.o, Some(&self.u))
        } else {
            (&self.u, None)
        };
        let validation = hardened_hash(pw, &salts[32..40], extra);
        let hash = hardened_hash(pw, &salts[40..48], extra);
        let mut key = if owner { self.oe } else { self.ue };
        if let Some(aes) = Aes::new(&hash, 256) {
            aes.cbc_decrypt(&mut [0; 16], &mut key);
        }
        self.key = key;
        validation
    }

    /// Algorithms 4 and 5: the /U value a password would produce. Also sets
    /// the key. Returns `None` for revisions MuPDF does not compute.
    fn compute_user_password(&mut self, password: &[u8]) -> Result<Option<[u8; 32]>, String> {
        let n = self.key_bytes();
        match self.r {
            2 => {
                self.compute_key(password);
                let mut out = PADDING;
                Arc4::new(&self.key[..n]).apply(&mut out);
                Ok(Some(out))
            }
            3 | 4 => {
                self.compute_key(password);
                let mut md5 = Md5::new();
                md5.update(PADDING);
                md5.update(&self.id);
                let mut out = [0u8; 32];
                out[..16].copy_from_slice(&md5.finalize());
                Arc4::new(&self.key[..n]).apply(&mut out[..16]);
                for x in 1..=19u8 {
                    let xor: Vec<u8> = self.key[..n].iter().map(|k| k ^ x).collect();
                    Arc4::new(&xor).apply(&mut out[..16]);
                }
                out[16..].copy_from_slice(&PADDING[..16]);
                Ok(Some(out))
            }
            5 => self.compute_key_r5(password, false).map(Some),
            6 => Ok(Some(self.compute_key_r6(password, false))),
            _ => Ok(None),
        }
    }

    /// Algorithm 6.
    fn authenticate_user(&mut self, password: &[u8]) -> Result<bool, String> {
        let Some(out) = self.compute_user_password(password)? else {
            return Ok(false);
        };
        Ok(match self.r {
            2 | 5 | 6 => out[..32] == self.u[..32],
            3 | 4 => out[..16] == self.u[..16],
            _ => false,
        })
    }

    /// Algorithm 7: recovers the user password from /O and tries it.
    fn authenticate_owner(&mut self, password: &[u8]) -> Result<bool, String> {
        let n = self.key_bytes();
        match self.r {
            2..=4 => {
                let mut key: [u8; 16] = Md5::digest(padded(password)).into();
                if self.r >= 3 {
                    for _ in 0..50 {
                        key = Md5::digest(&key[..n]).into();
                    }
                }
                let mut user = [0u8; 32];
                user.copy_from_slice(&self.o[..32]);
                if self.r == 2 {
                    Arc4::new(&key[..n]).apply(&mut user);
                } else {
                    for x in 0..20u8 {
                        let xor: Vec<u8> = key[..n].iter().map(|k| k ^ (19 - x)).collect();
                        Arc4::new(&xor).apply(&mut user);
                    }
                }
                self.authenticate_user(&user)
            }
            5 => Ok(self.compute_key_r5(password, true)? == self.o[..32]),
            6 => Ok(self.compute_key_r6(password, true) == self.o[..32]),
            _ => Ok(false),
        }
    }

    /// `pdf_authenticate_password`: tries `password` as the user and the
    /// owner password and returns the access granted. The key is left as
    /// MuPDF leaves it, whether or not either password worked. Fails only
    /// where MuPDF throws (an AES key of an invalid size, revision 5).
    pub fn authenticate(&mut self, password: &str) -> Result<u8, String> {
        let mut pw: Vec<u8> = if self.r <= 4 {
            crate::text_string::pdf_doc_encode(password)
        } else {
            password.as_bytes().to_vec()
        };
        // A C string of at most 2047 bytes.
        pw.truncate(2047);
        if let Some(nul) = pw.iter().position(|&b| b == 0) {
            pw.truncate(nul);
        }

        self.access = 0;
        if self.authenticate_user(&pw)? {
            self.access = 2;
        }
        if self.authenticate_owner(&pw)? {
            self.access |= 4;
        } else if self.access & 2 != 0 {
            // The failed owner attempt replaced the key; recompute it.
            self.authenticate_user(&pw)?;
        }
        // As in Acrobat, an empty owner password only counts when the user
        // password is also empty.
        if pw.is_empty() && self.access == 4 {
            self.access = 0;
        }
        Ok(self.access)
    }

    /// Algorithm 1: the key for one object's strings or streams.
    fn object_key(&self, cf: &CryptFilter, num: i32, generation: i32) -> Vec<u8> {
        let key_len = usize::try_from(self.length / 8).unwrap_or(0).min(32);
        // Version 0 uses the file key directly, as does AES-256.
        if self.v == 0 || cf.method == Method::AesV3 {
            return self.key[..key_len].to_vec();
        }
        let mut md5 = Md5::new();
        md5.update(&self.key[..key_len]);
        md5.update([
            num as u8,
            (num >> 8) as u8,
            (num >> 16) as u8,
            generation as u8,
            (generation >> 8) as u8,
        ]);
        if cf.method == Method::AesV2 {
            md5.update(b"sAlT");
        }
        let digest = md5.finalize();
        digest[..(key_len + 5).min(16)].to_vec()
    }

    /// `pdf_crypt_obj`: decrypts every string in `obj` (not following
    /// references, and skipping signature /Contents). On an AES key error
    /// the strings decrypted so far stay decrypted, as in MuPDF, and the
    /// error is returned with the object.
    pub fn decrypt_object(
        &self,
        obj: Object,
        num: i32,
        generation: i32,
        diag: &Diagnostics,
    ) -> (Object, Option<String>) {
        let key = self.object_key(&self.strf, num, generation);
        let mut error = None;
        let out = self.decrypt_strings(obj, &key, diag, &mut error);
        (out, error)
    }

    fn decrypt_strings(
        &self,
        obj: Object,
        key: &[u8],
        diag: &Diagnostics,
        error: &mut Option<String>,
    ) -> Object {
        if error.is_some() {
            return obj;
        }
        match obj {
            Object::String(s) => Object::String(self.decrypt_string(&s, key, diag, error).into()),
            Object::Array(items) => Object::Array(std::sync::Arc::new(
                std::sync::Arc::unwrap_or_clone(items)
                    .into_iter()
                    .map(|o| self.decrypt_strings(o, key, diag, error))
                    .collect(),
            )),
            Object::Dict(d) => {
                let signature = is_signature(&d);
                let mut d = std::sync::Arc::unwrap_or_clone(d);
                d.map_values(|k, v| {
                    if signature && k == b"Contents" {
                        v
                    } else {
                        self.decrypt_strings(v, key, diag, error)
                    }
                });
                Object::Dict(std::sync::Arc::new(d))
            }
            Object::Stream(s) => {
                let mut s = std::sync::Arc::unwrap_or_clone(s);
                let dict = std::mem::take(&mut s.dict);
                if let Object::Dict(d) =
                    self.decrypt_strings(Object::Dict(std::sync::Arc::new(dict)), key, diag, error)
                {
                    s.dict = std::sync::Arc::unwrap_or_clone(d);
                }
                Object::Stream(std::sync::Arc::new(s))
            }
            other => other,
        }
    }

    fn decrypt_string(
        &self,
        s: &[u8],
        key: &[u8],
        diag: &Diagnostics,
        error: &mut Option<String>,
    ) -> Vec<u8> {
        let mut out = s.to_vec();
        match self.strf.method {
            Method::Rc4 => Arc4::new(key).apply(&mut out),
            Method::AesV2 | Method::AesV3 => {
                let n = out.len();
                if n == 0 {
                    // Empty strings are allowed.
                } else if !n.is_multiple_of(16) || n < 32 {
                    diag.warn(
                        "crypt.bad_string_length",
                        "invalid string length for aes encryption",
                    );
                } else {
                    let Some(aes) = Aes::new(key, key.len() as i32 * 8) else {
                        *error = Some(report::format(&format!(
                            "AES key init failed (keylen={})",
                            key.len() * 8
                        )));
                        return out;
                    };
                    let mut iv = [0u8; 16];
                    iv.copy_from_slice(&out[..16]);
                    // Decrypted in place over the IV; the last 16 bytes keep
                    // the final ciphertext block until the length is cut.
                    let mut body = out[16..].to_vec();
                    aes.cbc_decrypt(&mut iv, &mut body);
                    out[..n - 16].copy_from_slice(&body);
                    let pad = out[n - 17];
                    if !(1..=16).contains(&pad) {
                        diag.warn("crypt.bad_padding", "aes padding out of range");
                    } else {
                        out.truncate(n - 16 - usize::from(pad));
                    }
                }
            }
            Method::None => {}
        }
        out
    }

    /// The stream crypt filter (`pdf_open_crypt`) applied to a whole stream.
    /// Fails only where opening the filter fails (an invalid AES key size).
    pub fn decrypt_stream(
        &self,
        data: &[u8],
        num: i32,
        generation: i32,
    ) -> Result<Decrypted, String> {
        self.decrypt_with(&self.stmf, data, num, generation)
    }

    /// `pdf_open_crypt_with_filter`: a /Crypt filter naming a crypt filter.
    pub fn decrypt_with_filter(
        &self,
        name: &[u8],
        data: &[u8],
        num: i32,
        generation: i32,
        diag: &Diagnostics,
    ) -> Result<Decrypted, String> {
        if name == b"Identity" {
            return Ok(Decrypted::complete(data.to_vec()));
        }
        let cf = self.parse_filter(name, diag)?;
        self.decrypt_with(&cf, data, num, generation)
    }

    fn decrypt_with(
        &self,
        cf: &CryptFilter,
        data: &[u8],
        num: i32,
        generation: i32,
    ) -> Result<Decrypted, String> {
        let key = self.object_key(cf, num, generation);
        match cf.method {
            Method::Rc4 => {
                let mut out = data.to_vec();
                Arc4::new(&key).apply(&mut out);
                Ok(Decrypted::complete(out))
            }
            Method::AesV2 | Method::AesV3 => aes_decrypt_stream(&key, data),
            Method::None => Ok(Decrypted::complete(data.to_vec())),
        }
    }
}

/// A decrypted stream. MuPDF's AES filter can fail partway: then `data` is
/// what it produced before the failing block, and `error` is what a reader
/// that wants more gets, followed by end of data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decrypted {
    pub data: Vec<u8>,
    pub error: Option<String>,
    /// AES output reaches the next filter one byte per read.
    pub byte_fed: bool,
}

impl Decrypted {
    fn complete(data: Vec<u8>) -> Decrypted {
        Decrypted {
            data,
            error: None,
            byte_fed: false,
        }
    }
}

/// `fz_open_aesd` read to the end: a 16-byte IV, whole blocks, and padding
/// stripped from the last block. A short IV, a partial block or bad padding
/// stops it right before the offending block.
fn aes_decrypt_stream(key: &[u8], data: &[u8]) -> Result<Decrypted, String> {
    let bits = key.len() as i32 * 8;
    let Some(aes) = Aes::new(key, bits) else {
        return Err(format!("argument error: aes invalid key size ({bits})"));
    };
    let stop = |data: Vec<u8>, msg: String| Decrypted {
        data,
        error: Some(report::format(&msg)),
        byte_fed: true,
    };
    if data.len() < 16 {
        return Ok(stop(Vec::new(), "premature end in aes filter".into()));
    }
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&data[..16]);
    let body = &data[16..];
    let whole = body.len() / 16 * 16;
    let mut out = body[..whole].to_vec();
    aes.cbc_decrypt(&mut iv, &mut out);
    if whole < body.len() {
        return Ok(stop(out, "partial block in aes filter".into()));
    }
    if let Some(&pad) = out.last() {
        if !(1..=16).contains(&pad) {
            out.truncate(out.len() - 16);
            return Ok(stop(out, format!("aes padding out of range: {pad}")));
        }
        out.truncate(out.len() - usize::from(pad));
    }
    Ok(Decrypted {
        data: out,
        error: None,
        byte_fed: true,
    })
}

/// `is_signature`: a direct /Type /Sig with /Contents, /ByteRange and
/// /Filter, whose /Contents are not encrypted.
fn is_signature(d: &Dict) -> bool {
    d.get(b"Type").is_some_and(|t| t.is_name(b"Sig"))
        && d.get(b"Contents").is_some()
        && d.get(b"ByteRange").is_some()
        && d.get(b"Filter").is_some()
}

/// The password truncated or padded to 32 bytes (algorithm 2, step a).
fn padded(password: &[u8]) -> [u8; 32] {
    let mut buf = PADDING;
    let n = password.len().min(32);
    buf[..n].copy_from_slice(&password[..n]);
    buf[n..].copy_from_slice(&PADDING[..32 - n]);
    buf
}

/// `pdf_compute_hardened_hash_r6`.
fn hardened_hash(password: &[u8], salt: &[u8], owner_u: Option<&[u8; 48]>) -> [u8; 32] {
    let mut block = [0u8; 64];
    let mut sha = Sha256::new();
    sha.update(password);
    sha.update(salt);
    if let Some(u) = owner_u {
        sha.update(u);
    }
    block[..32].copy_from_slice(&sha.finalize());
    let mut block_size = 32;
    let mut data: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < 64 || i < usize::from(data.last().copied().unwrap_or(0)) + 32 {
        let mut unit = Vec::with_capacity(password.len() + block_size + 48);
        unit.extend_from_slice(password);
        unit.extend_from_slice(&block[..block_size]);
        if let Some(u) = owner_u {
            unit.extend_from_slice(u);
        }
        data = unit.repeat(64);
        let mut iv = [0u8; 16];
        iv.copy_from_slice(&block[16..32]);
        if let Some(aes) = Aes::new(&block[..16], 128) {
            aes.cbc_encrypt(&mut iv, &mut data);
        }
        let sum: u32 = data[..16].iter().map(|&b| u32::from(b)).sum();
        block_size = 32 + (sum % 3) as usize * 16;
        match block_size {
            32 => block[..32].copy_from_slice(&Sha256::digest(&data)),
            48 => block[..48].copy_from_slice(&Sha384::digest(&data)),
            _ => block.copy_from_slice(&Sha512::digest(&data)),
        }
        i += 1;
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&block[..32]);
    hash
}

/// RC4 as `fz_arc4`.
#[derive(Debug, Clone)]
pub struct Arc4 {
    state: [u8; 256],
    x: u8,
    y: u8,
}

impl Arc4 {
    pub fn new(key: &[u8]) -> Arc4 {
        let mut state = [0u8; 256];
        for (i, s) in state.iter_mut().enumerate() {
            *s = i as u8;
        }
        let mut j: u8 = 0;
        for i in 0..256 {
            let k = if key.is_empty() {
                0
            } else {
                key[i % key.len()]
            };
            j = j.wrapping_add(k).wrapping_add(state[i]);
            state.swap(i, usize::from(j));
        }
        Arc4 { state, x: 0, y: 0 }
    }

    pub fn apply(&mut self, data: &mut [u8]) {
        for b in data {
            self.x = self.x.wrapping_add(1);
            let sx = self.state[usize::from(self.x)];
            self.y = sx.wrapping_add(self.y);
            let sy = self.state[usize::from(self.y)];
            self.state[usize::from(self.y)] = sx;
            self.state[usize::from(self.x)] = sy;
            *b ^= self.state[usize::from(sx.wrapping_add(sy))];
        }
    }
}

/// AES with a 128, 192 or 256-bit key, in CBC mode.
enum Aes {
    A128(aes::Aes128),
    A192(aes::Aes192),
    A256(aes::Aes256),
}

impl Aes {
    /// `fz_aes_setkey_*`: `bits` must be 128, 192 or 256, and `key` at
    /// least that long.
    fn new(key: &[u8], bits: i32) -> Option<Aes> {
        let n = usize::try_from(bits / 8).ok()?;
        let key = key.get(..n)?;
        match bits {
            128 => aes::Aes128::new_from_slice(key).ok().map(Aes::A128),
            192 => aes::Aes192::new_from_slice(key).ok().map(Aes::A192),
            256 => aes::Aes256::new_from_slice(key).ok().map(Aes::A256),
            _ => None,
        }
    }

    fn decrypt_block(&self, b: &mut aes::Block) {
        match self {
            Aes::A128(c) => c.decrypt_block(b),
            Aes::A192(c) => c.decrypt_block(b),
            Aes::A256(c) => c.decrypt_block(b),
        }
    }

    fn encrypt_block(&self, b: &mut aes::Block) {
        match self {
            Aes::A128(c) => c.encrypt_block(b),
            Aes::A192(c) => c.encrypt_block(b),
            Aes::A256(c) => c.encrypt_block(b),
        }
    }

    /// Decrypts whole blocks in place, updating `iv`.
    fn cbc_decrypt(&self, iv: &mut [u8; 16], data: &mut [u8]) {
        for chunk in data.as_chunks_mut::<16>().0 {
            let cipher = *chunk;
            let mut block = aes::Block::from(cipher);
            self.decrypt_block(&mut block);
            for (o, (p, v)) in chunk.iter_mut().zip(block.iter().zip(iv.iter())) {
                *o = p ^ v;
            }
            *iv = cipher;
        }
    }

    /// Encrypts whole blocks in place, updating `iv`.
    fn cbc_encrypt(&self, iv: &mut [u8; 16], data: &mut [u8]) {
        for chunk in data.as_chunks_mut::<16>().0 {
            let mut plain = [0u8; 16];
            for (p, (c, v)) in plain.iter_mut().zip(chunk.iter().zip(iv.iter())) {
                *p = c ^ v;
            }
            let mut block = aes::Block::from(plain);
            self.encrypt_block(&mut block);
            chunk.copy_from_slice(&block);
            iv.copy_from_slice(&block);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0))
            .collect()
    }

    #[test]
    fn rc4_matches_rfc_6229() {
        // RFC 6229, 40-bit key 0x0102030405, keystream offset 0.
        let mut ks = [0u8; 16];
        Arc4::new(&hex("0102030405")).apply(&mut ks);
        assert_eq!(ks.to_vec(), hex("b2396305f03dc027ccc3524a0a1118a8"));
    }

    #[test]
    fn aes_cbc_matches_nist() {
        // NIST SP 800-38A, F.2.1 / F.2.2 (CBC-AES128), first two blocks.
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hex("000102030405060708090a0b0c0d0e0f");
        let plain = hex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
        let cipher = hex("7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2");
        let aes = Aes::new(&key, 128).map(|a| {
            let mut data = plain.clone();
            let mut v = [0u8; 16];
            v.copy_from_slice(&iv);
            a.cbc_encrypt(&mut v, &mut data);
            assert_eq!(data, cipher);
            v.copy_from_slice(&iv);
            a.cbc_decrypt(&mut v, &mut data);
            assert_eq!(data, plain);
        });
        assert!(aes.is_some());
        assert!(Aes::new(&key, 80).is_none());
    }

    #[test]
    fn aes_streams_strip_padding_or_fail_like_mupdf() {
        let key = [7u8; 16];
        let aes = Aes::new(&key, 128);
        let encrypt = |plain: &[u8]| {
            let mut data = plain.to_vec();
            let mut iv = [9u8; 16];
            if let Some(a) = &aes {
                a.cbc_encrypt(&mut iv, &mut data);
            }
            [&[9u8; 16][..], &data].concat()
        };
        let mut padded = b"hello".to_vec();
        padded.extend([11u8; 11]);
        let run = |data: &[u8]| aes_decrypt_stream(&key, data).map(|d| (d.data.len(), d.error));
        assert_eq!(
            aes_decrypt_stream(&key, &encrypt(&padded)).map(|d| d.data),
            Ok(b"hello".to_vec())
        );
        assert_eq!(run(&[9u8; 16]), Ok((0, None)));
        assert_eq!(
            run(&[0u8; 10]),
            Ok((0, Some("format error: premature end in aes filter".into())))
        );
        // A partial block stops after the whole blocks.
        let mut partial = encrypt(&padded);
        partial.push(0);
        assert_eq!(
            run(&partial),
            Ok((16, Some("format error: partial block in aes filter".into())))
        );
        // Bad padding stops before the last block.
        let bad = encrypt(&[[1u8; 16], [0u8; 16]].concat());
        assert_eq!(
            run(&bad),
            Ok((16, Some("format error: aes padding out of range: 0".into())))
        );
        assert!(aes_decrypt_stream(&key[..5], &bad).is_err());
    }
}
