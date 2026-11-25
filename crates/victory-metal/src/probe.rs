use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;
use std::ptr::NonNull;
use std::time::Instant;
use victory_metal::mem::{Alias, Region, page_size};

type Dev = Retained<ProtocolObject<dyn MTLDevice>>;
type Buf = Retained<ProtocolObject<dyn MTLBuffer>>;
type Pso = Retained<ProtocolObject<dyn MTLComputePipelineState>>;
type Queue = Retained<ProtocolObject<dyn MTLCommandQueue>>;

const MSL: &str = r#"
#include <metal_stdlib>
using namespace metal;
kernel void xform(device const uint* in [[buffer(0)]], device uint* out [[buffer(1)]],
                  uint i [[thread_position_in_grid]]) { out[i] = in[i] * 2u + 1u; }
// Bindless style: the shader gets raw GPU addresses (what a root signature / descriptor heap becomes).
struct Ptrs { device const uint* in; device uint* out; };
kernel void xform_ptr(constant Ptrs& p [[buffer(0)]], uint i [[thread_position_in_grid]]) { p.out[i] = p.in[i] * 3u; }
kernel void nop(device uint* o [[buffer(0)]], uint i [[thread_position_in_grid]]) { if (i == 0) o[0] += 1u; }
"#;

/// Wraps `ptr..ptr+len` without copying. `None` if Metal refuses (alignment, size).
pub fn wrap(dev: &Dev, ptr: *mut u8, len: usize) -> Option<Buf> {
    let p = NonNull::new(ptr.cast())?;
    // SAFETY: the caller keeps the memory mapped for the buffer's lifetime (no deallocator).
    unsafe {
        dev.newBufferWithBytesNoCopy_length_options_deallocator(
            p,
            len,
            MTLResourceOptions::StorageModeShared,
            None,
        )
    }
}

pub fn bind(
    e: &ProtocolObject<dyn MTLComputeCommandEncoder>,
    b: Option<&Buf>,
    off: usize,
    idx: usize,
) {
    // SAFETY: offset is inside the buffer in every use here.
    unsafe { e.setBuffer_offset_atIndex(b.map(|b| &**b), off, idx) }
}

fn pso(dev: &Dev, lib: &Retained<ProtocolObject<dyn MTLLibrary>>, name: &str) -> Pso {
    let f = lib
        .newFunctionWithName(&NSString::from_str(name))
        .expect("function");
    dev.newComputePipelineStateWithFunction_error(&f)
        .expect("pso")
}

fn dispatch(
    q: &Queue,
    p: &Pso,
    n: usize,
    setup: impl FnOnce(&ProtocolObject<dyn MTLComputeCommandEncoder>),
) {
    let cb = q.commandBuffer().expect("cb");
    let e = cb.computeCommandEncoder().expect("enc");
    e.setComputePipelineState(p);
    setup(&e);
    e.dispatchThreads_threadsPerThreadgroup(
        MTLSize {
            width: n,
            height: 1,
            depth: 1,
        },
        MTLSize {
            width: 64.min(n),
            height: 1,
            depth: 1,
        },
    );
    e.endEncoding();
    cb.commit();
    cb.waitUntilCompleted();
    assert!(cb.error().is_none(), "{:?}", cb.error());
}

fn stats(label: &str, mut us: Vec<f64>) {
    us.sort_by(|a, b| a.total_cmp(b));
    let n = us.len();
    let mean = us.iter().sum::<f64>() / n as f64;
    println!(
        "  {label:<46} min {:7.1}  p50 {:7.1}  p99 {:7.1}  mean {:7.1} us  (n={n})",
        us[0],
        us[n / 2],
        us[n * 99 / 100],
        mean
    );
}

pub fn run() {
    let dev = MTLCreateSystemDefaultDevice().expect("no Metal device");
    let pg = page_size();
    println!("device: {}", dev.name());
    println!(
        "unified memory: {}  page: {} KiB  maxBufferLength: {} MiB  recommendedMaxWorkingSet: {} MiB",
        dev.hasUnifiedMemory(),
        pg / 1024,
        dev.maxBufferLength() / (1 << 20),
        dev.recommendedMaxWorkingSetSize() / (1 << 20)
    );
    let q = dev.newCommandQueue().expect("queue");
    let lib = dev
        .newLibraryWithSource_options_error(&NSString::from_str(MSL), None)
        .expect("msl compile");
    let (xform, xform_ptr, nop) = (
        pso(&dev, &lib, "xform"),
        pso(&dev, &lib, "xform_ptr"),
        pso(&dev, &lib, "nop"),
    );

    // ---- 1. wrap a page-aligned host allocation, no copy ----
    const N: usize = 1 << 16; // uints per half
    let half = (N * 4).next_multiple_of(pg);
    let region = Region::new(2 * half, None).expect("mmap");
    let buf = wrap(&dev, region.ptr, region.len).expect("bytesNoCopy on aligned region");
    println!("\n[1] zero-copy wrap");
    println!(
        "  host ptr {:p}  buffer.contents() {:p}  same: {}  gpuAddress 0x{:x}",
        region.ptr,
        buf.contents(),
        std::ptr::eq(buf.contents().as_ptr().cast::<u8>(), region.ptr),
        buf.gpuAddress()
    );
    let host = region.ptr.cast::<u32>();
    let inp = host;
    // SAFETY: the region is 2*half bytes; `out` starts at the second half.
    let out = unsafe { region.ptr.add(half).cast::<u32>() };
    // SAFETY: indices are < N within the mapped halves; GPU is idle between dispatches.
    let fill =
        |seed: u32| unsafe { (0..N).for_each(|i| inp.add(i).write_volatile(i as u32 ^ seed)) };
    // SAFETY: as for `fill`; reads only.
    let check = |f: &dyn Fn(u32) -> u32, seed: u32| unsafe {
        (0..N).all(|i| out.add(i).read_volatile() == f(i as u32 ^ seed))
    };

    // CPU writes made after buffer creation are visible to the GPU; GPU writes visible to the CPU.
    fill(0);
    dispatch(&q, &xform, N, |e| {
        bind(e, Some(&buf), 0, 0);
        bind(e, Some(&buf), half, 1);
    });
    println!(
        "  GPU out = in*2+1 after CPU wrote `in` post-creation, read via host pointer: {}",
        check(&|x| x * 2 + 1, 0)
    );
    fill(0xdead);
    dispatch(&q, &xform, N, |e| {
        bind(e, Some(&buf), 0, 0);
        bind(e, Some(&buf), half, 1);
    });
    println!(
        "  CPU rewrote `in` again, no sync call, GPU saw it: {}",
        check(&|x| x * 2 + 1, 0xdead)
    );

    // Bindless: raw GPU addresses in a constant struct, no setBuffer; residency via useResource.
    let ptrs: [u64; 2] = [buf.gpuAddress(), buf.gpuAddress() + half as u64];
    dispatch(&q, &xform_ptr, N, |e| {
        // SAFETY: `ptrs` is 16 bytes and outlives the call; setBytes copies it.
        unsafe { e.setBytes_length_atIndex(NonNull::new(ptrs.as_ptr() as *mut _).unwrap(), 16, 0) };
        e.useResource_usage(
            ProtocolObject::from_ref(&*buf),
            MTLResourceUsage::Read | MTLResourceUsage::Write,
        );
    });
    println!(
        "  bindless (GPU addresses via setBytes + useResource) out = in*3: {}",
        check(&|x| x * 3, 0xdead)
    );

    // ---- 2. constraints ----
    println!("\n[2] constraints");
    let probe = |what: &str, off: usize, len: usize| {
        // SAFETY: offsets stay inside the mapping; Metal only inspects the pointer value.
        let p = unsafe { region.ptr.add(off) };
        match wrap(&dev, p, len) {
            Some(b) => println!(
                "  {what}: accepted, length {} contents==ptr {}",
                b.length(),
                std::ptr::eq(b.contents().as_ptr().cast::<u8>(), p)
            ),
            None => println!("  {what}: REJECTED (nil)"),
        }
    };
    probe("ptr+4096 (4K-aligned, not 16K)", 4096, pg);
    probe("ptr+16 (16-byte aligned only)", 16, pg);
    probe("ptr+1", 1, pg);
    probe("length pg+1", 0, pg + 1);
    probe("length 4096", 0, 4096);
    probe("second buffer over same pages", 0, pg);
    probe("sub-range at +pg", pg, pg);

    // Is the unaligned buffer really the same memory, or a silent copy? Bump word 0 on the GPU, read it on the CPU.
    // SAFETY: +16 is inside the mapping and 4-byte aligned.
    let (p16, w) = unsafe { (region.ptr.add(16), region.ptr.add(16).cast::<u32>()) };
    if let Some(b) = wrap(&dev, p16, 4096) {
        // SAFETY: as above; GPU idle.
        unsafe { w.write_volatile(100) };
        dispatch(&q, &nop, 64, |e| bind(e, Some(&b), 0, 0));
        // SAFETY: as above.
        let v = unsafe { w.read_volatile() };
        println!(
            "  unaligned (+16) buffer: CPU wrote 100, GPU did +=1, CPU reads {v} (101 = same memory)"
        );
    }

    // Two virtual aliases of one VM object: the title's address and a Metal-facing address.
    println!("\n[3] one physical page set, two virtual addresses (mach_vm_remap, shared)");
    match Alias::new(2 * half) {
        Ok(a) => {
            let ab = wrap(&dev, a.alias, a.len).expect("wrap alias");
            // SAFETY: the second half is inside the alias mapping.
            let (ain, aout) = (a.primary.cast::<u32>(), unsafe {
                a.primary.add(half).cast::<u32>()
            });
            // SAFETY: both ranges are mapped for the Alias's lifetime.
            unsafe { (0..N).for_each(|i| ain.add(i).write_volatile(i as u32 + 7)) };
            dispatch(&q, &xform, N, |e| {
                bind(e, Some(&ab), 0, 0);
                bind(e, Some(&ab), half, 1);
            });
            // SAFETY: as above.
            let ok =
                unsafe { (0..N).all(|i| aout.add(i).read_volatile() == (i as u32 + 7) * 2 + 1) };
            println!(
                "  title addr {:p}, Metal buffer wraps alias {:p}; GPU write seen at title addr: {ok}",
                a.primary, a.alias
            );
        }
        Err(r) => println!("  mach_vm_remap failed: {r}"),
    }

    println!("\n[4] max wrap size (untouched virtual reservations)");
    for gib in [1usize, 4, 8, 16, 32, 64] {
        let len = gib << 30;
        let t = Instant::now();
        match Region::new(len, None) {
            None => println!("  {gib:>3} GiB: mmap failed"),
            Some(r) => match wrap(&dev, r.ptr, len) {
                Some(_) => println!(
                    "  {gib:>3} GiB: wrapped in {:.2} ms",
                    t.elapsed().as_secs_f64() * 1e3
                ),
                None => println!(
                    "  {gib:>3} GiB: REJECTED (maxBufferLength {} GiB)",
                    dev.maxBufferLength() >> 30
                ),
            },
        }
    }

    // Does a command buffer that names a huge buffer pay for it (page wiring)? First use vs later.
    println!("\n[4b] cost of a dispatch that binds a big buffer (first, second, third use)");
    for (gib, touch) in [(4usize, true), (8, true), (16, false)] {
        let len = gib << 30;
        let r = Region::new(len, None).unwrap();
        if touch {
            // SAFETY: the mapping is `len` bytes.
            unsafe { libc::memset(r.ptr.cast(), 1, len) };
        }
        let big = wrap(&dev, r.ptr, len).unwrap();
        let mut us = Vec::new();
        for _ in 0..3 {
            let t = Instant::now();
            dispatch(&q, &nop, 64, |e| bind(e, Some(&big), 0, 0));
            us.push(t.elapsed().as_secs_f64() * 1e6);
        }
        println!(
            "  {gib:>2} GiB {}: {:.0} / {:.0} / {:.0} us",
            if touch { "touched  " } else { "untouched" },
            us[0],
            us[1],
            us[2]
        );
    }

    // ---- latency ----
    println!(
        "\n[5] submit -> complete latency (commit + waitUntilCompleted), Wine/D3D12 baseline ~5000-8000 us"
    );
    let iters = 3000;
    let time = |f: &dyn Fn()| -> Vec<f64> {
        (0..200).for_each(|_| f());
        (0..iters)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1e6
            })
            .collect()
    };
    stats(
        "empty command buffer",
        time(&|| {
            let cb = q.commandBuffer().unwrap();
            cb.commit();
            cb.waitUntilCompleted();
        }),
    );
    let enc_dispatch = |wait: bool| {
        let cb = q.commandBuffer().unwrap();
        let e = cb.computeCommandEncoder().unwrap();
        e.setComputePipelineState(&nop);
        bind(&e, Some(&buf), 0, 0);
        e.dispatchThreads_threadsPerThreadgroup(
            MTLSize {
                width: 64,
                height: 1,
                depth: 1,
            },
            MTLSize {
                width: 64,
                height: 1,
                depth: 1,
            },
        );
        e.endEncoding();
        cb.commit();
        if wait {
            cb.waitUntilCompleted();
        }
    };
    stats(
        "tiny compute dispatch (64 threads)",
        time(&|| enc_dispatch(true)),
    );

    // MTLSharedEvent wait instead of waitUntilCompleted (the fence.rs model).
    let ev = dev.newSharedEvent().unwrap();
    let mut v = 0u64;
    let mut ev_us = Vec::new();
    for i in 0..iters + 200 {
        v += 1;
        let t = Instant::now();
        let cb = q.commandBuffer().unwrap();
        cb.encodeSignalEvent_value(ProtocolObject::from_ref(&*ev), v);
        cb.commit();
        assert!(ev.waitUntilSignaledValue_timeoutMS(v, 1000));
        if i >= 200 {
            ev_us.push(t.elapsed().as_secs_f64() * 1e6);
        }
    }
    stats("empty cb + MTLSharedEvent signal, CPU wait", ev_us);

    // Pipelined: many submissions, one wait at the end (what batching buys).
    for batch in [17usize, 35, 256] {
        let mut us = Vec::new();
        for _ in 0..200 {
            let t = Instant::now();
            for _ in 0..batch - 1 {
                enc_dispatch(false);
            }
            v += 1;
            let cb = q.commandBuffer().unwrap();
            cb.encodeSignalEvent_value(ProtocolObject::from_ref(&*ev), v);
            cb.commit();
            assert!(ev.waitUntilSignaledValue_timeoutMS(v, 1000));
            us.push(t.elapsed().as_secs_f64() * 1e6 / batch as f64);
        }
        stats(
            &format!("{batch} dispatch cbs, one event wait (per cb)"),
            us,
        );
    }
    // GPU-side time of one tiny dispatch, from the command buffer's own timestamps.
    let cb = q.commandBuffer().unwrap();
    let e = cb.computeCommandEncoder().unwrap();
    e.setComputePipelineState(&nop);
    bind(&e, Some(&buf), 0, 0);
    e.dispatchThreads_threadsPerThreadgroup(
        MTLSize {
            width: 64,
            height: 1,
            depth: 1,
        },
        MTLSize {
            width: 64,
            height: 1,
            depth: 1,
        },
    );
    e.endEncoding();
    cb.commit();
    cb.waitUntilCompleted();
    println!(
        "  (GPU execution of that dispatch, kernelStart..kernelEnd: {:.1} us)",
        (cb.GPUEndTime() - cb.GPUStartTime()) * 1e6
    );
}
