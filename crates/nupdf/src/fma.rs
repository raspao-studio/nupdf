// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

//! `a * b + c` as MuPDF's C computes it where it is built. C compilers fuse
//! a multiply and an add in one expression into one instruction where the
//! machine has one in its baseline: clang does on aarch64 (one rounding),
//! and on x86-64, whose baseline has no fused multiply-add, the two round
//! separately. The ports write `a.madd(b, c)` where MuPDF's C has
//! `a * b + c`, so nupdf prints the numbers MuPDF prints on each platform.

/// A multiply-add rounded as C rounds it on this machine.
pub trait Madd {
    fn madd(self, b: Self, c: Self) -> Self;
}

impl Madd for f32 {
    #[inline]
    fn madd(self, b: f32, c: f32) -> f32 {
        #[cfg(target_arch = "aarch64")]
        {
            self.mul_add(b, c)
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            self * b + c
        }
    }
}

impl Madd for f64 {
    #[inline]
    fn madd(self, b: f64, c: f64) -> f64 {
        #[cfg(target_arch = "aarch64")]
        {
            self.mul_add(b, c)
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            self * b + c
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_as_the_platform_rounds() {
        // 0.1 * 10 - 1 is 0 when rounded twice, and 2^-26-ish when fused.
        let fused = 0.1f32.mul_add(10.0, -1.0);
        let twice = 0.1f32 * 10.0 - 1.0;
        let got = 0.1f32.madd(10.0, -1.0);
        if cfg!(target_arch = "aarch64") {
            assert_eq!(got, fused);
        } else {
            assert_eq!(got, twice);
        }
    }
}
