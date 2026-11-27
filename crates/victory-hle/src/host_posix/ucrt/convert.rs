//! `api-ms-win-crt-convert`: number parsing (`strtol` family with the
//! Windows widths: `long` is 32 bits), `strtod`, `_itoa` and friends, and
//! the multibyte (UTF-8) to wide (UTF-16) conversions.

use super::runtime::bad_param;
use super::{Ch, EILSEQ, EINVAL, ERANGE, TRUNCATE, exports, set_errno, units};

fn is_space(c: u32) -> bool {
    matches!(c, 0x20 | 0x09..=0x0D)
}

fn digit_value(c: u32) -> Option<u32> {
    char::from_u32(c).and_then(|c| c.to_digit(36))
}

/// How a parsed integer is narrowed.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    I32,
    U32,
    I64,
    U64,
}

/// `strtol` and its relatives over narrow or wide units. The result is
/// sign-extended for signed kinds and zero-extended for unsigned ones.
pub(super) unsafe fn parse_int<C: Ch>(
    s: *const C,
    end: *mut *const C,
    base: i32,
    kind: Kind,
) -> u64 {
    if s.is_null() || base == 1 || !(0..=36).contains(&base) {
        if !end.is_null() {
            // SAFETY: the title passes a valid pointer or null.
            unsafe { end.write(s) };
        }
        return u64::from(bad_param(EINVAL) == 0);
    }
    // SAFETY: a NUL-terminated string.
    let u = unsafe { units(s) };
    let at = |i: usize| u.get(i).map_or(0, |c| c.get());
    let mut i = 0;
    while is_space(at(i)) {
        i += 1;
    }
    let negative = at(i) == u32::from(b'-');
    if matches!(at(i), 0x2B | 0x2D) {
        i += 1;
    }
    let mut base = base as u32;
    let hex_prefix = at(i) == u32::from(b'0')
        && matches!(at(i + 1), 0x78 | 0x58)
        && digit_value(at(i + 2)).is_some_and(|d| d < 16);
    if (base == 0 || base == 16) && hex_prefix {
        base = 16;
        i += 2;
    } else if base == 0 {
        base = if at(i) == u32::from(b'0') { 8 } else { 10 };
    }
    let first = i;
    let (mut value, mut overflow) = (0u128, false);
    while let Some(d) = digit_value(at(i)).filter(|&d| d < base) {
        value = value * u128::from(base) + u128::from(d);
        if value > u128::from(u64::MAX) * 2 {
            overflow = true;
            value = u128::from(u64::MAX) * 2;
        }
        i += 1;
    }
    if !end.is_null() {
        // No digits: nothing was converted, so the end is the start.
        // SAFETY: the title passes a valid pointer or null.
        unsafe { end.write(s.wrapping_add(if i == first { 0 } else { i })) };
    }
    if i == first {
        return 0;
    }
    let (lo, hi): (i128, u128) = match kind {
        Kind::I32 => (i128::from(i32::MIN), i32::MAX as u128),
        Kind::I64 => (i128::from(i64::MIN), i64::MAX as u128),
        Kind::U32 => (0, u128::from(u32::MAX)),
        Kind::U64 => (0, u128::from(u64::MAX)),
    };
    let signed = matches!(kind, Kind::I32 | Kind::I64);
    if signed {
        let limit = if negative { hi + 1 } else { hi };
        if overflow || value > limit {
            set_errno(ERANGE);
            return if negative {
                lo as i64 as u64
            } else {
                hi as u64
            };
        }
        if negative {
            (value as i128).wrapping_neg() as i64 as u64
        } else {
            value as u64
        }
    } else {
        if overflow || value > hi {
            set_errno(ERANGE);
            return hi as u64;
        }
        if negative {
            (value as u64).wrapping_neg() & hi as u64
        } else {
            value as u64
        }
    }
}

macro_rules! int_fns {
    ($($narrow:ident, $wide:ident, $ret:ty, $kind:expr;)*) => {$(
        pub extern "system" fn $narrow(s: *const u8, end: *mut *const u8, base: i32) -> $ret {
            // SAFETY: the title's string.
            unsafe { parse_int(s, end, base, $kind) as $ret }
        }
        pub extern "system" fn $wide(s: *const u16, end: *mut *const u16, base: i32) -> $ret {
            // SAFETY: the title's string.
            unsafe { parse_int(s, end, base, $kind) as $ret }
        }
    )*};
}

int_fns! {
    strtol, wcstol, i32, Kind::I32;
    strtoul, wcstoul, u32, Kind::U32;
    strtoll, wcstoll, i64, Kind::I64;
    strtoull, wcstoull, u64, Kind::U64;
}

pub extern "system" fn atoi(s: *const u8) -> i32 {
    strtol(s, std::ptr::null_mut(), 10)
}

pub extern "system" fn atoll(s: *const u8) -> i64 {
    strtoll(s, std::ptr::null_mut(), 10)
}

pub extern "system" fn _wtoi(s: *const u16) -> i32 {
    wcstol(s, std::ptr::null_mut(), 10)
}

pub extern "system" fn _wtoi64(s: *const u16) -> i64 {
    wcstoll(s, std::ptr::null_mut(), 10)
}

// Floating point.

/// What a floating-point prefix is.
#[derive(Debug, PartialEq)]
enum Float {
    Number(String),
    Hex {
        negative: bool,
        mantissa: u64,
        exponent: i32,
    },
    Inf(bool),
    Nan(bool),
}

/// The longest floating-point prefix of `s` (after whitespace) and its length.
fn float_prefix(s: &[u8]) -> Option<(Float, usize)> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while is_space(u32::from(at(i))) {
        i += 1;
    }
    let negative = at(i) == b'-';
    if matches!(at(i), b'+' | b'-') {
        i += 1;
    }
    let word = |i: usize, w: &[u8]| (0..w.len()).all(|k| at(i + k).to_ascii_lowercase() == w[k]);
    if word(i, b"inf") {
        let n = if word(i, b"infinity") { 8 } else { 3 };
        return Some((Float::Inf(negative), i + n));
    }
    if word(i, b"nan") {
        let mut n = i + 3;
        if at(n) == b'(' {
            let mut k = n + 1;
            while at(k).is_ascii_alphanumeric() || at(k) == b'_' {
                k += 1;
            }
            if at(k) == b')' {
                n = k + 1;
            }
        }
        return Some((Float::Nan(negative), n));
    }
    if at(i) == b'0' && matches!(at(i + 1), b'x' | b'X') {
        let (mut k, mut mantissa, mut exponent, mut any) = (i + 2, 0u64, 0i32, false);
        let mut seen_dot = false;
        loop {
            let c = at(k);
            if let Some(d) = (c as char).to_digit(16) {
                any = true;
                if mantissa >> 60 == 0 {
                    mantissa = mantissa << 4 | u64::from(d);
                    if seen_dot {
                        exponent -= 4;
                    }
                } else if !seen_dot {
                    exponent += 4;
                }
            } else if c == b'.' && !seen_dot {
                seen_dot = true;
            } else {
                break;
            }
            k += 1;
        }
        if any {
            if matches!(at(k), b'p' | b'P') {
                let mut j = k + 1;
                let eneg = at(j) == b'-';
                if matches!(at(j), b'+' | b'-') {
                    j += 1;
                }
                if at(j).is_ascii_digit() {
                    let mut e = 0i32;
                    while at(j).is_ascii_digit() {
                        e = e.saturating_mul(10).saturating_add(i32::from(at(j) - b'0'));
                        j += 1;
                    }
                    exponent = exponent.saturating_add(if eneg { -e } else { e });
                    k = j;
                }
            }
            return Some((
                Float::Hex {
                    negative,
                    mantissa,
                    exponent,
                },
                k,
            ));
        }
        // "0x" with no digits is the number 0 followed by garbage.
        return Some((
            Float::Number(if negative { "-0".into() } else { "0".into() }),
            i + 1,
        ));
    }
    let digits_start = i;
    let mut k = i;
    let mut any = false;
    while at(k).is_ascii_digit() {
        k += 1;
        any = true;
    }
    if at(k) == b'.' {
        k += 1;
        while at(k).is_ascii_digit() {
            k += 1;
            any = true;
        }
    }
    if !any {
        return None;
    }
    if matches!(at(k), b'e' | b'E') {
        let mut j = k + 1;
        if matches!(at(j), b'+' | b'-') {
            j += 1;
        }
        if at(j).is_ascii_digit() {
            while at(j).is_ascii_digit() {
                j += 1;
            }
            k = j;
        }
    }
    let mut text = String::from_utf8_lossy(&s[digits_start..k]).into_owned();
    if negative {
        text.insert(0, '-');
    }
    Some((Float::Number(text), k))
}

fn nan(negative: bool) -> f64 {
    if negative { -f64::NAN } else { f64::NAN }
}

fn to_f64(f: &Float) -> f64 {
    match f {
        Float::Number(t) => t.parse().unwrap_or(0.0),
        Float::Hex {
            negative,
            mantissa,
            exponent,
        } => {
            // One exact scaling: a power of two computed apart underflows to 0 below 2^-1022 and
            // makes 0 * inf for a zero mantissa with a large exponent.
            // SAFETY: libm's scalbn is a pure function of its arguments.
            let v = unsafe { super::libc::scalbn(*mantissa as f64, *exponent) };
            if *negative { -v } else { v }
        }
        Float::Inf(n) => {
            if *n {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        }
        Float::Nan(n) => nan(*n),
    }
}

/// Whether a number read as an infinity or a zero is out of range, which the CRT reports with
/// `ERANGE`: an infinity from digits (overflow), or a zero from nonzero digits (underflow).
fn out_of_range(f: &Float, infinite: bool, zero: bool) -> bool {
    match f {
        Float::Number(t) => {
            let digits = t.split(['e', 'E']).next().unwrap_or("");
            infinite || (zero && digits.bytes().any(|b| matches!(b, b'1'..=b'9')))
        }
        Float::Hex { mantissa, .. } => infinite || (zero && *mantissa != 0),
        Float::Inf(_) | Float::Nan(_) => false,
    }
}

/// Parses a prefix of `s` as a double: the value and the bytes consumed.
pub(super) fn parse_double(s: &[u8]) -> Option<(f64, usize)> {
    let (f, n) = float_prefix(s)?;
    let v = to_f64(&f);
    if out_of_range(&f, v.is_infinite(), v == 0.0) {
        set_errno(ERANGE);
    }
    Some((v, n))
}

fn parse_single(s: &[u8]) -> Option<(f32, usize)> {
    let (f, n) = float_prefix(s)?;
    let v = match &f {
        Float::Number(t) => t.parse().unwrap_or(0.0),
        other => to_f64(other) as f32,
    };
    if out_of_range(&f, v.is_infinite(), v == 0.0) {
        set_errno(ERANGE);
    }
    Some((v, n))
}

/// Narrow units of a wide string for scanning: anything above ASCII ends a number.
fn ascii_of(u: &[u16]) -> Vec<u8> {
    u.iter()
        .map(|&c| u8::try_from(c).ok().filter(u8::is_ascii).unwrap_or(0xFF))
        .collect()
}

pub extern "system" fn strtod(s: *const u8, end: *mut *const u8) -> f64 {
    if s.is_null() {
        if !end.is_null() {
            // SAFETY: the title passes a valid pointer or null.
            unsafe { end.write(s) };
        }
        bad_param(EINVAL);
        return 0.0;
    }
    // SAFETY: a NUL-terminated string.
    let b = unsafe { super::bytes(s) };
    let (v, n) = parse_double(b).unwrap_or((0.0, 0));
    if !end.is_null() {
        // SAFETY: the title passes a valid pointer or null.
        unsafe { end.write(s.wrapping_add(n)) };
    }
    v
}

pub extern "system" fn strtof(s: *const u8, end: *mut *const u8) -> f32 {
    if s.is_null() {
        if !end.is_null() {
            // SAFETY: the title passes a valid pointer or null.
            unsafe { end.write(s) };
        }
        bad_param(EINVAL);
        return 0.0;
    }
    // SAFETY: a NUL-terminated string.
    let b = unsafe { super::bytes(s) };
    let (v, n) = parse_single(b).unwrap_or((0.0, 0));
    if !end.is_null() {
        // SAFETY: the title passes a valid pointer or null.
        unsafe { end.write(s.wrapping_add(n)) };
    }
    v
}

pub extern "system" fn wcstod(s: *const u16, end: *mut *const u16) -> f64 {
    if s.is_null() {
        if !end.is_null() {
            // SAFETY: the title passes a valid pointer or null.
            unsafe { end.write(s) };
        }
        bad_param(EINVAL);
        return 0.0;
    }
    // SAFETY: a NUL-terminated string.
    let b = ascii_of(unsafe { super::wide(s) });
    let (v, n) = parse_double(&b).unwrap_or((0.0, 0));
    if !end.is_null() {
        // SAFETY: the title passes a valid pointer or null.
        unsafe { end.write(s.wrapping_add(n)) };
    }
    v
}

pub extern "system" fn wcstof(s: *const u16, end: *mut *const u16) -> f32 {
    if s.is_null() {
        if !end.is_null() {
            // SAFETY: the title passes a valid pointer or null.
            unsafe { end.write(s) };
        }
        bad_param(EINVAL);
        return 0.0;
    }
    // SAFETY: a NUL-terminated string.
    let b = ascii_of(unsafe { super::wide(s) });
    let (v, n) = parse_single(&b).unwrap_or((0.0, 0));
    if !end.is_null() {
        // SAFETY: the title passes a valid pointer or null.
        unsafe { end.write(s.wrapping_add(n)) };
    }
    v
}

pub extern "system" fn atof(s: *const u8) -> f64 {
    strtod(s, std::ptr::null_mut())
}

pub extern "system" fn _wtof(s: *const u16) -> f64 {
    wcstod(s, std::ptr::null_mut())
}

// Integers to text.

fn radix_digits(mut v: u64, radix: u32, negative: bool) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        out.push(char::from_digit((v % u64::from(radix)) as u32, radix).unwrap_or('0') as u8);
        v /= u64::from(radix);
        if v == 0 {
            break;
        }
    }
    if negative {
        out.push(b'-');
    }
    out.reverse();
    out
}

/// `_itoa` over either width: the text of `value` in `radix`, which is
/// signed only in radix 10 for signed types.
unsafe fn g_itoa<C: Ch>(
    value: i64,
    signed: bool,
    bits: u32,
    buf: *mut C,
    size: usize,
    radix: i32,
) -> i32 {
    if buf.is_null() || size == 0 {
        return bad_param(EINVAL);
    }
    if !(2..=36).contains(&radix) {
        // SAFETY: the title's buffer.
        unsafe { buf.write(C::default()) };
        return bad_param(EINVAL);
    }
    let (negative, magnitude) = if signed && radix == 10 && value < 0 {
        (true, value.unsigned_abs())
    } else if bits == 32 {
        (false, u64::from(value as u32))
    } else {
        (false, value as u64)
    };
    let text = radix_digits(magnitude, radix as u32, negative);
    if text.len() + 1 > size {
        // SAFETY: the title's buffer.
        unsafe { buf.write(C::default()) };
        return bad_param(ERANGE);
    }
    // SAFETY: the title's buffer, large enough.
    unsafe {
        for (i, &b) in text.iter().enumerate() {
            buf.add(i).write(C::make(u32::from(b)));
        }
        buf.add(text.len()).write(C::default());
    }
    0
}

macro_rules! itoa_fns {
    ($($name:ident, $name_s:ident, $wname:ident, $wname_s:ident, $ty:ty, $signed:expr, $bits:expr;)*) => {$(
        pub extern "system" fn $name(v: $ty, buf: *mut u8, radix: i32) -> *mut u8 {
            // SAFETY: the title's buffer (unbounded, as `_itoa` is).
            unsafe { g_itoa(v as i64, $signed, $bits, buf, usize::MAX, radix) };
            buf
        }
        pub extern "system" fn $name_s(v: $ty, buf: *mut u8, size: usize, radix: i32) -> i32 {
            // SAFETY: the title's buffer.
            unsafe { g_itoa(v as i64, $signed, $bits, buf, size, radix) }
        }
        pub extern "system" fn $wname(v: $ty, buf: *mut u16, radix: i32) -> *mut u16 {
            // SAFETY: the title's buffer.
            unsafe { g_itoa(v as i64, $signed, $bits, buf, usize::MAX, radix) };
            buf
        }
        pub extern "system" fn $wname_s(v: $ty, buf: *mut u16, size: usize, radix: i32) -> i32 {
            // SAFETY: the title's buffer.
            unsafe { g_itoa(v as i64, $signed, $bits, buf, size, radix) }
        }
    )*};
}

itoa_fns! {
    _itoa, _itoa_s, _itow, _itow_s, i32, true, 32;
    _ltoa, _ltoa_s, _ltow, _ltow_s, i32, true, 32;
    _ultoa, _ultoa_s, _ultow, _ultow_s, u32, false, 32;
    _i64toa, _i64toa_s, _i64tow, _i64tow_s, i64, true, 64;
    _ui64toa, _ui64toa_s, _ui64tow, _ui64tow_s, u64, false, 64;
}

// Multibyte (UTF-8) and wide (UTF-16).

/// A UTF-8 sequence: the code point and its length; `Err(true)` when the
/// bytes end early, `Err(false)` when they are not UTF-8.
fn decode(b: &[u8]) -> Result<(u32, usize), bool> {
    let Some(&first) = b.first() else {
        return Err(true);
    };
    let (len, init) = match first {
        0x00..=0x7F => return Ok((u32::from(first), 1)),
        0xC2..=0xDF => (2, u32::from(first & 0x1F)),
        0xE0..=0xEF => (3, u32::from(first & 0x0F)),
        0xF0..=0xF4 => (4, u32::from(first & 0x07)),
        _ => return Err(false),
    };
    let mut cp = init;
    for k in 1..len {
        match b.get(k) {
            None => return Err(true),
            Some(&c) if c & 0xC0 == 0x80 => cp = cp << 6 | u32::from(c & 0x3F),
            Some(_) => return Err(false),
        }
    }
    let min = [0, 0, 0x80, 0x800, 0x10000][len];
    if cp < min || cp > 0x10FFFF || (0xD800..0xE000).contains(&cp) {
        return Err(false);
    }
    Ok((cp, len))
}

/// A whole NUL-terminated (or `limit`-long) multibyte string as UTF-16.
fn to_wide(src: &[u8]) -> Option<Vec<u16>> {
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        let (cp, n) = decode(&src[i..]).ok()?;
        let mut buf = [0u16; 2];
        out.extend_from_slice(char::from_u32(cp)?.encode_utf16(&mut buf));
        i += n;
    }
    Some(out)
}

/// UTF-16 as UTF-8; `None` for a lone surrogate.
fn to_multibyte(src: &[u16]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(src.len());
    for c in char::decode_utf16(src.iter().copied()) {
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.ok()?.encode_utf8(&mut buf).as_bytes());
    }
    Some(out)
}

/// `(size_t)-1`.
const FAILED: usize = usize::MAX;

pub extern "system" fn mbstowcs(dst: *mut u16, src: *const u8, count: usize) -> usize {
    // SAFETY: a NUL-terminated string and the title's buffer of `count` units.
    unsafe {
        let Some(w) = to_wide(super::bytes(src)) else {
            set_errno(EILSEQ);
            return FAILED;
        };
        if dst.is_null() {
            return w.len();
        }
        let n = w.len().min(count);
        std::ptr::copy_nonoverlapping(w.as_ptr(), dst, n);
        if n < count {
            dst.add(n).write(0);
        }
        n
    }
}

/// The largest prefix of valid UTF-8 `m` of at most `limit` bytes on a character boundary.
fn utf8_floor(m: &[u8], limit: usize) -> usize {
    let mut n = m.len().min(limit);
    while n < m.len() && n > 0 && m[n] & 0xC0 == 0x80 {
        n -= 1;
    }
    n
}

pub extern "system" fn wcstombs(dst: *mut u8, src: *const u16, count: usize) -> usize {
    // SAFETY: a NUL-terminated string and the title's buffer of `count` bytes.
    unsafe {
        let Some(m) = to_multibyte(super::wide(src)) else {
            set_errno(EILSEQ);
            return FAILED;
        };
        if dst.is_null() {
            return m.len();
        }
        // Whole characters only.
        let n = utf8_floor(&m, count);
        std::ptr::copy_nonoverlapping(m.as_ptr(), dst, n);
        if n < count {
            dst.add(n).write(0);
        }
        n
    }
}

pub extern "system" fn mbstowcs_s(
    ret: *mut usize,
    dst: *mut u16,
    size: usize,
    src: *const u8,
    count: usize,
) -> i32 {
    if !ret.is_null() {
        // SAFETY: the title passes a valid pointer or null.
        unsafe { ret.write(0) };
    }
    if (dst.is_null() && size != 0) || (!dst.is_null() && size == 0) {
        return bad_param(EINVAL);
    }
    if src.is_null() {
        if !dst.is_null() {
            // SAFETY: the title's buffer has at least one unit.
            unsafe { dst.write(0) };
        }
        return bad_param(EINVAL);
    }
    // SAFETY: a NUL-terminated string.
    let Some(mut w) = to_wide(unsafe { super::bytes(src) }) else {
        if !dst.is_null() {
            // SAFETY: the title's buffer.
            unsafe { dst.write(0) };
        }
        return super::fail(EILSEQ);
    };
    // Size query: `count` is ignored when `dst` is null.
    if dst.is_null() {
        if !ret.is_null() {
            // SAFETY: valid pointer.
            unsafe { ret.write(w.len() + 1) };
        }
        return 0;
    }
    if count != TRUNCATE {
        w.truncate(count);
    }
    let mut status = 0;
    if w.len() + 1 > size {
        if count != TRUNCATE {
            // SAFETY: the title's buffer.
            unsafe { dst.write(0) };
            return bad_param(ERANGE);
        }
        w.truncate(size - 1);
        status = super::STRUNCATE;
    }
    // SAFETY: the title's buffer holds `w` and a NUL.
    unsafe {
        std::ptr::copy_nonoverlapping(w.as_ptr(), dst, w.len());
        dst.add(w.len()).write(0);
        if !ret.is_null() {
            ret.write(w.len() + 1);
        }
    }
    status
}

pub extern "system" fn wcstombs_s(
    ret: *mut usize,
    dst: *mut u8,
    size: usize,
    src: *const u16,
    count: usize,
) -> i32 {
    if !ret.is_null() {
        // SAFETY: the title passes a valid pointer or null.
        unsafe { ret.write(0) };
    }
    if (dst.is_null() && size != 0) || (!dst.is_null() && size == 0) {
        return bad_param(EINVAL);
    }
    if src.is_null() {
        if !dst.is_null() {
            // SAFETY: the title's buffer has at least one byte.
            unsafe { dst.write(0) };
        }
        return bad_param(EINVAL);
    }
    // SAFETY: a NUL-terminated string.
    let Some(mut m) = to_multibyte(unsafe { super::wide(src) }) else {
        if !dst.is_null() {
            // SAFETY: the title's buffer.
            unsafe { dst.write(0) };
        }
        return super::fail(EILSEQ);
    };
    // Size query: `count` is ignored when `dst` is null.
    if dst.is_null() {
        if !ret.is_null() {
            // SAFETY: valid pointer.
            unsafe { ret.write(m.len() + 1) };
        }
        return 0;
    }
    if count != TRUNCATE {
        m.truncate(utf8_floor(&m, count));
    }
    let mut status = 0;
    if m.len() + 1 > size {
        if count != TRUNCATE {
            // SAFETY: the title's buffer.
            unsafe { dst.write(0) };
            return bad_param(ERANGE);
        }
        m.truncate(utf8_floor(&m, size - 1));
        status = super::STRUNCATE;
    }
    // SAFETY: the title's buffer holds `m` and a NUL.
    unsafe {
        std::ptr::copy_nonoverlapping(m.as_ptr(), dst, m.len());
        dst.add(m.len()).write(0);
        if !ret.is_null() {
            ret.write(m.len() + 1);
        }
    }
    status
}

/// `mbtowc`/`mbrtowc` on at most `n` bytes at `s`: the unit, bytes taken.
fn first_char(s: *const u8, n: usize) -> Result<(Vec<u16>, usize), isize> {
    // SAFETY: the title's `n` bytes.
    let b = unsafe { std::slice::from_raw_parts(s, n) };
    match decode(b) {
        Ok((cp, len)) => {
            let mut buf = [0u16; 2];
            Ok((
                char::from_u32(cp)
                    .ok_or(-1isize)?
                    .encode_utf16(&mut buf)
                    .to_vec(),
                len,
            ))
        }
        Err(true) => Err(-2),
        Err(false) => Err(-1),
    }
}

pub extern "system" fn mbtowc(dst: *mut u16, s: *const u8, n: usize) -> i32 {
    if s.is_null() {
        return 0;
    }
    if n == 0 {
        return -1;
    }
    match first_char(s, n) {
        Ok((w, len)) => {
            if !dst.is_null() {
                // SAFETY: the title's unit.
                unsafe { dst.write(w[0]) };
            }
            // SAFETY: `len` bytes were read, the first of them readable.
            if unsafe { *s } == 0 { 0 } else { len as i32 }
        }
        Err(_) => {
            set_errno(EILSEQ);
            -1
        }
    }
}

pub extern "system" fn mblen(s: *const u8, n: usize) -> i32 {
    mbtowc(std::ptr::null_mut(), s, n)
}

pub extern "system" fn mbrtowc(dst: *mut u16, s: *const u8, n: usize, _state: usize) -> usize {
    if s.is_null() {
        return 0;
    }
    if n == 0 {
        return (-2isize) as usize;
    }
    match first_char(s, n) {
        Ok((w, len)) => {
            if !dst.is_null() {
                // SAFETY: the title's unit.
                unsafe { dst.write(w[0]) };
            }
            // SAFETY: the first byte is readable.
            if unsafe { *s } == 0 { 0 } else { len }
        }
        Err(e) => {
            if e == -1 {
                set_errno(EILSEQ);
            }
            e as usize
        }
    }
}

pub extern "system" fn mbrlen(s: *const u8, n: usize, state: usize) -> usize {
    mbrtowc(std::ptr::null_mut(), s, n, state)
}

pub extern "system" fn wcrtomb(dst: *mut u8, c: u16, _state: usize) -> usize {
    if dst.is_null() {
        return 1;
    }
    let Some(m) = to_multibyte(&[c]) else {
        set_errno(EILSEQ);
        return FAILED;
    };
    // SAFETY: the title's buffer of at least 4 bytes (`MB_LEN_MAX`).
    unsafe { std::ptr::copy_nonoverlapping(m.as_ptr(), dst, m.len()) };
    m.len()
}

pub extern "system" fn wctomb(dst: *mut u8, c: u16) -> i32 {
    if dst.is_null() {
        return 0;
    }
    match wcrtomb(dst, c, 0) {
        FAILED => -1,
        n => n as i32,
    }
}

pub extern "system" fn wctomb_s(ret: *mut i32, dst: *mut u8, size: usize, c: u16) -> i32 {
    if dst.is_null() {
        if !ret.is_null() {
            // SAFETY: valid pointer.
            unsafe { ret.write(0) };
        }
        return 0;
    }
    let Some(m) = to_multibyte(&[c]) else {
        return super::fail(EILSEQ);
    };
    if m.len() > size {
        return bad_param(ERANGE);
    }
    // SAFETY: the title's buffer of `size`.
    unsafe {
        std::ptr::copy_nonoverlapping(m.as_ptr(), dst, m.len());
        if !ret.is_null() {
            ret.write(m.len() as i32);
        }
    }
    0
}

pub extern "system" fn btowc(c: i32) -> u32 {
    if (0..128).contains(&c) {
        c as u32
    } else {
        0xFFFF
    }
}

pub extern "system" fn wctob(c: u32) -> i32 {
    if c < 128 { c as i32 } else { -1 }
}

pub extern "system" fn _mbstrlen(s: *const u8) -> usize {
    // SAFETY: a NUL-terminated string.
    match to_wide(unsafe { super::bytes(s) }) {
        Some(w) => char::decode_utf16(w).count(),
        None => {
            set_errno(EILSEQ);
            FAILED
        }
    }
}

/// The bytes of `s` that make its first `units` UTF-16 units, whole characters only.
fn bytes_for_units(s: &[u8], units: usize) -> usize {
    let (mut i, mut made) = (0, 0);
    while let Ok((cp, len)) = decode(&s[i..]) {
        let w = if cp >= 0x10000 { 2 } else { 1 };
        if made + w > units {
            break;
        }
        (made, i) = (made + w, i + len);
    }
    i
}

/// The UTF-16 units of `s` that make its first `bytes` UTF-8 bytes, whole characters only.
fn units_for_bytes(s: &[u16], bytes: usize) -> usize {
    let (mut i, mut made) = (0, 0);
    for c in char::decode_utf16(s.iter().copied()).map_while(Result::ok) {
        if made + c.len_utf8() > bytes {
            break;
        }
        (made, i) = (made + c.len_utf8(), i + c.len_utf16());
    }
    i
}

/// `mbsrtowcs(dst, &src, n, state)`: advances `*src` past what was converted.
pub extern "system" fn mbsrtowcs(
    dst: *mut u16,
    src: *mut *const u8,
    count: usize,
    _state: usize,
) -> usize {
    // SAFETY: the title's pointer to its string.
    let s = unsafe { *src };
    let n = mbstowcs(dst, s, count);
    if n != FAILED && !dst.is_null() {
        // SAFETY: the title's pointer: null once the whole string and its NUL are stored, else
        // just past the last character converted.
        unsafe {
            let all = super::bytes(s);
            let used = bytes_for_units(all, n);
            *src = if used == all.len() && n < count {
                std::ptr::null()
            } else {
                s.add(used)
            };
        }
    }
    n
}

pub extern "system" fn wcsrtombs(
    dst: *mut u8,
    src: *mut *const u16,
    count: usize,
    _state: usize,
) -> usize {
    // SAFETY: the title's pointer to its string.
    let s = unsafe { *src };
    let n = wcstombs(dst, s, count);
    if n != FAILED && !dst.is_null() {
        // SAFETY: the title's pointer, as in `mbsrtowcs`: whole characters are written, so a
        // character that does not fit stops the conversion with room left.
        unsafe {
            let all = super::wide(s);
            let used = units_for_bytes(all, n);
            *src = if used == all.len() && n < count {
                std::ptr::null()
            } else {
                s.add(used)
            };
        }
    }
    n
}

pub(super) fn utf8_to_utf16(b: &[u8]) -> Vec<u16> {
    String::from_utf8_lossy(b).encode_utf16().collect()
}

pub(super) fn utf16_to_utf8(u: &[u16]) -> Vec<u8> {
    String::from_utf16_lossy(u).into_bytes()
}

exports!(fn lookup_plain;
    strtol, wcstol, strtoul, wcstoul, strtoll, wcstoll, strtoull, wcstoull, atoi, atoll, _wtoi,
    _wtoi64, _itoa, _itoa_s, _itow, _itow_s, _ltoa, _ltoa_s, _ltow, _ltow_s, _ultoa, _ultoa_s,
    _ultow, _ultow_s, _i64toa, _i64toa_s, _i64tow, _i64tow_s, _ui64toa, _ui64toa_s, _ui64tow,
    _ui64tow_s, mbstowcs, wcstombs, mbstowcs_s, wcstombs_s, mbtowc, mblen, mbrtowc, mbrlen,
    wcrtomb, wctomb, wctomb_s, btowc, wctob, _mbstrlen, mbsrtowcs, wcsrtombs;
    "atol" => atoi, "_atoi64" => atoll, "_strtoi64" => strtoll, "_strtoui64" => strtoull,
    "_wcstoi64" => wcstoll, "_wcstoui64" => wcstoull, "_wtol" => _wtoi, "_wtoll" => _wtoi64,
    "_strtol_l" => strtol, "_strtoul_l" => strtoul, "_strtoll_l" => strtoll,
    "_strtoull_l" => strtoull, "_strtoi64_l" => strtoll, "_strtoui64_l" => strtoull,
    "_wcstol_l" => wcstol, "_wcstoul_l" => wcstoul, "_wcstoll_l" => wcstoll,
    "_wcstoull_l" => wcstoull, "_wcstoi64_l" => wcstoll, "_wcstoui64_l" => wcstoull,
    "_atoi_l" => atoi, "_atol_l" => atoi, "_atoll_l" => atoll, "_atoi64_l" => atoll,
    "_wtoi_l" => _wtoi, "_wtol_l" => _wtoi, "_wtoi64_l" => _wtoi64, "_mbstowcs_l" => mbstowcs,
    "_wcstombs_l" => wcstombs, "_mbstowcs_s_l" => mbstowcs_s, "_wcstombs_s_l" => wcstombs_s,
    "_mbtowc_l" => mbtowc, "_mblen_l" => mblen, "_wctomb_l" => wctomb, "_wctomb_s_l" => wctomb_s,
    "_mbstrlen_l" => _mbstrlen);

// Functions that return or take floating point are `extern "system"` too
// (a pointer argument first, then a double or float return only), so they
// need no `win64`. `_wtof` and `atof` take a pointer only.
exports!(fn lookup_float_ret;
    strtod, strtof, wcstod, wcstof, atof, _wtof;
    "strtold" => strtod, "wcstold" => wcstod, "_strtod_l" => strtod, "_strtof_l" => strtof,
    "_wcstod_l" => wcstod, "_wcstof_l" => wcstof, "_atof_l" => atof, "_wtof_l" => _wtof,
    "_strtold_l" => strtod, "_wcstold_l" => wcstod);

pub(super) fn lookup(name: &str) -> Option<usize> {
    lookup_plain(name).or_else(|| lookup_float_ret(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn end_of(s: &[u8], f: impl Fn(*const u8, *mut *const u8) -> i64) -> (i64, usize) {
        let mut end = std::ptr::null();
        let v = f(s.as_ptr(), &mut end);
        (v, end as usize - s.as_ptr() as usize)
    }

    #[test]
    fn integers_with_windows_widths() {
        assert_eq!(
            end_of(b"  -42abc\0", |s, e| i64::from(strtol(s, e, 10))),
            (-42, 5)
        );
        assert_eq!(
            end_of(b"0x1fz\0", |s, e| i64::from(strtol(s, e, 0))),
            (31, 4)
        );
        assert_eq!(
            end_of(b"0755\0", |s, e| i64::from(strtol(s, e, 0))),
            (493, 4)
        );
        assert_eq!(end_of(b"xyz\0", |s, e| i64::from(strtol(s, e, 10))), (0, 0));
        assert_eq!(end_of(b"0x\0", |s, e| i64::from(strtol(s, e, 16))), (0, 1));
        set_errno(0);
        assert_eq!(
            strtol(b"99999999999\0".as_ptr(), std::ptr::null_mut(), 10),
            i32::MAX
        );
        assert_eq!(super::super::errno(), ERANGE);
        assert_eq!(
            strtol(b"-99999999999\0".as_ptr(), std::ptr::null_mut(), 10),
            i32::MIN
        );
        assert_eq!(
            strtoul(b"-1\0".as_ptr(), std::ptr::null_mut(), 10),
            u32::MAX
        );
        assert_eq!(
            strtoull(b"18446744073709551615\0".as_ptr(), std::ptr::null_mut(), 10),
            u64::MAX
        );
        assert_eq!(
            strtoll(b"-9223372036854775808\0".as_ptr(), std::ptr::null_mut(), 10),
            i64::MIN
        );
        assert_eq!(atoi(b"123\0".as_ptr()), 123);
        assert_eq!(wcstol(w("  -7").as_ptr(), std::ptr::null_mut(), 10), -7);
        assert_eq!(_wtoi64(w("5000000000").as_ptr()), 5_000_000_000);
    }

    #[test]
    fn an_underflow_to_zero_sets_erange() {
        let read = |s: &[u8]| {
            set_errno(0);
            let v = strtod(s.as_ptr(), std::ptr::null_mut());
            (v, super::super::errno())
        };
        assert_eq!(read(b"1e-400\0"), (0.0, ERANGE));
        assert_eq!(read(b"-0.0000001e-330\0"), (-0.0, ERANGE));
        // Zero digits are a zero, not an underflow; a subnormal is no underflow to zero.
        assert_eq!(read(b"0e-400\0"), (0.0, 0));
        assert_eq!(read(b"0.000\0"), (0.0, 0));
        assert_eq!(read(b"4.9e-324\0"), (f64::from_bits(1), 0));
        set_errno(0);
        assert_eq!(strtof(b"1e-50\0".as_ptr(), std::ptr::null_mut()), 0.0);
        assert_eq!(super::super::errno(), ERANGE);
        set_errno(0);
        assert_eq!(strtof(b"1e-30\0".as_ptr(), std::ptr::null_mut()), 1e-30);
        assert_eq!(super::super::errno(), 0);
    }

    #[test]
    fn doubles() {
        let mut end = std::ptr::null();
        let s = b" -1.5e3xyz\0";
        assert_eq!(strtod(s.as_ptr(), &mut end), -1500.0);
        assert_eq!(end as usize - s.as_ptr() as usize, 7);
        assert_eq!(atof(b".5\0".as_ptr()), 0.5);
        assert_eq!(atof(b"5.\0".as_ptr()), 5.0);
        assert_eq!(strtod(b"1e\0".as_ptr(), &mut end), 1.0);
        assert_eq!(strtod(b"inf\0".as_ptr(), &mut end), f64::INFINITY);
        assert!(strtod(b"-nan(abc)\0".as_ptr(), &mut end).is_nan());
        assert_eq!(strtod(b"0x1.8p1\0".as_ptr(), &mut end), 3.0);
        assert_eq!(strtod(b"junk\0".as_ptr(), &mut end), 0.0);
        assert_eq!(strtof(b"0.1\0".as_ptr(), &mut end), 0.1f32);
        set_errno(0);
        assert_eq!(strtod(b"1e999\0".as_ptr(), &mut end), f64::INFINITY);
        assert_eq!(super::super::errno(), ERANGE);
        assert_eq!(wcstod(w("2.25 ").as_ptr(), std::ptr::null_mut()), 2.25);
    }

    #[test]
    fn hex_doubles_scale_exactly_at_both_ends() {
        let hex = |s: &[u8]| {
            set_errno(0);
            let v = strtod(s.as_ptr(), std::ptr::null_mut());
            (v, super::super::errno())
        };
        // What printf's %a writes for 2^-1000, and the smallest subnormal.
        assert_eq!(hex(b"0x1.0000000000000p-1000\0"), (2f64.powi(-1000), 0));
        assert_eq!(hex(b"0x1p-1074\0"), (f64::from_bits(1), 0));
        assert_eq!(hex(b"-0x1.8p-1073\0"), (-f64::from_bits(3), 0));
        assert_eq!(hex(b"0x1.fffffffffffffp1023\0"), (f64::MAX, 0));
        // A zero mantissa is zero whatever its exponent.
        assert_eq!(hex(b"0x0p2000\0"), (0.0, 0));
        assert_eq!(hex(b"0x1p1024\0"), (f64::INFINITY, ERANGE));
    }

    #[test]
    fn integers_to_text() {
        let mut b = [0u8; 40];
        _itoa(-255, b.as_mut_ptr(), 10);
        assert_eq!(&b[..5], b"-255\0");
        _itoa(-1, b.as_mut_ptr(), 16);
        assert_eq!(&b[..9], b"ffffffff\0");
        _ui64toa(u64::MAX, b.as_mut_ptr(), 10);
        assert_eq!(&b[..21], b"18446744073709551615\0");
        _i64toa(35, b.as_mut_ptr(), 36);
        assert_eq!(&b[..2], b"z\0");
        assert_eq!(_itoa_s(12345, b.as_mut_ptr(), 3, 10), ERANGE);
        assert_eq!(b[0], 0);
        let mut wbuf = [0u16; 8];
        _itow(-9, wbuf.as_mut_ptr(), 10);
        assert_eq!(&wbuf[..3], &w("-9")[..]);
        assert_eq!(_itoa_s(1, b.as_mut_ptr(), 8, 99), EINVAL);
    }

    #[test]
    fn multibyte_and_wide() {
        let text = "aé€😀";
        let mut m = text.as_bytes().to_vec();
        m.push(0);
        let mut out = [0u16; 8];
        assert_eq!(mbstowcs(std::ptr::null_mut(), m.as_ptr(), 0), 5);
        assert_eq!(mbstowcs(out.as_mut_ptr(), m.as_ptr(), 8), 5);
        assert_eq!(&out[..6], &w(text)[..]);
        let mut back = [0u8; 16];
        assert_eq!(wcstombs(back.as_mut_ptr(), out.as_ptr(), 16), 10);
        assert_eq!(&back[..10], text.as_bytes());
        // Never half a character.
        assert_eq!(wcstombs(back.as_mut_ptr(), out.as_ptr(), 5), 3);
        assert_eq!(
            wcstombs(std::ptr::null_mut(), [0xD800u16, 0].as_ptr(), 0),
            FAILED
        );
        assert_eq!(
            mbstowcs(std::ptr::null_mut(), [0xFFu8, 0].as_ptr(), 0),
            FAILED
        );
        let mut n = 0;
        assert_eq!(
            mbstowcs_s(&mut n, out.as_mut_ptr(), 8, m.as_ptr(), TRUNCATE),
            0
        );
        assert_eq!(n, 6);
        assert_eq!(
            mbstowcs_s(&mut n, out.as_mut_ptr(), 3, m.as_ptr(), TRUNCATE),
            super::super::STRUNCATE
        );
        assert_eq!(
            mbstowcs_s(&mut n, out.as_mut_ptr(), 3, m.as_ptr(), 10),
            ERANGE
        );
        assert_eq!(
            wcstombs_s(&mut n, back.as_mut_ptr(), 16, w("héllo").as_ptr(), TRUNCATE),
            0
        );
        assert_eq!(n, 7);
        assert_eq!(_mbstrlen(m.as_ptr()), 4);
        let mut c = 0u16;
        assert_eq!(mbtowc(&mut c, "é".as_ptr(), 2), 2);
        assert_eq!(c, 0xE9);
        assert_eq!(mbrtowc(&mut c, [0xE2u8, 0x82].as_ptr(), 2, 0) as isize, -2);
        assert_eq!(wctomb(back.as_mut_ptr(), 0x20AC), 3);
        assert_eq!(wcrtomb(back.as_mut_ptr(), 0xD800, 0), FAILED);
    }

    #[test]
    fn restartable_conversions_resume_after_the_last_character_converted() {
        // é (2 bytes), € (3 bytes), x: three UTF-16 units.
        let narrow = "é€x\0".as_bytes();
        let mut src = narrow.as_ptr();
        let mut wide = [0u16; 4];
        assert_eq!(mbsrtowcs(wide.as_mut_ptr(), &mut src, 2, 0), 2);
        assert_eq!(src as usize - narrow.as_ptr() as usize, 5);
        assert_eq!(mbsrtowcs(wide[2..].as_mut_ptr(), &mut src, 2, 0), 1);
        assert!(src.is_null());
        assert_eq!(String::from_utf16(&wide[..3]).unwrap(), "é€x");
        // A full buffer that holds every character leaves `*src` on the NUL.
        src = narrow.as_ptr();
        assert_eq!(mbsrtowcs(wide.as_mut_ptr(), &mut src, 3, 0), 3);
        assert_eq!(src as usize - narrow.as_ptr() as usize, 6);

        let units = w("é€x");
        let mut src = units.as_ptr();
        let mut bytes = [0u8; 7];
        // Room for é and part of €: only é is written, and `*src` stops on €.
        assert_eq!(wcsrtombs(bytes.as_mut_ptr(), &mut src, 3, 0), 2);
        assert_eq!(src as usize - units.as_ptr() as usize, 2);
        assert_eq!(wcsrtombs(bytes[2..].as_mut_ptr(), &mut src, 5, 0), 4);
        assert!(src.is_null());
        assert_eq!(&bytes[..6], "é€x".as_bytes());
    }

    #[test]
    fn size_queries_utf8_boundaries_and_null_float_inputs() {
        let narrow = "aé€😀\0".as_bytes();
        let mut n = 99;
        // Null destination queries full converted length + NUL, ignoring `count`.
        assert_eq!(
            mbstowcs_s(&mut n, std::ptr::null_mut(), 0, narrow.as_ptr(), 0),
            0
        );
        assert_eq!(n, 6);
        let wide = w("aé€");
        n = 99;
        assert_eq!(
            wcstombs_s(&mut n, std::ptr::null_mut(), 0, wide.as_ptr(), 0),
            0
        );
        assert_eq!(n, 7);

        // `wcstombs_s` with an explicit byte `count` stops on a UTF-8 character boundary.
        let mut out = [0xEEu8; 8];
        let a_euro = w("a€");
        assert_eq!(
            wcstombs_s(&mut n, out.as_mut_ptr(), out.len(), a_euro.as_ptr(), 2),
            0
        );
        assert_eq!(n, 2);
        assert_eq!(&out[..2], b"a\0");

        let euro = w("€");
        out = [0xEEu8; 8];
        assert_eq!(
            wcstombs_s(&mut n, out.as_mut_ptr(), out.len(), euro.as_ptr(), 2),
            0
        );
        assert_eq!(n, 1);
        assert_eq!(out[0], 0);

        // Null `src` empties `dst` and reports `EINVAL`.
        out[0] = b'x';
        assert_eq!(
            wcstombs_s(&mut n, out.as_mut_ptr(), out.len(), std::ptr::null(), 4),
            EINVAL
        );
        assert_eq!(out[0], 0);
        let mut wout = [u16::from(b'x'); 4];
        assert_eq!(
            mbstowcs_s(&mut n, wout.as_mut_ptr(), wout.len(), std::ptr::null(), 4),
            EINVAL
        );
        assert_eq!(wout[0], 0);

        // Null input pointers to floating-point parsers report `EINVAL` instead of dereferencing.
        let mut end8 = narrow.as_ptr();
        set_errno(0);
        assert_eq!(strtod(std::ptr::null(), &mut end8), 0.0);
        assert!(end8.is_null());
        assert_eq!(super::super::errno(), EINVAL);

        end8 = narrow.as_ptr();
        set_errno(0);
        assert_eq!(strtof(std::ptr::null(), &mut end8), 0.0);
        assert!(end8.is_null());
        assert_eq!(super::super::errno(), EINVAL);

        let mut end16 = wide.as_ptr();
        set_errno(0);
        assert_eq!(wcstod(std::ptr::null(), &mut end16), 0.0);
        assert!(end16.is_null());
        assert_eq!(super::super::errno(), EINVAL);

        end16 = wide.as_ptr();
        set_errno(0);
        assert_eq!(wcstof(std::ptr::null(), &mut end16), 0.0);
        assert!(end16.is_null());
        assert_eq!(super::super::errno(), EINVAL);
    }
}
