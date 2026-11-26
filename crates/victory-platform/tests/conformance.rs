//! The same checks run against both implementations: natively on macOS
//! (`cargo test`) and under Wine (`tools/macos/test-wine.sh -p victory-platform`).
#![cfg(any(windows, target_os = "macos"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use victory_platform::mem::{self, PagePool, Protect};
use victory_platform::sync::{Event, Semaphore, wait_on_address, wake_all, wake_one};
use victory_platform::thread;

fn read(p: *mut u8, at: usize) -> u8 {
    // SAFETY: tests only read inside their own mappings.
    unsafe { p.add(at).read_volatile() }
}

fn write(p: *mut u8, at: usize, v: u8) {
    // SAFETY: tests only write inside their own mappings.
    unsafe { p.add(at).write_volatile(v) }
}

#[test]
fn reserve_commit_protect_release() {
    let g = mem::granularity();
    // SAFETY: the test owns everything it reserves.
    unsafe {
        let p = mem::reserve(None, 4 * g).unwrap();
        assert_eq!(mem::query(p as usize).unwrap().protect, Protect::NO_ACCESS);
        mem::commit(p, 2 * g, Protect::READ_WRITE).unwrap();
        write(p, 0, 7);
        write(p, 2 * g - 1, 9);
        assert_eq!(read(p, 0), 7);
        let old = mem::protect(p, g, Protect::READ).unwrap();
        assert_eq!(old, Protect::READ_WRITE);
        assert_eq!(mem::query(p as usize).unwrap().protect, Protect::READ);
        // The second granule stays writable.
        write(p, g, 1);
        mem::decommit(p, 2 * g).unwrap();
        assert_eq!(mem::query(p as usize).unwrap().protect, Protect::NO_ACCESS);
        mem::commit(p, g, Protect::READ_WRITE).unwrap();
        assert_eq!(read(p, 0), 0, "decommitted pages come back zeroed");
        mem::release(p, 4 * g).unwrap();
    }
}

#[test]
fn reserve_at_an_address_fails_when_taken() {
    let g = mem::granularity();
    // SAFETY: the test owns everything it reserves.
    unsafe {
        let p = mem::reserve(None, g).unwrap();
        assert!(mem::reserve(Some(p as usize), g).is_err());
        mem::release(p, g).unwrap();
        // Free again, so exactly there works. Other tests reserve at the same
        // time and may take the address between the release and here, so
        // try new addresses until one is not taken.
        let mut at = p;
        let q = loop {
            if let Ok(q) = mem::reserve(Some(at as usize), g) {
                break q;
            }
            at = mem::reserve(None, g).unwrap();
            mem::release(at, g).unwrap();
        };
        assert_eq!(q, at);
        mem::release(q, g).unwrap();
    }
}

#[test]
fn a_page_pool_maps_at_several_addresses() {
    let g = mem::granularity();
    let pool = PagePool::new(4 * g).unwrap();
    assert_eq!(pool.len(), 4 * g);
    // SAFETY: the test owns every view it maps.
    unsafe {
        let a = pool.map(0, 4 * g, None, Protect::READ_WRITE).unwrap();
        let b = pool.map(0, 4 * g, None, Protect::READ_WRITE).unwrap();
        assert_ne!(a, b);
        write(a, 5, 0xAB);
        write(a, 3 * g, 0xCD);
        assert_eq!(
            read(b, 5),
            0xAB,
            "a write through one view shows in the other"
        );
        assert_eq!(read(b, 3 * g), 0xCD);

        // A sub-range is the same pages at another offset.
        let c = pool.map(3 * g, g, None, Protect::READ).unwrap();
        assert_eq!(read(c, 0), 0xCD);

        // At a chosen address. Windows maps over free space only, so the
        // reservation is let go first, and another test may take the address
        // in between (error 487): then try again elsewhere. macOS maps over the
        // reservation itself.
        let (free, d) = (0..8)
            .find_map(|_| {
                let free = mem::reserve(None, 4 * g).unwrap();
                #[cfg(windows)]
                mem::release(free, 4 * g).unwrap();
                let d = pool.map(0, 4 * g, Some(free as usize), Protect::READ_WRITE);
                d.ok().map(|d| (free, d))
            })
            .expect("a free address to map at");
        assert_eq!(d, free);
        assert_eq!(read(d, 5), 0xAB);

        // Views protect on their own.
        mem::protect(a, 4 * g, Protect::READ).unwrap();
        assert_eq!(mem::query(a as usize).unwrap().protect, Protect::READ);
        write(b, 6, 1);
        assert_eq!(read(d, 6), 1);

        for (p, len) in [(a, 4 * g), (b, 4 * g), (c, g), (d, 4 * g)] {
            mem::unmap(p, len).unwrap();
        }
    }
}

/// On macOS one VM call changes the protection of many mappings at once; a
/// Windows view must be protected on its own, so this one is macOS only.
#[cfg(target_os = "macos")]
#[test]
fn one_protect_call_covers_many_mappings_of_a_pool() {
    let g = mem::granularity();
    let n = 64;
    let pool = PagePool::new(n * g).unwrap();
    // SAFETY: the test owns the reservation and its views.
    unsafe {
        let base = mem::reserve(None, n * g).unwrap();
        // The pool's pages in reverse order, one mapping each.
        for i in 0..n {
            let at = base as usize + i * g;
            pool.map((n - 1 - i) * g, g, Some(at), Protect::READ_WRITE)
                .unwrap();
        }
        write(base, 0, 1);
        let alias = pool.map(0, n * g, None, Protect::READ).unwrap();
        assert_eq!(
            read(alias, (n - 1) * g),
            1,
            "first page of the base is the pool's last"
        );
        // One call over all 64 mappings.
        mem::protect(base, n * g, Protect::READ).unwrap();
        assert_eq!(mem::query(base as usize).unwrap().protect, Protect::READ);
        mem::unmap(alias, n * g).unwrap();
        mem::release(base, n * g).unwrap();
    }
}

/// The pool's own mapping of all of it, which stays where it is whatever
/// else is mapped and unmapped, and the windows cut from it.
#[test]
fn a_page_pool_has_one_permanent_base() {
    let g = mem::granularity();
    let pool = PagePool::new(4 * g).unwrap();
    let base = pool.base().expect("the host maps the pool");
    assert_eq!(base as usize % 0x4000, 0);
    // SAFETY: the test owns every view it maps.
    unsafe {
        write(base, 2 * g + 1, 0x5a);
        let a = pool.map(0, 4 * g, None, Protect::READ_WRITE).unwrap();
        assert_eq!(read(a, 2 * g + 1), 0x5a, "a view shows what base wrote");
        write(a, 3 * g, 0x77);
        mem::unmap(a, 4 * g).unwrap();
        assert_eq!(pool.base(), Some(base), "the base does not move");
        assert_eq!(read(base, 3 * g), 0x77, "and what a view wrote stays");
    }
    // A window is the same bytes at their place in the base.
    let w = pool.window(g, 2 * g).unwrap();
    assert_eq!(w.as_mut_ptr(), base.wrapping_add(g));
    assert_eq!(read(w.as_mut_ptr(), g + 1), 0x5a);
    assert!(pool.window(3 * g, 2 * g).is_err(), "past the end");
}

/// A mapping goes over part of a reservation and comes off it again, at
/// title addresses: one VM call each, the pool's base undisturbed. macOS
/// only: a Windows mapping goes over a placeholder (see the `xmem` tests).
#[cfg(target_os = "macos")]
#[test]
fn a_pool_maps_over_a_reservation_and_unmaps_again() {
    let g = mem::granularity();
    let pool = PagePool::new(8 * g).unwrap();
    assert!(pool.memory_entry() != 0);
    // SAFETY: the test owns the reservation and its views.
    unsafe {
        let base = pool.base().unwrap();
        let at = mem::reserve(None, 8 * g).unwrap();
        // Remap the pool in halves, swapped: one call per run.
        pool.map_over(4 * g, 4 * g, at as usize, Protect::READ_WRITE)
            .unwrap();
        pool.map_over(0, 4 * g, at as usize + 4 * g, Protect::READ_WRITE)
            .unwrap();
        write(base, 0, 1);
        assert_eq!(read(at, 4 * g), 1);
        write(at, 0, 2);
        assert_eq!(read(base, 4 * g), 2);
        // The Mach memory entry maps the same pages once more.
        let mut other: u64 = 0;
        let kr = mach_vm_map(
            mach_task_self_,
            &mut other,
            (8 * g) as u64,
            0,
            1, // anywhere
            pool.memory_entry(),
            0,
            0,
            3,
            3,
            2,
        );
        assert_eq!(kr, 0);
        assert_eq!(read(other as *mut u8, 4 * g), 2);
        mem::unmap(other as *mut u8, 8 * g).unwrap();
        // Unmapping a part cuts the mapping there and leaves no access.
        mem::unmap_over(at.wrapping_add(g), g).unwrap();
        assert_eq!(
            mem::query(at as usize).unwrap().protect,
            Protect::READ_WRITE
        );
        assert_eq!(
            mem::query(at as usize + g).unwrap().protect,
            Protect::NO_ACCESS
        );
        assert_eq!(
            read(at, 2 * g),
            0,
            "the pages after the cut are still there"
        );
        // The pool kept everything.
        assert_eq!(read(base, 4 * g), 2);
        assert_eq!(pool.base(), Some(base));
        // Mapped again over the cut, the same pages show.
        pool.map_over(5 * g, g, at as usize + g, Protect::READ_WRITE)
            .unwrap();
        write(base, 5 * g + 7, 9);
        assert_eq!(read(at, g + 7), 9);
        mem::release(at, 8 * g).unwrap();
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    static mach_task_self_: u32;
    #[allow(clippy::too_many_arguments)]
    fn mach_vm_map(
        task: u32,
        addr: *mut u64,
        size: u64,
        mask: u64,
        flags: i32,
        object: u32,
        offset: u64,
        copy: u32,
        cur: i32,
        max: i32,
        inherit: u32,
    ) -> i32;
}

#[test]
fn a_thread_returns_its_exit_code_and_has_its_own_id() {
    let main_id = thread::current_id();
    let (tx, rx) = std::sync::mpsc::channel();
    let t = thread::spawn("probe", 1 << 20, move || {
        tx.send(thread::current_id()).unwrap();
        42
    })
    .unwrap();
    let inside = rx.recv().unwrap();
    assert_ne!(inside, main_id);
    assert_eq!(inside, t.id());
    assert_eq!(t.join(), 42);
}

#[test]
fn events_follow_win32_rules() {
    let auto = Event::new(false, false).unwrap();
    assert!(!auto.wait(Some(Duration::from_millis(10))));
    auto.set();
    assert!(auto.wait(Some(Duration::ZERO)));
    assert!(
        !auto.wait(Some(Duration::from_millis(10))),
        "auto-reset consumed it"
    );

    let manual = Arc::new(Event::new(true, true).unwrap());
    assert!(manual.wait(None));
    assert!(manual.wait(None), "manual-reset stays set");
    manual.reset();
    let m = manual.clone();
    let t = thread::spawn("waiter", 1 << 20, move || {
        u32::from(m.wait(Some(Duration::from_secs(10))))
    })
    .unwrap();
    thread::sleep(Duration::from_millis(20));
    manual.set();
    assert_eq!(t.join(), 1);
}

#[test]
fn semaphores_count_up_to_their_maximum() {
    let s = Semaphore::new(1, 2).unwrap();
    assert!(s.wait(Some(Duration::ZERO)));
    assert!(!s.wait(Some(Duration::from_millis(10))));
    assert_eq!(s.release(2), Some(0));
    assert_eq!(s.release(1), None, "past the maximum");
    assert!(s.wait(None));
    assert!(s.wait(None));
    assert!(!s.wait(Some(Duration::from_millis(10))));
    assert!(Semaphore::new(3, 2).is_err());
}

#[test]
fn wait_on_address_wakes_and_times_out() {
    let word = Arc::new(AtomicU32::new(0));
    // The value differs: returns at once.
    assert!(wait_on_address(&word, 1, Some(Duration::from_secs(5))));
    // Zero timeout: returns false when equal, true when different.
    assert!(!wait_on_address(&word, 0, Some(Duration::ZERO)));
    assert!(wait_on_address(&word, 1, Some(Duration::ZERO)));
    let start = Instant::now();
    assert!(!wait_on_address(&word, 0, Some(Duration::from_millis(50))));
    assert!(start.elapsed() >= Duration::from_millis(40));

    let w = word.clone();
    let t = thread::spawn("waiter", 1 << 20, move || {
        while w.load(Ordering::Acquire) == 0 {
            wait_on_address(&w, 0, Some(Duration::from_secs(10)));
        }
        w.load(Ordering::Acquire)
    })
    .unwrap();
    thread::sleep(Duration::from_millis(20));
    word.store(5, Ordering::Release);
    wake_one(&word);
    wake_all(&word);
    assert_eq!(t.join(), 5);
}

#[cfg(target_os = "macos")]
#[test]
fn reserve_in_keeps_to_the_range_and_alignment() {
    const MIB: usize = 1 << 20;
    let (lowest, highest) = (1usize << 32, (1usize << 40) - 1);
    // SAFETY: the test owns everything it reserves.
    unsafe {
        let p = mem::reserve_in(lowest, highest, 4 * MIB, 2 * MIB).unwrap();
        let at = p as usize;
        assert_eq!(at % (2 * MIB), 0);
        assert!(at >= lowest && at + 4 * MIB - 1 <= highest);
        assert_eq!(mem::query(at).unwrap().protect, Protect::NO_ACCESS);
        // A range that cannot hold the request fails and leaves nothing behind.
        assert!(mem::reserve_in(lowest, lowest + MIB, 4 * MIB, 0).is_err());
        mem::release(p, 4 * MIB).unwrap();
    }
}

#[cfg(target_os = "macos")]
#[test]
fn next_region_finds_the_following_mapping() {
    let g = mem::granularity();
    // SAFETY: the test owns everything it reserves.
    unsafe {
        let p = mem::reserve(None, 2 * g).unwrap();
        let inside = mem::next_region(p as usize + 1).unwrap();
        assert!(inside.base <= p as usize && inside.base + inside.len >= p as usize + 2 * g);
        mem::release(p, 2 * g).unwrap();
    }
    assert!(mem::footprint() > 0);
}
