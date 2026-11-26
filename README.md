<h1 align="center">Native VIctory for macOS</h1>

<p align="center">
A <b>native</b> macOS port of <a href="https://github.com/Marvisak/VIctory">VIctory</a>, the Xbox Series emulator for GTA VI.<br>
No Wine, no CrossOver, no Game Porting Toolkit: the game's code runs in our own process, and its GPU work goes straight to Metal.
</p>

<p align="center">
<img alt="Apple Silicon M1 or newer" src="https://img.shields.io/badge/Apple%20Silicon-M1%2B-555555">
<img alt="macOS 15 or newer" src="https://img.shields.io/badge/macOS-15%2B-blue">
<img alt="Status: story mode loads, prologue crashes" src="https://img.shields.io/badge/status-story%20mode%20loads-orange">
<img alt="GPL-3.0 licence" src="https://img.shields.io/badge/licence-GPL--3.0-lightgrey">
</p>

> [!IMPORTANT]
> **Not playable yet, but it gets as far as the story.** The game boots natively on a Mac, plays its boot movie, and shows the start screen and every setup screen on Metal at about 30 fps, with sound, keyboard and controllers. Then it loads story mode and gets a few seconds into the prologue before it crashes. Getting the prologue to play through is what everyone is working on now: see [#15](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues/15). **No game files are included here, ever.** You need your own legally obtained copy of the game.

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
| Sound | 🟡 Plays through CoreAudio (the game's Windows Spatial Audio stream, mixed to stereo); reverb is still silent |
| Loading: DirectStorage reads about 9,500 requests at about 79 MB/s | ✅ Works |
| GPU: every draw gets a Metal pipeline, built from the console's own pipeline descriptions | ✅ Works (some parts are inferred, see [docs/native-host.md](docs/native-host.md)) |
| Shaders: GFX10 → DXIL → Metal | 🟡 Most translate. Refusals in the story load fell from 526 to about 150 and keep falling; a refused shader's work is skipped. Shaders are cached on disk between runs |
| Boot movie and menus in the window | ✅ The boot movie plays smoothly, and the start screen and setup screens show with correct text and blending. The black window on M1/M2 is fixed ([#91](https://github.com/LakshanSivakumar/Native-Victory-macOS/pull/91)) |
| Start screen ("press A") and input | ✅ Passed; the keyboard and Game Controller pads (Xbox, DualShock 4, DualSense) drive the gamepad in the window |
| Running past ~50 s | ✅ Fixed: the old Metal GPU timeout is gone ([#2](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues/2)) |
| Story mode load | ✅ Loads, and reaches the prologue |
| Prologue | 🔴 Crashes a few seconds in. Two causes are known and being fixed: a long stall on the first world frame that trips the game's own watchdog (fixed on a branch), and pointers read from data a GPU shader never filled in ([#15](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues/15)) |
| Memory on 16 GB Macs | 🟡 The Series S profile fits better; Series X can run out of GPU memory at the world load ([#8](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues/8), [#79](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues/79)) |
| Apple Silicon recompilation (no Rosetta) | 🟡 The whole game (440,000 functions) recompiles to arm64, and a contributor's branch boots it to the intro at 30 fps ([#115](https://github.com/LakshanSivakumar/Native-Victory-macOS/pull/115)); not on main yet |
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

**Controllers.** An Xbox pad, a PS4 DualShock 4 or a PS5 DualSense connected to the Mac (USB, or Bluetooth through System Settings → Bluetooth; on a DualShock 4 hold Share and PS until the light bar flashes to pair) drives the gamepad while the window is in front. The keyboard still works alongside it. On a PlayStation pad Cross, Circle, Square and Triangle are A, B, X and Y, Options is Menu, and Share or a touchpad click is View. With several controllers connected, the one you used last wins. The run log names the controller it picked (`controller: drives the pad`); if it says none, check that macOS sees the controller in System Settings → Game Controllers.

Add `--headless` to run without a window, or `--seconds 60` to stop after a minute.

**3. Tell us what happened.** Open a [Tester report](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues/new?template=tester-report.yml) and attach the log. Reports from different Macs (M1 to M5, 8 GB to 128 GB) are very useful.

## Help wanted

Whatever your background, there is something to do:

- **Testers:** run the game on your Mac and file a tester report. No coding needed.
- **Graphics and Metal:** the remaining shader refusals, GPU memory use on 16 GB Macs, and frame rate once the game is in the world.
- **Debugging:** the prologue crash ([#15](https://github.com/LakshanSivakumar/Native-Victory-macOS/issues/15)). Logs from your own runs that reach the prologue help a lot.
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
