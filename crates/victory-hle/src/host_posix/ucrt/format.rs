//! The `printf` and `scanf` engines behind `__stdio_common_v*`: MSVC's
//! format language over a Windows `va_list` (a pointer to 8-byte slots),
//! with UTF-16 wide variants. Nothing here calls the host's `printf`.
//!
//! Output is a vector of code units (bytes for narrow functions, UTF-16
//! units for wide ones), one per `u32`, so one engine serves both.
//!
//! Handled: flags `-+ #0`, `*` and numeric width and precision, `hh h l ll
//! w I I32 I64 z t j L`, and `c C s S d i u o x X e E f F g G a A p n %`.
//! `%s` and `%c` take the function's own width (`%hs`, `%ls`, `%S`, `%C`
//! take the other); option `LEGACY_WIDE_SPECIFIERS` makes a wide function's
//! plain `%s`/`%c` narrow (the ISO reading). A format the CRT would reject
//! as an invalid parameter gives `None`.

use super::convert::{parse_double, utf8_to_utf16, utf16_to_utf8};

pub(super) const PRINTF_LEGACY_VSPRINTF_NULL_TERMINATION: u64 = 0x1;
pub(super) const PRINTF_STANDARD_SNPRINTF_BEHAVIOR: u64 = 0x2;
pub(super) const PRINTF_LEGACY_WIDE_SPECIFIERS: u64 = 0x4;
pub(super) const SCANF_SECURECRT: u64 = 0x1;
pub(super) const SCANF_LEGACY_WIDE_SPECIFIERS: u64 = 0x2;

/// The arguments of a variadic call, in order.
pub(super) trait Args {
    fn u64(&mut self) -> u64;
    fn f64(&mut self) -> f64 {
        f64::from_bits(self.u64())
    }
}

/// A Windows x64 `va_list`: every argument takes one 8-byte slot (floats
/// as doubles; anything larger than 8 bytes is a pointer).
pub(super) struct VaList(pub *const u64);

impl Args for VaList {
    fn u64(&mut self) -> u64 {
        // SAFETY: the title's `va_list`, with an argument per conversion.
        unsafe {
            let v = self.0.read_unaligned();
            self.0 = self.0.add(1);
            v
        }
    }
}

#[cfg(test)]
pub(super) struct SliceArgs<'a>(pub &'a [u64], pub usize);

#[cfg(test)]
impl Args for SliceArgs<'_> {
    fn u64(&mut self) -> u64 {
        let v = self.0.get(self.1).copied().unwrap_or(0);
        self.1 += 1;
        v
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Len {
    Default,
    Hh,
    H,
    L,
    Ll,
    I32,
    Wide,
}

#[derive(Default)]
struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    alt: bool,
    zero: bool,
    width: usize,
    prec: Option<usize>,
}

fn ch(c: u8) -> u32 {
    u32::from(c)
}

fn push_str(out: &mut Vec<u32>, s: &str) {
    out.extend(s.bytes().map(u32::from));
}

/// Reads a string argument: up to `limit` units, to the NUL.
unsafe fn read_string(p: usize, wide: bool, limit: Option<usize>) -> Vec<u32> {
    let mut v = Vec::new();
    let max = limit.unwrap_or(usize::MAX);
    // SAFETY: the title's string (or its `limit` units).
    unsafe {
        while v.len() < max {
            let c = if wide {
                u32::from((p as *const u16).add(v.len()).read_unaligned())
            } else {
                u32::from((p as *const u8).add(v.len()).read())
            };
            if c == 0 {
                break;
            }
            v.push(c);
        }
    }
    v
}

/// Source units (of the given width) as units of the output's width.
fn convert(units: &[u32], from_wide: bool, to_wide: bool) -> Vec<u32> {
    match (from_wide, to_wide) {
        (false, false) | (true, true) => units.to_vec(),
        (false, true) => {
            let b: Vec<u8> = units.iter().map(|&u| u as u8).collect();
            utf8_to_utf16(&b).into_iter().map(u32::from).collect()
        }
        (true, false) => {
            let w: Vec<u16> = units.iter().map(|&u| u as u16).collect();
            utf16_to_utf8(&w).into_iter().map(u32::from).collect()
        }
    }
}

fn pad(out: &mut Vec<u32>, body: &[u32], spec: &Spec) {
    let fill = spec.width.saturating_sub(body.len());
    if spec.left {
        out.extend_from_slice(body);
        out.extend(std::iter::repeat_n(ch(b' '), fill));
    } else {
        out.extend(std::iter::repeat_n(ch(b' '), fill));
        out.extend_from_slice(body);
    }
}

/// Sign or prefix, then digits, padded as an integer or float is: zeros go
/// between the two.
fn pad_number(out: &mut Vec<u32>, prefix: &str, digits: &str, spec: &Spec, zeros_ok: bool) {
    let body = prefix.len() + digits.len();
    let fill = spec.width.saturating_sub(body);
    if spec.left {
        push_str(out, prefix);
        push_str(out, digits);
        out.extend(std::iter::repeat_n(ch(b' '), fill));
    } else if spec.zero && zeros_ok {
        push_str(out, prefix);
        out.extend(std::iter::repeat_n(ch(b'0'), fill));
        push_str(out, digits);
    } else {
        out.extend(std::iter::repeat_n(ch(b' '), fill));
        push_str(out, prefix);
        push_str(out, digits);
    }
}

fn integer(out: &mut Vec<u32>, conv: u8, len: Len, spec: &Spec, raw: u64) {
    let signed = matches!(conv, b'd' | b'i');
    let bits = match len {
        Len::Hh => 8,
        Len::H => 16,
        Len::Ll | Len::Wide => 64,
        _ => 32,
    };
    let value: u64 = if bits == 64 {
        raw
    } else if signed {
        // Sign-extend from the argument's width.
        (((raw << (64 - bits)) as i64) >> (64 - bits)) as u64
    } else {
        raw & ((1u64 << bits) - 1)
    };
    let (negative, magnitude) = if signed && (value as i64) < 0 {
        (true, (value as i64).unsigned_abs())
    } else {
        (false, value)
    };
    let mut digits = match conv {
        b'o' => format!("{magnitude:o}"),
        b'x' => format!("{magnitude:x}"),
        b'X' => format!("{magnitude:X}"),
        _ => magnitude.to_string(),
    };
    if let Some(p) = spec.prec {
        if p == 0 && magnitude == 0 {
            digits.clear();
        }
        if digits.len() < p {
            digits.insert_str(0, &"0".repeat(p - digits.len()));
        }
    }
    let mut prefix = String::new();
    if signed {
        if negative {
            prefix.push('-');
        } else if spec.plus {
            prefix.push('+');
        } else if spec.space {
            prefix.push(' ');
        }
    }
    if spec.alt {
        match conv {
            b'x' if magnitude != 0 => prefix.push_str("0x"),
            b'X' if magnitude != 0 => prefix.push_str("0X"),
            b'o' if !digits.starts_with('0') => digits.insert(0, '0'),
            _ => {}
        }
    }
    pad_number(out, &prefix, &digits, spec, spec.prec.is_none());
}

fn exponent_form(v: f64, prec: usize, upper: bool, alt: bool) -> String {
    let s = format!("{v:.prec$e}");
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let mut m = mantissa.to_string();
    if alt && prec == 0 {
        m.push('.');
    }
    let e = if upper { 'E' } else { 'e' };
    let sign = if exp < 0 { '-' } else { '+' };
    format!("{m}{e}{sign}{:02}", exp.abs())
}

fn fixed_form(v: f64, prec: usize, alt: bool) -> String {
    let mut s = format!("{v:.prec$}");
    if alt && prec == 0 {
        s.push('.');
    }
    s
}

fn strip_zeros(s: &str) -> String {
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let m = if mantissa.contains('.') {
        mantissa.trim_end_matches('0').trim_end_matches('.')
    } else {
        mantissa
    };
    format!("{m}{exp}")
}

fn general_form(v: f64, prec: Option<usize>, upper: bool, alt: bool) -> String {
    let p = match prec {
        None => 6,
        Some(0) => 1,
        Some(p) => p,
    };
    // The exponent after rounding to `p` significant digits decides the style.
    let probe = format!("{v:.prec$e}", prec = p - 1);
    let x: i32 = probe
        .split_once('e')
        .and_then(|(_, e)| e.parse().ok())
        .unwrap_or(0);
    let s = if x < -4 || x >= p as i32 {
        exponent_form(v, p - 1, upper, alt)
    } else {
        fixed_form(v, (p as i32 - 1 - x) as usize, alt)
    };
    if alt { s } else { strip_zeros(&s) }
}

fn hex_form(v: f64, prec: Option<usize>, upper: bool, alt: bool) -> String {
    let bits = v.to_bits();
    let frac = bits & ((1 << 52) - 1);
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let (mut lead, mut exp) = match (biased, frac) {
        (0, 0) => (0u64, 0),
        (0, _) => (0, -1022),
        _ => (1, biased - 1023),
    };
    let mut digits = format!("{frac:013x}");
    match prec {
        Some(p) if p < 13 => {
            let drop = (13 - p) * 4;
            let mut full = (lead << 52) | frac;
            let rem = full & ((1 << drop) - 1);
            full >>= drop;
            let half = 1u64 << (drop - 1);
            if rem > half || (rem == half && full & 1 == 1) {
                full += 1;
            }
            lead = full >> (p * 4);
            if lead > 1 && biased != 0 {
                // Rounded up to the next power of two.
                lead = 1;
                exp += 1;
                full = 1 << (p * 4);
            }
            digits = if p == 0 {
                String::new()
            } else {
                format!("{:0p$x}", full & ((1 << (p * 4)) - 1))
            };
        }
        Some(p) => digits.push_str(&"0".repeat(p - 13)),
        // The CRT's default precision is 13, so trailing zeros stay (glibc trims them).
        None => {}
    }
    let dot = if digits.is_empty() && !alt { "" } else { "." };
    let s = format!(
        "0x{lead}{dot}{digits}p{}{}",
        if exp < 0 { '-' } else { '+' },
        exp.abs()
    );
    if upper { s.to_uppercase() } else { s }
}

fn float(out: &mut Vec<u32>, conv: u8, spec: &Spec, v: f64) {
    let upper = conv.is_ascii_uppercase();
    let negative = v.is_sign_negative();
    let sign = if negative {
        "-"
    } else if spec.plus {
        "+"
    } else if spec.space {
        " "
    } else {
        ""
    };
    let (text, zeros_ok) = if v.is_nan() {
        let quiet = v.to_bits() & (1 << 51) != 0;
        let indefinite = negative && quiet && v.to_bits() & ((1 << 51) - 1) == 0;
        let t = if indefinite {
            "-nan(ind)"
        } else if !quiet {
            "nan(snan)"
        } else {
            "nan"
        };
        let t = if upper {
            t.to_uppercase()
        } else {
            t.to_string()
        };
        // The indefinite NaN prints its own sign.
        let t = if indefinite { t } else { format!("{sign}{t}") };
        pad_number(out, "", &t, spec, false);
        return;
    } else if v.is_infinite() {
        (if upper { "INF" } else { "inf" }.to_string(), false)
    } else {
        let a = v.abs();
        let t = match conv.to_ascii_lowercase() {
            b'f' => fixed_form(a, spec.prec.unwrap_or(6), spec.alt),
            b'e' => exponent_form(a, spec.prec.unwrap_or(6), upper, spec.alt),
            b'g' => general_form(a, spec.prec, upper, spec.alt),
            _ => hex_form(a, spec.prec, upper, spec.alt),
        };
        (t, true)
    };
    // `0x` stays ahead of the zeros of a hex float.
    if zeros_ok && matches!(conv, b'a' | b'A') {
        let (p, rest) = text.split_at(2);
        pad_number(out, &format!("{sign}{p}"), rest, spec, true);
    } else {
        pad_number(out, sign, &text, spec, zeros_ok);
    }
}

/// Formats `fmt` with `args`. `wide` is the function's own width (`vswprintf`).
pub(super) fn printf(
    fmt: &[u32],
    wide: bool,
    options: u64,
    args: &mut dyn Args,
) -> Option<Vec<u32>> {
    // In a wide function the plain `%s`/`%c` are wide unless the ISO option is set.
    let plain_is_wide = if wide {
        options & PRINTF_LEGACY_WIDE_SPECIFIERS == 0
    } else {
        false
    };
    let mut out = Vec::with_capacity(fmt.len() + 16);
    let mut i = 0;
    while i < fmt.len() {
        let c = fmt[i];
        i += 1;
        if c != ch(b'%') {
            out.push(c);
            continue;
        }
        let mut spec = Spec::default();
        loop {
            match fmt.get(i).and_then(|&c| u8::try_from(c).ok()) {
                Some(b'-') => spec.left = true,
                Some(b'+') => spec.plus = true,
                Some(b' ') => spec.space = true,
                Some(b'#') => spec.alt = true,
                Some(b'0') => spec.zero = true,
                _ => break,
            }
            i += 1;
        }
        if fmt.get(i) == Some(&ch(b'*')) {
            let w = args.u64() as i32;
            if w < 0 {
                spec.left = true;
            }
            spec.width = w.unsigned_abs() as usize;
            i += 1;
        } else {
            while let Some(d) = fmt
                .get(i)
                .and_then(|&c| char::from_u32(c))
                .and_then(|c| c.to_digit(10))
            {
                spec.width = spec.width.saturating_mul(10).saturating_add(d as usize);
                i += 1;
            }
        }
        if fmt.get(i) == Some(&ch(b'.')) {
            i += 1;
            if fmt.get(i) == Some(&ch(b'*')) {
                let p = args.u64() as i32;
                spec.prec = usize::try_from(p).ok();
                i += 1;
            } else {
                let mut p = 0usize;
                while let Some(d) = fmt
                    .get(i)
                    .and_then(|&c| char::from_u32(c))
                    .and_then(|c| c.to_digit(10))
                {
                    p = p.saturating_mul(10).saturating_add(d as usize);
                    i += 1;
                }
                spec.prec = Some(p);
            }
        }
        let at = |k: usize| {
            fmt.get(i + k)
                .and_then(|&c| u8::try_from(c).ok())
                .unwrap_or(0)
        };
        let len = match (at(0), at(1), at(2)) {
            (b'h', b'h', _) => {
                i += 2;
                Len::Hh
            }
            (b'h', _, _) => {
                i += 1;
                Len::H
            }
            (b'l', b'l', _) => {
                i += 2;
                Len::Ll
            }
            (b'l', _, _) => {
                i += 1;
                Len::L
            }
            (b'I', b'6', b'4') => {
                i += 3;
                Len::Ll
            }
            (b'I', b'3', b'2') => {
                i += 3;
                Len::I32
            }
            (b'I', _, _) | (b'z', _, _) | (b't', _, _) | (b'j', _, _) | (b'q', _, _) => {
                i += 1;
                Len::Ll
            }
            (b'w', _, _) => {
                i += 1;
                Len::Wide
            }
            (b'L', _, _) => {
                i += 1;
                Len::Default
            }
            _ => Len::Default,
        };
        let conv = fmt.get(i).and_then(|&c| u8::try_from(c).ok())?;
        i += 1;
        match conv {
            b'%' => out.push(ch(b'%')),
            b'c' | b'C' | b's' | b'S' => {
                // The width of the argument.
                let arg_wide = match len {
                    Len::H => false,
                    Len::L | Len::Wide => true,
                    _ => plain_is_wide == conv.is_ascii_lowercase(),
                };
                let body: Vec<u32> = if matches!(conv, b'c' | b'C') {
                    let v = args.u64() as u32;
                    let unit = if arg_wide { v & 0xFFFF } else { v & 0xFF };
                    convert(&[unit], arg_wide, wide)
                } else {
                    let p = args.u64() as usize;
                    if p == 0 {
                        let n = spec.prec.map_or(6, |p| p.min(6));
                        "(null)".bytes().take(n).map(u32::from).collect()
                    } else {
                        // SAFETY: the title's string.
                        let src = unsafe { read_string(p, arg_wide, spec.prec) };
                        convert(&src, arg_wide, wide)
                    }
                };
                pad(&mut out, &body, &spec);
            }
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' => {
                let raw = args.u64();
                integer(&mut out, conv, len, &spec, raw);
            }
            b'p' => {
                let v = args.u64();
                pad_number(
                    &mut out,
                    "",
                    &format!("{v:016X}"),
                    &Spec {
                        zero: false,
                        ..spec
                    },
                    false,
                );
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                let v = args.f64();
                float(&mut out, conv, &spec, v);
            }
            b'n' => {
                // Disabled in the CRT (an invalid parameter): the argument is skipped.
                args.u64();
            }
            _ => return None,
        }
    }
    Some(out)
}

// scanf

/// Characters of a scan, with pushback.
pub(super) trait Input {
    fn get(&mut self) -> Option<u32>;
    fn unget(&mut self, c: u32);
}

pub(super) struct UnitInput {
    pub units: Vec<u32>,
    pub pos: usize,
}

impl Input for UnitInput {
    fn get(&mut self) -> Option<u32> {
        let c = self.units.get(self.pos).copied()?;
        self.pos += 1;
        Some(c)
    }
    fn unget(&mut self, _c: u32) {
        self.pos -= 1;
    }
}

fn is_space(c: u32) -> bool {
    matches!(c, 0x20 | 0x09..=0x0D)
}

fn skip_space(input: &mut dyn Input, consumed: &mut usize) {
    while let Some(c) = input.get() {
        if !is_space(c) {
            input.unget(c);
            break;
        }
        *consumed += 1;
    }
}

/// Stores a scanned string at `dest` in the destination's width if the converted
/// units fit in `room`. Returns `false` (clearing `dest[0]` when `room > 0`) if
/// the buffer is too small or null.
unsafe fn store_units(
    dest: usize,
    room: usize,
    units: &[u32],
    src_wide: bool,
    dest_wide: bool,
    nul: bool,
) -> bool {
    let u = convert(units, src_wide, dest_wide);
    let need = u.len() + usize::from(nul);
    if dest == 0 || need > room {
        if dest != 0 && room > 0 {
            // SAFETY: the title's destination buffer has at least one unit.
            unsafe {
                if dest_wide {
                    (dest as *mut u16).write_unaligned(0);
                } else {
                    (dest as *mut u8).write(0);
                }
            }
        }
        return false;
    }
    // SAFETY: the title's destination buffer holds `u` (and NUL when `nul`).
    unsafe {
        if dest_wide {
            let p = dest as *mut u16;
            for (k, &c) in u.iter().enumerate() {
                p.add(k).write_unaligned(c as u16);
            }
            if nul {
                p.add(u.len()).write_unaligned(0);
            }
        } else {
            let p = dest as *mut u8;
            for (k, &c) in u.iter().enumerate() {
                p.add(k).write(c as u8);
            }
            if nul {
                p.add(u.len()).write(0);
            }
        }
    }
    true
}

/// Scans `input` by `fmt`: the number of fields stored, or -1 when the
/// input ran out before the first field.
pub(super) fn scanf(
    fmt: &[u32],
    wide: bool,
    options: u64,
    input: &mut dyn Input,
    args: &mut dyn Args,
) -> i32 {
    let secure = options & SCANF_SECURECRT != 0;
    let plain_is_wide = if wide {
        options & SCANF_LEGACY_WIDE_SPECIFIERS == 0
    } else {
        false
    };
    let (mut stored, mut consumed, mut i) = (0i32, 0usize, 0usize);
    // A failed conversion: -1 if the input ended before any field was stored.
    macro_rules! done {
        () => {{
            let eof = match input.get() {
                None => true,
                Some(c) => {
                    input.unget(c);
                    false
                }
            };
            return if stored == 0 && eof { -1 } else { stored };
        }};
    }
    while i < fmt.len() {
        let c = fmt[i];
        i += 1;
        if is_space(c) {
            skip_space(input, &mut consumed);
            continue;
        }
        if c != ch(b'%') {
            match input.get() {
                Some(x) if x == c => consumed += 1,
                Some(x) => {
                    input.unget(x);
                    done!()
                }
                None => done!(),
            }
            continue;
        }
        if fmt.get(i) == Some(&ch(b'%')) {
            i += 1;
            skip_space(input, &mut consumed);
            match input.get() {
                Some(x) if x == ch(b'%') => consumed += 1,
                Some(x) => {
                    input.unget(x);
                    done!()
                }
                None => done!(),
            }
            continue;
        }
        let suppress = fmt.get(i) == Some(&ch(b'*'));
        if suppress {
            i += 1;
        }
        let mut width = 0usize;
        while let Some(d) = fmt
            .get(i)
            .and_then(|&c| char::from_u32(c))
            .and_then(|c| c.to_digit(10))
        {
            width = width.saturating_mul(10).saturating_add(d as usize);
            i += 1;
        }
        let at = |k: usize| {
            fmt.get(i + k)
                .and_then(|&c| u8::try_from(c).ok())
                .unwrap_or(0)
        };
        let len = match (at(0), at(1), at(2)) {
            (b'h', b'h', _) => {
                i += 2;
                Len::Hh
            }
            (b'h', _, _) => {
                i += 1;
                Len::H
            }
            (b'l', b'l', _) => {
                i += 2;
                Len::Ll
            }
            (b'l', _, _) => {
                i += 1;
                Len::L
            }
            (b'I', b'6', b'4') => {
                i += 3;
                Len::Ll
            }
            (b'I', b'3', b'2') => {
                i += 3;
                Len::I32
            }
            (b'I', _, _) | (b'z', _, _) | (b't', _, _) | (b'j', _, _) => {
                i += 1;
                Len::Ll
            }
            (b'w', _, _) => {
                i += 1;
                Len::Wide
            }
            (b'L', _, _) => {
                i += 1;
                Len::Ll
            }
            _ => Len::Default,
        };
        let Some(conv) = fmt.get(i).and_then(|&c| u8::try_from(c).ok()) else {
            return stored;
        };
        i += 1;
        let limit = if width == 0 { usize::MAX } else { width };
        let dest_wide = match len {
            Len::H => false,
            Len::L | Len::Wide => true,
            _ => {
                (conv.is_ascii_lowercase() && plain_is_wide)
                    || (conv.is_ascii_uppercase() && !plain_is_wide)
            }
        };
        match conv {
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' => {
                skip_space(input, &mut consumed);
                let mut text = String::new();
                let mut negative = false;
                let mut count = 0;
                if count < limit
                    && let Some(c) = input.get()
                {
                    if c == ch(b'-') || c == ch(b'+') {
                        negative = c == ch(b'-');
                        count += 1;
                    } else {
                        input.unget(c);
                    }
                }
                let mut base = match conv {
                    b'o' => 8,
                    b'x' | b'X' | b'p' => 16,
                    b'i' => 0,
                    _ => 10,
                };
                // A leading 0 (and then x) selects the base for `%i` and `%x`.
                let mut leading_zero = false;
                if count < limit
                    && matches!(base, 0 | 16)
                    && let Some(z) = input.get()
                {
                    if z == ch(b'0') {
                        count += 1;
                        leading_zero = true;
                        match input.get() {
                            Some(x)
                                if (x == ch(b'x') || x == ch(b'X'))
                                    && count < limit
                                    && base != 8 =>
                            {
                                count += 1;
                                base = 16;
                                leading_zero = false;
                            }
                            Some(x) => {
                                input.unget(x);
                                if base == 0 {
                                    base = 8;
                                }
                            }
                            None => {
                                if base == 0 {
                                    base = 8;
                                }
                            }
                        }
                    } else {
                        input.unget(z);
                        if base == 0 {
                            base = 10;
                        }
                    }
                }
                while count < limit
                    && let Some(d) = input.get()
                {
                    if char::from_u32(d).and_then(|c| c.to_digit(base)).is_some() {
                        text.push(char::from_u32(d).unwrap_or('0'));
                        count += 1;
                    } else {
                        input.unget(d);
                        break;
                    }
                }
                consumed += count;
                if text.is_empty() && !leading_zero {
                    done!()
                }
                if text.is_empty() {
                    text.push('0');
                }
                let mag = u64::from_str_radix(&text, base).unwrap_or(u64::MAX);
                let v = if negative { mag.wrapping_neg() } else { mag };
                if !suppress {
                    let dest = args.u64() as usize;
                    // SAFETY: the title's destination.
                    unsafe {
                        match len {
                            Len::Hh => (dest as *mut u8).write(v as u8),
                            Len::H => (dest as *mut u16).write_unaligned(v as u16),
                            Len::Ll | Len::Wide => (dest as *mut u64).write_unaligned(v),
                            _ if conv == b'p' => (dest as *mut u64).write_unaligned(v),
                            _ => (dest as *mut u32).write_unaligned(v as u32),
                        }
                    }
                    stored += 1;
                }
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                skip_space(input, &mut consumed);
                let mut text = Vec::new();
                while text.len() < limit
                    && let Some(c) = input.get()
                {
                    let ok = u8::try_from(c).is_ok_and(|b| {
                        b.is_ascii_alphanumeric()
                            || matches!(b, b'+' | b'-' | b'.' | b'(' | b')' | b'_')
                    });
                    if ok {
                        text.push(c as u8);
                    } else {
                        input.unget(c);
                        break;
                    }
                }
                let Some((v, n)) = parse_double(&text) else {
                    for &b in text.iter().rev() {
                        input.unget(u32::from(b));
                    }
                    done!()
                };
                for &b in text[n..].iter().rev() {
                    input.unget(u32::from(b));
                }
                consumed += n;
                if !suppress {
                    let dest = args.u64() as usize;
                    // SAFETY: the title's destination.
                    unsafe {
                        if matches!(len, Len::L | Len::Ll) {
                            (dest as *mut f64).write_unaligned(v);
                        } else {
                            (dest as *mut f32).write_unaligned(v as f32);
                        }
                    }
                    stored += 1;
                }
            }
            b's' | b'S' => {
                skip_space(input, &mut consumed);
                let mut got = Vec::new();
                while got.len() < limit
                    && let Some(c) = input.get()
                {
                    if is_space(c) {
                        input.unget(c);
                        break;
                    }
                    got.push(c);
                }
                if got.is_empty() {
                    done!()
                }
                consumed += got.len();
                if !suppress {
                    let dest = args.u64() as usize;
                    let room = if secure {
                        args.u64() as u32 as usize
                    } else {
                        usize::MAX
                    };
                    // SAFETY: the title's destination.
                    if !unsafe { store_units(dest, room, &got, wide, dest_wide, true) } {
                        return stored;
                    }
                    stored += 1;
                }
            }
            b'c' | b'C' => {
                let n = width.max(1);
                let mut got = Vec::new();
                while got.len() < n
                    && let Some(c) = input.get()
                {
                    got.push(c);
                }
                if got.len() < n {
                    done!()
                }
                consumed += got.len();
                if !suppress {
                    let dest = args.u64() as usize;
                    let room = if secure {
                        args.u64() as u32 as usize
                    } else {
                        usize::MAX
                    };
                    // SAFETY: the title's destination.
                    if !unsafe { store_units(dest, room, &got, wide, dest_wide, false) } {
                        return stored;
                    }
                    stored += 1;
                }
            }
            b'[' => {
                let negate = fmt.get(i) == Some(&ch(b'^'));
                if negate {
                    i += 1;
                }
                let mut set: Vec<u32> = Vec::new();
                // A `]` first is a member.
                let mut first = true;
                while let Some(&m) = fmt.get(i) {
                    i += 1;
                    if m == ch(b']') && !first {
                        break;
                    }
                    first = false;
                    if fmt.get(i) == Some(&ch(b'-'))
                        && fmt.get(i + 1).is_some_and(|&e| e != ch(b']'))
                    {
                        let end = fmt[i + 1];
                        set.extend(m.min(end)..=m.max(end));
                        i += 2;
                    } else {
                        set.push(m);
                    }
                }
                let mut got = Vec::new();
                while got.len() < limit
                    && let Some(c) = input.get()
                {
                    if set.contains(&c) != negate {
                        got.push(c);
                    } else {
                        input.unget(c);
                        break;
                    }
                }
                if got.is_empty() {
                    done!()
                }
                consumed += got.len();
                if !suppress {
                    let dest = args.u64() as usize;
                    let room = if secure {
                        args.u64() as u32 as usize
                    } else {
                        usize::MAX
                    };
                    // SAFETY: the title's destination.
                    if !unsafe { store_units(dest, room, &got, wide, dest_wide, true) } {
                        return stored;
                    }
                    stored += 1;
                }
            }
            b'n' => {
                if !suppress {
                    let dest = args.u64() as usize;
                    let v = consumed as u64;
                    // SAFETY: the title's destination, sized by the length modifier.
                    unsafe {
                        match len {
                            Len::Hh => (dest as *mut u8).write(v as u8),
                            Len::H => (dest as *mut u16).write_unaligned(v as u16),
                            Len::Ll | Len::Wide => (dest as *mut u64).write_unaligned(v),
                            _ => (dest as *mut u32).write_unaligned(v as u32),
                        }
                    }
                }
            }
            _ => return stored,
        }
    }
    stored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(fmt: &str, args: &[u64]) -> String {
        let f: Vec<u32> = fmt.bytes().map(u32::from).collect();
        let out = printf(&f, false, 0, &mut SliceArgs(args, 0)).unwrap();
        String::from_utf8(out.into_iter().map(|u| u as u8).collect()).unwrap()
    }

    fn d(v: f64) -> u64 {
        v.to_bits()
    }

    #[test]
    fn integers() {
        assert_eq!(
            run("%d|%5d|%-5d|%05d|%+d|% d", &[42, 42, 42, 42, 42, 42]),
            "42|   42|42   |00042|+42| 42"
        );
        assert_eq!(run("%d", &[(-7i32) as u32 as u64]), "-7");
        assert_eq!(run("%u", &[(-1i32) as u32 as u64]), "4294967295");
        assert_eq!(
            run("%ld %lld", &[(-1i32) as u32 as u64, (-1i64) as u64]),
            "-1 -1"
        );
        assert_eq!(run("%I64d %zu %Iu", &[u64::MAX, 5, 6]), "-1 5 6");
        assert_eq!(run("%hd %hhd", &[0x1_0005, 0x1_01]), "5 1");
        assert_eq!(
            run("%x %X %#x %#o %o", &[255, 255, 255, 8, 8]),
            "ff FF 0xff 010 10"
        );
        assert_eq!(run("%.5d|%8.3d|%.0d|", &[42, 7, 0]), "00042|     007||");
        assert_eq!(run("%#x", &[0]), "0");
        assert_eq!(run("%*d|%-*d|", &[4, 7, 4, 7]), "   7|7   |");
        assert_eq!(run("%*d", &[(-4i32) as u32 as u64, 7]), "7   ");
        assert_eq!(run("%p", &[0x7FFE_1234_5678]), "00007FFE12345678");
        assert_eq!(run("100%%", &[]), "100%");
    }

    #[test]
    fn floats() {
        assert_eq!(run("%f", &[d(3.14159)]), "3.141590");
        assert_eq!(
            run(
                "%.2f|%8.3f|%-8.1f|%08.2f",
                &[d(2.675), d(3.14159), d(2.5), d(-3.14159)]
            ),
            "2.67|   3.142|2.5     |-0003.14"
        );
        assert_eq!(run("%e", &[d(12345.678)]), "1.234568e+04");
        assert_eq!(run("%E", &[d(0.000123)]), "1.230000E-04");
        assert_eq!(
            run(
                "%g %g %g %g",
                &[d(100000.0), d(1000000.0), d(0.0001), d(0.00001)]
            ),
            "100000 1e+06 0.0001 1e-05"
        );
        assert_eq!(
            run("%g|%G|%.3g|%#g", &[d(0.5), d(1e-10), d(1234.5678), d(1.5)]),
            "0.5|1E-10|1.23e+03|1.50000"
        );
        assert_eq!(run("%.0f %.0f %.0f", &[d(0.5), d(1.5), d(2.5)]), "0 2 2");
        assert_eq!(run("%#.0f", &[d(3.0)]), "3.");
        assert_eq!(run("%+f % f", &[d(1.0), d(1.0)]), "+1.000000  1.000000");
        assert_eq!(
            run("%f %e", &[d(f64::INFINITY), d(f64::NEG_INFINITY)]),
            "inf -inf"
        );
        assert_eq!(
            run("%F|%08f", &[d(f64::INFINITY), d(f64::INFINITY)]),
            "INF|     inf"
        );
        assert_eq!(run("%f %f", &[d(f64::NAN), d(-f64::NAN)]), "nan -nan(ind)");
        assert_eq!(run("%f", &[d(-0.0)]), "-0.000000");
        assert_eq!(run("%f", &[d(1e20)]), "100000000000000000000.000000");
        // Thirteen digits unless a precision is given.
        assert_eq!(
            run("%a %A", &[d(1.0), d(0.5)]),
            "0x1.0000000000000p+0 0X1.0000000000000P-1"
        );
        assert_eq!(
            run("%a %.2a %a", &[d(3.0), d(1.0 + 1.0 / 3.0), d(0.0)]),
            "0x1.8000000000000p+1 0x1.55p+0 0x0.0000000000000p+0"
        );
        assert_eq!(run("%.0a %#.0a", &[d(1.0), d(1.0)]), "0x1p+0 0x1.p+0");
        assert_eq!(run("%a", &[d(5e-324)]), "0x0.0000000000001p-1022");
    }

    #[test]
    fn strings_and_chars() {
        let hello = b"hello\0";
        let wide: Vec<u16> = "wörld".encode_utf16().chain([0]).collect();
        let s = hello.as_ptr() as u64;
        let w = wide.as_ptr() as u64;
        assert_eq!(
            run("[%s][%8s][%-8s][%.3s]", &[s, s, s, s]),
            "[hello][   hello][hello   ][hel]"
        );
        assert_eq!(run("%s", &[0]), "(null)");
        assert_eq!(run("%c%c", &[u64::from(b'o'), u64::from(b'k')]), "ok");
        // Wide strings in a narrow function: %ls, %S, %ws; the text comes out UTF-8.
        assert_eq!(run("%ls|%S|%ws", &[w, w, w]), "wörld|wörld|wörld");
        assert_eq!(run("%lc", &[0xE9]), "é");
        assert_eq!(run("%hs", &[s]), "hello");
    }

    #[test]
    fn wide_output() {
        let f: Vec<u32> = "%s|%S|%hs|%ls|%c|%d"
            .encode_utf16()
            .map(u32::from)
            .collect();
        let wide: Vec<u16> = "wörld".encode_utf16().chain([0]).collect();
        let narrow = "naïve\0".as_bytes();
        let args = [
            wide.as_ptr() as u64,
            narrow.as_ptr() as u64,
            narrow.as_ptr() as u64,
            wide.as_ptr() as u64,
            0x263A,
            5,
        ];
        let out = printf(&f, true, 0, &mut SliceArgs(&args, 0)).unwrap();
        let text = String::from_utf16(&out.iter().map(|&u| u as u16).collect::<Vec<_>>()).unwrap();
        assert_eq!(text, "wörld|naïve|naïve|wörld|\u{263A}|5");
        // ISO: the plain %s is narrow.
        let f: Vec<u32> = "%s %S".encode_utf16().map(u32::from).collect();
        let args = [narrow.as_ptr() as u64, wide.as_ptr() as u64];
        let out = printf(
            &f,
            true,
            PRINTF_LEGACY_WIDE_SPECIFIERS,
            &mut SliceArgs(&args, 0),
        )
        .unwrap();
        let text = String::from_utf16(&out.iter().map(|&u| u as u16).collect::<Vec<_>>()).unwrap();
        assert_eq!(text, "naïve wörld");
    }

    #[test]
    fn invalid_formats_are_rejected() {
        let f: Vec<u32> = "%y".bytes().map(u32::from).collect();
        assert!(printf(&f, false, 0, &mut SliceArgs(&[], 0)).is_none());
    }

    fn scan(input: &str, fmt: &str, secure: bool, args: &[u64]) -> (i32, usize) {
        let mut inp = UnitInput {
            units: input.bytes().map(u32::from).collect(),
            pos: 0,
        };
        let f: Vec<u32> = fmt.bytes().map(u32::from).collect();
        let n = scanf(
            &f,
            false,
            if secure { SCANF_SECURECRT } else { 0 },
            &mut inp,
            &mut SliceArgs(args, 0),
        );
        (n, inp.pos)
    }

    #[test]
    fn scanning() {
        let (mut a, mut b, mut c) = (0i32, 0u32, 0i64);
        let args = [
            &mut a as *mut i32 as u64,
            &mut b as *mut u32 as u64,
            &mut c as *mut i64 as u64,
        ];
        assert_eq!(
            scan("  12 0x1F -9000000000", "%d %i %lld", false, &args).0,
            3
        );
        assert_eq!((a, b, c), (12, 31, -9_000_000_000));
        let (mut f, mut g) = (0f32, 0f64);
        let args = [&mut f as *mut f32 as u64, &mut g as *mut f64 as u64];
        assert_eq!(scan("1.5 -2e3", "%f %lf", false, &args).0, 2);
        assert_eq!((f, g), (1.5, -2000.0));
        let mut s = [0u8; 16];
        let mut n = 0i32;
        let args = [s.as_mut_ptr() as u64, 16, &mut n as *mut i32 as u64];
        assert_eq!(scan("word rest", "%s%n", true, &args).0, 1);
        assert_eq!(&s[..5], b"word\0");
        assert_eq!(n, 4);
        let mut s = [0u8; 16];
        let args = [s.as_mut_ptr() as u64];
        assert_eq!(scan("abc123", "%[a-c]", false, &args).0, 1);
        assert_eq!(&s[..4], b"abc\0");
        // Literals, `%%`, suppression, and a short buffer.
        let mut a = 0i32;
        let args = [&mut a as *mut i32 as u64];
        assert_eq!(scan("x=7;", "x=%d;", false, &args).0, 1);
        assert_eq!(scan("5 6", "%*d %d", false, &args).0, 1);
        assert_eq!(a, 6);
        let mut s = [0u8; 3];
        let args = [s.as_mut_ptr() as u64, 3];
        assert_eq!(scan("longword", "%s", true, &args).0, 0);
        // Input exhausted before anything: -1. A mismatch after one field: the count.
        assert_eq!(scan("", "%d", false, &[0]).0, -1);
        let mut a = 0i32;
        let args = [&mut a as *mut i32 as u64];
        assert_eq!(scan("abc", "%d", false, &args).0, 0);
        let mut v = [0u8; 4];
        let args = [v.as_mut_ptr() as u64];
        assert_eq!(scan("ab", "%c", false, &args), (1, 1));
        assert_eq!(v[0], b'a');
    }

    #[test]
    fn printf_null_precision_and_scanf_secure_bounds_and_n_widths() {
        // Precision caps `(null)` just like any other string.
        assert_eq!(run("%.3s|%.0s|%5.2s", &[0, 0, 0]), "(nu||   (n");
        let wf: Vec<u32> = "%.3s".encode_utf16().map(u32::from).collect();
        let wout = printf(&wf, true, 0, &mut SliceArgs(&[0], 0)).unwrap();
        let wtext =
            String::from_utf16(&wout.iter().map(|&u| u as u16).collect::<Vec<_>>()).unwrap();
        assert_eq!(wtext, "(nu");

        // `%n` writes the width named by its length modifier.
        let mut byte_slot = [0xAAu8; 4];
        let mut short_slot = [0xBBBBu16; 2];
        let mut wide_slot = u64::MAX;
        let args = [
            byte_slot.as_mut_ptr() as u64,
            short_slot.as_mut_ptr() as u64,
            &mut wide_slot as *mut u64 as u64,
        ];
        assert_eq!(scan("abcdef", "ab%hhncd%hnef%lln", false, &args).0, 0);
        assert_eq!(byte_slot, [2, 0xAA, 0xAA, 0xAA]);
        assert_eq!(short_slot, [4, 0xBBBB]);
        assert_eq!(wide_slot, 6);

        // `scanf_s` enforces buffer size on `%c` and on converted wide-to-narrow `%hs`.
        let mut cbuf = [0xCCu8; 6];
        let args = [cbuf.as_mut_ptr() as u64, 2];
        assert_eq!(scan("abcd", "%4c", true, &args).0, 0);
        assert_eq!(cbuf[0], 0);
        assert_eq!(&cbuf[1..], &[0xCC; 5]);

        let mut cbuf = [0xCCu8; 4];
        let args = [cbuf.as_mut_ptr() as u64, 2];
        assert_eq!(scan("abcd", "%2c", true, &args).0, 1);
        assert_eq!(&cbuf, &[b'a', b'b', 0xCC, 0xCC]);

        // Wide input `"€€"` is 2 UTF-16 units but 6 UTF-8 bytes + NUL (7 > 4).
        let mut inp = UnitInput {
            units: "€€".encode_utf16().map(u32::from).collect(),
            pos: 0,
        };
        let fmt: Vec<u32> = "%hs".encode_utf16().map(u32::from).collect();
        let mut narrow = [0xDDu8; 8];
        let args = [narrow.as_mut_ptr() as u64, 4];
        assert_eq!(
            scanf(
                &fmt,
                true,
                SCANF_SECURECRT,
                &mut inp,
                &mut SliceArgs(&args, 0)
            ),
            0
        );
        assert_eq!(narrow[0], 0);
        assert_eq!(&narrow[4..], &[0xDD; 4]);

        // `%[` clears `dest[0]` and stops without overflowing when the buffer is too small.
        let mut sbuf = [0xEEu8; 6];
        let args = [sbuf.as_mut_ptr() as u64, 3];
        assert_eq!(scan("abcd", "%[a-d]", true, &args).0, 0);
        assert_eq!(sbuf[0], 0);
        assert_eq!(&sbuf[3..], &[0xEE; 3]);
    }
}
