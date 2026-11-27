//! The guest clock shared by the GPU and HLE.
//! On the native host, `VICTORY_CLOCK_SLOW=0.25` advances guest time at quarter speed.

use std::sync::LazyLock;
#[cfg(not(windows))]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(not(windows))]
const NANOS_PER_SECOND: u64 = 1_000_000_000;

#[cfg(not(windows))]
struct Clock {
    start: Instant,
    utc_at_start: Duration,
    rate: f64,
}

#[cfg(not(windows))]
static CLOCK: LazyLock<Clock> = LazyLock::new(|| Clock {
    start: Instant::now(),
    utc_at_start: SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default(),
    rate: std::env::var("VICTORY_CLOCK_SLOW")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .unwrap_or(1.0),
});

/// Capture the origin before starting the native title.
#[cfg(not(windows))]
pub fn initialize() {
    LazyLock::force(&CLOCK);
}

#[cfg(not(windows))]
pub fn counter_frequency() -> u64 {
    NANOS_PER_SECOND
}

#[cfg(not(windows))]
pub fn counter() -> u64 {
    let elapsed = CLOCK.start.elapsed().as_nanos();
    if CLOCK.rate == 1.0 {
        elapsed.min(i64::MAX as u128) as u64
    } else {
        // shortcut: debug scaling rounds to f64 precision, use fixed-point scaling if long runs need nanosecond precision.
        (elapsed as f64 * CLOCK.rate) as i64 as u64
    }
}

/// Native UTC, projected from the same origin as QPC.
#[cfg(not(windows))]
pub fn unix_time() -> Duration {
    CLOCK.utc_at_start + Duration::from_nanos(counter())
}

// shortcut: Windows HLE passes time APIs through, virtualize those APIs before enabling clock scaling on Windows.
#[cfg(windows)]
pub fn counter_frequency() -> u64 {
    static FREQUENCY: LazyLock<u64> = LazyLock::new(|| {
        let mut frequency = 0;
        // SAFETY: writable local. QPF is available on all supported Windows hosts.
        unsafe {
            windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency)
        };
        frequency as u64
    });
    *FREQUENCY
}

#[cfg(windows)]
pub fn counter() -> u64 {
    let mut count = 0;
    // SAFETY: writable local.
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
    count as u64
}

pub(crate) fn ticks_from_counter(count: u64, frequency: u64) -> u64 {
    let ticks = u128::from(count) * u128::from(frequency) / u128::from(counter_frequency());
    ticks.min(u128::from(u64::MAX)) as u64
}

pub fn ticks(frequency: u64) -> u64 {
    ticks_from_counter(counter(), frequency)
}
