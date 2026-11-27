//! `api-ms-win-crt-time`: 64-bit `time_t`, the `tm` conversions (UTC in
//! Rust, local time through libSystem's zone rules), `strftime` and
//! `wcsftime` in the "C" locale, `clock` and `_ftime64`.

use std::ffi::c_void;
use std::sync::LazyLock;


use victory_gpu::clock;

use super::runtime::bad_param;
use super::{Ch, EINVAL, ERANGE, exports, heap, leak_cstr, set_errno, units};

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
    let t = clock::unix_time().as_secs() as i64;
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

/// Milliseconds since the guest clock started (`CLOCKS_PER_SEC` is 1000).
pub extern "system" fn clock() -> i32 {
    clock::ticks(1_000) as i32
}

/// `struct __timeb64`: `time`, `millitm`, `timezone` (minutes west), `dstflag`.
pub extern "system" fn _ftime64_s(out: *mut u8) -> i32 {
    if out.is_null() {
        return bad_param(EINVAL);
    }
    let now = clock::unix_time();
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
    Some(local_zone(t)?.gmtoff as i32)
}

fn local_zone(t: i64) -> Option<MacTm> {
    let mut m = MacTm {
        f: [0; 9],
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    // SAFETY: libSystem fills the structure.
    (!unsafe { localtime_r(&t, &mut m) }.is_null()).then_some(m)
}

pub extern "system" fn timespec_get(out: *mut i64, base: i32) -> i32 {
    if out.is_null() || base != 1 {
        return 0;
    }
    let now = clock::unix_time();
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
    static UTC_NAME: LazyLock<usize> = LazyLock::new(|| leak_cstr(b"UTC") as usize);
    let now = clock::unix_time().as_secs() as i64;
    let off = zone_offset(now).unwrap_or(0);
    // SAFETY: the CRT's variables, shared with the title.
    unsafe {
        *TIMEZONE.ptr() = -off;
        *DAYLIGHT.ptr() = 0;
        *TZNAMES.ptr() = [*UTC_NAME, *UTC_NAME];
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

// The narrow and wide parts of the x64 __crt_lc_time_data layout.
#[repr(C)]
struct TimeNames<T> {
    wday_abbr: [T; 7],
    wday: [T; 7],
    month_abbr: [T; 12],
    month: [T; 12],
    ampm: [T; 2],
    short_date: T,
    long_date: T,
    time: T,
}

impl<T: Copy> TimeNames<T> {
    fn map<U>(&self, mut f: impl FnMut(T) -> U) -> TimeNames<U> {
        TimeNames {
            wday_abbr: self.wday_abbr.map(&mut f),
            wday: self.wday.map(&mut f),
            month_abbr: self.month_abbr.map(&mut f),
            month: self.month.map(&mut f),
            ampm: self.ampm.map(&mut f),
            short_date: f(self.short_date),
            long_date: f(self.long_date),
            time: f(self.time),
        }
    }
}

#[repr(C)]
struct TimeData {
    narrow: TimeNames<*const u8>,
    calendar: i32,
    refcount: i32,
    wide: TimeNames<*const u16>,
    locale_name: *const u16,
}

const fn abbreviations<const N: usize>(names: [&str; N]) -> [&str; N] {
    let mut abbr = names;
    let mut i = 0;
    while i < N {
        abbr[i] = names[i].split_at(3).0;
        i += 1;
    }
    abbr
}

const C_TIME: TimeNames<&str> = TimeNames {
    wday_abbr: abbreviations(DAYS),
    wday: DAYS,
    month_abbr: abbreviations(MONTHS),
    month: MONTHS,
    ampm: ["AM", "PM"],
    short_date: "MM/dd/yy",
    long_date: "dddd, MMMM dd, yyyy",
    time: "HH:mm:ss",
};

fn alloc_time_data() -> *mut TimeData {
    let mut count = 0;
    C_TIME.map(|s| count += s.len() + 1);
    let data = heap::malloc(size_of::<TimeData>() + 3 * count).cast::<TimeData>();
    if data.is_null() {
        return data;
    }
    // Both sets of strings follow the struct in the same allocation, wide
    // first for alignment. The caller only has to free the struct pointer.
    // SAFETY: all fields and strings fit in the allocation calculated above.
    unsafe {
        let mut wide = data.add(1).cast::<u16>();
        let wide_names = C_TIME.map(|s| {
            let start = wide;
            for b in s.bytes().chain([0]) {
                wide.write(u16::from(b));
                wide = wide.add(1);
            }
            start.cast_const()
        });
        let mut narrow = wide.cast::<u8>();
        let narrow_names = C_TIME.map(|s| {
            let start = narrow;
            std::ptr::copy_nonoverlapping(s.as_ptr(), narrow, s.len());
            narrow = narrow.add(s.len());
            narrow.write(0);
            narrow = narrow.add(1);
            start.cast_const()
        });
        data.write(TimeData {
            narrow: narrow_names,
            calendar: 1, // Gregorian
            refcount: 0,
            wide: wide_names,
            locale_name: std::ptr::null(),
        });
    }
    data
}

fn name_list<C: Ch>(abbr: &[&str], full: &[&str]) -> *mut C {
    let mut text = String::new();
    for (short, long) in abbr.iter().zip(full) {
        text.push(':');
        text.push_str(short);
        text.push(':');
        text.push_str(long);
    }
    let buf = heap::malloc((text.len() + 1) * size_of::<C>()).cast::<C>();
    if !buf.is_null() {
        // SAFETY: all the ASCII names and the NUL fit in the allocation.
        unsafe {
            for (i, b) in text.bytes().chain([0]).enumerate() {
                buf.add(i).write(C::make(u32::from(b)));
            }
        }
    }
    buf
}

pub extern "system" fn _Getdays() -> *mut u8 {
    name_list(&C_TIME.wday_abbr, &DAYS)
}

pub extern "system" fn _Getmonths() -> *mut u8 {
    name_list(&C_TIME.month_abbr, &MONTHS)
}

pub extern "system" fn _W_Getdays() -> *mut u16 {
    name_list(&C_TIME.wday_abbr, &DAYS)
}

pub extern "system" fn _W_Getmonths() -> *mut u16 {
    name_list(&C_TIME.month_abbr, &MONTHS)
}

pub extern "system" fn _Gettnames() -> *mut c_void {
    alloc_time_data().cast()
}

pub extern "system" fn _W_Gettnames() -> *mut c_void {
    alloc_time_data().cast()
}

fn valid(tm: &Tm) -> bool {
    (0..=60).contains(&tm.sec)
        && (0..=59).contains(&tm.min)
        && (0..=23).contains(&tm.hour)
        && (1..=31).contains(&tm.mday)
        && (0..=11).contains(&tm.mon)
        && (-1900..=i32::MAX - 1901).contains(&tm.year)
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

struct TimeFormatter<'a, C> {
    names: &'a TimeNames<*const C>,
    tm: &'a Tm,
    out: Vec<C>,
}

impl<C: Ch> TimeFormatter<'_, C> {
    fn ascii(&mut self, s: &str) {
        self.out.extend(s.bytes().map(|b| C::make(u32::from(b))));
    }

    fn number(&mut self, value: i32, width: usize) {
        self.ascii(&format!("{value:0>width$}"));
    }

    fn name(&mut self, s: *const C) {
        // SAFETY: a name from the caller's time data or the C locale.
        self.out.extend_from_slice(unsafe { units(s) });
    }

    fn hour12(&self) -> i32 {
        (self.tm.hour + 11) % 12 + 1
    }

    fn ampm(&self) -> *const C {
        self.names.ampm[usize::from(self.tm.hour >= 12)]
    }

    /// Expands a Windows date or time picture (e.g. "dddd, MMMM dd, yyyy").
    fn picture(&mut self, pattern: *const C) {
        // SAFETY: a NUL-terminated picture in the time data.
        let mut rest = unsafe { units(pattern) };
        let mut quoted = false;
        let tm = self.tm;
        while let Some((&c, tail)) = rest.split_first() {
            let ch = char::from_u32(c.get());
            if ch == Some('\'') {
                if tail.first() == Some(&c) {
                    self.out.push(c);
                    rest = &tail[1..];
                } else {
                    quoted = !quoted;
                    rest = tail;
                }
                continue;
            }
            if quoted {
                self.out.push(c);
                rest = tail;
                continue;
            }
            let count = 1 + tail.iter().take_while(|&&next| next == c).count();
            match (ch, count) {
                (Some('d'), 3) => self.name(self.names.wday_abbr[tm.wday as usize]),
                (Some('d'), 4) => self.name(self.names.wday[tm.wday as usize]),
                (Some('d'), 1 | 2) => self.number(tm.mday, count),
                (Some('M'), 3) => self.name(self.names.month_abbr[tm.mon as usize]),
                (Some('M'), 4) => self.name(self.names.month[tm.mon as usize]),
                (Some('M'), 1 | 2) => self.number(tm.mon + 1, count),
                (Some('y'), 1 | 2) => self.number((tm.year + 1900) % 100, count),
                (Some('y'), 3..=5) => self.number(tm.year + 1900, count),
                (Some('H'), 1 | 2) => self.number(tm.hour, count),
                (Some('h'), 1 | 2) => self.number(self.hour12(), count),
                (Some('m'), 1 | 2) => self.number(tm.min, count),
                (Some('s'), 1 | 2) => self.number(tm.sec, count),
                (Some('t'), 1 | 2) => {
                    // SAFETY: the locale's NUL-terminated AM or PM indicator.
                    let ampm = unsafe { units(self.ampm()) };
                    let len = if count == 1 {
                        ampm.len().min(1)
                    } else {
                        ampm.len()
                    };
                    self.out.extend_from_slice(&ampm[..len]);
                }
                _ => self.out.extend_from_slice(&rest[..count]),
            }
            rest = &rest[count..];
        }
    }

    fn format(&mut self, mut fmt: impl Iterator<Item = C>, strip: bool) -> Option<()> {
        let tm = self.tm;
        while let Some(c) = fmt.next() {
            if c.get() != u32::from(b'%') {
                self.out.push(c);
                continue;
            }
            let mut spec = char::from_u32(fmt.next()?.get())?;
            let strip = if spec == '#' {
                spec = char::from_u32(fmt.next()?.get())?;
                true
            } else {
                strip
            };
            // The existing formatter accepts alternate-digit modifiers.
            while matches!(spec, 'E' | 'O') {
                spec = char::from_u32(fmt.next()?.get())?;
            }
            let width = |n| if strip { 0 } else { n };
            match spec {
                'a' => self.name(self.names.wday_abbr[tm.wday as usize]),
                'A' => self.name(self.names.wday[tm.wday as usize]),
                'b' | 'h' => self.name(self.names.month_abbr[tm.mon as usize]),
                'B' => self.name(self.names.month[tm.mon as usize]),
                'c' | 'x' => {
                    self.picture(if strip {
                        self.names.long_date
                    } else {
                        self.names.short_date
                    });
                    if spec == 'c' {
                        self.ascii(" ");
                        self.picture(self.names.time);
                    }
                }
                'X' => self.picture(self.names.time),
                'C' => self.number((tm.year + 1900) / 100, width(2)),
                'd' => self.number(tm.mday, width(2)),
                'D' => self.composite(b"%m/%d/%y", strip)?,
                'e' => self.ascii(&format!("{:>width$}", tm.mday, width = width(2))),
                'F' => self.composite(b"%Y-%m-%d", strip)?,
                'g' => self.number(iso_week(tm).1.rem_euclid(100), 2),
                'G' => self.number(iso_week(tm).1, 4),
                'H' => self.number(tm.hour, width(2)),
                'I' => self.number(self.hour12(), width(2)),
                'j' => self.number(tm.yday + 1, width(3)),
                'm' => self.number(tm.mon + 1, width(2)),
                'M' => self.number(tm.min, width(2)),
                'n' => self.ascii("\n"),
                'p' => self.name(self.ampm()),
                'r' => self.composite(b"%I:%M:%S %p", strip)?,
                'R' => self.composite(b"%H:%M", strip)?,
                'S' => self.number(tm.sec, width(2)),
                't' => self.ascii("\t"),
                'T' => self.composite(b"%H:%M:%S", strip)?,
                'u' => self.number(if tm.wday == 0 { 7 } else { tm.wday }, 0),
                'U' => self.number((tm.yday + 7 - tm.wday) / 7, width(2)),
                'V' => self.number(iso_week(tm).0, width(2)),
                'w' => self.number(tm.wday, 0),
                'W' => self.number((tm.yday + 7 - (tm.wday + 6) % 7) / 7, width(2)),
                'y' => self.number((tm.year + 1900) % 100, width(2)),
                'Y' => self.number(tm.year + 1900, width(4)),
                'z' | 'Z' => {
                    let mut copy = *tm;
                    if tm.isdst >= 0
                        && let Some(zone) = mktime_local(&mut copy).and_then(local_zone)
                    {
                        if spec == 'z' {
                            let off = zone.gmtoff;
                            let (sign, a) = if off < 0 { ('-', -off) } else { ('+', off) };
                            self.ascii(&format!("{sign}{:02}{:02}", a / 3600, a / 60 % 60));
                        } else if !zone.zone.is_null() {
                            // SAFETY: libSystem supplied a NUL-terminated zone abbreviation.
                            let name = unsafe { super::bytes(zone.zone) };
                            self.out.extend(name.iter().map(|&b| C::make(u32::from(b))));
                        }
                    }
                }
                '%' => self.ascii("%"),
                _ => return None,
            }
        }
        Some(())
    }

    fn composite(&mut self, fmt: &[u8], strip: bool) -> Option<()> {
        self.format(fmt.iter().map(|&b| C::make(u32::from(b))), strip)
    }
}

fn format_time<C: Ch>(
    buf: *mut C,
    max: usize,
    fmt: *const C,
    tm: *const Tm,
    data: *const c_void,
    names: impl FnOnce(&TimeData) -> &TimeNames<*const C>,
) -> usize {
    if buf.is_null() || max == 0 {
        bad_param(EINVAL);
        return 0;
    }
    // SAFETY: the caller provided space for at least one unit.
    unsafe { buf.write(C::default()) };
    // SAFETY: the title's `tm`, checked before dereferencing.
    let Some(tm) = (unsafe { tm.as_ref() }).filter(|t| valid(t)) else {
        bad_param(EINVAL);
        return 0;
    };
    if fmt.is_null() {
        bad_param(EINVAL);
        return 0;
    }
    let data = if data.is_null() {
        static DEFAULT_TIME: LazyLock<usize> = LazyLock::new(|| alloc_time_data() as usize);
        if *DEFAULT_TIME == 0 {
            set_errno(super::ENOMEM);
            return 0;
        }
        *DEFAULT_TIME as *const TimeData
    } else {
        data.cast()
    };
    // SAFETY: the supplied or default time-data block and title's NUL-terminated format.
    let (names, fmt) = unsafe { (names(&*data), units(fmt)) };
    let mut formatter = TimeFormatter {
        tm,
        names,
        out: Vec::new(),
    };
    if formatter.format(fmt.iter().copied(), false).is_none() {
        bad_param(EINVAL);
        return 0;
    }
    let mut text = formatter.out;
    if text.len() >= max {
        set_errno(ERANGE);
        return 0;
    }
    text.push(C::default());
    // SAFETY: the caller's buffer has room for the result and terminator.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), buf, text.len()) };
    text.len() - 1
}

pub extern "system" fn _Strftime(
    buf: *mut u8,
    max: usize,
    fmt: *const u8,
    tm: *const Tm,
    data: *const c_void,
) -> usize {
    format_time(buf, max, fmt, tm, data, |loc| &loc.narrow)
}

pub extern "system" fn _Wcsftime(
    buf: *mut u16,
    max: usize,
    fmt: *const u16,
    tm: *const Tm,
    data: *const c_void,
) -> usize {
    format_time(buf, max, fmt, tm, data, |loc| &loc.wide)
}

pub extern "system" fn strftime(buf: *mut u8, max: usize, fmt: *const u8, tm: *const Tm) -> usize {
    _Strftime(buf, max, fmt, tm, std::ptr::null())
}

pub extern "system" fn wcsftime(
    buf: *mut u16,
    max: usize,
    fmt: *const u16,
    tm: *const Tm,
) -> usize {
    _Wcsftime(buf, max, fmt, tm, std::ptr::null())
}

/// `"Sun Sep 16 01:03:52 1973\n"`.
fn asctime_text(tm: &Tm) -> Option<String> {
    (valid(tm) && tm.year <= 8099).then(|| {
        format!(
            "{} {} {:>2} {:02}:{:02}:{:02} {:04}\n",
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
    if buf.is_null() || size == 0 {
        return bad_param(EINVAL);
    }
    // SAFETY: `buf` holds at least one byte.
    unsafe { buf.write(0) };
    if size < 26 || tm.is_null() {
        return bad_param(EINVAL);
    }
    // SAFETY: the title's `tm` and buffer.
    unsafe {
        let Some(t) = asctime_text(&tm.read()) else {
            return bad_param(EINVAL);
        };
        std::ptr::copy_nonoverlapping(t.as_ptr(), buf, t.len());
        buf.add(t.len()).write(0);
    }
    0
}

pub extern "system" fn _ctime64_s(buf: *mut u8, size: usize, t: *const i64) -> i32 {
    if buf.is_null() || size == 0 {
        return bad_param(EINVAL);
    }
    // SAFETY: `buf` holds at least one byte.
    unsafe { buf.write(0) };
    if size < 26 {
        return bad_param(EINVAL);
    }
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
    strftime, wcsftime, _Strftime, _Wcsftime, _Getdays, _Getmonths, _Gettnames,
    _W_Getdays, _W_Getmonths, _W_Gettnames, asctime, _asctime_s, _ctime64_s, _ctime64;
    "time" => _time64, "difftime" => _difftime64, "_timespec64_get" => timespec_get,
    "_strftime_l" => strftime, "_wcsftime_l" => wcsftime, "asctime_s" => _asctime_s);

#[cfg(test)]
mod tests {
    use super::*;

    fn text(fmt: &str, tm: &Tm) -> String {
        let mut buf = [0u8; 128];
        let fmt = std::ffi::CString::new(fmt).unwrap();
        let len = strftime(buf.as_mut_ptr(), buf.len(), fmt.as_ptr().cast(), tm);
        String::from_utf8(buf[..len].to_vec()).unwrap()
    }

    #[test]
    fn getdays_and_getmonths_return_colon_separated_names_in_both_widths() {
        let days = concat!(
            ":Sun:Sunday:Mon:Monday:Tue:Tuesday:Wed:Wednesday:Thu:Thursday",
            ":Fri:Friday:Sat:Saturday"
        );
        let months = concat!(
            ":Jan:January:Feb:February:Mar:March:Apr:April:May:May:Jun:June",
            ":Jul:July:Aug:August:Sep:September:Oct:October:Nov:November:Dec:December"
        );
        for (narrow, wide, expected) in [
            (_Getdays(), _W_Getdays(), days),
            (_Getmonths(), _W_Getmonths(), months),
        ] {
            assert!(!narrow.is_null() && !wide.is_null());
            assert_eq!(unsafe { super::super::bytes(narrow) }, expected.as_bytes());
            assert_eq!(
                String::from_utf16(unsafe { super::super::wide(wide) }).unwrap(),
                expected
            );
            heap::free(narrow.cast());
            heap::free(wide.cast());
        }
    }

    #[test]
    fn gettnames_and_w_gettnames_return_independent_x64_time_blocks() {
        assert_eq!(size_of::<TimeData>(), 704);
        assert_eq!(std::mem::offset_of!(TimeData, calendar), 344);
        assert_eq!(std::mem::offset_of!(TimeData, refcount), 348);
        assert_eq!(std::mem::offset_of!(TimeData, wide), 352);
        assert_eq!(std::mem::offset_of!(TimeData, locale_name), 696);
        let narrow = _Gettnames();
        let wide = _W_Gettnames();
        assert!(!narrow.is_null() && !wide.is_null());
        assert_ne!(narrow, wide);
        for data in [narrow, wide] {
            let names = unsafe { &*data.cast::<TimeData>() };
            let start = data as usize + size_of::<TimeData>();
            let end = data as usize + heap::_msize(data);
            names
                .narrow
                .map(|s| assert!((start..end).contains(&(s as usize))));
            names
                .wide
                .map(|s| assert!((start..end).contains(&(s as usize))));
            assert_eq!(names.calendar, 1);
            assert!(names.locale_name.is_null());
            unsafe {
                assert_eq!(units(names.narrow.wday_abbr[0]), b"Sun");
                assert_eq!(units(names.narrow.wday[6]), b"Saturday");
                assert_eq!(units(names.narrow.month_abbr[0]), b"Jan");
                assert_eq!(units(names.narrow.month[11]), b"December");
                assert_eq!(units(names.narrow.ampm[1]), b"PM");
                assert_eq!(units(names.narrow.short_date), b"MM/dd/yy");
                assert_eq!(units(names.narrow.long_date), b"dddd, MMMM dd, yyyy");
                assert_eq!(units(names.narrow.time), b"HH:mm:ss");
                assert_eq!(units(names.wide.wday_abbr[0]), [83, 117, 110]);
                assert_eq!(units(names.wide.ampm[0]), [65, 77]);
            }
            heap::free(data);
        }
    }

    #[test]
    fn strftime_uses_supplied_narrow_time_data_and_checks_capacity() {
        let tm = gmtime(951_827_696);
        let data = _Gettnames();
        assert!(!data.is_null());
        unsafe {
            let loc = &mut *data.cast::<TimeData>();
            loc.narrow.wday[2] = c"mardi".as_ptr().cast();
            loc.narrow.ampm[1] = c"pm".as_ptr().cast();
            loc.narrow.short_date = c"yyyy-MM-dd".as_ptr().cast();
            loc.narrow.time = c"h:mm tt".as_ptr().cast();
        }
        let fmt = c"café %A %c".as_ptr().cast();
        let mut buf = [0u8; 64];
        let len = _Strftime(buf.as_mut_ptr(), buf.len(), fmt, &tm, data);
        assert_eq!(&buf[..=len], "café mardi 2000-02-29 12:34 pm\0".as_bytes());
        assert_eq!(_Strftime(buf.as_mut_ptr(), len, fmt, &tm, data), 0);
        assert_eq!(buf[0], 0);
        let n = _Strftime(buf.as_mut_ptr(), buf.len(), fmt, &tm, std::ptr::null());
        assert_eq!(&buf[..n], "café Tuesday 02/29/00 12:34:56".as_bytes());
        heap::free(data);
    }

    #[test]
    fn wcsftime_uses_supplied_wide_time_data_and_counts_utf16_units() {
        let tm = gmtime(0);
        let data = _W_Gettnames();
        assert!(!data.is_null());
        let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<_>>();
        let day = w("木曜日");
        let date = w("yyyy'年'M'月'd'日'");
        unsafe {
            let loc = &mut *data.cast::<TimeData>();
            loc.wide.wday[4] = day.as_ptr();
            loc.wide.long_date = date.as_ptr();
        }
        let fmt = w("📅 %A %#x");
        let expected = w("📅 木曜日 1970年1月1日");
        let mut buf = [0u16; 64];
        let len = _Wcsftime(buf.as_mut_ptr(), expected.len(), fmt.as_ptr(), &tm, data);
        assert_eq!(&buf[..=len], expected);
        assert_eq!(_Wcsftime(buf.as_mut_ptr(), len, fmt.as_ptr(), &tm, data), 0);
        assert_eq!(buf[0], 0);
        let n = _Wcsftime(
            buf.as_mut_ptr(),
            buf.len(),
            fmt.as_ptr(),
            &tm,
            std::ptr::null(),
        );
        assert_eq!(&buf[..=n], w("📅 Thursday Thursday, January 01, 1970"));
        heap::free(data);
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
        assert_eq!(text("%Q", &tm), "");
        assert_eq!(super::super::errno(), EINVAL);
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

    #[test]
    fn tzset_reuses_stable_tzname_pointers() {
        // SAFETY: `__p__tzname` returns the CRT's two-entry NUL-terminated table.
        unsafe {
            let first = *__p__tzname();
            _tzset();
            let second = *__p__tzname();
            assert_eq!(first, second);
            assert_eq!(super::super::bytes(first[0] as *const u8), b"UTC");
            assert_eq!(super::super::bytes(first[1] as *const u8), b"UTC");
        }
    }

    #[test]
    fn asctime_and_ctime_reject_five_digit_years_and_clear_buffer_on_error() {
        let mut far = gmtime(0);
        far.year = 9000; // year 10900: would overflow a 26-byte buffer
        set_errno(0);
        assert!(asctime(&far).is_null());
        assert_eq!(super::super::errno(), EINVAL);

        let mut buf = [b'Z'; 28];
        assert_eq!(_asctime_s(buf.as_mut_ptr(), 26, &far), EINVAL);
        assert_eq!(buf[0], 0);
        assert_eq!(buf[26], b'Z');

        buf = [b'Z'; 28];
        assert_eq!(_asctime_s(buf.as_mut_ptr(), 10, &gmtime(0)), EINVAL);
        assert_eq!(buf[0], 0);

        let neg = -1i64;
        buf = [b'Z'; 28];
        assert_eq!(_ctime64_s(buf.as_mut_ptr(), 26, &neg), EINVAL);
        assert_eq!(buf[0], 0);

        let zero = 0i64;
        buf = [b'Z'; 28];
        assert_eq!(_ctime64_s(buf.as_mut_ptr(), 10, &zero), EINVAL);
        assert_eq!(buf[0], 0);
    }
}
