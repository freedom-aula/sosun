# Contributing to Native VIctory for macOS

Thanks for helping. The goal: GTA VI running natively on an Apple Silicon Mac, from a copy the player owns. There is work for testers, Rust programmers, graphics people, reverse engineers and compiler people.

## The one rule

**Nothing from the game is ever committed or posted.** That covers executables, DLLs, shaders, textures, audio, keys, dumps, decrypted files, disassembly listings and screenshots of game content. The same goes for Microsoft's GDK and Apple's SDK files (the Metal Shader Converter is installed by each user, never vendored). `cargo xtask policy` checks this. Pull requests that break it are closed. Findings such as "the boot waits on the record at this address" are fine and welcome; copied code or bytes are not.

## Set up

Follow the [Quick start](README.md#quick-start). Then make sure the tests pass before you change anything:

```bash
rustup run 1.98.1 cargo test --target x86_64-apple-darwin -p victory-hle -p victory-loader -p victory
```

```bash
rustup run 1.98.1 cargo test -p victory-shader -p victory-hle
```

The CI gate is a strict clippy run for Windows, which you can run on a Mac:

```bash
rustup run 1.98.1 cargo clippy --workspace --all-targets --locked --target x86_64-pc-windows-gnullvm -- -D warnings
```

## Find something to work on

- [docs/ROADMAP.md](docs/ROADMAP.md) lists the open problems, with what is already known about each.
- Issues labelled `good first issue` are small and self-contained.
- Comment on an issue before starting something big, so two people don't do the same work.

## How the code is laid out

| Crate | What it does |
| --- | --- |
| `crates/victory-loader` | Maps the game's PE files, TLS, exceptions, the Windows↔macOS calling-convention bridge (`native/`) |
| `crates/victory-hle` | The Windows and Xbox system calls, reimplemented (`host_posix/` for macOS, `modules/` for Xbox APIs) |
| `crates/victory-gpu` | Console Direct3D 12.X; `native/` is the Metal path |
| `crates/victory-shader` | GFX10 (RDNA2) shader decoding and translation to DXIL |
| `crates/victory-metal` | Metal and Metal Shader Converter glue |
| `crates/victory-platform` | Memory, threads and sync on macOS and Windows |
| `apps/victory` | The command-line front end (`victory run`) |

## Debugging a run

Every diagnostic is an environment variable, documented in [docs/native-host.md](docs/native-host.md) under "Bring-up diagnostics". A few common ones:

| Variable | What it does |
| --- | --- |
| `VICTORY_GUESS_GRAPHICS=1` | Build graphics pipelines from the console descriptions (on by default in `run-native.sh`) |
| `VICTORY_DUMP_TARGETS=<dir>,<s>,<every>` | Save render targets as PNGs |
| `VICTORY_TRACE_FAILS=1` | Log every failing system call |
| `VICTORY_DUMP_THREADS=<s>` | Dump every thread's stack after `<s>` seconds |
| `VICTORY_DEBUG_PS=in<N>` / `VICTORY_DEBUG_VS_POS=1` | Graphics bisection switches |

## Pull requests

- Keep each pull request to one change, with a test where it is testable without the game.
- Run the tests and the clippy gate above.
- Gate new diagnostics behind a `VICTORY_*` variable and document them in docs/native-host.md.
- When you guess at undocumented console behaviour, say so in a comment and in native-host.md's list of guesses.
- By contributing, you agree to license your work under the GPL-3.0.

## Be kind

Be patient with newcomers, assume good faith, and keep discussion about the code. Harassment of any kind isn't tolerated.
