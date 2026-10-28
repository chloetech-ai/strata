//! The SIMD instruction sets of the CPU a process runs on, detected once.
//!
//! Kernels are compiled for each instruction set they use with
//! `#[target_feature(enable = ...)]` and chosen per call with these checks,
//! so one binary runs on any CPU of its architecture and uses what that CPU
//! has. A feature the build already assumes (`-C target-cpu=native`, or an
//! aarch64 target that includes it) is a compile-time `true`, and the check
//! folds away.
//!
//! `STRATA_SIMD=portable` turns every check off (the portable kernels run,
//! as on a CPU without these features); `STRATA_SIMD=avx2` stops x86_64 at
//! AVX2. Features the build assumes stay on.

use std::sync::atomic::{AtomicU32, Ordering};

const AVX2: u32 = 1 << 0; // with FMA
const BMI2: u32 = 1 << 1;
const SSSE3: u32 = 1 << 2;
const AVX512: u32 = 1 << 3; // F, BW, VL
const AVX512VNNI: u32 = 1 << 4;
const AVX512VBMI: u32 = 1 << 5;
const AVX512VPOPCNTDQ: u32 = 1 << 6;
const DOTPROD: u32 = 1 << 7;
const KNOWN: u32 = 1 << 31;

static FEATURES: AtomicU32 = AtomicU32::new(0);

#[cold]
fn detect() -> u32 {
    let mut f = 0;
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::is_x86_feature_detected as has;
        let set = |on: bool, bit: u32| if on { bit } else { 0 };
        f |= set(has!("ssse3"), SSSE3);
        f |= set(has!("bmi2") && fast_pdep(), BMI2);
        f |= set(has!("avx2") && has!("fma"), AVX2);
        let avx512 = has!("avx512f") && has!("avx512bw") && has!("avx512vl");
        if avx512 {
            f |= AVX512;
            f |= set(has!("avx512vnni"), AVX512VNNI);
            f |= set(has!("avx512vbmi"), AVX512VBMI);
            f |= set(has!("avx512vpopcntdq"), AVX512VPOPCNTDQ);
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("dotprod") {
            f |= DOTPROD;
        }
    }
    match std::env::var("STRATA_SIMD").as_deref() {
        Ok("portable") | Ok("scalar") | Ok("none") => f = 0,
        Ok("avx2") => f &= AVX2 | BMI2 | SSSE3,
        _ => {}
    }
    f
}

/// Whether `pdep` and `pext` take a few cycles: not on AMD (or Hygon)
/// before Zen 3, which run them in microcode, hundreds of cycles.
#[cfg(target_arch = "x86_64")]
fn fast_pdep() -> bool {
    use std::arch::x86_64::__cpuid;
    if cfg!(miri) {
        // Miri has no CPUID; it runs the `pdep` the build enables.
        return true;
    }
    #[allow(unused_unsafe)]
    // SAFETY: CPUID exists on every x86_64 CPU.
    let (v, l1) = unsafe { (__cpuid(0), __cpuid(1)) };
    let vendor: Vec<u8> = [v.ebx, v.edx, v.ecx].iter().flat_map(|r| r.to_le_bytes()).collect();
    let base = (l1.eax >> 8) & 0xf;
    let family = if base == 0xf { base + ((l1.eax >> 20) & 0xff) } else { base };
    !((vendor == b"AuthenticAMD" || vendor == b"HygonGenuine") && family < 0x19)
}

#[inline]
fn has(bit: u32) -> bool {
    let mut f = FEATURES.load(Ordering::Relaxed);
    if f & KNOWN == 0 {
        f = detect() | KNOWN;
        FEATURES.store(f, Ordering::Relaxed);
    }
    f & bit != 0
}

/// AVX2 with FMA.
#[inline]
pub fn avx2() -> bool {
    cfg!(all(target_arch = "x86_64", target_feature = "avx2", target_feature = "fma")) || has(AVX2)
}

/// BMI2 (`pdep`, `pext`), where they are fast: a build that assumes BMI2
/// uses them anyway, but at run time AMD CPUs before Zen 3 are left out.
#[inline]
pub fn bmi2() -> bool {
    cfg!(all(target_arch = "x86_64", target_feature = "bmi2")) || has(BMI2)
}

/// SSSE3.
#[inline]
pub fn ssse3() -> bool {
    cfg!(all(target_arch = "x86_64", target_feature = "ssse3")) || has(SSSE3)
}

/// AVX-512 F, BW and VL (Skylake-SP, Ice Lake, Zen 4 and later).
#[inline]
pub fn avx512() -> bool {
    cfg!(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512vl")) || has(AVX512)
}

/// [`avx512`] and VNNI (`vpdpbusd`).
#[inline]
pub fn avx512vnni() -> bool {
    cfg!(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512vl", target_feature = "avx512vnni"))
        || has(AVX512VNNI)
}

/// [`avx512`] and VBMI (`vpermb`, `vpmultishiftqb`).
#[inline]
pub fn avx512vbmi() -> bool {
    cfg!(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512vl", target_feature = "avx512vbmi"))
        || has(AVX512VBMI)
}

/// [`avx512`] and VPOPCNTDQ.
#[inline]
pub fn avx512vpopcntdq() -> bool {
    cfg!(all(
        target_arch = "x86_64",
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vl",
        target_feature = "avx512vpopcntdq"
    )) || has(AVX512VPOPCNTDQ)
}

/// The aarch64 dot product instructions (`sdot`, `udot`): Apple M1,
/// Graviton 2 and later.
#[inline]
pub fn dotprod() -> bool {
    cfg!(all(target_arch = "aarch64", target_feature = "dotprod")) || has(DOTPROD)
}

/// The kernels this process runs, for reports and logs: the widest tier
/// on this CPU.
pub fn describe() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        if avx512vnni() {
            "avx512-vnni"
        } else if avx512() {
            "avx512"
        } else if avx2() {
            "avx2"
        } else {
            "portable"
        }
    } else if cfg!(target_arch = "aarch64") {
        if cfg!(target_feature = "i8mm") {
            "neon-i8mm"
        } else if dotprod() {
            "neon-dotprod"
        } else {
            "neon"
        }
    } else {
        "portable"
    }
}
