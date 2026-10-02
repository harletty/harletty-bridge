// SPDX-License-Identifier: Apache-2.0
//
// Which instruction set the decoder's vector paths may use.

/// Whether the CPU runs the code written or compiled a second time for AVX2.
/// That code also assumes BMI1, BMI2 and LZCNT, which every AVX2 processor
/// has; all four are checked.
///
/// Building with `--cfg dca_force_scalar` answers no, so the portable code
/// runs everywhere. Both give the same samples; that cfg is how the tests
/// and the reference decodes are run against the portable code on a machine
/// that has AVX2.
#[cfg(target_arch = "x86_64")]
#[inline]
pub(crate) fn has_avx2() -> bool {
    !cfg!(dca_force_scalar)
        && std::arch::is_x86_feature_detected!("avx2")
        && std::arch::is_x86_feature_detected!("bmi1")
        && std::arch::is_x86_feature_detected!("bmi2")
        && std::arch::is_x86_feature_detected!("lzcnt")
}
