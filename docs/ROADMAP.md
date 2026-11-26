# Roadmap and open problems

Updated 26 November 2026. Each problem lists what is already known, so you don't have to rediscover it. The deep detail, with addresses and every diagnostic switch, is in [native-host.md](native-host.md).

## 1. From the UI target to the window (graphics)

**Solved on 26 November:** content draws used to be black. There were two bugs. Guessed render pipelines were built without the host root layout, so shaders read their user data from address 0. And descriptor records the game copies into the shader-visible heap with plain stores were never mirrored. Now the start screen ("ACCOUNT PICKER / SCREEN NARRATION / PLAYER") and the "FINISH SETUP?" menu render correctly into the 3840x2160 UI target.

**Now:**
- **The presented image is still black.** The pass that composites the UI target into the swapchain uses a pixel shader with static samplers, and the host `RootLayout` (and `msc.rs`) can't declare static samplers yet, so that stage still gets the old layout. Adding static samplers is the next step to a visible window.
- **Some glyphs draw as filled boxes,** while the heading font works. It is probably a font-atlas texture format or channel issue.
- **There is a white band** across the top of the setup menu.
- Table re-mirroring (`NativeHost::remirror_tables`) only checks the first 16 entries per table.

**Start here:** `crates/victory-gpu/src/native/shaders.rs`, `crates/victory-metal/src/msc.rs`, and the "First real frame" section of native-host.md.

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
