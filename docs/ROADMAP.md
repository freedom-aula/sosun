# Roadmap and open problems

Updated 26 November 2026. Each problem lists what is already known, so you don't have to rediscover it. The deep detail, with addresses and every diagnostic switch, is in [native-host.md](native-host.md).

## 1. The first proper frame (graphics)

**Symptom:** from frame ~830 the game issues about 20 indexed "content" draws per frame into a 3840x2160 target. In a normal run they draw nothing.

**Known:**
- With `VICTORY_DEBUG_VS_POS=1` (vertex positions taken from the raw vertex data), the draws render the game's boot logo, stretched over the screen. So vertex fetch, the vertex→pixel link and the pixel shader all work.
- `VICTORY_DEBUG_PS=in2` shows that the vertex shader's **constant-buffer reads return zero on the GPU**. The position matrix comes from the same buffer, so every vertex collapses to a point.
- Ruled out: depth test, depth/stencil formats, Metal command errors, shader signatures, user-data register mapping, descriptor heap index range.
- The constant buffers sit at low addresses (below 4 GiB, e.g. `0x19abdc00`). The GPU memory pools sit above 4 GiB. **Leading hypothesis:** the GPU address translation has no mapping for that low region.

**Start here:** `crates/victory-gpu/src/native/` (memory, host), the "Guessed graphics pipelines" section of native-host.md.

## 2. Running past about 50 seconds (GPU timeout) and live input

**Solved on 26 November:** the old stop at about 19 s was the game's "press A" start screen. The macOS host had no input, so nothing ever accepted it, and the game's profiling buffer overflowed into its own `int3`. `GameInput` now feeds scripted keys (`VICTORY_KEYS`, Enter = A) to a stand-in pad. Getting past it also needed SSE4a `insertq`/`extrq` emulation, cheaper cache-flush handling and clamped Metal texture views.

**Now:**
- At about 53 s (about 1,400 frames) a Metal command buffer fails with `kIOGPUCommandBufferCallbackErrorTimeout`. A fence is left one step behind (signal 2836, GPU at 2832) and presents stop. Not investigated yet.
- There is no live keyboard or gamepad on macOS. Wiring the window's key events and Game Controller framework pads into `GameInput` is a self-contained task.

**Start here:** `crates/victory-gpu/src/native/` and `queue.rs` for the timeout; `crates/victory-hle/src/modules/gameinput.rs` and `apps/victory/src/window.rs` for input.

## 3. Shader translation gaps

Seven shader shapes are still refused, on the guessed path:
- pixel: "a computed buffer descriptor used as a view" ×3, "a hardware SGPR input" ×1;
- vertex: "an address in an SGPR that is no root parameter" ×3.

**Start here:** `crates/victory-shader/src/emit/`. Tests use hand-assembled GFX10 code (`gfx_refusal_tests.rs`).

## 4. Running without Rosetta (arm64 recompilation)

The whole game executable (about 440,000 functions) is recompiled to C and then to arm64, and it presents its first frame. Current work is translation bugs found one at a time: jump-table bounds, then the next trap. A DirectStorage request also stalls on this path.

**Start here:** the recompiler is not in this repo yet. Ask in an issue if you want to work on it.

## 5. Everything after that

Textures and the boot movie (Bink), the loading screens, input (GameInput), more of the GPU (mesh shaders, indirect draws), speed, and eventually the prologue.

## Good first contributions

- Run the game on your Mac and file a tester report; different chips and RAM sizes help.
- Turn a finding in native-host.md into a unit test.
- Pick a refused shader shape from section 3. It's self-contained and testable without the game.
- Improve `tools/macos/native-setup.sh` and `run-native.sh` for setups we haven't seen.
