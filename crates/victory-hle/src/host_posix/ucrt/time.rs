//! `api-ms-win-crt-time`: 64-bit `time_t`, the `tm` conversions (UTC in
//! Rust, local time through libSystem's zone rules), `strftime` and
//! `wcsftime` in the "C" locale, `clock` and `_ftime64`.

use std::sync::LazyLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::runtime::bad_param;
use super::{EINVAL, ERANGE, exports, leak_cstr, set_errno};

/// MSVC's `struct tm`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(super) struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
}

/// macOS's `struct tm`, for the zone database.
#[repr(C)]
struct MacTm {
    f: [i32; 9],
    gmtoff: i64,
    zone: *const u8,
}

unsafe extern "C" {
    fn localtime_r(t: *const i64, out: *mut MacTm) -> *mut MacTm;
    fn mktime(t: *mut MacTm) -> i64;
}

/// The latest `time_t` the CRT converts: the end of 3000-12-31.
const MAX_TIME: i64 = 32_535_215_999;

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn gmtime(t: i64) -> Tm {
    let days = t.div_euclid(86_400);
    let tod = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    Tm {
        sec: (tod % 60) as i32,
        min: (tod / 60 % 60) as i32,
        hour: (tod / 3600) as i32,
        mday: d as i32,
        mon: m as i32 - 1,
        year: (y - 1900) as i32,
        wday: (days + 4).rem_euclid(7) as i32,
        yday: (days - days_from_civil(y, 1, 1)) as i32,
        isdst: 0,
    }
}

/// Seconds since the epoch of `tm` read as UTC, its fields normalized.
fn timegm(tm: &Tm) -> i64 {
    let months = i64::from(tm.year) * 12 + i64::from(tm.mon);
    let (year, mon) = (months.div_euclid(12) + 1900, months.rem_euclid(12));
    let days = days_from_civil(year, mon + 1, 1) + i64::from(tm.mday) - 1;
    days * 86_400 + i64::from(tm.hour) * 3600 + i64::from(tm.min) * 60 + i64::from(tm.sec)
}

fn localtime(t: i64) -> Option<Tm> {
    let mut m = MacTm {
        f: [0; 9],
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    // SAFETY: libSystem fills the structure.
    if unsafe { localtime_r(&t, &mut m) }.is_null() {
        return None;
    }
    let f = m.f;
    Some(Tm {
        sec: f[0],
        min: f[1],
        hour: f[2],
        mday: f[3],
        mon: f[4],
        year: f[5],
        wday: f[6],
        yday: f[7],
        isdst: f[8],
    })
}

/// Local time as seconds, with the tm normalized in place.
fn mktime_local(tm: &mut Tm) -> Option<i64> {
    let mut m = MacTm {
        f: [
            tm.sec, tm.min, tm.hour, tm.mday, tm.mon, tm.year, 0, 0, tm.isdst,
        ],
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    // SAFETY: libSystem normalizes the structure.
    let t = unsafe { mktime(&mut m) };
    if t == -1 {
        return None;
    }
    *tm = localtime(t)?;
    Some(t)
}

pub extern "system" fn _time64(out: *mut i64) -> i64 {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    if !out.is_null() {
        // SAFETY: the title's pointer.
        unsafe { out.write(t) };
    }
    t
}

pub extern "system" fn _difftime64(a: i64, b: i64) -> f64 {
    (a - b) as f64
}

fn read_time(p: *const i64) -> Option<i64> {
    if p.is_null() {
        bad_param(EINVAL);
        return None;
    }
    // SAFETY: the title's pointer.
    let t = unsafe { p.read_unaligned() };
    if !(0..=MAX_TIME).contains(&t) {
        set_errno(EINVAL);
        return None;
    }
    Some(t)
}

pub extern "system" fn _gmtime64_s(out: *mut Tm, t: *const i64) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    let Some(t) = read_time(t) else {
        // SAFETY: the title's pointer; all fields -1 on failure.
        unsafe {
            out.write(Tm {
                sec: -1,
                min: -1,
                hour: -1,
                mday: -1,
                mon: -1,
                year: -1,
                wday: -1,
                yday: -1,
                isdst: -1,
            })
        };
        return EINVAL;
    };
    // SAFETY: the title's pointer.
    unsafe { out.write(gmtime(t)) };
    0
}

pub extern "system" fn _localtime64_s(out: *mut Tm, t: *const i64) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    let Some(tm) = read_time(t).and_then(localtime) else {
        // SAFETY: the title's pointer.
        unsafe {
            out.write(Tm {
                sec: -1,
                min: -1,
                hour: -1,
                mday: -1,
                mon: -1,
                year: -1,
                wday: -1,
                yday: -1,
                isdst: -1,
            })
        };
        return EINVAL;
    };
    // SAFETY: the title's pointer.
    unsafe { out.write(tm) };
    0
}

/// One `tm` per thread, as the CRT keeps.
fn thread_tm(tm: Option<Tm>) -> *mut Tm {
    thread_local! { static SLOT: std::cell::UnsafeCell<Tm> = std::cell::UnsafeCell::new(Tm::default()); }
    match tm {
        Some(tm) => SLOT.with(|s| {
            // SAFETY: this thread's slot.
            unsafe { *s.get() = tm };
            s.get()
        }),
        None => std::ptr::null_mut(),
    }
}

pub extern "system" fn _gmtime64(t: *const i64) -> *mut Tm {
    thread_tm(read_time(t).map(gmtime))
}

pub extern "system" fn _localtime64(t: *const i64) -> *mut Tm {
    thread_tm(read_time(t).and_then(localtime))
}

pub extern "system" fn _mktime64(tm: *mut Tm) -> i64 {
    if tm.is_null() {
        bad_param(EINVAL);
        return -1;
    }
    // SAFETY: the title's pointer.
    unsafe {
        let mut t = tm.read();
        let r = mktime_local(&mut t);
        if let Some(r) = r.filter(|r| (0..=MAX_TIME).contains(r)) {
            tm.write(t);
            return r;
        }
        set_errno(EINVAL);
        -1
    }
}

pub extern "system" fn _mkgmtime64(tm: *mut Tm) -> i64 {
    if tm.is_null() {
        bad_param(EINVAL);
        return -1;
    }
    // SAFETY: the title's pointer.
    unsafe {
        let t = timegm(&tm.read());
        if !(0..=MAX_TIME).contains(&t) {
            set_errno(EINVAL);
            return -1;
        }
        tm.write(gmtime(t));
        t
    }
}

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Milliseconds since the first call (`CLOCKS_PER_SEC` is 1000).
pub extern "system" fn clock() -> i32 {
    START.elapsed().as_millis() as i32
}

/// `struct __timeb64`: `time`, `millitm`, `timezone` (minutes west), `dstflag`.
pub extern "system" fn _ftime64_s(out: *mut u8) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let local = localtime(secs);
    let west = local.and_then(|_| zone_offset(secs)).map_or(0, |o| -o / 60);
    // SAFETY: the title's `__timeb64`.
    unsafe {
        out.cast::<i64>().write_unaligned(secs);
        out.add(8)
            .cast::<u16>()
            .write_unaligned(now.subsec_millis() as u16);
        out.add(10).cast::<i16>().write_unaligned(west as i16);
        out.add(12)
            .cast::<i16>()
            .write_unaligned(local.map_or(0, |l| i16::from(l.isdst > 0)));
    }
    0
}

pub extern "system" fn _ftime64(out: *mut u8) {
    _ftime64_s(out);
}

/// Seconds east of UTC at `t`.
fn zone_offset(t: i64) -> Option<i32> {
    let mut m = MacTm {
        f: [0; 9],
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    // SAFETY: libSystem fills the structure.
    (!unsafe { localtime_r(&t, &mut m) }.is_null()).then_some(m.gmtoff as i32)
}

pub extern "system" fn timespec_get(out: *mut i64, base: i32) -> i32 {
    if out.is_null() || base != 1 {
        return 0;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // SAFETY: the title's `timespec` (an `__int64` and a `long`).
    unsafe {
        out.write_unaligned(now.as_secs() as i64);
        out.add(1)
            .cast::<i32>()
            .write_unaligned(now.subsec_nanos() as i32);
    }
    base
}

// Time zone variables.

static TIMEZONE: super::Shared<i32> = super::Shared::new(0);
static DAYLIGHT: super::Shared<i32> = super::Shared::new(0);
static DSTBIAS: super::Shared<i32> = super::Shared::new(0);
static TZNAMES: super::Shared<[usize; 2]> = super::Shared::new([0; 2]);

pub extern "system" fn _tzset() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let off = zone_offset(now).unwrap_or(0);
    // SAFETY: the CRT's variables, shared with the title.
    unsafe {
        *TIMEZONE.ptr() = -off;
        *DAYLIGHT.ptr() = 0;
        *TZNAMES.ptr() = [leak_cstr(b"UTC") as usize, leak_cstr(b"UTC") as usize];
    }
}

pub extern "system" fn __p__timezone() -> *mut i32 {
    _tzset();
    TIMEZONE.ptr()
}

pub extern "system" fn __p__daylight() -> *mut i32 {
    DAYLIGHT.ptr()
}

pub extern "system" fn __p__dstbias() -> *mut i32 {
    DSTBIAS.ptr()
}

pub extern "system" fn __p__tzname() -> *mut [usize; 2] {
    _tzset();
    TZNAMES.ptr()
}

pub extern "system" fn _get_timezone(out: *mut i32) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's pointer.
    unsafe { out.write(*__p__timezone()) };
    0
}

pub extern "system" fn _get_daylight(out: *mut i32) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's pointer.
    unsafe { out.write(0) };
    0
}

pub extern "system" fn _get_dstbias(out: *mut i32) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's pointer.
    unsafe { out.write(0) };
    0
}

// Formatting.

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn valid(tm: &Tm) -> bool {
    (0..=60).contains(&tm.sec)
        && (0..=59).contains(&tm.min)
        && (0..=23).contains(&tm.hour)
        && (1..=31).contains(&tm.mday)
        && (0..=11).contains(&tm.mon)
        && tm.year >= -1900
        && (0..=6).contains(&tm.wday)
        && (0..=365).contains(&tm.yday)
}

/// ISO 8601 week number and week-based year.
fn iso_week(tm: &Tm) -> (i32, i32) {
    let wday = (tm.wday + 6) % 7;
    let mut year = tm.year + 1900;
    let mut week = (tm.yday - wday + 10) / 7;
    let leap = |y: i32| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    if week < 1 {
        year -= 1;
        let days = if leap(year) { 366 } else { 365 };
        week = (tm.yday + days - wday + 10) / 7;
    } else if week == 53 {
        let days = if leap(year) { 366 } else { 365 };
        if tm.yday - wday + 3 >= days {
            week = 1;
            year += 1;
        }
    }
    (week, year)
}

/// Formats `tm` by `fmt` in the "C" locale; `None` for an unknown specifier.
pub(super) fn strftime_text(fmt: &[u8], tm: &Tm) -> Option<String> {
    let mut out = String::new();
    let mut i = 0;
    while i < fmt.len() {
        let c = fmt[i];
        i += 1;
        if c != b'%' {
            out.push(c as char);
            continue;
        }
        let mut strip = false;
        if fmt.get(i) == Some(&b'#') {
            strip = true;
            i += 1;
        }
        // `%E` and `%O` modifiers only choose alternate digits.
        while matches!(fmt.get(i), Some(b'E' | b'O')) {
            i += 1;
        }
        let spec = *fmt.get(i)?;
        i += 1;
        let num = |v: i32, width: usize| {
            if strip {
                v.to_string()
            } else {
                format!("{v:0>width$}")
            }
        };
        let hour12 = if tm.hour % 12 == 0 { 12 } else { tm.hour % 12 };
        let sub = |f: &str| strftime_text(f.as_bytes(), tm);
        match spec {
            b'a' => out.push_str(&DAYS[tm.wday as usize][..3]),
            b'A' => out.push_str(DAYS[tm.wday as usize]),
            b'b' | b'h' => out.push_str(&MONTHS[tm.mon as usize][..3]),
            b'B' => out.push_str(MONTHS[tm.mon as usize]),
            b'c' => out.push_str(&sub(if strip {
                "%A, %B %d, %Y %H:%M:%S"
            } else {
                "%m/%d/%y %H:%M:%S"
            })?),
            b'x' => out.push_str(&sub(if strip { "%A, %B %d, %Y" } else { "%m/%d/%y" })?),
            b'X' | b'T' => out.push_str(&sub("%H:%M:%S")?),
            b'C' => out.push_str(&num((tm.year + 1900).div_euclid(100), 2)),
            b'd' => out.push_str(&num(tm.mday, 2)),
            b'D' => out.push_str(&sub("%m/%d/%y")?),
            b'e' => out.push_str(&if strip {
                tm.mday.to_string()
            } else {
                format!("{:>2}", tm.mday)
            }),
            b'F' => out.push_str(&sub("%Y-%m-%d")?),
            b'g' => out.push_str(&num(iso_week(tm).1.rem_euclid(100), 2)),
            b'G' => out.push_str(&iso_week(tm).1.to_string()),
            b'H' => out.push_str(&num(tm.hour, 2)),
            b'I' => out.push_str(&num(hour12, 2)),
            b'j' => out.push_str(&num(tm.yday + 1, 3)),
            b'm' => out.push_str(&num(tm.mon + 1, 2)),
            b'M' => out.push_str(&num(tm.min, 2)),
            b'n' => out.push('\n'),
            b'p' => out.push_str(if tm.hour < 12 { "AM" } else { "PM" }),
            b'r' => out.push_str(&sub("%I:%M:%S %p")?),
            b'R' => out.push_str(&sub("%H:%M")?),
            b'S' => out.push_str(&num(tm.sec, 2)),
            b't' => out.push('\t'),
            b'u' => out.push_str(&(if tm.wday == 0 { 7 } else { tm.wday }).to_string()),
            b'U' => out.push_str(&num((tm.yday + 7 - tm.wday) / 7, 2)),
            b'V' => out.push_str(&num(iso_week(tm).0, 2)),
            b'w' => out.push_str(&tm.wday.to_string()),
            b'W' => out.push_str(&num((tm.yday + 7 - (tm.wday + 6) % 7) / 7, 2)),
            b'y' => out.push_str(&num((tm.year + 1900).rem_euclid(100), 2)),
            b'Y' => out.push_str(&(tm.year + 1900).to_string()),
            b'z' | b'Z' => {
                let mut copy = *tm;
                let off = mktime_local(&mut copy).and_then(zone_offset).unwrap_or(0);
                if spec == b'z' {
                    let (sign, a) = if off < 0 { ('-', -off) } else { ('+', off) };
                    out.push_str(&format!("{sign}{:02}{:02}", a / 3600, a / 60 % 60));
                } else {
                    out.push_str("UTC");
                }
            }
            b'%' => out.push('%'),
            _ => return None,
        }
    }
    Some(out)
}

pub extern "system" fn strftime(buf: *mut u8, max: usize, fmt: *const u8, tm: *const Tm) -> usize {
    if buf.is_null() || max == 0 || fmt.is_null() || tm.is_null() {
        bad_param(EINVAL);
        return 0;
    }
    // SAFETY: the title's `tm` and NUL-terminated format.
    let (tm, f) = unsafe { (tm.read(), super::bytes(fmt)) };
    let text = if valid(&tm) {
        strftime_text(f, &tm)
    } else {
        None
    };
    let Some(text) = text else {
        // SAFETY: the title's buffer.
        unsafe { buf.write(0) };
        bad_param(EINVAL);
        return 0;
    };
    if text.len() + 1 > max {
        // SAFETY: the title's buffer.
        unsafe { buf.write(0) };
        set_errno(ERANGE);
        return 0;
    }
    // SAFETY: the title's buffer of `max`.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf, text.len());
        buf.add(text.len()).write(0);
    }
    text.len()
}

pub extern "system" fn wcsftime(
    buf: *mut u16,
    max: usize,
    fmt: *const u16,
    tm: *const Tm,
) -> usize {
    if buf.is_null() || max == 0 || fmt.is_null() || tm.is_null() {
        bad_param(EINVAL);
        return 0;
    }
    // SAFETY: the title's `tm` and NUL-terminated format.
    let (tm, f) = unsafe { (tm.read(), super::wide(fmt)) };
    let f: Vec<u8> = f.iter().map(|&c| u8::try_from(c).unwrap_or(b'?')).collect();
    let text = if valid(&tm) {
        strftime_text(&f, &tm)
    } else {
        None
    };
    let Some(text) = text.filter(|t| t.len() < max) else {
        // SAFETY: the title's buffer.
        unsafe { buf.write(0) };
        set_errno(EINVAL);
        return 0;
    };
    // SAFETY: the title's buffer of `max`.
    unsafe {
        for (i, b) in text.bytes().enumerate() {
            buf.add(i).write(u16::from(b));
        }
        buf.add(text.len()).write(0);
    }
    text.len()
}

/// `"Sun Sep 16 01:03:52 1973\n"`.
fn asctime_text(tm: &Tm) -> Option<String> {
    valid(tm).then(|| {
        format!(
            "{} {} {:>2} {:02}:{:02}:{:02} {}\n",
            &DAYS[tm.wday as usize][..3],
            &MONTHS[tm.mon as usize][..3],
            tm.mday,
            tm.hour,
            tm.min,
            tm.sec,
            tm.year + 1900
        )
    })
}

pub extern "system" fn asctime(tm: *const Tm) -> *const u8 {
    if tm.is_null() {
        bad_param(EINVAL);
        return std::ptr::null();
    }
    // SAFETY: the title's `tm`.
    match asctime_text(&unsafe { tm.read() }) {
        Some(t) => {
            thread_local! { static BUF: std::cell::UnsafeCell<[u8; 32]> = const { std::cell::UnsafeCell::new([0; 32]) }; }
            BUF.with(|b| {
                // SAFETY: this thread's buffer, 26 bytes needed.
                unsafe {
                    let p = b.get().cast::<u8>();
                    std::ptr::copy_nonoverlapping(t.as_ptr(), p, t.len());
                    p.add(t.len()).write(0);
                    p
                }
            })
        }
        None => {
            set_errno(EINVAL);
            std::ptr::null()
        }
    }
}

pub extern "system" fn _asctime_s(buf: *mut u8, size: usize, tm: *const Tm) -> i32 {
    if buf.is_null() || size < 26 || tm.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's `tm` and buffer.
    unsafe {
        let Some(t) = asctime_text(&tm.read()) else {
            buf.write(0);
            return bad_param(EINVAL);
        };
        std::ptr::copy_nonoverlapping(t.as_ptr(), buf, t.len());
        buf.add(t.len()).write(0);
    }
    0
}

pub extern "system" fn _ctime64_s(buf: *mut u8, size: usize, t: *const i64) -> i32 {
    let mut tm = Tm::default();
    match _localtime64_s(&mut tm, t) {
        0 => _asctime_s(buf, size, &tm),
        e => e,
    }
}

pub extern "system" fn _ctime64(t: *const i64) -> *const u8 {
    let tm = _localtime64(t);
    if tm.is_null() {
        std::ptr::null()
    } else {
        asctime(tm)
    }
}

exports!(pub(super) fn lookup;
    _time64, _difftime64, _gmtime64_s, _localtime64_s, _gmtime64, _localtime64, _mktime64,
    _mkgmtime64, clock, _ftime64_s, _ftime64, timespec_get, _tzset, __p__timezone,
    __p__daylight, __p__dstbias, __p__tzname, _get_timezone, _get_daylight, _get_dstbias,
    strftime, wcsftime, asctime, _asctime_s, _ctime64_s, _ctime64;
    "time" => _time64, "difftime" => _difftime64, "_timespec64_get" => timespec_get,
    "_strftime_l" => strftime, "_wcsftime_l" => wcsftime, "asctime_s" => _asctime_s);

#[cfg(test)]
mod tests {
    use super::*;

    fn text(fmt: &str, tm: &Tm) -> String {
        strftime_text(fmt.as_bytes(), tm).unwrap()
    }

    #[test]
    fn gmtime_matches_known_dates() {
        let tm = gmtime(0);
        assert_eq!(
            (tm.year, tm.mon, tm.mday, tm.wday, tm.yday),
            (70, 0, 1, 4, 0)
        );
        // 2000-02-29 12:34:56 UTC, a Tuesday, day 59 of a leap year.
        let tm = gmtime(951_827_696);
        assert_eq!(
            (
                tm.year, tm.mon, tm.mday, tm.hour, tm.min, tm.sec, tm.wday, tm.yday
            ),
            (100, 1, 29, 12, 34, 56, 2, 59)
        );
        assert_eq!(timegm(&tm), 951_827_696);
        let tm = gmtime(MAX_TIME);
        assert_eq!((tm.year, tm.mon, tm.mday, tm.hour), (1100, 11, 31, 23));
    }

    #[test]
    fn conversion_entry_points_check_ranges() {
        let mut tm = Tm::default();
        let t = 1_700_000_000i64;
        assert_eq!(_gmtime64_s(&mut tm, &t), 0);
        assert_eq!((tm.year, tm.mon, tm.mday), (123, 10, 14));
        let neg = -1i64;
        assert_eq!(_gmtime64_s(&mut tm, &neg), EINVAL);
        assert_eq!(tm.year, -1);
        assert!(_gmtime64(&neg).is_null());
        // SAFETY: the thread's slot.
        assert_eq!(unsafe { (*_gmtime64(&t)).mday }, 14);
        let mut tm = gmtime(t);
        tm.mday += 20;
        assert_eq!(_mkgmtime64(&mut tm), t + 20 * 86_400);
        assert_eq!((tm.mon, tm.mday), (11, 4));
        // Local time round trips whatever the zone.
        let mut local = Tm::default();
        assert_eq!(_localtime64_s(&mut local, &t), 0);
        assert_eq!(_mktime64(&mut local), t);
        assert_eq!(_difftime64(10, 4), 6.0);
        assert!(_time64(std::ptr::null_mut()) > 1_700_000_000);
    }

    #[test]
    fn strftime_c_locale() {
        let tm = gmtime(951_827_696);
        assert_eq!(text("%Y-%m-%d %H:%M:%S", &tm), "2000-02-29 12:34:56");
        assert_eq!(text("%a %A %b %B", &tm), "Tue Tuesday Feb February");
        assert_eq!(text("%c|%x|%X", &tm), "02/29/00 12:34:56|02/29/00|12:34:56");
        assert_eq!(text("%#c", &tm), "Tuesday, February 29, 2000 12:34:56");
        assert_eq!(text("%I:%M %p|%j|%e|%y|%C", &tm), "12:34 PM|060|29|00|20");
        assert_eq!(text("%U %W %V %G %u %w", &tm), "09 09 09 2000 2 2");
        assert_eq!(
            text("%#d|%#m|%#H|%%", &gmtime(86_400 * 3 + 3600)),
            "4|1|1|%"
        );
        assert_eq!(
            text("%F %T %R %D %r", &tm),
            "2000-02-29 12:34:56 12:34 02/29/00 12:34:56 PM"
        );
        assert!(strftime_text(b"%Q", &tm).is_none());
        // 2021-01-03 is a Sunday in ISO week 53 of 2020.
        let tm = gmtime(1_609_632_000);
        assert_eq!(text("%V %G", &tm), "53 2020");
    }

    #[test]
    fn strftime_buffers() {
        let tm = gmtime(0);
        let mut buf = [b'x'; 16];
        let n = strftime(buf.as_mut_ptr(), 16, c"%Y".as_ptr().cast(), &tm);
        assert_eq!((n, &buf[..5]), (4, &b"1970\0"[..]));
        assert_eq!(strftime(buf.as_mut_ptr(), 4, c"%Y".as_ptr().cast(), &tm), 0);
        let mut wbuf = [0u16; 8];
        let fmt: Vec<u16> = "%d/%m".encode_utf16().chain([0]).collect();
        assert_eq!(wcsftime(wbuf.as_mut_ptr(), 8, fmt.as_ptr(), &tm), 5);
        assert_eq!(String::from_utf16(&wbuf[..5]).unwrap(), "01/01");
        let mut a = [0u8; 26];
        assert_eq!(_asctime_s(a.as_mut_ptr(), 26, &gmtime(951_827_696)), 0);
        assert_eq!(&a[..25], b"Tue Feb 29 12:34:56 2000\n");
    }
}
