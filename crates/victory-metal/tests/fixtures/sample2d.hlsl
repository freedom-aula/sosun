// Our own test shader: a g.x * g.y grid of threads samples a Texture2D at the cell centres of a g grid
// (SampleLevel 0) and stores the float4 results. SRV t0 + UAV u0 in one table, sampler s0 in another.
cbuffer C : register(b0) { uint2 g; };
Texture2D<float4> tex : register(t0);
RWByteAddressBuffer dst : register(u0);
SamplerState smp : register(s0);
[RootSignature("RootConstants(num32BitConstants=2, b0), DescriptorTable(SRV(t0), UAV(u0)), DescriptorTable(Sampler(s0))")]
[numthreads(4, 4, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= g.x || id.y >= g.y) return;
    float2 uv = (float2(id.xy) + 0.5) / float2(g);
    dst.Store4((id.y * g.x + id.x) * 16, asuint(tex.SampleLevel(smp, uv, 0)));
}
