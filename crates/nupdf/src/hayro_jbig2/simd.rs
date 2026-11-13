pub(crate) const SIMD_WIDTH: usize = 8;

mod inner {
    use super::SIMD_WIDTH;
    use core::ops::{BitAnd, BitOr, BitXor, BitXorAssign};
    use fearless_simd::{Select, SimdBase};

    pub(crate) use fearless_simd::{Level, Simd, dispatch};

    #[derive(Copy, Clone)]
    #[allow(non_camel_case_types)]
    #[repr(C, align(32))]
    pub(crate) struct mask32x8<S: Simd> {
        inner: fearless_simd::mask32x8<S>,
    }

    impl<S: Simd> mask32x8<S> {
        #[inline(always)]
        pub(crate) fn select(self, if_true: u32x8<S>, if_false: u32x8<S>) -> u32x8<S> {
            u32x8 {
                inner: self.inner.select(if_true.inner, if_false.inner),
            }
        }
    }

    #[derive(Copy, Clone)]
    #[allow(non_camel_case_types)]
    #[repr(C, align(32))]
    pub(crate) struct u32x8<S: Simd> {
        inner: fearless_simd::u32x8<S>,
    }

    impl<S: Simd> u32x8<S> {
        #[inline(always)]
        pub(crate) fn from_slice(simd: S, slice: &[u32]) -> Self {
            Self {
                inner: fearless_simd::u32x8::from_slice(simd, &slice[..SIMD_WIDTH]),
            }
        }

        #[inline(always)]
        pub(crate) fn splat(simd: S, val: u32) -> Self {
            Self {
                inner: fearless_simd::u32x8::splat(simd, val),
            }
        }

        #[inline(always)]
        pub(crate) fn store(self, slice: &mut [u32]) {
            self.inner.store_slice(&mut slice[..SIMD_WIDTH]);
        }

        #[inline(always)]
        pub(crate) fn simd_gt(self, other: Self) -> mask32x8<S> {
            mask32x8 {
                inner: self.inner.simd_gt(other.inner),
            }
        }
    }

    impl<S: Simd> BitAnd for u32x8<S> {
        type Output = Self;
        #[inline(always)]
        fn bitand(self, rhs: Self) -> Self {
            Self {
                inner: self.inner & rhs.inner,
            }
        }
    }

    impl<S: Simd> BitOr for u32x8<S> {
        type Output = Self;
        #[inline(always)]
        fn bitor(self, rhs: Self) -> Self {
            Self {
                inner: self.inner | rhs.inner,
            }
        }
    }

    impl<S: Simd> BitXor for u32x8<S> {
        type Output = Self;
        #[inline(always)]
        fn bitxor(self, rhs: Self) -> Self {
            Self {
                inner: self.inner ^ rhs.inner,
            }
        }
    }

    impl<S: Simd> BitXorAssign for u32x8<S> {
        #[inline(always)]
        fn bitxor_assign(&mut self, rhs: Self) {
            self.inner = self.inner ^ rhs.inner;
        }
    }
}

pub(crate) use inner::*;
