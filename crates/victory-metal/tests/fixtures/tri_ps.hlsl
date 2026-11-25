// Our own test pixel shader: vertex colour times a texture sampled through a descriptor table.
struct PSIn { float4 pos : SV_Position; float2 uv : TEXCOORD; float4 col : COLOR; };
cbuffer C : register(b0) { float z; float3 pad; };
Texture2D<float4> tex : register(t0);
SamplerState smp : register(s0);
[RootSignature("RootConstants(num32BitConstants=4, b0), DescriptorTable(SRV(t0), visibility=SHADER_VISIBILITY_PIXEL), DescriptorTable(Sampler(s0), visibility=SHADER_VISIBILITY_PIXEL), RootFlags(ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT)")]
float4 main(PSIn i) : SV_Target0 { return i.col * tex.Sample(smp, i.uv); }
