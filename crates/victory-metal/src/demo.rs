//! End to end: HLSL -> DXIL (dxc) -> metallib (Metal Shader Converter) -> dispatch over zero-copy title memory.
//! The shader reads and writes through D3D12 root descriptors, which the converter turns into GPU
//! addresses in a top-level argument buffer bound at slot 2 (`kIRArgumentBufferBindPoint`).

use crate::probe::wrap;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSString, NSURL};
use objc2_metal::*;
use std::ptr::NonNull;
use victory_metal::mem::{Region, page_size};

const HLSL: &str = r#"
cbuffer C : register(b0) { uint2 k; };
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
[RootSignature("RootConstants(num32BitConstants=2, b0), SRV(t0), UAV(u0)")]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) { dst.Store(id.x * 4, src.Load(id.x * 4) * k.x + k.y); }
"#;

pub fn run() {
    let dxc_path =
        std::env::var("VICTORY_DXC_DYLIB").expect("set VICTORY_DXC_DYLIB to libdxcompiler.dylib");
    let dxil = crate::dxc::hlsl_to_dxil(&dxc_path, HLSL, "cs_6_0").expect("dxc");
    println!("HLSL -> DXIL container: {} B", dxil.len());
    // Regenerates `tests/affine.dxil`, the fixture of `metal.rs`'s tests.
    if let Ok(p) = std::env::var("VICTORY_WRITE_DXIL") {
        std::fs::write(p, &dxil).expect("write fixture");
    }
    let msc = victory_metal::msc::Msc::open_default();
    let (stage, lib) = msc.convert(&dxil, None).expect("convert");
    println!("DXIL -> metallib: stage {stage}, {} B", lib.len());

    let dev = MTLCreateSystemDefaultDevice().unwrap();
    let out = std::env::temp_dir().join("victory-metal-demo.metallib");
    std::fs::write(&out, &lib).unwrap();
    let l = dev
        .newLibraryWithURL_error(&NSURL::fileURLWithPath(&NSString::from_str(
            out.to_str().unwrap(),
        )))
        .expect("load metallib");
    let name = l.functionNames().iter().next().unwrap();
    let pso = dev
        .newComputePipelineStateWithFunction_error(&l.newFunctionWithName(&name).unwrap())
        .expect("pso");

    const N: usize = 4096;
    let half = (N * 4).next_multiple_of(page_size());
    let region = Region::new(2 * half, None).unwrap();
    let buf = wrap(&dev, region.ptr, region.len).unwrap();
    let src = region.ptr.cast::<u32>();
    // SAFETY: the second half is inside the 2*half mapping.
    let dst = unsafe { region.ptr.add(half).cast::<u32>() };
    // SAFETY: both halves are mapped and the GPU is idle while the CPU writes.
    unsafe { (0..N).for_each(|i| src.add(i).write_volatile(i as u32)) };

    // Top-level argument buffer in root-parameter order: 2 inline constants, SRV address, UAV address.
    let mut ab = [0u8; 24];
    ab[0..4].copy_from_slice(&5u32.to_le_bytes());
    ab[4..8].copy_from_slice(&7u32.to_le_bytes());
    ab[8..16].copy_from_slice(&buf.gpuAddress().to_le_bytes());
    ab[16..24].copy_from_slice(&(buf.gpuAddress() + half as u64).to_le_bytes());

    let q = dev.newCommandQueue().unwrap();
    let cb = q.commandBuffer().unwrap();
    let e = cb.computeCommandEncoder().unwrap();
    e.setComputePipelineState(&pso);
    // SAFETY: `ab` is 24 bytes and setBytes copies it.
    unsafe {
        e.setBytes_length_atIndex(NonNull::new(ab.as_mut_ptr().cast()).unwrap(), ab.len(), 2)
    };
    e.useResource_usage(
        ProtocolObject::from_ref(&*buf),
        MTLResourceUsage::Read | MTLResourceUsage::Write,
    );
    e.dispatchThreadgroups_threadsPerThreadgroup(
        MTLSize {
            width: N / 64,
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
    assert!(cb.error().is_none(), "{:?}", cb.error());

    // SAFETY: as above; the GPU has finished.
    let ok = unsafe { (0..N).all(|i| dst.add(i).read_volatile() == i as u32 * 5 + 7) };
    println!("DXIL shader ran on zero-copy memory, dst[i] == src[i]*5+7 for all {N}: {ok}");
    assert!(ok);
}
