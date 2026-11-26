// Our own test pixel shader, bound as translated shaders are: its texture straight from the
// descriptor heap at the index root constant 0 holds (SM 6.6), sampled with static sampler s0,
// times the vertex colour. No root signature: the host's layout gives the constants and s0.
struct PSIn { float4 pos : SV_Position; float2 uv : TEXCOORD; float4 col : COLOR; };
cbuffer C : register(b0) { uint index; uint3 pad; };
SamplerState smp : register(s0);
float4 main(PSIn i) : SV_Target0 {
    Texture2D<float4> tex = ResourceDescriptorHeap[index];
    return i.col * tex.Sample(smp, i.uv);
}
