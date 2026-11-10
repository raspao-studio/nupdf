// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! Writing side of the standard security handler, for the smoke fixtures.
//!
//! Deliberately separate from nupdf's decryption code: the fixtures are
//! checked against MuPDF, so a mistake shared by both sides would still show
//! up. Salts, IVs and file keys are derived from fixed seeds so the files are
//! reproducible.

use aes::cipher::{BlockCipherEncrypt, KeyInit};
use md5::{Digest, Md5};
use sha2::{Sha256, Sha384, Sha512};

const PADDING: [u8; 32] = [
    0x28, 0xbf, 0x4e, 0x5e, 0x4e, 0x75, 0x8a, 0x41, 0x64, 0x00, 0x4e, 0x56, 0xff, 0xfa, 0x01, 0x08,
    0x2e, 0x2e, 0x00, 0xb6, 0xd0, 0x68, 0x3e, 0x80, 0x2f, 0x0c, 0xa9, 0xfe, 0x64, 0x53, 0x69, 0x7a,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Rc4,
    AesV2,
    AesV3,
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub v: i32,
    pub r: i32,
    /// Key length in bits.
    pub bits: usize,
    pub method: Method,
    /// Use a /CF crypt filter (V4 and V5) rather than the V1/V2 defaults.
    pub crypt_filter: bool,
    pub user: &'static str,
    pub owner: &'static str,
    pub encrypt_metadata: bool,
}

pub struct Handler {
    spec: Spec,
    pub id: [u8; 16],
    key: Vec<u8>,
    o: Vec<u8>,
    u: Vec<u8>,
    oe: Vec<u8>,
    ue: Vec<u8>,
    perms: Vec<u8>,
    p: i32,
}

impl Handler {
    pub fn new(spec: Spec, seed: &str) -> Handler {
        let id: [u8; 16] = Md5::digest(format!("id {seed}")).into();
        let p: i32 = -4;
        let mut h = Handler {
            spec,
            id,
            key: Vec::new(),
            o: Vec::new(),
            u: Vec::new(),
            oe: Vec::new(),
            ue: Vec::new(),
            perms: Vec::new(),
            p,
        };
        if spec.r <= 4 {
            h.legacy_keys();
        } else {
            h.aes256_keys(seed);
        }
        h
    }

    /// Algorithms 2, 3, 4 and 5 (revisions 2 to 4).
    fn legacy_keys(&mut self) {
        let n = self.spec.bits / 8;
        let r = self.spec.r;

        // Algorithm 3: /O from the owner and user passwords.
        let mut okey: [u8; 16] = Md5::digest(pad(self.spec.owner)).into();
        if r >= 3 {
            for _ in 0..50 {
                okey = Md5::digest(&okey[..n]).into();
            }
        }
        let mut o = pad(self.spec.user).to_vec();
        rc4(&okey[..n], &mut o);
        if r >= 3 {
            for i in 1..=19u8 {
                let k: Vec<u8> = okey[..n].iter().map(|b| b ^ i).collect();
                rc4(&k, &mut o);
            }
        }
        self.o = o;

        // Algorithm 2: the file key.
        let mut md5 = Md5::new();
        md5.update(pad(self.spec.user));
        md5.update(&self.o);
        md5.update((self.p as u32).to_le_bytes());
        md5.update(self.id);
        if r >= 4 && !self.spec.encrypt_metadata {
            md5.update([0xff; 4]);
        }
        let mut key: [u8; 16] = md5.finalize().into();
        if r >= 3 {
            for _ in 0..50 {
                key = Md5::digest(&key[..n]).into();
            }
        }
        self.key = key[..n].to_vec();

        // Algorithms 4 and 5: /U.
        self.u = if r == 2 {
            let mut u = PADDING.to_vec();
            rc4(&self.key, &mut u);
            u
        } else {
            let mut md5 = Md5::new();
            md5.update(PADDING);
            md5.update(self.id);
            let mut u: Vec<u8> = md5.finalize().to_vec();
            rc4(&self.key, &mut u);
            for i in 1..=19u8 {
                let k: Vec<u8> = self.key.iter().map(|b| b ^ i).collect();
                rc4(&k, &mut u);
            }
            u.extend_from_slice(&PADDING[..16]);
            u
        };
    }

    /// Revision 5 (Adobe extension level 3) and 6 (ISO 32000-2).
    fn aes256_keys(&mut self, seed: &str) {
        let salt =
            |what: &str| -> Vec<u8> { Sha256::digest(format!("{what} {seed}"))[..8].to_vec() };
        self.key = Sha256::digest(format!("file key {seed}")).to_vec();
        let (uvs, uks, ovs, oks) = (salt("uv"), salt("uk"), salt("ov"), salt("ok"));
        let user = self.spec.user.as_bytes();
        let owner = self.spec.owner.as_bytes();

        let mut u = self.hash(user, &uvs, &[]);
        u.extend_from_slice(&uvs);
        u.extend_from_slice(&uks);
        let ue_key = self.hash(user, &uks, &[]);
        self.ue = aes256_cbc_no_iv(&ue_key, &self.key);

        let mut o = self.hash(owner, &ovs, &u);
        o.extend_from_slice(&ovs);
        o.extend_from_slice(&oks);
        let oe_key = self.hash(owner, &oks, &u);
        self.oe = aes256_cbc_no_iv(&oe_key, &self.key);
        self.u = u;
        self.o = o;

        let mut perms = [0u8; 16];
        perms[..4].copy_from_slice(&(self.p as u32).to_le_bytes());
        perms[4..8].copy_from_slice(&[0xff; 4]);
        perms[8] = if self.spec.encrypt_metadata {
            b'T'
        } else {
            b'F'
        };
        perms[9..12].copy_from_slice(b"adb");
        perms[12..].copy_from_slice(&Sha256::digest(format!("perms {seed}"))[..4]);
        let mut block = aes::Block::from(perms);
        if let Ok(c) = aes::Aes256::new_from_slice(&self.key) {
            c.encrypt_block(&mut block);
        }
        self.perms = block.to_vec();
    }

    /// The revision 5 or 6 password hash.
    fn hash(&self, password: &[u8], salt: &[u8], udata: &[u8]) -> Vec<u8> {
        let mut sha = Sha256::new();
        sha.update(password);
        sha.update(salt);
        sha.update(udata);
        let first: Vec<u8> = sha.finalize().to_vec();
        if self.spec.r == 5 {
            return first;
        }
        // ISO 32000-2 algorithm 2.B.
        let mut k = first;
        let mut round = 0usize;
        loop {
            let unit: Vec<u8> = [password, &k, udata].concat();
            let mut e = unit.repeat(64);
            let mut iv = [0u8; 16];
            iv.copy_from_slice(&k[16..32]);
            aes128_cbc(&k[..16], &mut iv, &mut e);
            let sum: u32 = e[..16].iter().map(|&b| u32::from(b)).sum();
            k = match sum % 3 {
                0 => Sha256::digest(&e).to_vec(),
                1 => Sha384::digest(&e).to_vec(),
                _ => Sha512::digest(&e).to_vec(),
            };
            round += 1;
            let last = usize::from(e[e.len() - 1]);
            if round >= 64 && round >= last + 32 {
                break;
            }
        }
        k.truncate(32);
        k
    }

    fn object_key(&self, num: u32, generation: u16) -> Vec<u8> {
        if self.spec.method == Method::AesV3 {
            return self.key.clone();
        }
        let mut md5 = Md5::new();
        md5.update(&self.key);
        md5.update(&num.to_le_bytes()[..3]);
        md5.update(generation.to_le_bytes());
        if self.spec.method == Method::AesV2 {
            md5.update(b"sAlT");
        }
        let digest = md5.finalize();
        digest[..(self.key.len() + 5).min(16)].to_vec()
    }

    /// Encrypts a string or stream of object `num`. AES data starts with an
    /// IV derived from the object number and the data's length.
    pub fn encrypt(&self, num: u32, data: &[u8]) -> Vec<u8> {
        let key = self.object_key(num, 0);
        match self.spec.method {
            Method::Rc4 => {
                let mut out = data.to_vec();
                rc4(&key, &mut out);
                out
            }
            Method::AesV2 | Method::AesV3 => {
                let mut iv = [0u8; 16];
                iv.copy_from_slice(&Md5::digest(format!("iv {num} {}", data.len())));
                let pad = 16 - data.len() % 16;
                let mut body = data.to_vec();
                body.extend(std::iter::repeat_n(pad as u8, pad));
                let mut chain = iv;
                aes_cbc(&key, &mut chain, &mut body);
                [&iv[..], &body].concat()
            }
        }
    }

    /// The /Encrypt dictionary.
    pub fn dict(&self) -> String {
        let s = &self.spec;
        let mut d = format!(
            "<</Filter/Standard/V {}/R {}/O <{}>/U <{}>/P {}",
            s.v,
            s.r,
            hex(&self.o),
            hex(&self.u),
            self.p
        );
        if s.v >= 2 {
            d += &format!("/Length {}", s.bits);
        }
        if s.crypt_filter {
            let cfm = match s.method {
                Method::Rc4 => "V2",
                Method::AesV2 => "AESV2",
                Method::AesV3 => "AESV3",
            };
            d += &format!(
                "/CF<</StdCF<</CFM/{cfm}/AuthEvent/DocOpen/Length {}>>>>/StmF/StdCF/StrF/StdCF",
                s.bits / 8
            );
        }
        if s.r >= 5 {
            d += &format!(
                "/OE <{}>/UE <{}>/Perms <{}>",
                hex(&self.oe),
                hex(&self.ue),
                hex(&self.perms)
            );
        }
        if !s.encrypt_metadata {
            d += "/EncryptMetadata false";
        }
        d + ">>"
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn pad(password: &str) -> [u8; 32] {
    let pw = password.as_bytes();
    let mut out = PADDING;
    let n = pw.len().min(32);
    out[..n].copy_from_slice(&pw[..n]);
    out[n..].copy_from_slice(&PADDING[..32 - n]);
    out
}

fn rc4(key: &[u8], data: &mut [u8]) {
    let mut s: Vec<u8> = (0..=255).collect();
    let mut j: u8 = 0;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, usize::from(j));
    }
    let (mut x, mut y) = (0u8, 0u8);
    for b in data {
        x = x.wrapping_add(1);
        y = y.wrapping_add(s[usize::from(x)]);
        s.swap(usize::from(x), usize::from(y));
        *b ^= s[usize::from(s[usize::from(x)].wrapping_add(s[usize::from(y)]))];
    }
}

fn aes_cbc(key: &[u8], iv: &mut [u8; 16], data: &mut [u8]) {
    match key.len() {
        16 => aes128_cbc(key, iv, data),
        _ => {
            if let Ok(c) = aes::Aes256::new_from_slice(key) {
                cbc(|b| c.encrypt_block(b), iv, data);
            }
        }
    }
}

fn aes128_cbc(key: &[u8], iv: &mut [u8; 16], data: &mut [u8]) {
    if let Ok(c) = aes::Aes128::new_from_slice(key) {
        cbc(|b| c.encrypt_block(b), iv, data);
    }
}

fn aes256_cbc_no_iv(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    if let Ok(c) = aes::Aes256::new_from_slice(key) {
        cbc(|b| c.encrypt_block(b), &mut [0u8; 16], &mut out);
    }
    out
}

fn cbc(encrypt: impl Fn(&mut aes::Block), iv: &mut [u8; 16], data: &mut [u8]) {
    for chunk in data.as_chunks_mut::<16>().0 {
        let mut plain = [0u8; 16];
        for (p, (c, v)) in plain.iter_mut().zip(chunk.iter().zip(iv.iter())) {
            *p = c ^ v;
        }
        let mut block = aes::Block::from(plain);
        encrypt(&mut block);
        chunk.copy_from_slice(&block);
        iv.copy_from_slice(&block);
    }
}
