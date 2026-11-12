// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! RSA signature verification (PKCS #1 v1.5), the public-key operation
//! only, for checking signatures in PDF files. Everything here is public
//! data, so nothing needs to run in constant time.

/// A natural number, 32-bit limbs, least significant first, no high zeros.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Nat(Vec<u32>);

/// Moduli larger than this are refused (16384 bits).
const MAX_BYTES: usize = 2048;
/// Public exponents with more bits than this are refused.
const MAX_EXPONENT_BITS: usize = 64;

impl Nat {
    fn from_be(bytes: &[u8]) -> Nat {
        let mut limbs: Vec<u32> = bytes
            .rchunks(4)
            .map(|c| c.iter().fold(0u32, |v, &b| (v << 8) | u32::from(b)))
            .collect();
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        Nat(limbs)
    }

    fn to_be(&self, len: usize) -> Option<Vec<u8>> {
        let mut out: Vec<u8> = self.0.iter().rev().flat_map(|l| l.to_be_bytes()).collect();
        let lead = out.iter().take_while(|&&b| b == 0).count();
        out.drain(..lead);
        if out.len() > len {
            return None;
        }
        let mut padded = vec![0; len - out.len()];
        padded.extend(out);
        Some(padded)
    }

    fn bits(&self) -> usize {
        self.0.last().map_or(0, |&top| {
            (self.0.len() - 1) * 32 + (32 - top.leading_zeros() as usize)
        })
    }

    fn bit(&self, i: usize) -> bool {
        self.0.get(i / 32).is_some_and(|l| l >> (i % 32) & 1 == 1)
    }

    fn mul(&self, b: &Nat) -> Nat {
        let mut out = vec![0u32; self.0.len() + b.0.len()];
        for (i, &x) in self.0.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &y) in b.0.iter().enumerate() {
                let t = u64::from(x) * u64::from(y) + u64::from(out[i + j]) + carry;
                out[i + j] = t as u32;
                carry = t >> 32;
            }
            out[i + b.0.len()] = carry as u32;
        }
        let mut n = Nat(out);
        n.trim();
        n
    }

    fn trim(&mut self) {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
    }

    fn cmp(&self, b: &Nat) -> std::cmp::Ordering {
        self.0
            .len()
            .cmp(&b.0.len())
            .then_with(|| self.0.iter().rev().cmp(b.0.iter().rev()))
    }

    /// `self -= b`, where `b <= self`.
    fn sub_assign(&mut self, b: &Nat) {
        let mut borrow = 0i64;
        for i in 0..self.0.len() {
            let t = i64::from(self.0[i]) - i64::from(b.0.get(i).copied().unwrap_or(0)) - borrow;
            self.0[i] = t.rem_euclid(1 << 32) as u32;
            borrow = i64::from(t < 0);
        }
        self.trim();
    }

    /// `self mod m`, by shifting and subtracting a bit at a time.
    fn rem(&self, m: &Nat) -> Nat {
        let mut r = Nat(Vec::new());
        for i in (0..self.bits()).rev() {
            // r = 2r + bit
            let mut carry = u32::from(self.bit(i));
            for l in &mut r.0 {
                let next = *l >> 31;
                *l = (*l << 1) | carry;
                carry = next;
            }
            if carry != 0 {
                r.0.push(carry);
            }
            if r.cmp(m) != std::cmp::Ordering::Less {
                r.sub_assign(m);
            }
        }
        r
    }

    fn pow_mod(&self, e: &Nat, m: &Nat) -> Nat {
        let mut result = Nat(vec![1]).rem(m);
        let base = self.rem(m);
        for i in (0..e.bits()).rev() {
            result = result.mul(&result).rem(m);
            if e.bit(i) {
                result = result.mul(&base).rem(m);
            }
        }
        result
    }
}

/// `RSA_public_decrypt` with PKCS #1 v1.5 signature padding: what the
/// signature `sig` carries under the key (`n`, `e`), or None where it is
/// not a well-formed signature for that key.
pub fn recover(n: &[u8], e: &[u8], sig: &[u8]) -> Option<Vec<u8>> {
    let n = Nat::from_be(n);
    let e = Nat::from_be(e);
    let k = n.bits().div_ceil(8);
    if k == 0 || k > MAX_BYTES || e.bits() > MAX_EXPONENT_BITS || sig.len() != k {
        return None;
    }
    let s = Nat::from_be(sig);
    if s.cmp(&n) != std::cmp::Ordering::Less {
        return None;
    }
    let em = s.pow_mod(&e, &n).to_be(k)?;
    // 00 01 FF..FF 00 payload, eight bytes of padding at least.
    if em.first() != Some(&0) || em.get(1) != Some(&1) {
        return None;
    }
    let ff = em[2..].iter().take_while(|&&b| b == 0xff).count();
    if ff < 8 || em.get(2 + ff) != Some(&0) {
        return None;
    }
    Some(em[3 + ff..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_holds() {
        let a = Nat::from_be(&[0x12, 0x34, 0x56, 0x78, 0x9a]);
        assert_eq!(a.to_be(5).unwrap(), vec![0x12, 0x34, 0x56, 0x78, 0x9a]);
        let m = Nat::from_be(&[0x01, 0x00, 0x01]);
        // 0x123456789a mod 65537 = 78187493530 mod 65537
        let r = a.rem(&m);
        assert_eq!(r, Nat::from_be(&(78_187_493_530u64 % 65_537).to_be_bytes()));
        // 3^200 mod 1000003
        let p = Nat::from_be(&[3]).pow_mod(
            &Nat::from_be(&[200]),
            &Nat::from_be(&1_000_003u32.to_be_bytes()),
        );
        let mut want = 1u64;
        for _ in 0..200 {
            want = want * 3 % 1_000_003;
        }
        assert_eq!(p, Nat::from_be(&want.to_be_bytes()));
    }

    #[test]
    fn refuses_what_is_not_a_signature() {
        let n = hex(
            "c3a2d4da9be2e6fa9d1b5c3b0b5d25f2ae0e1ee3b1c3b3de0fb7f7d4b4bb2a4b\
                     b65e7f0cdc2ac6ae2c38bb7c7d0d5d3bd6d9e1cfa30c8b4d9b27b2a1c5c5f18f",
        );
        // Random bytes decrypt to no padding; a short signature is refused.
        assert_eq!(recover(&n, &[1, 0, 1], &[0x12; 64]), None);
        assert_eq!(recover(&n, &[1, 0, 1], &[0x12; 63]), None);
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }
}
