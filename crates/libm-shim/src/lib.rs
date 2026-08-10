
#![no_std]
#![allow(clippy::missing_safety_doc)]

use core::ffi::{c_char, c_int, c_long, c_longlong};

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {

    unsafe { core::arch::asm!("ud2", options(noreturn)) }
}

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

#[no_mangle]
pub extern "C" fn ilogb(x: f64) -> c_int {
    libm::ilogb(x) as c_int
}

#[no_mangle]
pub extern "C" fn ldexp(x: f64, n: c_int) -> f64 {
    libm::ldexp(x, n)
}
#[no_mangle]
pub extern "C" fn ldexpf(x: f32, n: c_int) -> f32 {
    libm::ldexpf(x, n)
}

#[no_mangle]
pub unsafe extern "C" fn frexp(x: f64, exp: *mut c_int) -> f64 {
    let (m, e) = libm::frexp(x);
    if !exp.is_null() {

        unsafe { *exp = e as c_int };
    }
    m
}

#[no_mangle]
pub unsafe extern "C" fn frexpf(x: f32, exp: *mut c_int) -> f32 {
    let (m, e) = libm::frexpf(x);
    if !exp.is_null() {

        unsafe { *exp = e as c_int };
    }
    m
}

#[no_mangle]
pub unsafe extern "C" fn modf(x: f64, iptr: *mut f64) -> f64 {

    let (frac, int) = libm::modf(x);
    if !iptr.is_null() {

        unsafe { *iptr = int };
    }
    frac
}

#[no_mangle]
pub unsafe extern "C" fn modff(x: f32, iptr: *mut f32) -> f32 {
    let (frac, int) = libm::modff(x);
    if !iptr.is_null() {

        unsafe { *iptr = int };
    }
    frac
}

#[no_mangle]
pub unsafe extern "C" fn sincos(x: f64, s: *mut f64, c: *mut f64) {
    let (sin, cos) = libm::sincos(x);
    if !s.is_null() {

        unsafe { *s = sin };
    }
    if !c.is_null() {

        unsafe { *c = cos };
    }
}

#[no_mangle]
pub unsafe extern "C" fn sincosf(x: f32, s: *mut f32, c: *mut f32) {
    let (sin, cos) = libm::sincosf(x);
    if !s.is_null() {

        unsafe { *s = sin };
    }
    if !c.is_null() {

        unsafe { *c = cos };
    }
}

#[no_mangle]
pub unsafe extern "C" fn remquof(x: f32, y: f32, quo: *mut c_int) -> f32 {
    let (rem, q) = libm::remquof(x, y);
    if !quo.is_null() {

        unsafe { *quo = q as c_int };
    }
    rem
}

#[no_mangle]
pub extern "C" fn lround(x: f64) -> c_long {
    libm::round(x) as c_long
}

#[no_mangle]
pub extern "C" fn lroundf(x: f32) -> c_long {
    libm::roundf(x) as c_long
}

#[no_mangle]
pub extern "C" fn llround(x: f64) -> c_longlong {
    libm::round(x) as c_longlong
}

#[no_mangle]
pub extern "C" fn llroundf(x: f32) -> c_longlong {
    libm::roundf(x) as c_longlong
}

#[no_mangle]
pub extern "C" fn nan(_tag: *const c_char) -> f64 {
    f64::NAN
}
