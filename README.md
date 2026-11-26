<h1 align="center">Native VIctory for macOS</h1>

<p align="center">
A <b>native</b> macOS port of <a href="https://github.com/Marvisak/VIctory">VIctory</a>, the Xbox Series emulator for GTA VI.<br>
No Wine, no CrossOver, no Game Porting Toolkit: the game's code runs in our own process, and its GPU work goes straight to Metal.
</p>

<p align="center">
<img alt="Apple Silicon M1 or newer" src="https://img.shields.io/badge/Apple%20Silicon-M1%2B-555555">
<img alt="macOS 15 or newer" src="https://img.shields.io/badge/macOS-15%2B-blue">
<img alt="Status: boot movie and menus on screen" src="https://img.shields.io/badge/status-menus%20on%20screen-orange">
<img alt="GPL-3.0 licence" src="https://img.shields.io/badge/licence-GPL--3.0-lightgrey">
</p>

> [!IMPORTANT]
> **Not playable yet, but you can see it run.** The game boots natively on a Mac, plays its boot movie and shows its menus in the window: the start screen, FINISH SETUP and LEGAL NOTICES, rendered on Metal at about 30 fps. Text and blending are now correct on the screens we have checked. You can move through the menus with the keyboard. We know where to look, and we need help. **No game files are included here, ever.** You need your own legally obtained copy of the game.

📸 **[See the latest screenshots](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues/5)** (the game's start screen and setup menus, rendered natively on Metal).

## Why a native port?

The [Wine-based route](https://github.com/pragyaangaur/VIctory-macOS) stacks the game on Wine, a Windows Direct3D 12 layer and Apple's D3DMetal. This project removes those layers:

| Layer | Wine route | Native route (this repo) |
| --- | --- | --- |
| Game code (x86-64) | Wine process under Rosetta | Our own process under Rosetta, or recompiled to arm64 |
| Windows and Xbox system calls | Wine + VIctory | Reimplemented in Rust for macOS (`victory-hle`, about 1,000 functions) |
| Console graphics (D3D12.X) | D3D12 → D3DMetal → Metal | Straight to Metal (`victory-gpu`) |
| Console shaders (GFX10) | → DXIL → D3DMetal | → DXIL → Apple Metal Shader Converter |

Fewer layers means fewer places for things to break, and much better speed: boot frames run at about 28 to 30 fps.

## Where we are

| Area | State |
| --- | --- |
| Loader: maps the game and its DLLs, TLS, exceptions, threads | ✅ Works |
| System calls: memory, files, threads, time, C runtime, sockets, audio (XAudio2), DirectStorage | ✅ Works for everything the boot uses |
| Loading: DirectStorage reads about 9,500 requests at about 79 MB/s | ✅ Works |
| GPU: every draw gets a Metal pipeline, built from the console's own pipeline descriptions | ✅ Works (some parts are inferred, see [docs/native-host.md](docs/native-host.md)) |
| Shaders: GFX10 → DXIL → Metal | 🟡 Most translate; 7 known shapes still refused |
| Boot movie and menus in the window | 🟡 The boot movie plays and the start screen, FINISH SETUP and LEGAL NOTICES show in the window with correct text and blending ([#6](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues/6) fixed); other screens not checked yet |
| Start screen ("press A") and input | ✅ Passed; the keyboard drives the gamepad in the window (real controllers not yet) |
| Running past ~50 s | 🔴 A Metal GPU command times out and frames stop |
| Apple Silicon recompilation (no Rosetta) | 🟡 The whole game (440,000 functions) recompiles to arm64 and presents its first frame; fixing translation bugs one by one |
| Gameplay | 🔴 Not yet |

The live list of open problems, with everything we already know about each one, is in **[docs/ROADMAP.md](docs/ROADMAP.md)**.

## Quick start

You need:

- an Apple Silicon Mac (M1 or newer) with macOS 15 or newer and about 20 GB free;
- [rustup](https://rustup.rs) (not Homebrew's `rust` package);
- Apple's free [Metal Shader Converter](https://developer.apple.com/metal/shader-converter/) (download and run the installer);
- your own copy of the Xbox Series game files.

**1. Get the code and build it.**

```bash
git clone https://github.com/LakshanSivakumar/Native-Victory-macOS.git
```

```bash
cd Native-Victory-macOS && tools/macos/native-setup.sh
```

The setup script checks Rosetta, the Xcode command line tools, rustup and the Metal Shader Converter. It tells you exactly what to install if anything is missing, then builds `victory`.

**2. Run the game.**

```bash
tools/macos/run-native.sh /path/to/your/game/folder
```

Point it at the folder that holds `game_scarlett_master.exe`. A window opens; after a few seconds of black while it loads, you should see the boot movie and then the menus, and the full log is saved under `runs/`. The script also presses Enter for you every two seconds until 80 s to get past the start screen. Click the window and the keyboard drives the game's gamepad:

| Keys | Pad |
| --- | --- |
| Enter, Space | A |
| Escape, Backspace | B |
| X, Y | X, Y |
| Arrow keys | D-pad |
| W A S D | left stick |
| Tab | Menu (Start) |
| Q, E | left, right shoulder |
| Z, C | left, right trigger |

Real controllers aren't supported yet. Add `--headless` to run without a window, or `--seconds 60` to stop after a minute.

**3. Tell us what happened.** Open a [Tester report](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues/new?template=tester-report.yml) and attach the log. Reports from different Macs (M1 to M5, 8 GB to 128 GB) are very useful.

## Help wanted

Whatever your background, there is something to do:

- **Testers:** run the game on your Mac and file a tester report. No coding needed.
- **Graphics and Metal:** the still-guessed pipeline state (culling, depth, stencil, write mask), shader refusals, the GPU timeout at about 53 s.
- **Debugging:** the Metal GPU timeout at about 50 s, and real controllers through the Game Controller framework.
- **Compilers:** the arm64 recompiler (jump tables, SSE, exceptions).
- **Rust and systems:** missing Windows and Xbox system calls, speed, tests.

Read **[CONTRIBUTING.md](CONTRIBUTING.md)** to get started, then pick something from [docs/ROADMAP.md](docs/ROADMAP.md) or the issues labelled `good first issue`.

## Documentation

| Document | What it covers |
| --- | --- |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Open problems, what is known, where to start |
| [docs/native-host.md](docs/native-host.md) | The native host in depth: loader, system calls, findings, every diagnostic switch |
| [docs/metal-backend.md](docs/metal-backend.md) | The Metal backend and the Metal Shader Converter |
| [docs/architecture.md](docs/architecture.md) | VIctory's overall design |
| [docs/asset-policy.md](docs/asset-policy.md) | What may and may not be committed |

## Legal

This project contains no game code, assets, keys or Microsoft or Apple SDK files, and never will. Emulation is built from documentation, observation and clean-room analysis. You must supply your own legally obtained copy of the game. Do not post game files or links to them in issues or pull requests.

Not affiliated with or endorsed by Rockstar Games, Take-Two Interactive, Microsoft or Apple. All trademarks belong to their owners.

## Credits

Native VIctory for macOS builds on [VIctory](https://github.com/Marvisak/VIctory) by Marvisak and its contributors, and on [VIctory for macOS](https://github.com/pragyaangaur/VIctory-macOS) by Pragyaan Gaur and its contributors. Their full history is kept in this repository. Licensed under the [GPL-3.0](LICENSE).
