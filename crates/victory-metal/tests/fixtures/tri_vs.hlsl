// Our own test vertex shader: interleaved POSITION float2, TEXCOORD float2, COLOR float4; z from a root constant.
struct VSIn { float2 pos : POSITION; float2 uv : TEXCOORD; float4 col : COLOR; };
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD; float4 col : COLOR; };
cbuffer C : register(b0) { float z; float3 pad; };
[RootSignature("RootConstants(num32BitConstants=4, b0), DescriptorTable(SRV(t0), visibility=SHADER_VISIBILITY_PIXEL), DescriptorTable(Sampler(s0), visibility=SHADER_VISIBILITY_PIXEL), RootFlags(ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT)")]
VSOut main(VSIn i) { VSOut o; o.pos = float4(i.pos, z, 1); o.uv = i.uv; o.col = i.col; return o; }
