//! `api-ms-win-crt-math`: `libm` with MSVC's results, classification codes
//! and `errno` (`EDOM` for a NaN from numbers, `ERANGE` for an infinity
//! from finite ones). Anything with a floating-point argument is `extern
//! "win64"`; the `*f` functions compute in double and round.

use super::{EDOM, ERANGE, Shared, exports64, libc, set_errno};

/// Safe calls of libm's functions, which are pure.
mod libm {
    use super::libc;

    macro_rules! wrap {
        ($($n:ident($($a:ident: $t:ty),*) -> $r:ty;)*) => {$(
            pub fn $n($($a: $t),*) -> $r {
                // SAFETY: a pure function of its arguments.
                unsafe { libc::$n($($a),*) }
            }
        )*};
    }

    wrap! {
        erf(x: f64) -> f64;
        erfc(x: f64) -> f64;
        tgamma(x: f64) -> f64;
        lgamma(x: f64) -> f64;
        logb(x: f64) -> f64;
        j0(x: f64) -> f64;
        j1(x: f64) -> f64;
        y0(x: f64) -> f64;
        y1(x: f64) -> f64;
        jn(n: i32, x: f64) -> f64;
        yn(n: i32, x: f64) -> f64;
        remainder(x: f64, y: f64) -> f64;
        fmod(x: f64, y: f64) -> f64;
        nextafter(x: f64, y: f64) -> f64;
        nextafterf(x: f32, y: f32) -> f32;
        scalbn(x: f64, n: i32) -> f64;
        ilogb(x: f64) -> i32;
    }
}

/// Sets `errno` the way the CRT does for a result `r` of arguments `args`.
fn check(args: &[f64], r: f64) -> f64 {
    if r.is_nan() && !args.iter().any(|a| a.is_nan()) {
        set_errno(EDOM);
    } else if r.is_infinite() && args.iter().all(|a| a.is_finite()) {
        set_errno(ERANGE);
    }
    r
}

macro_rules! unary {
    ($($d:ident, $f:ident => $e:expr;)*) => {$(
        pub extern "win64" fn $d(x: f64) -> f64 {
            let g: fn(f64) -> f64 = $e;
            check(&[x], g(x))
        }
        pub extern "win64" fn $f(x: f32) -> f32 {
            let g: fn(f64) -> f64 = $e;
            // Checked as the float it returns: a double can be finite where the float is not.
            check(&[f64::from(x)], f64::from(g(f64::from(x)) as f32)) as f32
        }
    )*};
}

/// No `errno`: results that are always defined.
macro_rules! plain {
    ($($d:ident, $f:ident => $e:expr;)*) => {$(
        pub extern "win64" fn $d(x: f64) -> f64 {
            let g: fn(f64) -> f64 = $e;
            g(x)
        }
        pub extern "win64" fn $f(x: f32) -> f32 {
            let g: fn(f64) -> f64 = $e;
            g(f64::from(x)) as f32
        }
    )*};
}

macro_rules! binary {
    ($($d:ident, $f:ident => $e:expr;)*) => {$(
        pub extern "win64" fn $d(x: f64, y: f64) -> f64 {
            let g: fn(f64, f64) -> f64 = $e;
            check(&[x, y], g(x, y))
        }
        pub extern "win64" fn $f(x: f32, y: f32) -> f32 {
            let g: fn(f64, f64) -> f64 = $e;
            let r = g(f64::from(x), f64::from(y)) as f32;
            check(&[f64::from(x), f64::from(y)], f64::from(r)) as f32
        }
    )*};
}

unary! {
    sin, sinf => f64::sin;
    cos, cosf => f64::cos;
    tan, tanf => f64::tan;
    asin, asinf => f64::asin;
    acos, acosf => f64::acos;
    atan, atanf => f64::atan;
    sinh, sinhf => f64::sinh;
    cosh, coshf => f64::cosh;
    tanh, tanhf => f64::tanh;
    asinh, asinhf => f64::asinh;
    acosh, acoshf => f64::acosh;
    atanh, atanhf => f64::atanh;
    exp, expf => f64::exp;
    exp2, exp2f => f64::exp2;
    expm1, expm1f => f64::exp_m1;
    log, logf => f64::ln;
    log2, log2f => f64::log2;
    log10, log10f => f64::log10;
    log1p, log1pf => f64::ln_1p;
    sqrt, sqrtf => f64::sqrt;
    cbrt, cbrtf => f64::cbrt;
    erf, erff => libm::erf;
    erfc, erfcf => libm::erfc;
    tgamma, tgammaf => libm::tgamma;
    lgamma, lgammaf => libm::lgamma;
    logb, logbf => libm::logb;
}

plain! {
    ceil, ceilf => f64::ceil;
    floor, floorf => f64::floor;
    trunc, truncf => f64::trunc;
    round, roundf => f64::round;
    rint, rintf => f64::round_ties_even;
    nearbyint, nearbyintf => f64::round_ties_even;
    fabs, fabsf => f64::abs;
}

pub extern "win64" fn _j0(x: f64) -> f64 {
    libm::j0(x)
}

pub extern "win64" fn _j1(x: f64) -> f64 {
    libm::j1(x)
}

pub extern "win64" fn _y0(x: f64) -> f64 {
    libm::y0(x)
}

pub extern "win64" fn _y1(x: f64) -> f64 {
    libm::y1(x)
}

binary! {
    pow, powf => f64::powf;
    atan2, atan2f => f64::atan2;
    fmod, fmodf => libm::fmod;
    hypot, hypotf => f64::hypot;
    remainder, remainderf => libm::remainder;
    fdim, fdimf => |x, y| if x.is_nan() || y.is_nan() { f64::NAN } else if x > y { x - y } else { 0.0 };
}

macro_rules! simple2 {
    ($($d:ident, $f:ident => $e:expr;)*) => {$(
        pub extern "win64" fn $d(x: f64, y: f64) -> f64 {
            let g: fn(f64, f64) -> f64 = $e;
            g(x, y)
        }
        pub extern "win64" fn $f(x: f32, y: f32) -> f32 {
            let g: fn(f64, f64) -> f64 = $e;
            g(f64::from(x), f64::from(y)) as f32
        }
    )*};
}

simple2! {
    copysign, copysignf => f64::copysign;
    fmax, fmaxf => f64::max;
    fmin, fminf => f64::min;
}

pub extern "win64" fn nextafter(x: f64, y: f64) -> f64 {
    let r = libm::nextafter(x, y);
    if r.is_infinite() && x.is_finite() {
        set_errno(ERANGE);
    }
    r
}

/// `long double` is `double` on this ABI.
pub extern "win64" fn nexttoward(x: f64, y: f64) -> f64 {
    nextafter(x, y)
}

pub extern "win64" fn nextafterf(x: f32, y: f32) -> f32 {
    let r = libm::nextafterf(x, y);
    if r.is_infinite() && x.is_finite() {
        set_errno(ERANGE);
    }
    r
}

pub extern "win64" fn nexttowardf(x: f32, y: f64) -> f32 {
    // Compared as doubles: `y` rounded to a float can be `x` itself, which would not move.
    let wide = f64::from(x);
    if x.is_nan() || y.is_nan() {
        return (wide + y) as f32;
    }
    if wide == y {
        return y as f32;
    }
    let toward = if y > wide {
        f32::INFINITY
    } else {
        f32::NEG_INFINITY
    };
    let r = libm::nextafterf(x, toward);
    // A finite float stepping past FLT_MAX overflows.
    if r.is_infinite() && x.is_finite() {
        set_errno(ERANGE);
    }
    r
}

pub extern "win64" fn fma(x: f64, y: f64, z: f64) -> f64 {
    x.mul_add(y, z)
}

pub extern "win64" fn fmaf(x: f32, y: f32, z: f32) -> f32 {
    x.mul_add(y, z)
}

pub extern "win64" fn ldexp(x: f64, e: i32) -> f64 {
    scalbn_checked(x, e)
}

pub extern "win64" fn ldexpf(x: f32, e: i32) -> f32 {
    scalbnf_checked(x, e)
}

fn scalbn_(x: f64, e: i32) -> f64 {
    libm::scalbn(x, e)
}

/// `scalbn` with `ERANGE` on overflow, as the CRT documents.
fn scalbn_checked(x: f64, e: i32) -> f64 {
    check(&[x], scalbn_(x, e))
}

/// The float form, checked after rounding to the float it returns.
fn scalbnf_checked(x: f32, e: i32) -> f32 {
    check(&[f64::from(x)], f64::from(scalbn_(f64::from(x), e) as f32)) as f32
}

pub extern "win64" fn scalbn(x: f64, e: i32) -> f64 {
    scalbn_checked(x, e)
}

pub extern "win64" fn scalbnf(x: f32, e: i32) -> f32 {
    scalbnf_checked(x, e)
}

/// `scalbln`'s exponent is a 32-bit `long` here.
pub extern "win64" fn scalbln(x: f64, e: i32) -> f64 {
    scalbn_checked(x, e)
}

pub extern "win64" fn scalblnf(x: f32, e: i32) -> f32 {
    scalbnf_checked(x, e)
}

pub extern "win64" fn _scalb(x: f64, e: i32) -> f64 {
    scalbn_checked(x, e)
}

pub extern "win64" fn ilogb(x: f64) -> i32 {
    // The CRT's FP_ILOGBNAN is INT_MAX, as for an infinity; macOS gives a NaN FP_ILOGB0, INT_MIN.
    if x.is_nan() { i32::MAX } else { libm::ilogb(x) }
}

pub extern "win64" fn ilogbf(x: f32) -> i32 {
    ilogb(f64::from(x))
}

fn frexp_(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    // Scale subnormals into the normal range first.
    let (x, bias) = if x.abs() < f64::MIN_POSITIVE {
        (x * 2f64.powi(54), -54)
    } else {
        (x, 0)
    };
    let bits = x.to_bits();
    let e = ((bits >> 52) & 0x7FF) as i32 - 1022;
    let m = f64::from_bits((bits & !(0x7FF << 52)) | (1022 << 52));
    (m, e + bias)
}

pub extern "win64" fn frexp(x: f64, exp: *mut i32) -> f64 {
    let (m, e) = frexp_(x);
    // SAFETY: the title passes a valid pointer.
    unsafe { exp.write(e) };
    m
}

pub extern "win64" fn frexpf(x: f32, exp: *mut i32) -> f32 {
    frexp(f64::from(x), exp) as f32
}

fn modf_(x: f64) -> (f64, f64) {
    if x.is_infinite() {
        return (0f64.copysign(x), x);
    }
    let i = x.trunc();
    ((x - i).copysign(x), i)
}

pub extern "win64" fn modf(x: f64, int: *mut f64) -> f64 {
    let (f, i) = modf_(x);
    // SAFETY: the title passes a valid pointer.
    unsafe { int.write(i) };
    f
}

pub extern "win64" fn modff(x: f32, int: *mut f32) -> f32 {
    let (f, i) = modf_(f64::from(x));
    // SAFETY: the title passes a valid pointer.
    unsafe { int.write(i as f32) };
    f as f32
}

fn remquo_(x: f64, y: f64) -> (f64, i32) {
    let r = libm::remainder(x, y);
    if !r.is_finite() {
        return (check(&[x, y], r), 0);
    }
    // The low three bits of the quotient's magnitude. `(x - r) / y` loses them once the
    // quotient passes 2^53; `|x| mod 8|y|` (exact) keeps them: |x| = 8k|y| + m, so the
    // quotient is 8k plus that of m, which `remainder` rounds the same way (8k is even).
    // When 8|y| overflows, |x / y| is below 8 and m is |x| itself.
    let (ax, ay) = (x.abs(), y.abs());
    let m = if ay <= f64::MAX / 8.0 {
        libm::fmod(ax, 8.0 * ay)
    } else {
        ax
    };
    // Divided before subtracting: m - remainder(m, |y|) itself can overflow near DBL_MAX.
    let q = (m / ay - libm::remainder(m, ay) / ay).round() as u64 & 7;
    (
        r,
        if (x < 0.0) != (y < 0.0) {
            -(q as i32)
        } else {
            q as i32
        },
    )
}

pub extern "win64" fn remquo(x: f64, y: f64, quo: *mut i32) -> f64 {
    let (r, q) = remquo_(x, y);
    // SAFETY: the title passes a valid pointer.
    unsafe { quo.write(q) };
    r
}

pub extern "win64" fn remquof(x: f32, y: f32, quo: *mut i32) -> f32 {
    remquo(f64::from(x), f64::from(y), quo) as f32
}

// `long` is 32 bits.

pub extern "win64" fn lround(x: f64) -> i32 {
    x.round() as i32
}

pub extern "win64" fn lroundf(x: f32) -> i32 {
    x.round() as i32
}

pub extern "win64" fn llround(x: f64) -> i64 {
    x.round() as i64
}

pub extern "win64" fn llroundf(x: f32) -> i64 {
    x.round() as i64
}

pub extern "win64" fn lrint(x: f64) -> i32 {
    x.round_ties_even() as i32
}

pub extern "win64" fn lrintf(x: f32) -> i32 {
    x.round_ties_even() as i32
}

pub extern "win64" fn llrint(x: f64) -> i64 {
    x.round_ties_even() as i64
}

pub extern "win64" fn llrintf(x: f32) -> i64 {
    x.round_ties_even() as i64
}

pub extern "win64" fn _jn(n: i32, x: f64) -> f64 {
    libm::jn(n, x)
}

pub extern "win64" fn _yn(n: i32, x: f64) -> f64 {
    libm::yn(n, x)
}

// The `_` names and classification.

pub extern "win64" fn _finite(x: f64) -> i32 {
    i32::from(x.is_finite())
}

pub extern "win64" fn _finitef(x: f32) -> i32 {
    i32::from(x.is_finite())
}

pub extern "win64" fn _isnan(x: f64) -> i32 {
    i32::from(x.is_nan())
}

pub extern "win64" fn _isnanf(x: f32) -> i32 {
    i32::from(x.is_nan())
}

pub extern "win64" fn _chgsign(x: f64) -> f64 {
    -x
}

/// `FP_NAN`, `FP_INFINITE`, `FP_ZERO`, `FP_SUBNORMAL`, `FP_NORMAL`.
fn class(x: f64, subnormal: bool) -> i32 {
    if x.is_nan() {
        2
    } else if x.is_infinite() {
        1
    } else if x == 0.0 {
        0
    } else if subnormal {
        -2
    } else {
        -1
    }
}

pub extern "win64" fn _dclass(x: f64) -> i32 {
    class(x, x.is_subnormal())
}

pub extern "win64" fn _fdclass(x: f32) -> i32 {
    class(f64::from(x), x.is_subnormal())
}

pub extern "win64" fn _dtest(p: *const f64) -> i32 {
    // SAFETY: the title passes a valid pointer.
    _dclass(unsafe { p.read_unaligned() })
}

pub extern "win64" fn _fdtest(p: *const f32) -> i32 {
    // SAFETY: the title passes a valid pointer.
    _fdclass(unsafe { p.read_unaligned() })
}

/// Nonzero (0x8000) when the sign bit is set.
pub extern "win64" fn _dsign(x: f64) -> i32 {
    if x.is_sign_negative() { 0x8000 } else { 0 }
}

pub extern "win64" fn _fdsign(x: f32) -> i32 {
    if x.is_sign_negative() { 0x8000 } else { 0 }
}

/// `_fpclass`: the `_FPCLASS_*` bit.
pub extern "win64" fn _fpclass(x: f64) -> i32 {
    let neg = x.is_sign_negative();
    if x.is_nan() {
        // Bit 51 set is quiet.
        return if x.to_bits() & (1 << 51) != 0 {
            0x2
        } else {
            0x1
        };
    }
    match (x.is_infinite(), x == 0.0, x.is_subnormal(), neg) {
        (true, _, _, true) => 0x4,
        (true, _, _, false) => 0x200,
        (_, true, _, true) => 0x20,
        (_, true, _, false) => 0x40,
        (_, _, true, true) => 0x10,
        (_, _, true, false) => 0x80,
        (_, _, _, true) => 0x8,
        _ => 0x100,
    }
}

// Floating-point environment: round-to-nearest, 53-bit, all exceptions masked.

const FP_DEFAULT_CONTROL: u32 = 0x0009_001F;

pub extern "system" fn _clearfp() -> u32 {
    0
}

pub extern "system" fn _statusfp() -> u32 {
    0
}

pub extern "system" fn _fpreset() {}

pub extern "system" fn _controlfp(_new: u32, _mask: u32) -> u32 {
    FP_DEFAULT_CONTROL
}

pub extern "system" fn _control87(new: u32, mask: u32) -> u32 {
    _controlfp(new, mask)
}

pub extern "system" fn _controlfp_s(current: *mut u32, _new: u32, _mask: u32) -> i32 {
    if !current.is_null() {
        // SAFETY: the title passes a valid pointer or null.
        unsafe { current.write(FP_DEFAULT_CONTROL) };
    }
    0
}

pub extern "system" fn _set_FMA3_enable(_enable: i32) -> i32 {
    1
}

pub extern "system" fn _get_FMA3_enable() -> i32 {
    1
}

/// The user's `_matherr` handler, which nothing here calls.
static MATH_ERR: Shared<usize> = Shared::new(0);

pub extern "system" fn __setusermatherr(handler: usize) {
    // SAFETY: a word written under the C runtime's rules.
    unsafe { *MATH_ERR.ptr() = handler };
}

// Integer math.

pub extern "system" fn abs(x: i32) -> i32 {
    x.wrapping_abs()
}

pub extern "system" fn labs(x: i32) -> i32 {
    x.wrapping_abs()
}

pub extern "system" fn llabs(x: i64) -> i64 {
    x.wrapping_abs()
}

/// `div_t` and `ldiv_t` are two `int`s, returned in `rax`.
fn pack(q: i32, r: i32) -> u64 {
    u64::from(q as u32) | (u64::from(r as u32) << 32)
}

pub extern "system" fn div(n: i32, d: i32) -> u64 {
    if d == 0 {
        return 0;
    }
    pack(n.wrapping_div(d), n.wrapping_rem(d))
}

pub extern "system" fn ldiv(n: i32, d: i32) -> u64 {
    div(n, d)
}

/// `lldiv_t` is 16 bytes: Windows returns it through a pointer in `rcx`,
/// which the bridge hands over as the first argument.
pub extern "system" fn lldiv(out: *mut [i64; 2], n: i64, d: i64) -> *mut [i64; 2] {
    let v = if d == 0 {
        [0, 0]
    } else {
        [n.wrapping_div(d), n.wrapping_rem(d)]
    };
    // SAFETY: the caller's return slot.
    unsafe { out.write_unaligned(v) };
    out
}

pub extern "system" fn _rotl(x: u32, n: i32) -> u32 {
    x.rotate_left(n as u32 & 31)
}

pub extern "system" fn _rotr(x: u32, n: i32) -> u32 {
    x.rotate_right(n as u32 & 31)
}

pub extern "system" fn _rotl64(x: u64, n: i32) -> u64 {
    x.rotate_left(n as u32 & 63)
}

pub extern "system" fn _rotr64(x: u64, n: i32) -> u64 {
    x.rotate_right(n as u32 & 63)
}

pub extern "system" fn _byteswap_ushort(x: u16) -> u16 {
    x.swap_bytes()
}

pub extern "system" fn _byteswap_ulong(x: u32) -> u32 {
    x.swap_bytes()
}

pub extern "system" fn _byteswap_uint64(x: u64) -> u64 {
    x.swap_bytes()
}

exports64!(pub(super) fn lookup_float;
    sin, sinf, cos, cosf, tan, tanf, asin, asinf, acos, acosf, atan, atanf, sinh, sinhf, cosh,
    coshf, tanh, tanhf, asinh, asinhf, acosh, acoshf, atanh, atanhf, exp, expf, exp2, exp2f,
    expm1, expm1f, log, logf, log2, log2f, log10, log10f, log1p, log1pf, sqrt, sqrtf, cbrt,
    cbrtf, erf, erff, erfc, erfcf, tgamma, tgammaf, lgamma, lgammaf, logb, logbf, ceil, ceilf,
    floor, floorf, trunc, truncf, round, roundf, rint, rintf, nearbyint, nearbyintf, fabs,
    fabsf, _j0, _j1, _y0, _y1, _jn, _yn, pow, powf, atan2, atan2f, fmod, fmodf, hypot, hypotf,
    remainder, remainderf, copysign, copysignf, fmax, fmaxf, fmin, fminf, fdim, fdimf,
    nextafter, nexttoward, nextafterf, nexttowardf, fma, fmaf, ldexp, ldexpf, scalbn, scalbnf,
    scalbln, scalblnf, _scalb, ilogb, ilogbf, frexp, frexpf, modf, modff, remquo, remquof,
    lround, lroundf, llround, llroundf, lrint, lrintf, llrint, llrintf, _finite, _finitef,
    _isnan, _isnanf, _chgsign, _dclass, _fdclass, _dtest, _fdtest, _dsign, _fdsign, _fpclass;
    "_hypot" => hypot, "_copysign" => copysign, "_nextafter" => nextafter, "_logb" => logb,
    "_hypotf" => hypotf, "_copysignf" => copysignf, "_nextafterf" => nextafterf,
    "_logbf" => logbf, "finite" => _finite, "_fdnorm" => _fdclass,
    // `long double` is `double` on this ABI.
    "_ldsign" => _dsign);

super::exports!(fn lookup_int;
    _clearfp, _statusfp, _fpreset, _controlfp, _control87, _controlfp_s, _set_FMA3_enable,
    _get_FMA3_enable, __setusermatherr, abs, labs, llabs, div, ldiv, lldiv, _rotl, _rotr,
    _rotl64, _rotr64, _byteswap_ushort, _byteswap_ulong, _byteswap_uint64;
    "_abs64" => llabs, "_lrotl" => _rotl, "_lrotr" => _rotr);

pub(super) fn lookup(name: &str) -> Option<usize> {
    lookup_float(name).or_else(|| lookup_int(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_double_sign_is_the_double_one() {
        // `long double` is `double` on this ABI, and `_ldsign` is `_dsign`.
        assert!(lookup("_ldsign").is_some());
        assert_eq!(lookup("_ldsign"), lookup("_dsign"));
    }
    use crate::host_posix::ucrt::errno;

    #[test]
    fn ilogb_gives_the_crts_special_values() {
        // FP_ILOGBNAN (INT_MAX) for NaNs and infinities, FP_ILOGB0 (INT_MIN) for zeros.
        for x in [f64::NAN, -f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(ilogb(x), i32::MAX, "{x}");
        }
        assert_eq!((ilogb(0.0), ilogb(-0.0)), (i32::MIN, i32::MIN));
        assert_eq!((ilogb(8.0), ilogb(0.75), ilogb(f64::from_bits(1))), (3, -1, -1074));
        assert_eq!((ilogbf(f32::NAN), ilogbf(1024.0)), (i32::MAX, 10));
    }

    #[test]
    fn functions_match_libm() {
        assert!((sin(1.0) - 1f64.sin()).abs() < 1e-15);
        assert_eq!(pow(2.0, 10.0), 1024.0);
        assert_eq!(fmod(7.5, 2.0), 1.5);
        assert_eq!(remainder(7.0, 4.0), -1.0);
        assert_eq!(hypot(3.0, 4.0), 5.0);
        assert_eq!(sqrtf(9.0), 3.0);
        assert_eq!(roundf(2.5), 3.0);
        assert_eq!(rint(2.5), 2.0);
        assert_eq!(lround(-2.5), -3);
        assert_eq!(ldexp(1.5, 4), 24.0);
        assert_eq!(fmax(f64::NAN, 1.0), 1.0);
        assert_eq!(nextafterf(1.0, 2.0), f32::from_bits(1.0f32.to_bits() + 1));
        assert!((erff(0.5) - 0.5204999).abs() < 1e-6);
        assert!((tgamma(5.0) - 24.0).abs() < 1e-12);
    }

    #[test]
    fn nexttowardf_steps_toward_its_double_target() {
        // The target is a double: one between two floats still moves x a step.
        let one = 1.0f32.to_bits();
        assert_eq!(nexttowardf(1.0, 1.0 + 1e-10).to_bits(), one + 1);
        assert_eq!(nexttowardf(1.0, 1.0 - 1e-10).to_bits(), one - 1);
        assert_eq!(nexttowardf(0.0, 1e-300).to_bits(), 1);
        assert_eq!(nexttowardf(-0.0, -1e-300).to_bits(), 0x8000_0001);
        assert_eq!(nexttowardf(1.0, 1.0), 1.0);
        assert_eq!(nexttowardf(1.0, 2.0).to_bits(), one + 1);
        assert_eq!(nexttowardf(0.0, -0.0).to_bits(), 0x8000_0000);
        assert_eq!(nexttowardf(-0.0, 0.0).to_bits(), 0);
        for (target, sign) in [
            (f64::INFINITY, 1.0),
            (1e300, 1.0),
            (f64::NEG_INFINITY, -1.0),
        ] {
            set_errno(0);
            let r = nexttowardf(f32::MAX * sign as f32, target);
            assert_eq!(r, f32::INFINITY * sign as f32);
            assert_eq!(errno(), ERANGE, "overflow toward {target:e}");
        }
        set_errno(0);
        assert_eq!(nexttowardf(f32::INFINITY, f64::INFINITY), f32::INFINITY);
        assert_eq!(errno(), 0, "an infinity in is no overflow");
        assert!(nexttowardf(1.0, f64::NAN).is_nan());
        assert!(nexttowardf(f32::NAN, 1.0).is_nan());
    }

    #[test]
    fn errno_follows_the_domain_and_the_range() {
        set_errno(0);
        assert!(sqrt(-1.0).is_nan());
        assert_eq!(errno(), EDOM);
        set_errno(0);
        assert!(exp(1000.0).is_infinite());
        assert_eq!(errno(), ERANGE);
        set_errno(0);
        assert!(fmod(1.0, 0.0).is_nan());
        assert_eq!(errno(), EDOM);
        // A NaN in is a NaN out and no error; so is an infinity in.
        set_errno(0);
        assert!(sin(f64::NAN).is_nan());
        assert!(exp(f64::INFINITY).is_infinite());
        assert_eq!(errno(), 0);
    }

    #[test]
    fn a_float_that_overflows_sets_erange() {
        // Finite as a double, infinite once rounded to a float.
        fn overflows(what: &str, call: impl Fn() -> f32) {
            set_errno(0);
            assert!(call().is_infinite(), "{what}");
            assert_eq!(errno(), ERANGE, "{what}");
        }
        overflows("expf(100)", || expf(100.0));
        overflows("coshf(100)", || coshf(100.0));
        overflows("powf(10, 50)", || powf(10.0, 50.0));
        overflows("hypotf(3e38, 3e38)", || hypotf(3e38, 3e38));
        overflows("ldexpf(1, 200)", || ldexpf(1.0, 200));
        // A finite result, and an infinity from an infinite argument, leave errno as it was.
        set_errno(7);
        assert_eq!(expf(10.0), 10f32.exp());
        assert_eq!(errno(), 7);
        assert_eq!(expf(f32::INFINITY), f32::INFINITY);
        assert_eq!(powf(f32::INFINITY, 2.0), f32::INFINITY);
        assert_eq!(ldexpf(f32::INFINITY, 1), f32::INFINITY);
        assert_eq!(errno(), 7);
    }

    #[test]
    fn scalbn_and_its_relatives_set_erange_on_overflow() {
        fn overflows(what: &str, call: impl Fn() -> f64) {
            set_errno(0);
            assert!(call().is_infinite(), "{what}");
            assert_eq!(errno(), ERANGE, "{what}");
        }
        overflows("scalbn(1, 2000)", || scalbn(1.0, 2000));
        overflows("scalbln(-1, 2000)", || scalbln(-1.0, 2000));
        overflows("_scalb(1, 2000)", || _scalb(1.0, 2000));
        overflows("scalbnf(1, 200)", || f64::from(scalbnf(1.0, 200)));
        overflows("scalblnf(1, 200)", || f64::from(scalblnf(1.0, 200)));
        // In range, an infinity in, or an underflow to zero: errno stays as it was.
        set_errno(7);
        assert_eq!(scalbn(1.5, 4), 24.0);
        assert_eq!(scalbnf(1.5, 4), 24.0);
        assert!(scalbn(f64::INFINITY, 1).is_infinite());
        assert_eq!(scalbn(1.0, -2000), 0.0);
        assert_eq!(scalbnf(1.0, -200), 0.0);
        assert_eq!(errno(), 7);
    }

    #[test]
    fn fdim_and_nextafter_set_erange_on_overflow() {
        fn overflows(what: &str, call: impl Fn() -> f64) {
            set_errno(0);
            assert!(call().is_infinite(), "{what}");
            assert_eq!(errno(), ERANGE, "{what}");
        }
        overflows("fdim", || fdim(f64::MAX, -f64::MAX));
        overflows("fdimf", || f64::from(fdimf(f32::MAX, -f32::MAX)));
        overflows("nextafter +", || nextafter(f64::MAX, f64::INFINITY));
        overflows("nextafter -", || nextafter(-f64::MAX, f64::NEG_INFINITY));
        overflows("nexttoward +", || nexttoward(f64::MAX, f64::INFINITY));
        overflows("nextafterf +", || {
            f64::from(nextafterf(f32::MAX, f32::INFINITY))
        });
        overflows("nextafterf -", || {
            f64::from(nextafterf(-f32::MAX, f32::NEG_INFINITY))
        });
        set_errno(7);
        assert_eq!(fdim(5.0, 2.0), 3.0);
        assert_eq!(fdim(1.0, 5.0), 0.0);
        assert_eq!(fdimf(5.0, 2.0), 3.0);
        assert!(fdim(f64::INFINITY, 1.0).is_infinite());
        assert!(fdim(f64::NAN, 1.0).is_nan());
        assert_eq!(nextafter(f64::INFINITY, f64::INFINITY), f64::INFINITY);
        assert_eq!(nextafterf(f32::INFINITY, f32::INFINITY), f32::INFINITY);
        assert_eq!(errno(), 7);
    }

    #[test]
    fn classification_uses_msvc_codes() {
        assert_eq!(_dclass(f64::NAN), 2);
        assert_eq!(_dclass(f64::INFINITY), 1);
        assert_eq!(_dclass(0.0), 0);
        assert_eq!(_dclass(5e-324), -2);
        assert_eq!(_dclass(1.0), -1);
        assert_eq!(_fdclass(1e-40), -2);
        let x = -3.0f32;
        assert_eq!(_fdtest(&x), -1);
        assert_eq!(_dsign(-0.0), 0x8000);
        assert_eq!(_fdsign(1.0), 0);
        assert_eq!(_finite(f64::INFINITY), 0);
        assert_eq!(_isnan(f64::NAN), 1);
        assert_eq!(_fpclass(f64::NEG_INFINITY), 0x4);
        assert_eq!(_fpclass(-0.0), 0x20);
        assert_eq!(_fpclass(1.0), 0x100);
        assert_eq!(_fpclass(f64::NAN), 0x2);
    }

    #[test]
    fn frexp_modf_and_remquo() {
        let mut e = 0;
        assert_eq!(frexp(48.0, &mut e), 0.75);
        assert_eq!(e, 6);
        assert_eq!(frexp(5e-324, &mut e), 0.5);
        assert_eq!(e, -1073);
        let mut i = 0.0;
        assert_eq!(modf(-3.25, &mut i), -0.25);
        assert_eq!(i, -3.0);
        let mut q = 0;
        assert_eq!(remquo(10.0, 3.0, &mut q), 1.0);
        assert_eq!(q, 3);
    }

    unsafe extern "C" {
        /// The host's own `remquo`, the reference for the quotient bits.
        #[link_name = "remquo"]
        fn host_remquo(x: f64, y: f64, quo: *mut i32) -> f64;
    }

    #[test]
    fn remquo_keeps_the_low_bits_of_a_huge_quotient() {
        // The quotient here is about 5.9e156, far past 2^53; its low three bits are 100.
        let mut q = 0;
        remquo(7.324363029649866e218, 1.232635503059459e62, &mut q);
        assert_eq!((q, q & 7), (4, 4));
        remquo(-7.324363029649866e218, 1.232635503059459e62, &mut q);
        assert_eq!(q, -4);
        // Near DBL_MAX: |x| - r would overflow, the quotient is 2.
        remquo(f64::MAX, 1.0e308, &mut q);
        assert_eq!(q, 2);
        // Every exponent: the sign and the low three bits of the magnitude, as C asks.
        let mut s = 0x1234_5678_9abc_def1u64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..100_000 {
            let (x, y) = (f64::from_bits(next() >> 1), f64::from_bits(next() >> 1));
            if !x.is_finite() || !y.is_finite() || y == 0.0 {
                continue;
            }
            for (x, y) in [(x, y), (-x, y), (x, -y)] {
                let (mut ours, mut host) = (0, 0);
                let r = remquo(x, y, &mut ours);
                // SAFETY: a pure libm function.
                let host_r = unsafe { host_remquo(x, y, &mut host) };
                assert_eq!(r.to_bits(), host_r.to_bits(), "remquo({x:e}, {y:e})");
                let low = host.unsigned_abs() & 7;
                assert_eq!(
                    ours.unsigned_abs() & 7,
                    low,
                    "remquo({x:e}, {y:e}) gave {ours}, the host {host}"
                );
                if low != 0 {
                    assert_eq!(
                        ours < 0,
                        host < 0,
                        "remquo({x:e}, {y:e}) gave {ours}, the host {host}"
                    );
                }
            }
        }
    }

    #[test]
    fn integer_math() {
        assert_eq!(abs(-5), 5);
        assert_eq!(div(-7, 2), pack(-3, -1));
        let mut out = [0i64; 2];
        lldiv(&mut out, -7_000_000_000, 2);
        assert_eq!(out, [-3_500_000_000, 0]);
        assert_eq!(_rotl(0x8000_0001, 1), 3);
        assert_eq!(_byteswap_ulong(0x11223344), 0x44332211);
    }
}
