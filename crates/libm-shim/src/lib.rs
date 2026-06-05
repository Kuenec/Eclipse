//! Eclipse apkenv-loadable `libm.so` — clean-room re-export of the pure-Rust `libm` crate's correct
//! math under the C libm symbol names (2026-06-05).
//!
//! Built as a standalone `#![no_std]` cdylib so the produced `libeclipse_libm_shim.so` is an ELF the
//! apkenv / `bionic_translation` shim linker CAN load. The host glibc `libm.so.6` cannot be used as
//! the app's `libm.so` because it carries an `R_X86_64_TPOFF64` (modern TLS reloc — "unknown reloc
//! type 18") and a `.relr.dyn` packed-reloc section the older apkenv linker cannot apply (plus
//! `NEEDED ld-linux-x86-64.so.2`), so its load aborts during Roblox's `androidx.startup`
//! `System.loadLibrary("zstd-jni")` (zstd-jni `NEEDED libm.so`). This shim has ONLY
//! `R_X86_64_{64,GLOB_DAT,RELATIVE}` relocs (the same set zstd-jni itself uses, which apkenv provably
//! handles), no RELR, and no `NEEDED` — verified by build.rs's `readelf` check and the workspace test.
//!
//! Scope (AGENTS.md "Simplicity First"): exactly the libm math surface the run's apkenv-loaded libs
//! (`libzstd-jni`, `libeigen_blas`, `libeigen_lapack`) `NEEDED libm.so` for, plus the math
//! `libroblox.so` imports — the 49 symbols measured by `readelf --dyn-syms` on the extracted libs on
//! 2026-06-05. Every value is CORRECT (forwarded to the `libm` crate), never a stub — a wrong `sin`
//! would corrupt the engine.
//!
//! Provenance: written from the PUBLIC C `<math.h>` signatures + the public `libm` crate API. No
//! bionic / NDK / glibc-libm / apkenv-linker source was read.

#![no_std]
#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_char, c_int, c_long, c_longlong};

/// `panic = "abort"` makes this unreachable, but `#![no_std]` still requires a handler. Abort the
/// process rather than loop: a panic inside a math forward would mean a `libm`-crate bug, which must
/// be loud, not a hang.
///
/// `#[cfg(not(test))]`: under `cargo test --all-targets`, the spurious `lib test` target links `std`,
/// which already provides `panic_impl`; defining our own then is a duplicate-lang-item error. The
/// cdylib has no tests, so excluding the handler from the test build is harmless (the real cdylib
/// artifact always builds with this handler).
#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // SAFETY (2026-06-05): the SIGILL-raising abort intrinsic is the documented no_std abort path; it
    // takes no arguments and never returns. Reached only on an impossible math-crate panic.
    unsafe { core::arch::asm!("ud2", options(noreturn)) }
}

// ---------------------------------------------------------------------------------------------------
// f64, single argument.
// ---------------------------------------------------------------------------------------------------
macro_rules! fwd1_f64 {
    ($($name:ident),* $(,)?) => {$(
        #[no_mangle]
        pub extern "C" fn $name(x: f64) -> f64 { libm::$name(x) }
    )*};
}
fwd1_f64!(
    acos, asin, atan, cbrt, cos, cosh, exp, exp2, expm1, log, log10, log2, round, sin, sinh, tan,
    tanh,
);

// ---------------------------------------------------------------------------------------------------
// f32, single argument.
// ---------------------------------------------------------------------------------------------------
macro_rules! fwd1_f32 {
    ($($name:ident),* $(,)?) => {$(
        #[no_mangle]
        pub extern "C" fn $name(x: f32) -> f32 { libm::$name(x) }
    )*};
}
fwd1_f32!(
    acosf, asinf, atanf, cbrtf, cosf, coshf, erfcf, erff, exp2f, expf, log10f, log2f, logf, sinf,
    tanf, tanhf,
);

// ---------------------------------------------------------------------------------------------------
// f64 / f32, two arguments.
// ---------------------------------------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}
#[no_mangle]
pub extern "C" fn fmod(x: f64, y: f64) -> f64 {
    libm::fmod(x, y)
}
#[no_mangle]
pub extern "C" fn pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}
#[no_mangle]
pub extern "C" fn atan2f(y: f32, x: f32) -> f32 {
    libm::atan2f(y, x)
}
#[no_mangle]
pub extern "C" fn fmodf(x: f32, y: f32) -> f32 {
    libm::fmodf(x, y)
}
#[no_mangle]
pub extern "C" fn powf(x: f32, y: f32) -> f32 {
    libm::powf(x, y)
}
#[no_mangle]
pub extern "C" fn remainderf(x: f32, y: f32) -> f32 {
    libm::remainderf(x, y)
}
#[no_mangle]
pub extern "C" fn nextafterf(x: f32, y: f32) -> f32 {
    libm::nextafterf(x, y)
}

// ---------------------------------------------------------------------------------------------------
// ilogb: f64 -> int exponent.
// ---------------------------------------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn ilogb(x: f64) -> c_int {
    libm::ilogb(x) as c_int
}

// ---------------------------------------------------------------------------------------------------
// ldexp / scalbn: scale by a power of two (C: double ldexp(double, int)).
// ---------------------------------------------------------------------------------------------------
// `c_int == i32` on every target this runs on, so `libm::ldexp{,f}`'s `i32` exponent takes `n`
// directly (an explicit cast would be a redundant `i32 -> i32` no-op, which clippy rejects).
#[no_mangle]
pub extern "C" fn ldexp(x: f64, n: c_int) -> f64 {
    libm::ldexp(x, n)
}
#[no_mangle]
pub extern "C" fn ldexpf(x: f32, n: c_int) -> f32 {
    libm::ldexpf(x, n)
}

// ---------------------------------------------------------------------------------------------------
// Pointer-out functions. The `libm` crate returns the out-values as a tuple; the C ABI writes them
// through caller-supplied pointers. Each wrapper writes through its out-pointer iff non-null (the C
// contract requires a valid pointer, but guarding avoids UB if a caller passes NULL).
// ---------------------------------------------------------------------------------------------------

/// C: `double frexp(double x, int *exp)` — split into mantissa in [0.5,1) and a power-of-two exponent.
#[no_mangle]
pub unsafe extern "C" fn frexp(x: f64, exp: *mut c_int) -> f64 {
    let (m, e) = libm::frexp(x);
    if !exp.is_null() {
        // SAFETY (2026-06-05): `exp` is the caller's `int*` out-param; non-null checked. Single aligned write.
        unsafe { *exp = e as c_int };
    }
    m
}
/// C: `float frexpf(float x, int *exp)`.
#[no_mangle]
pub unsafe extern "C" fn frexpf(x: f32, exp: *mut c_int) -> f32 {
    let (m, e) = libm::frexpf(x);
    if !exp.is_null() {
        // SAFETY (2026-06-05): non-null `int*` out-param; single aligned write.
        unsafe { *exp = e as c_int };
    }
    m
}
/// C: `double modf(double x, double *iptr)` — split into integral (via `*iptr`) and fractional parts.
#[no_mangle]
pub unsafe extern "C" fn modf(x: f64, iptr: *mut f64) -> f64 {
    // libm::modf returns (fractional, integral).
    let (frac, int) = libm::modf(x);
    if !iptr.is_null() {
        // SAFETY (2026-06-05): non-null `double*` out-param; single aligned write.
        unsafe { *iptr = int };
    }
    frac
}
/// C: `float modff(float x, float *iptr)`.
#[no_mangle]
pub unsafe extern "C" fn modff(x: f32, iptr: *mut f32) -> f32 {
    let (frac, int) = libm::modff(x);
    if !iptr.is_null() {
        // SAFETY (2026-06-05): non-null `float*` out-param; single aligned write.
        unsafe { *iptr = int };
    }
    frac
}
/// C: `void sincos(double x, double *s, double *c)`.
#[no_mangle]
pub unsafe extern "C" fn sincos(x: f64, s: *mut f64, c: *mut f64) {
    let (sin, cos) = libm::sincos(x);
    if !s.is_null() {
        // SAFETY (2026-06-05): non-null `double*` out-param; single aligned write.
        unsafe { *s = sin };
    }
    if !c.is_null() {
        // SAFETY (2026-06-05): non-null `double*` out-param; single aligned write.
        unsafe { *c = cos };
    }
}
/// C: `void sincosf(float x, float *s, float *c)`.
#[no_mangle]
pub unsafe extern "C" fn sincosf(x: f32, s: *mut f32, c: *mut f32) {
    let (sin, cos) = libm::sincosf(x);
    if !s.is_null() {
        // SAFETY (2026-06-05): non-null `float*` out-param; single aligned write.
        unsafe { *s = sin };
    }
    if !c.is_null() {
        // SAFETY (2026-06-05): non-null `float*` out-param; single aligned write.
        unsafe { *c = cos };
    }
}
/// C: `float remquof(float x, float y, int *quo)` — remainder plus low bits of the quotient.
#[no_mangle]
pub unsafe extern "C" fn remquof(x: f32, y: f32, quo: *mut c_int) -> f32 {
    let (rem, q) = libm::remquof(x, y);
    if !quo.is_null() {
        // SAFETY (2026-06-05): non-null `int*` out-param; single aligned write.
        unsafe { *quo = q as c_int };
    }
    rem
}

// ---------------------------------------------------------------------------------------------------
// Round-to-integer-type and nan: not exposed by the `libm` crate as C-typed returns, implemented on
// top of its correct `round`/`roundf`. C rounds half-away-from-zero (matches `libm::round`), then
// converts to the C integer type. `nan(tag)` returns a quiet NaN (the tag-string payload is ignored —
// the common, correct-enough behavior; no engine path inspects the NaN payload).
// ---------------------------------------------------------------------------------------------------
/// C: `long lround(double)` — round half-away-from-zero to the nearest `long`.
#[no_mangle]
pub extern "C" fn lround(x: f64) -> c_long {
    libm::round(x) as c_long
}
/// C: `long lroundf(float)`.
#[no_mangle]
pub extern "C" fn lroundf(x: f32) -> c_long {
    libm::roundf(x) as c_long
}
/// C: `long long llround(double)`.
#[no_mangle]
pub extern "C" fn llround(x: f64) -> c_longlong {
    libm::round(x) as c_longlong
}
/// C: `long long llroundf(float)`.
#[no_mangle]
pub extern "C" fn llroundf(x: f32) -> c_longlong {
    libm::roundf(x) as c_longlong
}
/// C: `double nan(const char *tag)` — a quiet NaN. The tag (payload) string is ignored.
#[no_mangle]
pub extern "C" fn nan(_tag: *const c_char) -> f64 {
    f64::NAN
}
