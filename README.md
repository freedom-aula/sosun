<h1 align="center">SOSUN</h1>

<p align="center">
Xbox Series X|S software, <b>natively</b> on Apple Silicon Macs.<br>
The title's own x86-64 code runs inside our process under Rosetta 2, its system calls are answered by our Rust runtime, and its GPU work goes straight to Metal.<br>
<i>No Wine, no CrossOver, no Game Porting Toolkit, no D3DMetal.</i>
</p>

<p align="center">
<img alt="Apple Silicon M1 or newer" src="https://img.shields.io/badge/Apple%20Silicon-M1%2B-555555">
<img alt="macOS 15 or newer" src="https://img.shields.io/badge/macOS-15%2B-blue">
<img alt="Status: boots to the prologue; the world is still dark and slow" src="https://img.shields.io/badge/status-boots%20to%20the%20prologue-orange">
<img alt="GPL-3.0 licence" src="https://img.shields.io/badge/licence-GPL--3.0-lightgrey">
</p>

SOSUN is a separate project, continued from **[Native-Victory-macOS](https://github.com/LakshanSivakumar/Native-Victory-macOS)** by LakshanSivakumar and its contributors, which builds on **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and **[VIctory-macOS](https://github.com/pragyaangaur/VIctory-macOS)** by Pragyaan Gaur. The full git history is kept, and the README of the project we forked from is preserved verbatim as **[README-upstream.md](README-upstream.md)**.

This README walks through SOSUN itself: what it is, how a run works end to end, how to build and drive it, and where it stands.

> [!IMPORTANT]
> **Not playable yet, but it gets as far as the story.** The game boots natively, plays its boot movie, shows the start screen and every setup screen on Metal at about 30 fps, with sound, keyboard and controllers, then loads story mode and reaches the prologue's world. The world is not watchable yet: it draws, but the lighting passes are still partly refused, so the picture is mostly black, and it runs at about 0.5 fps. **No game files are included here, ever.** You need your own legally obtained copy of the game.

## Contents

1. [What SOSUN is](#what-sosun-is)
2. [How a run works](#how-a-run-works)
3. [Repository layout](#repository-layout)
4. [Requirements](#requirements)
5. [Quick start](#quick-start)
6. [Configuration](#configuration)
7. [Diagnostics](#diagnostics)
8. [Memory profiles](#memory-profiles)
9. [Where it stands](#where-it-stands)
10. [Documentation](#documentation)
11. [Contributing](#contributing)
12. [Legal](#legal)
13. [Credits](#credits)

## What SOSUN is

SOSUN is a native compatibility layer for Xbox Series X|S software on Apple Silicon Macs. "Native" is the whole point: the usual route stacks Wine, a Windows Direct3D 12 layer and Apple's D3DMetal between the game and the Mac; SOSUN's own process loads the game's x86-64 image and runs it under Rosetta 2, answers its system calls from Rust code, and submits its GPU work to Metal directly. Fewer layers means fewer places to break and much less overhead — boot frames run at about 28 to 30 fps.

Two hard rules shape every decision in this tree:

- **no compatibility middleware** (Wine, CrossOver, Game Porting Toolkit, D3DMetal) anywhere in the stack;
- **no game files in this repository, ever** — emulation is built from documentation, observation and clean-room analysis, and you supply your own legally obtained copy.

The project's driving target is the smallest machine that could plausibly run the game: a fanless MacBook Air on Apple Silicon, inside the Series S memory profile, with no GPU watchdog resets and no out-of-memory kills at the world load.

What SOSUN has added on top of the fork point:

- the workspace builds and its whole test suite runs on machines with **no GPU backend at all** (plain Linux, no macOS), so every change is verifiable off-Mac;
- `VICTORY_AMBIENT=<level>`: a guaranteed-visible present while lighting passes are still refused — near-black world pixels are shaded by the frame's depth target;
- the 64-frame log line counts **dropped draws and dispatches by reason**, so what the world is missing can be read off any run log;
- mesh shaders whose declared output would exceed Metal's **32 KiB per-threadgroup limit** are refused by name before Apple's compiler service ever sees them (it crashes on them otherwise);
- the ambient frame tests are serialised, so they cannot interleave through the process-wide frame.

## How a run works

`tools/macos/run-native.sh <game folder>` ends up invoking one binary:

```text
victory run --execute --console series-s --unimplemented zero <path to game_scarlett_master.exe>
```

Between that and the first presented frame, five layers do the work.

### 1. The loader — the game's code, our process (`victory-loader`)

`victory-pe` parses the game's PE32+ image and its import libraries. The loader maps the sections, sets up each thread's TLS and TEB, stacks, and thunks every import to `victory-hle`. Structured exception handling runs through signal handlers: the title's `int3` asserts and access violations get a first-chance log with registers and a call chain, and unwinding uses the title's own tables. Instructions Rosetta will not run (`clwb`/`clflushopt` flush loops, `clzero`, `monitorx`/`mwaitx`, the SSE4a `insertq`/`extrq`) are skipped or emulated in the `SIGILL` path, and the AVX masked stores Rosetta cannot resume partway are located in the image up front.

The x86-64 code then simply executes under Rosetta 2, inside our x86_64 macOS process. No recompilation is needed to run; an arm64 static recompiler for the whole game exists on a branch (see [Where it stands](#where-it-stands)).

### 2. The system layer (`victory-hle`)

About a thousand functions of the Xbox and Windows surface, implemented in Rust:

- **Memory** — the `VirtualAlloc` and `XMem` families, physical pages, and the console's quotas (7.6 GiB of title memory as Series S, 13.5 GiB as Series X).
- **Files** — the console's fixed drives, synchronous and async IO, and **DirectStorage**: a story load streams about 9,500 reads at about 79 MB/s.
- **Audio** — XAudio2 onto CoreAudio; the title's Windows Spatial Audio stream is mixed to stereo; Opus and XMA voices are decoded. (Reverb is still silent.)
- **Input** — GameInput, fed by the keyboard and by `GCController` pads.
- **Networking** — winsock and winhttp.
- **The rest of the console surface** — `xgameui`, `xgamesave`, `xuser`, `xpackage`, lifecycle, and so on.

### 3. The GPU — D3D12.X straight onto Metal (`victory-gpu` on `victory-metal`)

The title speaks D3D12.X; the native host records its command lists and submits Metal command buffers:

- **Pipeline state is decoded, not guessed**: the graphics state comes out of the console's own register images (device slots `0x1c8`/`0x218` — blend controls, cull mode and winding, depth and stencil, write masks), and the D3D12 subobjects are read directly.
- **Title memory is shared with the GPU zero-copy**: buffers wrap the title's own pages with `newBufferWithBytesNoCopy` (16 KiB-aligned page multiples), so the GPU reads what the game wrote with no staging copies.
- **Residency follows the frame**: what a descriptor heap names stays resident through the heap's `MTLResidencySet` (the default on Macs with 32 GiB or more; smaller Macs name resources per encoder — see [Memory profiles](#memory-profiles)).
- **CPU/GPU sync uses `MTLSharedEvent` fences**, and a command buffer the GPU fails still signals its fences, so a GPU fault cannot leave the game's threads waiting forever.
- Command buffers are labelled with per-encoder state, so a failure says what was running.

### 4. The shaders — GFX10 to Metal (`victory-shader`)

Console shader containers carry DXBC/DXIL and precompiled GFX10 (RDNA2) machine code. The translation path is **GFX10 → DXIL → Apple's Metal Shader Converter (`libmetalirconverter`, in-process) → `MTLLibrary`**, and everything is cached on disk, keyed by the shader's hash and every option that changes the translation.

A console wave of 64 lanes has to run on the Mac's 32-lane SIMD groups: cross-lane operations (ballots, lane reads, reductions) go through threadgroup scratch with a proof that both halves arrive, shapes that cannot be proven run in lockstep (every host thread follows the console wave's one program counter), and a shape that would still split the halves is refused by name — its work is skipped, never mis-executed. A per-lane loop cap (`VICTORY_SHADER_LOOP_LIMIT`, default 16384) keeps unbounded loops off Metal's command-buffer timeout, and mesh shaders past the 32 KiB output limit are refused before the compiler service sees them.

### 5. The window, the pad and the picture

A CAMetalLayer window (AppKit) presents the frames; `--headless` renders offscreen instead. The keyboard and any connected controller drive a virtual Xbox pad. Presents go through the host's composite pass — which is also where the `VICTORY_AMBIENT` debug shading hooks in.

### What you see, in order

| About | What happens |
| --- | --- |
| 0–5 s | Black while the image maps and subsystems initialise |
| ~5 s | The boot movie plays (about 30 fps, with sound) |
| 30–80 s | Start screen, "press A" (the script presses it for you), setup screens |
| ~90–120 s | Story mode loads; DirectStorage streams the world in |
| then | The prologue's world: geometry draws, but lighting passes are still partly refused — the picture is mostly black, and slow |

Times vary with the machine and how warm the shader cache is; the first world frame with a cold cache can take over 30 s.

## Repository layout

| Path | What it is |
| --- | --- |
| `apps/victory` | The command-line front end (`inspect`, `census`, `convert`, `run`) |
| `crates/victory-core` | Shared plumbing: configuration, paths, the asset-policy guard, logging |
| `crates/victory-platform` | The platform seam (memory, threads, synchronisation) with macOS and win32 implementations |
| `crates/victory-pe` | Read-only analysis of PE32+ images and COFF import libraries |
| `crates/victory-loader` | Runs the title's original x86-64 image natively inside our process |
| `crates/victory-hle` | Our implementation of the Xbox system API surface |
| `crates/victory-formats` | Platform-level data formats: texture layouts, compressed textures, XMA audio |
| `crates/victory-game` | Everything specific to the title: containers, build identities, hooks |
| `crates/victory-gpu` | D3D12.X command streams and GPU memory, onto the host backend |
| `crates/victory-backend` | The host-GPU traits (the seam `victory-metal` implements) |
| `crates/victory-metal` | The native Metal backend (macOS only) |
| `crates/victory-shader` | Xbox shader containers (DXBC/DXIL + precompiled GFX10) to host shaders |
| `crates/victory-pipeline` | Offline conversion of game data into host formats, content-addressed cache |
| `xtask` | Repository tasks, including the asset-policy gate (`cargo xtask policy`) |
| `tools/macos` | The setup, run and test scripts this README uses |
| `tools/swizzle` | Texture XOR table generation |

## Requirements

- an Apple Silicon Mac (M1 or newer) with macOS 15 or newer and about 20 GB free;
- [rustup](https://rustup.rs) (not Homebrew's `rust` package);
- the Xcode command line tools and Rosetta 2;
- Apple's free [Metal Shader Converter](https://developer.apple.com/metal/shader-converter/) (download and run the installer);
- your own copy of the Xbox Series game files.

## Quick start

**1. Get the code and build it.**

```bash
git clone https://github.com/freedom-aula/sosun.git
```

```bash
cd sosun && tools/macos/native-setup.sh
```

The setup script checks Rosetta 2, the Xcode command line tools, rustup and the Metal Shader Converter, tells you exactly what to install if anything is missing, installs the pinned Rust toolchain (1.98.1) with its x86_64 target, and builds `victory` (release, x86_64, runs under Rosetta).

**2. Run the game.**

```bash
tools/macos/run-native.sh /path/to/your/game/folder
```

Point it at the folder that holds `game_scarlett_master.exe`. A window opens; after a few seconds of black, the boot movie and then the menus appear, and the full log is saved under `runs/`. The script presses Enter (the pad's A) every two seconds until 80 s to get past the start screen. Click the window and the keyboard drives the game's gamepad:

| Keys | Pad |
| --- | --- |
| Enter, Space | A |
| Escape, Backspace | B |
| X, Y | X, Y |
| Arrow keys | D-pad |
| W A S D | left stick |
| I J K L | right stick |
| F, H | stick clicks |
| Tab | Menu (Start) |
| V | View |
| Q, E | shoulders |
| Z, C | triggers |

**Controllers.** An Xbox pad, a PS4 DualShock 4 or a PS5 DualSense connected to the Mac (USB or Bluetooth) drives the gamepad while the window is in front, alongside the keyboard. On a PlayStation pad, Cross/Circle/Square/Triangle are A/B/X/Y, Options is Menu, and Share or a touchpad click is View. With several controllers connected, the one you used last wins, and the run log names the one it picked.

Useful flags: `--headless` (no window), `--seconds 60` (stop after a minute), `--console series-x|series-s` (override the console model; the default is Series S on Macs with 16 GB or less, Series X above that).

**3. Tell us what happened.** Open an issue on this repository and attach the log from `runs/`; if it is about an upstream-known problem, the [upstream issue tracker](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues) carries the detail. Reports from different Macs (M1 to M5, 8 GB to 128 GB) are very useful.

## Configuration

The `victory` front end reads `victory.toml` (git-ignored; copy `victory.example.toml`), and environment variables take precedence over the file. `VICTORY_CONFIG` selects a different file. Every path must point **outside** the repository — the tools refuse otherwise.

| File key | What it sets | Environment variable |
| --- | --- | --- |
| `game_dir` | Your own dump of the game | `VICTORY_GAME_DIR` |
| `cache_dir` | Derived data (default: the user's cache directory) | `VICTORY_CACHE_DIR` |
| `data_dir` | What titles save, kept apart from the cache | `VICTORY_DATA_DIR` |
| `fidelityfx_dir` | The PC FidelityFX upscaler DLL folder, if you want it in place of the console build | `VICTORY_FIDELITYFX_DIR` |
| `test_titles` | Other Xbox Series titles used to test platform-level code | `VICTORY_TEST_TITLES` |

## Diagnostics

Off by default, read once at startup. The ones you will actually reach for:

| Switch | What it does |
| --- | --- |
| `VICTORY_AMBIENT=<level>` | Shade the unlit world's near-black pixels by the frame's depth target (`level` a fraction in (0, 1]; geometry shows while lighting is refused) |
| `VICTORY_CAPTURE=<dir>` | Save presented frames as PNGs (every frame whose number is a power of two) |
| `VICTORY_CAPTURE_FRAMES=<n>,<n>,...` | Save these frames too |
| `VICTORY_CAPTURE_WORLD=<n>,...` | Count frames from the world's first frame |
| `VICTORY_DUMP_TARGETS=<dir>,<seconds>[,every]` | Dump the colour targets drawn since the last present as PPMs (needs `VICTORY_TRACE_TARGETS=1`) |
| `VICTORY_DUMP_REFUSED=<dir>` | Dump the container of every shader the host cannot translate |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | Cap translated loops per entry (default 16384; part of the shader cache key) |
| `VICTORY_METAL_RESIDENCY_SETS=0\|heap\|1` | How resources stay resident (see [Memory profiles](#memory-profiles)) |
| `VICTORY_TRACE_GPU_FAILS=1` | Label command buffers and encoders; log GPU failures, slow (>1 s) and long-in-flight (>3 s) buffers |
| `VICTORY_COUNT_GREEN=1` | Present pacing and blank-movie-frame counts, once a second |
| `VICTORY_WASAPI_WAV=<file>` | Write everything that reaches the audio device as a float WAV |
| `VICTORY_KEYS=<seconds>:<key>,...` | Scripted input next to the live keyboard |
| `VICTORY_MSC_DYLIB=<path>` | Where `libmetalirconverter.dylib` lives (default `/usr/local/lib`) |

The complete list — tracing owners, descriptor stores, root spans, thread dumps, memory scans and the diagnostics-build-only switches — is in **[docs/native-host.md](docs/native-host.md)**, "Bring-up diagnostics".

## Memory profiles

The console model decides how much title memory the game expects: 7.6 GiB (Series S) or 13.5 GiB (Series X). `run-native.sh` defaults to **Series S on Macs with 16 GB or less** and Series X above that, because Series X can exhaust Metal's working set at the world load even on 24 GB Macs.

On Macs with 32 GiB or more, GPU memory the world's descriptor heap names is kept resident in one `MTLResidencySet` (the fast path — naming about 37,000 allocations per encoder instead took most of a frame). Smaller Macs name resources per encoder by default; `VICTORY_METAL_RESIDENCY_SETS=heap` tries the sets anyway, `=0` forces the per-encoder path, `=1` also keeps all of title memory resident (not recommended below 32 GB — it has ended in GPU timeouts on an 18 GB Mac).

## Where it stands

| Area | State |
| --- | --- |
| Loader, system calls, DirectStorage, sound, input | **Works** for everything the boot and story load use |
| Boot movie, menus, start screen | **Works** on Metal at about 30 fps, with sound |
| Story mode load | **Works**; reaches the prologue |
| Prologue | Runs into the world; stability there is the top open problem |
| World picture | Geometry draws (the G-buffer has content), but lighting passes are still partly refused — the picture is mostly black |
| World frame rate | About 0.5 fps on an M5 Pro; host texture copies are the main suspect |
| Shaders | Most translate, including the world's mesh shaders; a few hundred are still refused in the world, each by named cause, and cached on disk between runs |
| Apple Silicon recompilation (no Rosetta) | The whole game (440,000 functions) recompiles to arm64; a contributor's branch boots it to the intro at 30 fps — not on main yet |
| Gameplay | Not yet |

The live list of open problems, with everything already known about each, is in **[docs/ROADMAP.md](docs/ROADMAP.md)**; the upstream project's detailed status table is in [README-upstream.md](README-upstream.md).

## Documentation

| Document | What it covers |
| --- | --- |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Open problems, what is known, where to start |
| [docs/native-host.md](docs/native-host.md) | The native host in depth: loader, system calls, findings, every diagnostic switch |
| [docs/metal-backend.md](docs/metal-backend.md) | The Metal backend and the Metal Shader Converter |
| [docs/architecture.md](docs/architecture.md) | The overall design |
| [docs/macos.md](docs/macos.md) | The macOS port's status history |
| [docs/asset-policy.md](docs/asset-policy.md) | What may and may not be committed |
| [README-upstream.md](README-upstream.md) | The README of the project this fork continues |

## Contributing

Whatever your background, there is something to do: **testers** (run the game on your Mac and attach the log — no coding needed), **graphics and Metal** (the shader refusals that keep the world unlit, frame rate, GPU memory), **debugging** (prologue runs), **compilers** (the arm64 recompiler: jump tables, SSE, exceptions) and **Rust and systems** (missing system calls, speed, tests). Start with [CONTRIBUTING.md](CONTRIBUTING.md), then pick something from [docs/ROADMAP.md](docs/ROADMAP.md).

The repository gate is `cargo xtask policy` (assets and secrets); `cargo build --workspace` and `cargo test --workspace` run on any host, no Mac or GPU needed.

## Legal

This project contains no game code, assets, keys or Microsoft or Apple SDK files, and never will. Emulation is built from documentation, observation and clean-room analysis. You must supply your own legally obtained copy of the game. Do not post game files or links to them in issues or pull requests.

Not affiliated with or endorsed by Rockstar Games, Take-Two Interactive, Microsoft or Apple. All trademarks belong to their owners.

## Credits

SOSUN continues the work of **[Native-Victory-macOS](https://github.com/LakshanSivakumar/Native-Victory-macOS)** (LakshanSivakumar and contributors), which builds on **[VIctory](https://github.com/Marvisak/VIctory)** (Marvisak and contributors) and **[VIctory-macOS](https://github.com/pragyaangaur/VIctory-macOS)** (Pragyaan Gaur and contributors). Their full history is kept in this repository, and their README is preserved as [README-upstream.md](README-upstream.md). Licensed under the [GPL-3.0](LICENSE).
