---
name: add-hle-function
description: Implement a missing Windows, Xbox or GDK function (or COM interface) the game calls on the native macOS host. Use when the log shows a missing, stubbed or zero-returning import, a class-not-registered COM request, or the game stalls waiting on a system call.
---

# Add or fix a system call (HLE)

The game's imports are bound to Rust implementations in `crates/victory-hle`. Unimplemented imports return zero under `--unimplemented zero`, which hides problems, so look for these first:

- The `bound ... N hle, N host, N title, N stubbed` lines at start-up: anything stubbed is unimplemented.
- `VICTORY_TRACE_IMPORTS=1` (loader bridge, `crates/victory-loader/src/native/bridge.rs`): traces calls through imports.
- `VICTORY_TRACE_MISSING=1`: calls to D3D/GPU object methods we don't implement (`crates/victory-gpu/src/object.rs`).
- COM requests for classes we don't implement are logged once with their GUIDs (see `host_posix/system.rs`, `CoCreateInstance`).

## Where to put it

- Xbox and GDK APIs (XGameRuntime, GameInput, DirectStorage, XMem, AcpHal audio...): `crates/victory-hle/src/modules/<module>.rs`, exported with `hle_export!`.
- Win32 and C runtime on macOS: `crates/victory-hle/src/host_posix/` (`ucrt/` for the C runtime, `wasapi.rs` and `xaudio2.rs` for audio, `files.rs`, `sync.rs`, `system.rs`).
- Copy the patterns already in those files: the calling convention (`extern "system"`), HRESULT constants, COM vtables.

## Rules

- Implement what the game actually uses, found from a trace, not the whole API. Log anything unexpected once (`warn_once`-style), never silently.
- Document behaviour you had to infer in `docs/native-host.md`, and mark it in the code.
- Add a unit test for the behaviour; there is a probe harness in `crates/victory-hle/tests/native_probe.rs` for running a call the way the title does.
- Check: `cargo test -p victory-hle -p victory` and `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnullvm -- -D warnings`.
