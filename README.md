<h1 align="center">Native VIctory for macOS</h1>

<p align="center">
A <b>native</b> macOS port of <a href="https://github.com/Marvisak/VIctory">VIctory</a>, the Xbox Series emulator for GTA VI.<br>
No Wine, no CrossOver, no Game Porting Toolkit: the game's code runs in our own process, and its GPU work goes straight to Metal.
</p>

<p align="center">
<img alt="Apple Silicon M1 or newer" src="https://img.shields.io/badge/Apple%20Silicon-M1%2B-555555">
<img alt="macOS 15 or newer" src="https://img.shields.io/badge/macOS-15%2B-blue">
<img alt="Status: boots, renders its first image" src="https://img.shields.io/badge/status-early%20bring--up-orange">
<img alt="GPL-3.0 licence" src="https://img.shields.io/badge/licence-GPL--3.0-lightgrey">
</p>

> [!IMPORTANT]
> **Not playable yet.** The game boots natively on a Mac, loads its data, runs its GPU work on Metal at about 30 fps, gets past its "press A" start screen and runs for about 50 seconds (around 1,400 frames). It has drawn its first real image, its boot logo, in a debug mode, but normal frames are still black. We understand where both problems are, and we need help. **No game files are included here, ever.** You need your own legally obtained copy of the game.

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
| First image on screen | 🟡 The boot logo renders when vertex positions are taken from raw data; in normal runs the vertex shader's constant buffer reads zero |
| Start screen ("press A") | ✅ Passed with scripted key presses (no live keyboard or gamepad yet) |
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

Point it at the folder that holds `game_scarlett_master.exe`. A window opens, and the full log is saved under `runs/`. The script presses Enter for you every two seconds to get past the start screen, because keyboard input isn't wired up yet. Add `--headless` to run without a window, or `--seconds 60` to stop after a minute.

**3. Tell us what happened.** Open a [Tester report](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues/new?template=tester-report.yml) and attach the log. Reports from different Macs (M1 to M5, 8 GB to 128 GB) are very useful.

## Help wanted

Whatever your background, there is something to do:

- **Testers:** run the game on your Mac and file a tester report. No coding needed.
- **Graphics and Metal:** find out why the content draws' constant buffer reads zero on the GPU. This is the step to the first proper frame.
- **Debugging:** the Metal GPU timeout at about 50 s, and live keyboard and gamepad input.
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
