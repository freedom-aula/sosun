//! `VICTORY_TRACE_DESCRIPTORS` (#43): how many descriptor records the title rewrites with plain
//! stores, which no device call reports. `NativeHost::remirror_tables` compares the records of
//! each bound table with those it last mirrored at every draw and dispatch; this counts, per heap,
//! the records it checked and those it found changed, and per submission the most changed. The
//! frame log prints the counts since its last line. Off, the cost is one read of the switch per
//! table remirror and per submission.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// Whether the trace is on (any value of the variable).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TRACE_DESCRIPTORS").is_some())
}

/// `VICTORY_TRACE_DESCRIPTORS=sites`: also the title's store sites, through the write watch of
/// `victory_platform::watch` over the heaps' pages, closed again at each submission.
#[cfg(target_os = "macos")]
pub(crate) fn sites() -> bool {
    static SITES: OnceLock<bool> = OnceLock::new();
    *SITES.get_or_init(|| std::env::var("VICTORY_TRACE_DESCRIPTORS").is_ok_and(|v| v == "sites"))
}

/// One heap's records checked and found changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Heap {
    pub checked: u64,
    pub changed: u64,
}

/// Remirror counts since the last summary.
#[derive(Debug, Default)]
pub(crate) struct Counts {
    /// By the heap's first descriptor address.
    pub heaps: BTreeMap<u64, Heap>,
    pub submissions: u64,
    /// Changed in the submission under way, and the most in one submission.
    current: u64,
    pub most: u64,
}

impl Counts {
    /// One table remirror of heap `heap`.
    pub(crate) fn add(&mut self, heap: u64, checked: u64, changed: u64) {
        let h = self.heaps.entry(heap).or_default();
        h.checked += checked;
        h.changed += changed;
        self.current += changed;
    }

    /// A submission ended.
    pub(crate) fn submitted(&mut self) {
        self.submissions += 1;
        self.most = self.most.max(self.current);
        self.current = 0;
    }

    /// One line over `frames` frames: totals, then the heaps by records changed, most first.
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn summary(&self, frames: u64) -> String {
        let changed: u64 = self.heaps.values().map(|h| h.changed).sum();
        let checked: u64 = self.heaps.values().map(|h| h.checked).sum();
        let mut heaps: Vec<_> = self.heaps.iter().collect();
        heaps.sort_by(|a, b| b.1.changed.cmp(&a.1.changed).then(a.0.cmp(b.0)));
        let by_heap: Vec<String> = heaps
            .iter()
            .map(|(at, h)| format!("{at:#x} {} of {}", h.changed, h.checked))
            .collect();
        format!(
            "{changed} records changed of {checked} checked over {frames} frames ({} a frame), \
             {} submissions, most in one {}; by heap (changed of checked): {}",
            changed / frames.max(1),
            self.submissions,
            self.most,
            by_heap.join(", ")
        )
    }
}

fn counts() -> std::sync::MutexGuard<'static, Counts> {
    static COUNTS: Mutex<Counts> = Mutex::new(Counts {
        heaps: BTreeMap::new(),
        submissions: 0,
        current: 0,
        most: 0,
    });
    COUNTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A table remirror of heap `heap` checked `checked` records and found `changed` changed.
pub(crate) fn record(heap: u64, checked: u64, changed: u64) {
    if on() {
        counts().add(heap, checked, changed);
    }
}

/// Held through a submission (`NativeHost::execute`): its end, however it returns, counts it.
pub(crate) struct Submission;

impl Drop for Submission {
    fn drop(&mut self) {
        if on() {
            counts().submitted();
        }
    }
}

/// Logs the counts since the last call (from the frame log, at `frame`) and starts again.
#[cfg(target_os = "macos")]
pub(crate) fn log(frame: u64) {
    if !on() {
        return;
    }
    static LAST: Mutex<u64> = Mutex::new(0);
    let frames = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let n = frame.saturating_sub(*last);
        *last = frame;
        n
    };
    let taken = std::mem::take(&mut *counts());
    tracing::info!(frame, "descriptor remirror: {}", taken.summary(frames));
    if sites() {
        let (sites, overflow) = victory_platform::watch::sites();
        let top: Vec<String> = sites
            .iter()
            .take(SITES_LOGGED)
            .map(|(at, n)| format!("image+{at:#x} {n}"))
            .collect();
        tracing::info!(
            frame,
            distinct = sites.len(),
            overflow,
            "descriptor store sites since the start (first store per page per submission), most \
             first: {}",
            top.join(", ")
        );
    }
}

/// Store sites a summary names (all are counted in `distinct`).
#[cfg(target_os = "macos")]
const SITES_LOGGED: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_add_up_by_heap_and_submission() {
        let mut c = Counts::default();
        c.add(0x2000, 16, 3);
        c.add(0x1000, 16, 0);
        c.submitted();
        c.add(0x2000, 8, 5);
        c.add(0x1000, 16, 4);
        c.submitted();
        c.submitted();
        assert_eq!(c.submissions, 3);
        assert_eq!(c.most, 9, "the second submission changed 5 + 4");
        assert_eq!(
            c.heaps[&0x2000],
            Heap {
                checked: 24,
                changed: 8
            }
        );
        assert_eq!(
            c.heaps[&0x1000],
            Heap {
                checked: 32,
                changed: 4
            }
        );
        assert_eq!(
            c.summary(4),
            "12 records changed of 56 checked over 4 frames (3 a frame), 3 submissions, most in \
             one 9; by heap (changed of checked): 0x2000 8 of 24, 0x1000 4 of 32"
        );
        assert!(
            Counts::default()
                .summary(0)
                .starts_with("0 records changed of 0 checked")
        );
    }
}
