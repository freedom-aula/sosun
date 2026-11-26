// Our own test shader: dst[i] = src[i] * k.x + k.y, with src (SRV) and dst (UAV) reached through one
// descriptor table (heap slots t, t+1), k as root constants.
cbuffer C : register(b0) { uint2 k; };
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
[RootSignature("RootConstants(num32BitConstants=2, b0), DescriptorTable(SRV(t0), UAV(u0))")]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) { dst.Store(id.x * 4, src.Load(id.x * 4) * k.x + k.y); }
