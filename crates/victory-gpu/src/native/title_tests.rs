//! The title-facing objects on the native host, called the way a title calls them: through the
//! method tables of queues, fences and lists (slot offsets of the older layout), with title memory
//! shown at an alias address of a pool the pool observer was told about. Our own HLSL
//! (`victory-metal/tests/fixtures`), our own data. Skipped without the Metal Shader Converter.

use super::Pool;
use crate::native::{glue, memory::PoolInfo};
use crate::object::{This, S_OK, slot};
use std::time::{Duration, Instant};
use victory_metal::mem::{Alias, page_size};

type HResult = i32;

fn available() -> bool {
    if std::path::Path::new(victory_metal::msc::DEFAULT_DYLIB).exists()
        || std::env::var_os("VICTORY_MSC_DYLIB").is_some()
    {
        return true;
    }
    eprintln!("skipped: Metal Shader Converter not installed");
    false
}

fn read(path: &str) -> Vec<u8> {
    let p = format!("{}/../victory-metal/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
}

/// A pool of `pages` pages told to the observer under `id` and shown at an alias address.
fn pool(id: usize, pages: usize) -> Pool {
    let mem = Alias::new(pages * page_size().max(16384)).unwrap();
    let info = PoolInfo {
        id,
        base: mem.primary as usize,
        len: mem.len,
        memory_entry: None,
    };
    let observer = glue::pool_observer();
    observer.pool_created(&info);
    observer.mapped(mem.alias as usize, mem.len, &info, 0);
    Pool { mem }
}

use crate::native::memory::PoolObserver as _;

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn nothing_for(ms: u64, mut changed: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(ms) {
        if changed() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

struct Fence(This);

impl Fence {
    fn new(initial: u64) -> Fence {
        let mut f = std::ptr::null();
        assert_eq!(
            crate::fence::create(std::ptr::null(), initial, 0, &crate::fence::FENCE_IIDS[0], &mut f),
            S_OK
        );
        Fence(f)
    }
    fn completed(&self) -> u64 {
        slot::<extern "system" fn(This) -> u64>(self.0, 0x40)(self.0)
    }
    fn signal(&self, v: u64) {
        assert_eq!(
            slot::<extern "system" fn(This, u64) -> HResult>(self.0, 0x50)(self.0, v),
            S_OK
        );
    }
    /// `SetEventOnCompletion(value, null)`: blocks.
    fn wait(&self, v: u64) -> HResult {
        slot::<extern "system" fn(This, u64, *mut std::ffi::c_void) -> HResult>(self.0, 0x48)(
            self.0,
            v,
            std::ptr::null_mut(),
        )
    }
    /// What a title reads straight from the fence's address.
    fn in_memory(&self) -> u64 {
        let at = slot::<extern "system" fn(This) -> u64>(self.0, 0x60)(self.0);
        // SAFETY: the fence's value.
        unsafe { (at as *const u64).read_volatile() }
    }
}

struct Queue(This);

impl Queue {
    fn new() -> Queue {
        Queue(crate::queue::test_queue(false))
    }
    fn execute(&self, lists: &[&List]) {
        let ptrs: Vec<This> = lists.iter().map(|l| l.0).collect();
        slot::<extern "system" fn(This, u32, *const This)>(self.0, 0x50)(
            self.0,
            ptrs.len() as u32,
            ptrs.as_ptr(),
        );
    }
    fn signal(&self, f: &Fence, v: u64) {
        assert_eq!(
            slot::<extern "system" fn(This, This, u64) -> HResult>(self.0, 0x70)(self.0, f.0, v),
            S_OK
        );
    }
    fn wait(&self, f: &Fence, v: u64) {
        assert_eq!(
            slot::<extern "system" fn(This, This, u64) -> HResult>(self.0, 0x78)(self.0, f.0, v),
            S_OK
        );
    }
}

struct List(This);

impl List {
    /// A compute list whose pipeline is `pso`.
    fn new(pso: This) -> List {
        List(crate::list::test_list(false, pso))
    }
    fn constants(&self, slot_index: u32, words: &[u32]) {
        slot::<extern "system" fn(This, u32, u32, *const u32, u32)>(self.0, 0x138)(
            self.0,
            slot_index,
            words.len() as u32,
            words.as_ptr(),
            0,
        );
    }
    fn view(&self, slot_index: u32, address: u64) {
        slot::<extern "system" fn(This, u32, u64)>(self.0, 0x158)(self.0, slot_index, address);
    }
    fn dispatch(&self, x: u32) {
        slot::<extern "system" fn(This, u32, u32, u32)>(self.0, 0x70)(self.0, x, 1, 1);
    }
    fn write32(&self, address: u64, value: u32) {
        slot::<extern "system" fn(This, u64, u32, u32, u32)>(self.0, 0x228)(
            self.0, address, value, 0, 0,
        );
    }
    /// `Wait32BitValueX(address, comparison, reference, mask, flags)`.
    fn wait_for(&self, address: u64, comparison: u32, reference: u32) {
        slot::<extern "system" fn(This, u64, u32, u32, u32, u32)>(self.0, 0x238)(
            self.0, address, comparison, reference, u32::MAX, 0,
        );
    }
    fn set_pipeline(&self, pso: This) {
        slot::<extern "system" fn(This, This)>(self.0, 0xc8)(self.0, pso);
    }
    fn close(&self) {
        assert_eq!(slot::<extern "system" fn(This) -> HResult>(self.0, 0x48)(self.0), S_OK);
    }
}

const N: usize = 1024;
const EQUAL: u32 = 3;

/// `dst[i] = src[i] * mul + add` over `N` words: a title's pipeline, root constants and root views.
fn affine(list: &List, mul: u32, add: u32, src: u64, dst: u64) {
    list.constants(0, &[mul, add]);
    list.view(1, src);
    list.view(2, dst);
    list.dispatch((N / 64) as u32);
}

fn pipeline() -> This {
    let pso = crate::pipeline::test_compute(b"title shader (installed pre-translated)");
    glue::install_compute_pipeline(pso as usize, &read("tests/affine.dxil"), [64, 1, 1]).unwrap();
    pso
}

/// A list on a queue runs on the GPU: root constants, root views of title memory, a dispatch and
/// a bottom-of-pipe write; the queue's Signal reaches the fence (read from the fence's own
/// address as a title does), and a CPU wait on it returns.
#[test]
fn a_submitted_list_runs_and_its_fence_is_signalled() {
    if !available() {
        return;
    }
    let pool = pool(9001, 4);
    let (src, dst, done) = (0, 16384, 32768);
    pool.write(src, &(0..N as u32).collect::<Vec<_>>());
    pool.write(dst, &[0xEEEE_EEEEu32; N]);
    let (q, f) = (Queue::new(), Fence::new(0));
    let list = List::new(pipeline());
    affine(&list, 5, 7, pool.va(src), pool.va(dst));
    list.write32(pool.va(done), 0xD0D0);
    list.close();
    q.execute(&[&list]);
    q.signal(&f, 1);
    assert_eq!(f.wait(1), S_OK);
    assert!(f.completed() >= 1);
    wait_until("the title's copy of the fence value", || f.in_memory() >= 1);
    let out: Vec<u32> = pool.read(dst, N);
    assert!(out.iter().enumerate().all(|(i, v)| *v == i as u32 * 5 + 7), "{:?}", &out[..4]);
    assert_eq!(pool.read::<u32>(done, 1), [0xD0D0]);
}

/// A wait for a memory value another queue's list writes: a list is cut where it waits after
/// commands. The part before the wait runs at once, the part after it only once the other queue's
/// list has written the value, and the queue's Signal comes after both.
#[test]
fn a_wait_on_a_memory_value_cuts_the_list_and_holds_the_rest() {
    if !available() {
        return;
    }
    let pool = pool(9002, 4);
    let (src, a, b, flag) = (0, 8192, 16384, 24576);
    pool.write(src, &(0..N as u32).collect::<Vec<_>>());
    pool.write(a, &[0u32; N]);
    pool.write(b, &[0u32; N]);
    pool.write(flag, &[0u32; 4]);
    let pso = pipeline();
    let (q1, q2) = (Queue::new(), Queue::new());
    let (f1, f2) = (Fence::new(0), Fence::new(0));

    let waiting = List::new(pso);
    affine(&waiting, 2, 0, pool.va(src), pool.va(a));
    waiting.wait_for(pool.va(flag), EQUAL, 1);
    affine(&waiting, 3, 0, pool.va(src), pool.va(b));
    waiting.close();
    q1.execute(&[&waiting]);
    q1.signal(&f1, 1);

    // The part before the wait ran; the part after it, and the Signal, did not.
    wait_until("the part before the wait", || {
        pool.read::<u32>(a, N).iter().enumerate().all(|(i, v)| *v == i as u32 * 2)
    });
    assert!(nothing_for(50, || f1.completed() >= 1 || pool.read::<u32>(b, 1)[0] != 0 || pool.read::<u32>(b, N)[7] != 0));
    assert_eq!(pool.read::<u32>(b, N)[7], 0, "held behind the wait");

    // Another queue's list writes the value at its end.
    let writer = List::new(pso);
    affine(&writer, 1, 1, pool.va(src), pool.va(b + 8192));
    writer.write32(pool.va(flag), 1);
    writer.close();
    q2.execute(&[&writer]);
    q2.signal(&f2, 1);

    assert_eq!(f1.wait(1), S_OK);
    assert_eq!(f2.wait(1), S_OK);
    let out: Vec<u32> = pool.read(b, N);
    assert!(out.iter().enumerate().all(|(i, v)| *v == i as u32 * 3), "{:?}", &out[..4]);
    let stats = glue::stats().unwrap();
    assert!(stats.waited >= 1 && stats.pieces >= 3, "{stats:?}");
}

/// A queue's Wait on a fence the CPU signals later: the GPU holds the queue's later work (an
/// `MTLSharedEvent` wait), and the CPU's Signal lets it go.
#[test]
fn a_queue_waits_for_a_fence_the_cpu_signals() {
    if !available() {
        return;
    }
    let pool = pool(9003, 4);
    let (src, dst) = (0, 16384);
    pool.write(src, &(0..N as u32).collect::<Vec<_>>());
    pool.write(dst, &[0u32; N]);
    let (q, gate, done) = (Queue::new(), Fence::new(0), Fence::new(0));
    q.wait(&gate, 5);
    let list = List::new(pipeline());
    affine(&list, 1, 100, pool.va(src), pool.va(dst));
    list.close();
    q.execute(&[&list]);
    q.signal(&done, 1);
    assert!(nothing_for(50, || done.completed() >= 1 || pool.read::<u32>(dst, 1)[0] != 0));
    gate.signal(5);
    assert_eq!(done.wait(1), S_OK);
    let out: Vec<u32> = pool.read(dst, N);
    assert!(out.iter().enumerate().all(|(i, v)| *v == i as u32 + 100));
}

/// A queue's Wait on a fence the CPU signals more than 5 s later, as D3D12 allows. Metal ends a
/// command buffer whose `MTLSharedEvent` wait lasts about 5 s with
/// `kIOGPUCommandBufferCallbackErrorTimeout`, and drops its work and its signals: a wait encoded
/// on the GPU before its signal exists loses the queue's later work and leaves its fence behind.
#[test]
fn a_queue_wait_longer_than_metals_event_wait_limit_still_runs_the_work() {
    if !available() {
        return;
    }
    let pool = pool(9008, 4);
    let (src, dst) = (0, 16384);
    pool.write(src, &(0..N as u32).collect::<Vec<_>>());
    pool.write(dst, &[0u32; N]);
    let (q, gate, done) = (Queue::new(), Fence::new(0), Fence::new(0));
    q.wait(&gate, 1);
    let list = List::new(pipeline());
    affine(&list, 1, 100, pool.va(src), pool.va(dst));
    list.close();
    q.execute(&[&list]);
    q.signal(&done, 1);
    assert!(nothing_for(6_000, || done.completed() >= 1 || pool.read::<u32>(dst, 1)[0] != 0));
    gate.signal(1);
    wait_until("the work after the wait", || done.completed() >= 1);
    let out: Vec<u32> = pool.read(dst, N);
    assert!(out.iter().enumerate().all(|(i, v)| *v == i as u32 + 100), "{:?}", &out[..4]);
}

/// A queue's Wait for a value another queue's Signal will reach once its work has run: the
/// waiting queue's list reads what the signalling queue's list wrote.
#[test]
fn a_queue_waits_for_another_queues_signal() {
    if !available() {
        return;
    }
    let pool = pool(9009, 4);
    let (src, mid, dst) = (0, 8192, 16384);
    pool.write(src, &(0..N as u32).collect::<Vec<_>>());
    pool.write(mid, &[0u32; N]);
    pool.write(dst, &[0u32; N]);
    let pso = pipeline();
    let (q1, q2) = (Queue::new(), Queue::new());
    let (between, done) = (Fence::new(0), Fence::new(0));
    let first = List::new(pso);
    affine(&first, 2, 1, pool.va(src), pool.va(mid));
    first.close();
    q1.execute(&[&first]);
    q1.signal(&between, 3);
    q2.wait(&between, 3);
    let second = List::new(pso);
    affine(&second, 3, 0, pool.va(mid), pool.va(dst));
    second.close();
    q2.execute(&[&second]);
    q2.signal(&done, 1);
    assert_eq!(done.wait(1), S_OK);
    let out: Vec<u32> = pool.read(dst, N);
    assert!(out.iter().enumerate().all(|(i, v)| *v == (i as u32 * 2 + 1) * 3), "{:?}", &out[..4]);
}

/// Views a title writes (`CreateShaderResourceView` of a buffer resource it made at an address of
/// its own memory, `CreateUnorderedAccessView`) into a shader-visible heap reach the shader
/// through `SetDescriptorHeaps` and a root descriptor table.
#[test]
fn buffer_views_in_a_descriptor_heap_reach_the_shader() {
    if !available() {
        return;
    }
    let pool = pool(9004, 4);
    let (src, dst) = (0, 16384);
    pool.write(src, &(0..64u32).collect::<Vec<_>>());
    pool.write(dst, &[0u32; 64]);
    let heap = crate::descriptor::test_heap(false, 8);
    let start = slot::<extern "system" fn(This) -> u64>(heap, 0x48)(heap);
    let (rs, rd) = (
        crate::resource::test_buffer_at(pool.va(src), 4096),
        crate::resource::test_buffer_at(pool.va(dst), 4096),
    );
    // SRV words: format, buffer dimension, mapping, pad, first element (8 bytes), count, stride,
    // flags (raw); UAV words: format, dimension, first element, count, stride, counter, flags.
    let mut srv = [0u32; 10];
    (srv[1], srv[6], srv[8]) = (1, 64, 1);
    let mut uav = [0u32; 10];
    (uav[1], uav[4], uav[8]) = (1, 64, 1);
    crate::descriptor::create_srv(std::ptr::null(), rs, &srv, start);
    crate::descriptor::create_uav(std::ptr::null(), rd, std::ptr::null(), &uav, start + 32);

    let pso = crate::pipeline::test_compute(b"heap shader");
    glue::install_compute_pipeline(pso as usize, &read("tests/fixtures/heap_rw.dxil"), [64, 1, 1])
        .unwrap();
    let (q, f) = (Queue::new(), Fence::new(0));
    let list = List::new(pso);
    slot::<extern "system" fn(This, u32, *const This)>(list.0, 0x100)(list.0, 1, &heap);
    list.constants(0, &[3, 1]);
    slot::<extern "system" fn(This, u32, u64)>(list.0, 0x118)(list.0, 1, start);
    list.dispatch(1);
    list.close();
    q.execute(&[&list]);
    q.signal(&f, 1);
    assert_eq!(f.wait(1), S_OK);
    assert_eq!(
        pool.read::<u32>(dst, 64),
        (0..64).map(|i| i * 3 + 1).collect::<Vec<_>>()
    );
}

/// `CopyBufferRegion` between title buffers runs on the GPU in order with the list's dispatch
/// (the copy feeds it).
#[test]
fn a_copy_runs_in_order_with_the_dispatch_it_feeds() {
    if !available() {
        return;
    }
    let pool = pool(9005, 4);
    let (staged, src, dst) = (0, 16384, 32768);
    pool.write(staged, &(0..N as u32).collect::<Vec<_>>());
    pool.write(dst, &[0u32; N]);
    let (rstaged, rsrc) = (
        crate::resource::test_buffer_at(pool.va(staged), 8192),
        crate::resource::test_buffer_at(pool.va(src), 8192),
    );
    let (q, f) = (Queue::new(), Fence::new(0));
    let pso = pipeline();
    let list = List::new(pso);
    slot::<extern "system" fn(This, This, u64, This, u64, u64)>(list.0, 0x78)(
        list.0,
        rsrc,
        0,
        rstaged,
        0,
        (N * 4) as u64,
    );
    affine(&list, 1, 9, pool.va(src), pool.va(dst));
    list.close();
    q.execute(&[&list]);
    q.signal(&f, 1);
    assert_eq!(f.wait(1), S_OK);
    assert!(pool.read::<u32>(dst, N).iter().enumerate().all(|(i, v)| *v == i as u32 + 9));
}

/// A title pipeline the host refuses (here a container that does not translate) drops the work
/// that uses it and counts the refusal; the submission, the list's other work and the fence go on.
#[test]
fn a_refused_pipeline_drops_its_work_and_nothing_else() {
    if !available() {
        return;
    }
    let pool = pool(9006, 4);
    let (src, dropped, dst, flag) = (0, 8192, 16384, 24576);
    pool.write(src, &(0..N as u32).collect::<Vec<_>>());
    pool.write(dropped, &[0xAAAA_AAAAu32; N]);
    pool.write(dst, &[0u32; N]);
    let (bad, good) = (crate::pipeline::test_compute(b"not a container"), pipeline());
    let (q, f) = (Queue::new(), Fence::new(0));
    let list = List::new(bad);
    affine(&list, 1, 1, pool.va(src), pool.va(dropped));
    list.set_pipeline(good);
    affine(&list, 1, 2, pool.va(src), pool.va(dst));
    list.write32(pool.va(flag), 7);
    list.close();
    q.execute(&[&list]);
    q.signal(&f, 1);
    assert_eq!(f.wait(1), S_OK);
    let out: Vec<u32> = pool.read(dst, N);
    assert!(out.iter().enumerate().all(|(i, v)| *v == i as u32 + 2));
    assert_eq!(pool.read::<u32>(flag, 1), [7]);
    assert_eq!(pool.read::<u32>(dropped, 1), [0xAAAA_AAAA], "the refused pipeline's dispatch did not run");
    let stats = glue::shader_stats().unwrap();
    assert!(
        stats.refused.iter().any(|(why, n)| why.contains("translation") && *n >= 1),
        "{stats:?}"
    );
}
