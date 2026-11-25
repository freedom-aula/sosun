// Our own test shader: as sample2d.hlsl over a Texture3D; dispatch z = depth slice, g.x = g.y = depth = 4.
cbuffer C : register(b0) { uint2 g; };
Texture3D<float4> tex : register(t0);
RWByteAddressBuffer dst : register(u0);
SamplerState smp : register(s0);
[RootSignature("RootConstants(num32BitConstants=2, b0), DescriptorTable(SRV(t0), UAV(u0)), DescriptorTable(Sampler(s0))")]
[numthreads(4, 4, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= g.x || id.y >= g.y) return;
    float3 uvw = (float3(id) + 0.5) / float3(g.x, g.y, g.x);
    dst.Store4(((id.z * g.y + id.y) * g.x + id.x) * 16, asuint(tex.SampleLevel(smp, uvw, 0)));
}
