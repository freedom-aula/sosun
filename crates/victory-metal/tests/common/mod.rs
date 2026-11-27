//! Shared helpers of the native tests: skip without the converter, zero-copy arenas, run-and-wait.
#![allow(dead_code)]

use victory_metal::backend::{ComputeBackend, Dispatch, Submission};
use victory_metal::mem::{Region, page_size};
use victory_metal::metal::{Buffer, Metal};

/// `None` (and a note on stderr) when Apple's Metal Shader Converter is not installed.
pub fn backend() -> Option<Metal> {
    if !std::path::Path::new(victory_metal::msc::DEFAULT_DYLIB).exists()
        && std::env::var_os("VICTORY_MSC_DYLIB").is_none()
    {
        eprintln!("skipped: Metal Shader Converter not installed");
        return None;
    }
    Some(Metal::new().unwrap())
}

/// The residency modes every heap test runs in: a set per heap with pool buffers named by the
/// encoders (the default on a large Mac), sets for the pool too, and `useResource` per encoder
/// for everything (the default on a small one).
pub fn backends() -> Vec<Metal> {
    let Some(m) = backend() else { return vec![] };
    let new = || Metal::new().unwrap();
    vec![m.with_heap_sets(), new().with_residency_sets(true), new().with_residency_sets(false)]
}

/// Page-aligned zero-copy memory wrapped as one buffer.
pub struct Arena {
    pub region: Region,
    pub buf: Buffer,
}

impl Arena {
    pub fn new(m: &Metal, pages: usize) -> Arena {
        let region = Region::new(pages * page_size(), None).unwrap();
        let buf = m.wrap_title_memory(region.ptr, region.len).unwrap();
        Arena { region, buf }
    }

    pub fn at(&self, byte: usize) -> *mut u8 {
        assert!(byte <= self.region.len);
        // SAFETY: inside the region.
        unsafe { self.region.ptr.add(byte) }
    }

    pub fn write<T: Copy>(&self, byte: usize, v: &[T]) {
        // SAFETY: tests write while the GPU is idle; bounds asserted.
        unsafe {
            assert!(byte + std::mem::size_of_val(v) <= self.region.len);
            std::ptr::copy_nonoverlapping(v.as_ptr(), self.at(byte).cast(), v.len())
        }
    }

    pub fn read<T: Copy + Default>(&self, byte: usize, n: usize) -> Vec<T> {
        // SAFETY: tests read after the fence wait; bounds asserted.
        unsafe {
            assert!(byte + n * std::mem::size_of::<T>() <= self.region.len);
            std::slice::from_raw_parts(self.at(byte).cast::<T>(), n).to_vec()
        }
    }
}

/// Submit dispatches on a fresh queue and wait for them.
pub fn run(m: &Metal, work: &[Dispatch<'_, Metal>]) {
    let q = m.create_queue().unwrap();
    let f = m.create_fence(0).unwrap();
    m.submit(
        &q,
        &Submission {
            waits: &[],
            dispatches: work,
            signals: &[(&f, 1)],
        },
    )
    .unwrap();
    assert!(m.wait(&f, 1, 10_000), "GPU did not finish");
}

pub fn fixture(name: &str) -> Vec<u8> {
    let p = format!("{}/tests/fixtures/{name}.dxil", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
}
