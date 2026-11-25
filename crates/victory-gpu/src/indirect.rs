//! Indirect execution's arguments, bounded before the GPU reads them. The
//! title's own compute passes write most of them; a pass we drop or
//! translate wrongly leaves whatever the memory held, and a draw of four
//! billion vertices or a dispatch past the host's limits runs until the
//! host resets the display driver. So every record is clamped on the GPU,
//! just before its execution, by a compute shader of ours (HLSL compiled by
//! the system's compiler; a 5.1 shader with its root signature inside).
//!
//! Each list also has a budget (2^27 vertex instances or threads, groups
//! counted as 64 threads, mesh groups as 128): records past it run empty,
//! so a list of thousands of garbage records cannot run for seconds
//! either.
//!
//! With a count buffer ExecuteIndirect runs only the first min(max, count)
//! records, so the clamp reads the count word and stops there: the maximum
//! can be far above the count, and the records between never run.
//!
//! shortcut: the bounds (2^24 vertices, 2^26 vertex instances, 65,535
//! groups per dimension and 2^20 per dispatch, one-dimensional mesh
//! dispatches, the budget) cut legitimate work beyond them; raise them if
//! the title needs more.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

use std::sync::atomic::{AtomicU64, Ordering};

use crate::host::{HostList, host, transition};

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), UAV(u0), UAV(u1), UAV(u2)"
cbuffer Record : register(b0) { uint kind; uint stride; uint records; uint counted; };
RWByteAddressBuffer args : register(u0);
// The list's work so far (vertex instances, threads): past the budget,
// records run empty.
RWByteAddressBuffer budget : register(u1);
// The title's record count: read only if `counted`.
RWByteAddressBuffer countWord : register(u2);
static const uint BUDGET = 1u << 27;
// Groups of one dispatch; more run empty.
static const uint GROUPS = 1u << 20;
// Clamps the record at `at` in place; its cost.
uint clamped(uint at) {
    uint cost;
    if (kind <= 1) {
        // Draws: vertex or index count, then instances.
        uint count = min(args.Load(at), 1u << 24);
        uint instances = min(args.Load(at + 4), (1u << 26) / max(count, 1u));
        cost = count * instances;
        args.Store2(at, uint2(count, instances));
    } else if (kind == 2) {
        uint3 g = min(args.Load3(at), uint3(65535, 65535, 65535));
        // x * y fits 32 bits; the whole count may not.
        if (g.z != 0 && g.x * g.y > GROUPS / g.z) {
            g = uint3(0, 0, 0);
        }
        cost = g.x * g.y * g.z * 64;
        args.Store3(at, g);
    } else {
        // Mesh dispatches: the console's are one-dimensional.
        uint3 g = args.Load3(at);
        g = uint3(min(g.x, 65535u), min(g.y, 1u), min(g.z, 1u));
        cost = g.x * g.y * g.z * 128;
        args.Store3(at, g);
    }
    return cost;
}
// A record past the budget (no cost reaches it).
static const uint PAST = 0xffffffffu;
groupshared uint costs[64];
// One group walks the records in tiles of 64: every lane clamps its own
// record, then the first lane charges the tile's records in order. No
// other group shares the budget: a shared add wraps once enough records
// are rejected, and a compare-exchange retries more the more records fit.
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint lane : SV_GroupIndex) {
    uint left = 0;
    if (lane == 0) {
        left = BUDGET - min(budget.Load(0), BUDGET);
    }
    // ExecuteIndirect runs min(max, count) records: no others are clamped or
    // charged. The loop still counts tiles up to `records`, a constant, as
    // the compiler cannot prove that a value read from memory is the same
    // in every lane, and barriers need that; tiles past `walked` only wait
    // at them.
    uint walked = records;
    if (counted != 0) {
        walked = min(records, countWord.Load(0));
    }
    for (uint tile = 0; tile < records / 64 + min(records % 64, 1u); tile++) {
        uint record = tile * 64 + lane;
        uint at = record * stride;
        uint cost = 0;
        if (record < walked) {
            cost = clamped(at);
        }
        costs[lane] = cost;
        GroupMemoryBarrierWithGroupSync();
        if (lane == 0 && tile * 64 < walked) {
            for (uint i = 0; i < 64; i++) {
                if (costs[i] > left) {
                    // The first record that does not fit takes what is
                    // left, so those after it run empty too.
                    costs[i] = PAST;
                    left = 0;
                } else {
                    left -= costs[i];
                }
            }
        }
        GroupMemoryBarrierWithGroupSync();
        if (costs[lane] == PAST) {
            // Draws keep their layout with no vertices; groups become zero.
            args.Store(at, 0);
        }
    }
    if (lane == 0) {
        budget.Store(0, BUDGET - left);
    }
}
"#;

struct Clamp {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    /// Built from [`WORDS_SOURCE`], which also reads the two buffers as
    /// words through root views 3 and 4, and the count through 5 (the
    /// count is view 3 otherwise).
    words: bool,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Clamp {}
// SAFETY: as above.
unsafe impl Sync for Clamp {}

fn clamp() -> Option<&'static Clamp> {
    static CLAMP: std::sync::OnceLock<Option<Clamp>> = std::sync::OnceLock::new();
    CLAMP
        .get_or_init(|| match make() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "no argument clamp: indirect execution dropped");
                None
            }
        })
        .as_ref()
}

fn make() -> windows::core::Result<Clamp> {
    make_fxc().or_else(|e| {
        tracing::info!(error = %e, "argument clamp: building the one-thread form");
        make_words()
    })
}

/// The clamp for Wine's HLSL compiler (vkd3d-shader), which has no `Load`
/// on `RWByteAddressBuffer`, no stores to indexed group-shared memory and
/// no `[RootSignature]`: [`SOURCE`]'s clamp, read through structured views
/// of the same buffers, and one thread charging the records in order
/// instead of a group of 64. The results are the same.
const WORDS_SOURCE: &str = r#"
cbuffer Record : register(b0) { uint kind; uint stride; uint records; uint counted; };
RWByteAddressBuffer args : register(u0);
RWByteAddressBuffer budget : register(u1);
RWStructuredBuffer<uint> argWords : register(u2);
RWStructuredBuffer<uint> budgetWords : register(u3);
RWStructuredBuffer<uint> countWords : register(u4);
static const uint BUDGET = 1u << 27;
static const uint GROUPS = 1u << 20;
uint3 load3(uint at) { return uint3(argWords[at / 4], argWords[at / 4 + 1], argWords[at / 4 + 2]); }
uint clamped(uint at) {
    uint cost;
    if (kind <= 1) {
        uint count = min(argWords[at / 4], 1u << 24);
        uint instances = min(argWords[at / 4 + 1], (1u << 26) / max(count, 1u));
        cost = count * instances;
        args.Store2(at, uint2(count, instances));
    } else if (kind == 2) {
        uint3 g = min(load3(at), uint3(65535, 65535, 65535));
        if (g.z != 0 && g.x * g.y > GROUPS / g.z) {
            g = uint3(0, 0, 0);
        }
        cost = g.x * g.y * g.z * 64;
        args.Store3(at, g);
    } else {
        uint3 g = load3(at);
        g = uint3(min(g.x, 65535u), min(g.y, 1u), min(g.z, 1u));
        cost = g.x * g.y * g.z * 128;
        args.Store3(at, g);
    }
    return cost;
}
[numthreads(1, 1, 1)]
void main() {
    uint left = BUDGET - min(budgetWords[0], BUDGET);
    uint walked = records;
    if (counted != 0) {
        walked = min(records, countWords[0]);
    }
    for (uint record = 0; record < walked; record++) {
        uint at = record * stride;
        uint cost = clamped(at);
        if (cost > left) {
            // The first record that does not fit takes what is left, so
            // those after it run empty too.
            args.Store(at, 0);
            left = 0;
        } else {
            left -= cost;
        }
    }
    budget.Store(0, BUDGET - left);
}
"#;

fn make_words() -> windows::core::Result<Clamp> {
    use crate::compute::Param;
    let params = [
        Param::Constants { register: 0, count: 4 },
        Param::Uav { register: 0 },
        Param::Uav { register: 1 },
        Param::Uav { register: 2 },
        Param::Uav { register: 3 },
        Param::Uav { register: 4 },
    ];
    let (root, pso) = crate::compute::pipeline(WORDS_SOURCE, s!("clamp"), 0, &params)?;
    Ok(Clamp { root, pso, words: true })
}

fn make_fxc() -> windows::core::Result<Clamp> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("clamp"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize())
            };
            tracing::warn!("argument clamp: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes; the root signature is inside the shader.
    unsafe {
        let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Clamp { root, pso, words: false })
    }
}

/// Records, in `list`, the clamp of `records` records of `kind` (PC
/// argument numbering) `stride` bytes apart at host address `gpu` in
/// `buffer`, and leaves the buffer ready to be read as arguments. Replaces
/// the list's compute root signature and pipeline (callers bind theirs
/// after). False if the clamp is not available.
///
/// `count` is the host address of the title's record count: the clamp then
/// stops at the lesser of it and `records`, which is where ExecuteIndirect
/// stops. A count that is not 4-byte aligned, or that lies among the
/// records (the clamp writes them, so ExecuteIndirect would read it
/// changed), is not read: every record is clamped.
pub(crate) fn bound(
    list: &HostList,
    buffer: &ID3D12Resource,
    gpu: u64,
    (kind, stride, records): (u32, u32, u32),
    count: Option<u64>,
) -> bool {
    let (Some(c), Some(budget)) = (clamp(), list.budget()) else {
        return false;
    };
    if !gpu.is_multiple_of(4) || !stride.is_multiple_of(4) {
        return false;
    }
    let end = gpu.saturating_add(u64::from(stride) * u64::from(records));
    let count = count.filter(|at| at.is_multiple_of(4) && (*at >= end || at + 4 <= gpu));
    RECORDS[0].fetch_add(u64::from(records), Ordering::Relaxed);
    if count.is_some() {
        RECORDS[1].fetch_add(u64::from(records), Ordering::Relaxed);
    }
    let l = &list.list;
    let constants = [kind, stride, records, u32::from(count.is_some())];
    // SAFETY: an open list; our root signature, pipeline and a live buffer
    // the caller's list holds.
    unsafe {
        // Earlier shader writes (the title's argument passes) first.
        l.ResourceBarrier(&[uav_barrier()]);
        l.SetComputeRootSignature(&c.root);
        l.SetPipelineState(&c.pso);
        l.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
        l.SetComputeRootUnorderedAccessView(1, gpu);
        l.SetComputeRootUnorderedAccessView(2, budget);
        // Set even when not read: a root view has to be set.
        let count = count.unwrap_or(gpu);
        if c.words {
            l.SetComputeRootUnorderedAccessView(3, gpu);
            l.SetComputeRootUnorderedAccessView(4, budget);
            l.SetComputeRootUnorderedAccessView(5, count);
        } else {
            l.SetComputeRootUnorderedAccessView(3, count);
        }
        // One group walks every record (up to the count).
        l.Dispatch(records.min(1), 1, 1);
        l.ResourceBarrier(&[
            uav_barrier(),
            transition(
                buffer,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            ),
        ]);
    }
    true
}

/// Records the clamps were asked to walk (the maximum of each execution)
/// since the last call, and the part of them under a count buffer, where
/// the walk stops at the count.
pub(crate) fn take_records() -> [u64; 2] {
    RECORDS.each_ref().map(|n| n.swap(0, Ordering::Relaxed))
}
static RECORDS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// After the execution: the buffer back to unordered access, where the
/// title's passes keep using it.
pub(crate) fn done(list: &HostList, buffer: &ID3D12Resource) {
    // SAFETY: an open list and a live buffer.
    unsafe {
        list.list.ResourceBarrier(&[transition(
            buffer,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )])
    };
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
        },
    }
}

#[cfg(test)]
mod tests {
    /// The clamp compiles with the system's compiler and the host takes it.
    #[test]
    fn argument_clamp_builds() {
        if super::host().is_none() {
            return; // no host GPU here
        }
        assert!(super::clamp().is_some());
    }

    /// Clamps `records` records of `kind`, filled by repeating `words`,
    /// `calls` times in one list: each record's first word after (zero when
    /// it runs empty). One record more follows them and must stay as
    /// written. `None` without a host GPU.
    fn clamped(kind: u32, words: &[u32], records: u32, calls: u32) -> Option<Vec<u32>> {
        clamped_under(kind, words, records, calls, None)
    }

    /// As [`clamped`], with the title's record count `count` (a word in a
    /// buffer of its own) if there is one.
    fn clamped_under(
        kind: u32,
        words: &[u32],
        records: u32,
        calls: u32,
        count: Option<u32>,
    ) -> Option<Vec<u32>> {
        let host = super::host()?;
        // Words in a record of `kind`.
        let per = match kind {
            0 => 4,
            1 => 5,
            _ => 3,
        };
        let spec = (kind, per as u32 * 4, records);
        let all = per * (records as usize + 1);
        let (buffer, data) = host.shared_buffer(all as u64 * 4).unwrap();
        let data = data.cast::<u32>();
        let list = host.new_list().unwrap();
        // SAFETY: our mapped buffer, written before the list runs and read
        // after it has; a live buffer.
        unsafe {
            for at in 0..all {
                data.add(at).write_volatile(words[at % words.len()]);
            }
            let gpu = buffer.GetGPUVirtualAddress();
            let (counter, word) = host.shared_buffer(4).unwrap();
            word.cast::<u32>().write_volatile(count.unwrap_or(0));
            let counted = count.map(|_| counter.GetGPUVirtualAddress());
            for _ in 0..calls {
                assert!(super::bound(&list, &buffer, gpu, spec, counted));
                super::done(&list, &buffer);
            }
            list.close();
            host.execute(&[&list]);
            for at in all - per..all {
                assert_eq!(data.add(at).read_volatile(), words[at % words.len()]);
            }
            let first = |i: usize| data.add(i * per).read_volatile();
            Some((0..records as usize).map(first).collect())
        }
    }

    /// Rejected records do not wrap the budget: of 128 garbage draws (2^26
    /// vertex instances each once clamped) two run, where a wrapping add
    /// let four; of 600 garbage mesh dispatches 16, not 31.
    #[test]
    fn rejected_records_do_not_wrap_the_budget() {
        let Some(draws) = clamped(0, &[u32::MAX], 128, 1) else {
            return; // no host GPU here
        };
        assert_eq!(draws.iter().filter(|&&count| count != 0).count(), 2);
        let meshes = clamped(10, &[u32::MAX], 600, 1).unwrap();
        assert_eq!(meshes.iter().filter(|&&groups| groups != 0).count(), 16);
    }

    /// The budget goes to the first records, in order: of 200 draws of
    /// 3 * 2^18 vertices the first 170 run (it ends inside the third tile);
    /// small records that all fit all run.
    #[test]
    fn the_budget_goes_to_the_first_records() {
        let Some(draws) = clamped(0, &[3 << 18, 1, 0, 0], 200, 1) else {
            return; // no host GPU here
        };
        assert!(draws[..170].iter().all(|&count| count != 0));
        assert!(draws[170..].iter().all(|&count| count == 0));
        let small = clamped(0, &[1], 1 << 16, 1).unwrap();
        assert!(small.iter().all(|&count| count == 1));
    }

    /// A record that does not fit ends the budget: after two draws of
    /// 3 * 2^24 vertex instances a third takes what is left, and the small
    /// ones after it run empty.
    #[test]
    fn a_record_that_does_not_fit_ends_the_budget() {
        let mut words = [1 << 24, 3, 0, 0].repeat(3);
        words.extend([3, 1, 0, 0].repeat(6));
        let Some(draws) = clamped(0, &words, 8, 1) else {
            return; // no host GPU here
        };
        let ran: Vec<bool> = draws.iter().map(|&count| count != 0).collect();
        assert_eq!(ran, [true, true, false, false, false, false, false, false]);
    }

    /// The budget is the list's: a second execution of the same records
    /// finds it spent, and they all run empty.
    #[test]
    fn a_spent_budget_stays_spent() {
        let Some(twice) = clamped(1, &[u32::MAX], 64, 2) else {
            return; // no host GPU here
        };
        assert!(twice.iter().all(|&count| count == 0));
    }

    /// The walk stops at the count: of 128 garbage draws under a count of 3
    /// the first two run and the third runs empty, as they do without one,
    /// and the others are never read (they stay as written). A count above
    /// the records changes nothing; a count of zero walks none.
    #[test]
    fn the_walk_stops_at_the_count() {
        let Some(under) = clamped_under(0, &[u32::MAX], 128, 1, Some(3)) else {
            return; // no host GPU here
        };
        assert_eq!(under[..4], [1 << 24, 1 << 24, 0, u32::MAX]);
        assert!(under[3..].iter().all(|&count| count == u32::MAX));
        let all = clamped(0, &[u32::MAX], 128, 1).unwrap();
        assert_eq!(under[..3], all[..3]);
        let above = clamped_under(0, &[u32::MAX], 128, 1, Some(1 << 20)).unwrap();
        assert_eq!(above, all);
        let none = clamped_under(0, &[u32::MAX], 128, 1, Some(0)).unwrap();
        assert!(none.iter().all(|&count| count == u32::MAX));
    }

    /// Only the records that run are charged to the budget: after a call
    /// under a count of 1 a second finds room for that record again, where
    /// the same draws with no count spent it all on records that never ran.
    #[test]
    fn records_past_the_count_are_not_charged() {
        let Some(twice) = clamped_under(0, &[u32::MAX], 128, 2, Some(1)) else {
            return; // no host GPU here
        };
        assert_eq!(twice[0], 1 << 24);
        let spent = clamped(0, &[u32::MAX], 128, 2).unwrap();
        assert!(spent.iter().all(|&count| count == 0));
    }
}
