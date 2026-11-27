// Our own test shader: an 8x8-thread group fill of a 2D texture through a UAV table; the texel at
// (x, y) is (x, y, 64, 255) / 255.
RWTexture2D<float4> dst : register(u0);
[RootSignature("DescriptorTable(UAV(u0))")]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) { dst[id.xy] = float4(id.x, id.y, 64, 255) / 255.0; }
