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

## 2. The boot stops at about 19 s (system calls)

**Symptom:** the game's main thread waits in a boot loop (an update, `Sleep(33)` and a 100 ms semaphore wait per pass). Each pass appends to a fixed profiling buffer; after about 57 passes it overflows and the game hits its own `int3`.

**Known:**
- It is time-based, not frame-based, and it happens before the boot movie or any audio starts.
- The loop ends when a "ready" record is marked by a listener object that never gets called.
- Ruled out: app-state notifications, fences, frame statistics, clock speed, the Bink movie, XAudio2.
- **Candidates:** an async system request that never completes (user sign-in, GameInput device arrival, storage, network), or the thread that should release the semaphore being stuck elsewhere.

**Start here:** `crates/victory-hle/src/modules/` (xgameruntime, gameinput, xuser), the "The int3" and "What the boot waits on" sections of native-host.md, and `VICTORY_DUMP_THREADS`.

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
