// SPDX-License-Identifier: Apache-2.0
//
// DCA core decoder internals: generated tables, Huffman VLCs, and the subband
// DSP decode (ported incrementally from ffmpeg's dca_core.c / dcadsp.c).

/// Define an elementwise loop over sample buffers that runs as AVX-512 or
/// AVX2 code where the CPU has it: the same loop compiled three times. The
/// arithmetic is integer, so all give the same samples; the baseline x86-64
/// target has no instructions for the 24-bit clips and the 64-bit products
/// these loops are made of, and AVX2 none for the 64-bit arithmetic shifts
/// and full 64-bit products that AVX-512 (DQ) adds on twice the width.
macro_rules! sample_loop {
    ($(#[$doc:meta])* fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $body:block) => {
        $(#[$doc])*
        fn $name($($arg: $ty),*) {
            #[inline(always)]
            fn body($($arg: $ty),*) $body
            #[cfg(target_arch = "x86_64")]
            {
                #[target_feature(enable = "avx2,bmi1,bmi2,lzcnt,avx512f,avx512bw,avx512dq,avx512vl")]
                fn avx512($($arg: $ty),*) {
                    body($($arg),*)
                }
                #[target_feature(enable = "avx2")]
                fn avx2($($arg: $ty),*) {
                    body($($arg),*)
                }
                if $crate::cpu::has_avx512() {
                    // SAFETY: `has_avx512` covers the features enabled above.
                    return unsafe { avx512($($arg),*) };
                }
                if $crate::cpu::has_avx2() {
                    // SAFETY: AVX2 was just detected.
                    return unsafe { avx2($($arg),*) };
                }
            }
            body($($arg),*)
        }
    };
}

pub(crate) mod buffers;
pub(crate) mod core;
pub(crate) mod exss;
pub(crate) mod huffman;
pub(crate) mod synth;
#[cfg(target_arch = "x86_64")]
mod synth_avx2;
#[cfg(target_arch = "x86_64")]
mod synth_avx512;
pub(crate) mod tables;
pub(crate) mod xll;
#[cfg(target_arch = "x86_64")]
mod xll_avx2;
pub(crate) mod xmeta;
pub(crate) mod xrender;
