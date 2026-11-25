# Native Metal backend

Design notes for running `victory-gpu` on Metal with no Wine, and the results of the first
prototype (`crates/victory-metal`). Measured on an Apple M5 Pro, macOS 27.2, 48 GB, 16 KiB pages.
Nothing here changes the D3D12 path.

## Why

Under Wine and D3DMetal most of a frame goes to the layering, not the GPU work:

- the host GPU cannot read title memory, so `shadow.rs` keeps host copies of title buffers, `watch.rs`
  write-guards title pages (64 KiB blocks, vectored fault handler) to know what changed, `memory.rs`
  and `texture.rs` refresh copies before each submission, and shader writes are copied back
  (`Shadow::copy_back_page`, the page bitmaps of `victory_shader::emit` "Title memory");
- `host.rs` waits for every submission (`shortcut: every submission is waited for`), 17-35 per
  frame, at about 5-8 ms each.

Apple Silicon and the console both have one memory the CPU and GPU share, which is what the title
already assumes ("a resource's GPU address is its CPU address"). On Metal that can be literally true.

## 1. D3D12 concepts onto Metal

| victory-gpu (D3D12.X on host D3D12) | Metal |
|---|---|
| `host.rs`: one device, one direct queue | one `MTLDevice`, one or more `MTLCommandQueue` |
| `queue.rs`, `hold.rs`: queues, Signals, memory-value waits across queues | `MTLCommandQueue`s; Signal/Wait on `MTLSharedEvent`, or `MTLEvent` within one device. Waits on memory values (`Wait32BitValueX`, a graphics list writes the word) stay a host-side hold (`hold.rs` logic) or become shared-event signals when both lists are known |
| `list.rs`: command allocator + list, recorded as the title calls methods | `MTLCommandBuffer` per submission segment; draws and dispatches go into one `MTLRenderCommandEncoder` / `MTLComputeCommandEncoder` per pass. Allocators disappear (command buffers are transient). A list cut at a wait (`hold.rs`) is two command buffers |
| `ExecuteCommandLists` + wait | `commit()` with no wait; completion by `addCompletedHandler` or a shared-event value |
| `fence.rs`: 64-bit value, events | `MTLSharedEvent`: `encodeSignalEvent`, `encodeWaitForEvent`, CPU `waitUntilSignaledValue`, or `notifyListener` instead of a wait |
| root signature (`root.rs`, `pipeline/host.rs`: per-stage root constants, root views, static samplers, directly indexed heaps) | the converter's top-level argument buffer, bound at buffer slot 2: root constants inline, root SRV/UAV/CBV as 64-bit GPU addresses, descriptor tables as GPU addresses of table memory. Static samplers are baked into the pipeline |
| descriptor heaps (`descriptor.rs`, host heap, `descriptor::host`) | a descriptor heap is an `MTLBuffer` of fixed-size entries (GPU address, texture resource ID, metadata, written with the converter runtime's `IRDescriptorTableSet*` helpers), bound at slot 0; samplers likewise at slot 1 (argument-buffer sampler IDs). `ResourceDescriptorHeap[i]` in the DXIL is then an index into that buffer. Heap-wide residency by `useResource` / `MTLResidencySet` |
| committed/placed resources, buffers (`resource.rs`) | buffers: `MTLBuffer` over title memory (below). Textures: `MTLTexture` from `MTLHeap` (replaces `placement.rs`) |
| title GPU address = CPU address | a constant or table offset from title address to GPU address (below) |
| pipeline states (`pipeline.rs`, `pipeline/*`): made at first draw, ~60,000 created, dozens drawn | same laziness. Compute: `MTLComputePipelineState` from the converted function. Graphics: `MTLRenderPipelineState`, with the stage-in function, blending and render-target formats the converter needs at link time; mesh shaders (`ms_6_6`) map to Metal mesh pipelines |
| `shader_cache.rs` (translated DXIL by content hash) | cache the converted `.metallib` next to it; conversion measured at 8-55 ms per shader |
| `twin.rs` (depth also written by UAV: colour twin) | Metal textures allow `shaderWrite` on depth-format views only through a pixel-format view; likely still needs the twin until checked on real shaders |
| `query.rs` | `MTLCounterSampleBuffer` / visibility result buffer |
| `signature.rs` (ExecuteIndirect) | indirect command buffers (`MTLIndirectCommandBuffer`), or compute-generated arguments |
| present | `CAMetalLayer` |

What is not a mapping but a gain: the per-draw "dynamic slots" of the host heap, the per-submission
refresh, and the wait after every submission exist to feed a GPU that cannot see title memory. They
go (next section).

Submission cost, measured with the probe (below): commit + wait of an empty command buffer is
about 14 us; with a shared-event wait, 14 us; a tiny dispatch end to end about 140 us, but the same
dispatches pipelined (35 command buffers, one wait at the end) cost 10 us each. The ~5-8 ms per
submission under Wine does not exist here; what remains is that the **dependency structure** (one
wait per submission) should still go, because a 140 us round trip times 35 is 5 ms.

## 2. Title memory as GPU memory, with no copy

`newBufferWithBytesNoCopy(pointer:length:options:deallocator:)` makes an `MTLBuffer` whose storage
is the caller's memory. With `StorageModeShared` the buffer's `contents()` is the pointer given, the
GPU reads and writes the same pages, and there is no copy and no synchronisation call
(`didModifyRange` is for managed buffers on discrete GPUs; not needed).

### What the prototype shows (`[1]`-`[4b]` of `cargo run --release -p victory-metal`)

- `buffer.contents()` equals the host pointer, byte for byte.
- A CPU write made after the buffer was created is seen by the next dispatch, with no call in between;
  a GPU write is seen through the host pointer after completion. Checked on 64 K words, twice.
- The same through raw GPU addresses (`buffer.gpuAddress()` passed in a constant struct, `useResource`
  for residency): the shape bindless access takes.
- **One set of physical pages, two virtual addresses**: memory allocated with `mach_vm_allocate` and
  aliased with `mach_vm_remap` (shared, not copied), buffer wrapped over the alias; a GPU write is seen
  at the other address. So the title's addresses and the Metal-facing mapping can be different
  mappings of the same physical pages.
- Several buffers over one range, and sub-range buffers inside a bigger one, are accepted.
- `maxBufferLength` is 28 753 MiB here (of 48 GB); a 16 GiB untouched reservation wraps in 0.5 ms,
  32 GiB is rejected. The console's 16 GB fits in one buffer.

### Constraints

- **Alignment.** Apple documents that the pointer must be page-aligned (16 KiB on Apple Silicon) and the
  length a multiple of it. This macOS/M5 build accepted an unaligned pointer and any length and the
  memory was still shared (CPU wrote 100, GPU added one, CPU read 101). Do not rely on that; page-align
  both. Consequence for the host: **every mapping the GPU may touch is 16 KiB granular**, finer than the
  console's 4 KiB small pages only if the title asks for them; 64 KiB large pages are fine.
- **Lifetime.** With no deallocator the memory must stay mapped until the buffer and every command
  buffer that used it have finished. Unmapping title memory the GPU still reads is the title's bug on
  the console, and a GPU fault here.
- **Size.** One buffer is at most `maxBufferLength` (28.7 GiB here, device dependent); larger memory
  takes several buffers (the shader then needs a table, below).
- **Residency / first use.** Metal wires the pages when a command buffer first uses the buffer, once:
  a dispatch binding a fresh 4 GiB touched buffer took 34 ms the first time and 0.2 ms after; 8 GiB took
  68 ms; 16 GiB untouched 281 ms. About 8-17 ms per GiB, once. So wrap the title's pools **once at
  start-up** and prime them with a dummy dispatch; never create buffers per frame. A
  `MTLResidencySet` on the queue keeps them resident without a `useResource` per encoder.
- **Same address for CPU and GPU: no.** The GPU address (`gpuAddress`) is chosen by Metal
  (`0x10000080000` against a host pointer `0x1135a0000`). The title's address is the CPU address, so
  each region needs `gpu = title + delta`, with one delta per buffer. `useResource` / argument buffers
  take the GPU address; the table that does this is smaller than what exists now.
- Not yet tested: memory from `mach_vm_map` of a memory entry (what the host will create), executable
  or guard-page regions inside a wrapped range (a guard page inside the range must be mapped, or the GPU
  faults if a draw reaches it), GPU access while the CPU holds the same cache line through the aliased
  mapping (the same physical memory, so coherent, but unmeasured under contention).

### What it removes from victory-gpu

| Today | With zero-copy |
|---|---|
| `shadow.rs` (1 828 lines): a persistent host buffer per title buffer a draw uses, ranges refreshed per submission | gone: the title buffer is the GPU buffer, vertex/index/constant data are read in place (`setVertexBuffer` at `title_addr - base`) |
| `watch.rs` (1 677 lines): read-only guards, vectored fault handler, "pages the title wrote since taken", alias tracking, `title_memory_remapped` | gone: nothing needs to know what the title wrote. (The Wine build already runs "nothing tracked" under Rosetta, `fault_tracking`.) The host still tells us about map/unmap, below |
| copy-back: `Shadow::copy_back_page`, the `memory_writes` page bitmap (256 KiB per list), `view_marks`, `VIEW_WRITES` bitmaps, atomic locks for 64-bit image atomics via buffer | gone from the shader contract: shader writes land in title memory. 64-bit image atomics still need a Metal-side answer (atomic on a 2xR32 view is not a Metal image op) |
| `memory.rs` "Title memory at raw addresses": page table of heap indices of raw SRV/UAV pairs of copies, rewritten when copies come and go | simpler: the table entry is an offset into one big raw buffer (or `{buffer, delta}`), or with one contiguous arena, `addr - base` and no table |
| `hold.rs`/`host.rs` wait after every submission to make writes visible | not needed: writes are visible when the command buffer completes; queue dependencies use events, many submissions in flight |
| `texture.rs` detile into staging, then copy to the host texture, per changed page | still needs detiling (the console's tiled layout is not Apple's), but as a **compute detile reading title memory directly** and writing the `MTLTexture`; the CPU staging buffer and compare-per-submission go |
| `placement.rs` heaps | `MTLHeap` |

### Interface needed from the native host agent

The host reserves title memory; the GPU layer needs, for each **physical-page pool** (one mach VM
object per pool, as agreed):

1. `base: *mut u8` and `len`, **page-aligned to 16 KiB**, the pool's mapping in the host process, plus the
   title VA it is the primary mapping of. If the title maps a pool's pages at several addresses (it does:
   "physical pages may be mapped at several addresses"), one mapping per pool that is **permanent for the
   process lifetime** is what we wrap; the other addresses are `mach_vm_remap` aliases (or the same
   memory entry mapped again) and need no GPU buffer.
2. The mach memory entry port, if the pool was made with `mach_make_memory_entry_64`, so we can map our own
   GPU-facing alias without asking for a remap (or the permission to `mach_vm_remap` from the pool address).
3. Map/unmap events, as `title_memory_remapped` is today: `map(title_va, len, pool, pool_offset)` and
   `unmap(title_va, len)`, called before the title can use the address. The GPU layer keeps the
   `title page -> (pool buffer, offset)` table from these; no write tracking.
4. A guarantee that **a pool's address range is never unmapped or `mprotect`ed to no-access while the
   process runs**, and that placeholders (reserved, nothing mapped) are not inside a pool.
5. Ideally one contiguous range for all GPU-visible pools (an arena of up to ~28 GiB): then the shader
   address is `title_va - arena_base` for a single raw buffer view and the page table in
   `victory_shader::emit` becomes one subtraction. If the title VA layout is sparse, make the arena a
   second, contiguous `mach_vm_remap` alias of the pools and keep a per-64 KiB-page offset table.

## 3. Shader path

`victory-shader` already ends at DXIL (an `SM 6.6` container: heap-indexed resources, wave ops, mesh
shaders), and `victory-gpu` hands that to D3D12. Two ways to reach Metal:

**A. Metal Shader Converter (MSC) on the DXIL we already emit.** Apple's `metal-shaderconverter` /
`libmetalirconverter`: DXIL container in, `.metallib` out. Supports SM 6.0-6.6 (wave intrinsics, 64-bit
ints, ray tracing, mesh and amplification shaders, dynamic resources), version 4.0 beta is current. It
is what D3DMetal is built on, so it is the path that handles D3D12 semantics (root signatures,
descriptor heaps, resource binding model) on Metal.

**B. Emit MSL or AIR from victory-shader.** A second back end next to `emit/`: no external dependency, full
control (title memory loads as real 64-bit pointers, 64-bit image atomics, no page table). But all the
D3D12 semantics MSC already implements (binding model, descriptor heaps, UAV barriers, wave semantics, mesh
pipelines) must be re-done, MSL needs the `metal` compiler at run time (or AIR emitted by hand, an
undocumented LLVM dialect), and every shader path would be tested again.

**Recommendation: A first, B later and narrowly.** The DXIL emitter is the product of a long effort and
is tested; MSC takes it as it is. Prototype results:

- It converted 5 of 6 DXIL containers left in an earlier session's scratch dump (they look like victory-shader output for title shaders, not regenerated here; all compute,
  11-127 KB, 8-54 ms each), and Metal loaded and built a compute pipeline from every converted library.
- The sixth failed with `IRError` code 6 (probably `UnsupportedWaveSize`, not confirmed against the
  header); to look at next, with the wave size the emitter requests (GFX10 wave32/wave64).
- An end-to-end shader (HLSL compiled to DXIL by dxc, `cs_6_0`, root signature `RootConstants(2),
  SRV(t0), UAV(u0)`) converted and ran over a `bytesNoCopy` buffer: root SRV and UAV are 64-bit GPU
  addresses in the top-level argument buffer, root constants inline. 4096 of 4096 outputs correct
  (`victory-metal --demo`). That is the no-copy path for shaders: a root view *is* a title address plus a
  delta.
- Later, B can replace MSC for the hot or unsupported pieces (page table loads, 64-bit atomics) without
  changing the host side.

### Obtaining MSC

Not on PATH; it is a separate Apple download, not part of Xcode or the Metal toolchain
(`xcrun --find metal` finds the compiler, not `metal-shaderconverter`).
Download "Metal shader converter" (4.0 beta at the time of writing, macOS and Windows, with a PDF
reference) from `developer.apple.com/metal/shader-converter/` (links to
`developer.apple.com/download/all/?q=metal%20shader%20converter`, an Apple ID is needed). It installs
`metal-shaderconverter`, `libmetalirconverter.dylib` and the headers `metal_irconverter.h` and
`metal_irconverter_runtime.h`. Requires macOS 13 and Xcode 15 or later; the output requires argument
buffers tier 2 (every Apple Silicon Mac).

On this machine a copy of the library is already present inside the D3DMetal framework bundled with
GPTK/CrossOver (`.../D3DMetal.framework/Versions/A/Resources/libmetalirconverter.dylib`, with
`libdxcompiler.dylib`; version.plist 4.0b3). It is **x86_64 only**, so a native arm64 process cannot load
it; the prototype was built for `x86_64-apple-darwin` and run under Rosetta to use it. For the real
backend install Apple's download (it is a universal macOS binary, expected, not checked here). The
library's licence is Apple's: do not commit it or its headers; load it at run time (the prototype
`dlopen`s `VICTORY_MSC_DYLIB` and declares the half-dozen C functions itself) and keep the path in the
config. Conversion could also run offline in `victory translate --cache`, caching `.metallib`s.

The converter header was not available to read, so `crates/victory-metal/src/msc.rs` declares the C
signatures from Apple's published sample; the enum values (stage 8 = compute) come from observation.

## Prototype: `crates/victory-metal`

macOS-only (target-gated dependencies and modules; builds as an empty binary on the Windows targets, so
the D3D12 workspace and its clippy gate are untouched). `objc2-metal` 0.3, `libc`.

```
cargo run --release -p victory-metal                       # memory + latency probe
# DXIL conversion (libmetalirconverter is x86_64 in GPTK; Apple's download should run natively):
cargo build --release -p victory-metal --target x86_64-apple-darwin
R=.../D3DMetal.framework/Versions/A/Resources
VICTORY_MSC_DYLIB=$R/libmetalirconverter.dylib ./target/x86_64-apple-darwin/release/victory-metal shader.dxil...
VICTORY_MSC_DYLIB=$R/libmetalirconverter.dylib VICTORY_DXC_DYLIB=$R/libdxcompiler.dylib \
  ./target/x86_64-apple-darwin/release/victory-metal --demo
```

Submit-to-complete latency (3000 iterations, release, native arm64; Wine/D3D12 today ~5000-8000 us):

| | min | p50 | p99 |
|---|---|---|---|
| empty command buffer, commit + `waitUntilCompleted` | 9.7 us | 14.2 us | 20.4 us |
| tiny compute dispatch (64 threads), same | 66.6 us | 137.5 us | 243.4 us |
| empty command buffer + `MTLSharedEvent` signal, CPU `waitUntilSignaledValue` | 6.0 us | 13.9 us | 19.4 us |
| per dispatch command buffer, 17 in flight, one event wait | 9.9 us | 14.8 us | 25.2 us |
| per dispatch command buffer, 35 in flight, one event wait | 8.6 us | 9.9 us | 12.5 us |
| per dispatch command buffer, 256 in flight, one event wait | 8.0 us | 8.7 us | 9.7 us |

GPU time of the tiny dispatch is 2 us, so the 140 us is wake-up and completion latency, which
pipelining removes: a frame of 35 submissions with one wait is about 0.35 ms of submission overhead, not
175-280 ms.

## Next steps

1. Get Apple's MSC 4.0 and run native arm64: confirm the universal dylib, drop the Rosetta build.
2. Find why one of six shader containers fails (code 6) and run the rest of the victory-shader test
   corpus through MSC with a compute PSO build; list what does not convert.
3. Add the host interface above to the native-host agent's work, then wrap real pools and re-run the
   probe on them (memory entry mappings, guard pages, 64 KiB mapping granularity on 16 KiB pages).
4. A `victory-gpu` backend trait around `host.rs`, `queue.rs`, `fence.rs`: the title-facing
   objects stay, the host calls go to D3D12 or Metal. Start with queues, fences (`MTLSharedEvent`),
   and compute dispatch of the converted shaders, since compute (GTA VI's four compute queues) needs no
   render pipeline.
5. Descriptor heaps: the title writes 32-byte RDNA descriptors itself; build the MSC-format heap from
   them (the existing `HostHeap` does this for D3D12), then bind slot 0/1/2.
6. Replace the page-table "Title memory" contract in `victory_shader::emit` by the arena/offset one,
   and delete copy-back, view marks and the shadow (`shadow.rs`, `watch.rs`) behind a feature once the
   Metal path runs.
7. Compute detile of the console texture layouts reading title memory; render pipelines (stage-in,
   blend, mesh); present on `CAMetalLayer`; then pipeline many submissions with events instead of waits.
