# macOS port status

This file tracks the work to run VIctory on Apple Silicon Macs. No game files are used or needed for any step that is marked as done.

## Done on 24 November 2026

The whole workspace builds natively on macOS (aarch64, Homebrew Rust 1.100). The Windows-only parts are gated with `cfg(windows)`, and a few small stubs stand in for them. The text entry dialog is cancelled on other hosts, a relaunch request stops the process, and the `translate` command reports that it needs Windows. On a Mac M4 the test suite gives 162 passed, 0 failed and 10 ignored. The ignored tests read their own test binary as a PE file or use Windows paths, so they only run on Windows.

A native build proves that the platform code (formats, shader recompiler, PE analysis and most of the HLE layer) is portable. It does not run a title, because the loader maps a Windows PE image into the process and calls Windows APIs.

## Done on 24 November 2026: the Windows build under Wine

VIctory now cross builds on the Mac for `x86_64-pc-windows-gnullvm` and runs under Wine 11.14. The `windows-gnu` target cannot be used, because Rust keeps its thread-locals in OS TLS slots there and the loader only saves and restores static TLS, so std aborted when a thread started. Homebrew has no llvm-mingw, so `tools/macos/gnullvm-cc.sh` links with Homebrew clang and lld against the mingw-w64 runtime.

Run the tests under Wine with this command.

```sh
WINE=/path/to/wine tools/macos/test-wine.sh --workspace --no-fail-fast
```

The loader probe passes, so the stand-in title is mapped, bound, delay loaded and run with its thread callbacks. The HLE tests pass (86 of 86), and `victory.exe --help` runs. Wine lacks three network exports (`WSAConnectByList`, `GetAddrInfoExA` and `WinHttpGetProxySettingsEx`), and VIctory now fails those calls cleanly. Two kinds of test still fail under Wine.

- Seven shader tests need a D3D12 device with shader model 6.6. The Wine build has no D3D12 that works on Metal, so this is the GPU gap that D3DMetal has to fill.
- The xtask policy test starts git from inside Wine. Run `cargo xtask policy` natively instead.

## Route A: next steps

The first playable route is the upstream Windows build running under Wine, with D3D12 going to Metal through Apple's D3DMetal. DXMT cannot be used because it covers D3D11 only. Rosetta 2 runs the title's x86-64 code and has supported AVX2 since macOS 15.

The steps are as follows.

1. Install rustup and add the `x86_64-pc-windows-gnu` or `x86_64-pc-windows-msvc` target. The MSVC route through `cargo-xwin` downloads the Microsoft CRT and SDK under Microsoft's licence.
2. Build `apps/victory` and the `tests/fixtures/hle-probe` stand-in title for Windows.
3. Run the probe under Wine to test the loader and HLE layer without any game files.
4. Add D3DMetal from Apple's Game Porting Toolkit and check that VIctory creates a D3D12 device with mesh shaders and shader model 6.6.

## Route B: a native macOS host

Later work replaces the Windows host underneath the title. That needs a Rosetta x86-64 process that maps the PE image, a macOS version of the host pass-through in `victory-hle`, a Metal backend for `victory-gpu`, and DXIL to Metal conversion with Apple's Metal shader converter. This only makes sense after Route A shows where things break.
