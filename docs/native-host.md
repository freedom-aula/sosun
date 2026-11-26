# Native macOS host

This is the design and status of the host that runs VIctory on macOS with no Wine: a macOS process that maps the title's x86-64 PE image itself, binds its imports to `victory-hle`, and runs it. Today the title's code runs as x86-64 under Rosetta in an x86-64 macOS process. The ARM64 static recompiler will plug into the same host interfaces from an arm64 process, and the Metal backend will take over from D3D12 behind `victory-gpu`.

Wine-based work continues on the `test/run-C*` branches. This branch (`native/host-2`) only adds code beside it. On Windows and Wine the loader and runtime behave as before.

## Status

| Piece | State |
|-------|-------|
| `victory-platform`: memory (reserve, commit, protect, query, page pools), threads, events, semaphores, wait-on-address | Done, with a Win32 and a macOS backend and the same conformance tests on both |
| Loader and thunks on the Wine path call `victory-platform` for memory | Done (`windows.rs`, `thunks.rs`, `tls.rs`) |
| `victory-loader::native`: map, relocate, protect, bind, TLS, TEB, calling-convention bridge, run | Done, x86-64 Rosetta host |
| `victory-hle::host_posix`: first kernel32 calls on POSIX | 24 functions; `VirtualAlloc` is 64 KiB granular |
| Probe PE end to end under Rosetta | Done |
| Exceptions: signal handler, vectored handlers, SEH over `.pdata`, `RaiseException` | Done (`native::seh`), probe-tested |
| Threads: `DLL_THREAD_ATTACH/DETACH`, `CREATE_SUSPENDED`, `SuspendThread`/`ResumeThread`/`GetThreadContext` (Mach), `WaitForMultipleObjects`, critical sections, SRW locks, condition variables | Done (`host_posix`, `host_posix/sync.rs`) |
| `xmem` physical pages (`XMemAllocatePhysicalPages`, map, scatter, `map_alias`, DirectStorage writes) on `PagePool`, `modules::xmem_physical` | Done on both hosts: one pool per allocation, one Mach memory entry on macOS; Wine keeps one view per run of consecutive pages. |
| The rest of `xmem` (`XMemVirtualAlloc`, `XMemVirtualQuery`, `XMemAlloc` and the hooks, working-set statistics, transfers), `modules::xmem_posix` | Done on macOS, on `mem::reserve_in` / `next_region` / `footprint`; served from the registry. Hooks are found by reading the mapped title's export table |
| DirectStorage (`modules::dstorage`) | Done on macOS: the scheduler and completion rules are unchanged; file reads are `pread` on its worker threads, destinations go through `write_physical` and `page_window`. Paths go through `host_posix::files::translate`, so virtual drives (`G:`, `T:`) work |
| Files: `CreateFile`, `ReadFile`, `WriteFile`, `SetFilePointer(Ex)`, `GetFileSize(Ex)`, `FindFirstFile`, attributes, virtual drives | Done (`host_posix/files.rs`) |
| Title-shipped DLLs (`native::dll`): msvcp140, vcruntime140, Bink, libHttpClient, ... | Done: mapped beside the exe in dependency order (a DLL is wanted unless our runtime serves its name, so the C++ runtime comes from the title's folder), relocated, own static TLS, bound through the same resolver (hle, then `host_posix`, then the other title DLLs' exports, forwarders followed), `.pdata` added to the exception dispatcher, `DllMain` and TLS callbacks run in dependency order on the main title thread before the exe's own, `DLL_THREAD_ATTACH/DETACH` delivered (`DisableThreadLibraryCalls` honoured), `LoadLibrary`/`GetModuleHandle`/`GetProcAddress`/`GetModuleFileName` see them, late `LoadLibrary` maps, binds and starts a DLL on the calling thread. `--list-imports` binds them too and runs none of their code |
| kernel32 and friends beyond the first slice (`host_posix/system.rs`) | Time and time zones, strings and locales, system information, mutexes, waitable timers, the thread pool, FLS, SLists, `InitOnce`, registry/ETW/COM/crypt32/bcrypt/user32 message stubs. The C runtime (`api-ms-win-crt-*`) is separate |
| `GetModuleHandle`/`LoadLibrary`/`GetProcAddress` on system DLLs (`kernel32.dll`, `api-ms-win-core-*`, `ucrtbase.dll`, ...) | Done (`host_posix/fake_dll.rs`): one stable handle per normalised name, a 64 KiB read-only region holding a minimal PE header (no sections, no exports) so header reads do not crash; `GetProcAddress` (name or ordinal) resolves through the binder's resolver and returns a cached bridge stub (`bridge::stub_for`), NULL with `ERROR_PROC_NOT_FOUND` otherwise; `GetModuleFileName` gives `C:\Windows\System32\<name>`; `FreeLibrary` succeeds |
| Universal C runtime (`api-ms-win-crt-*`, `ucrtbase.dll`) | Done (`host_posix/ucrt.rs`): about 700 exports, see below |
| Winsock (`host_posix/winsock.rs`) | Done: startup/cleanup, sockets (a `SOCKET` is a handle of ours), connect/bind/listen/accept, send/recv/sendto/recvfrom, `select`, `WSAPoll`, `ioctlsocket` (`FIONBIO`, `FIONREAD`), socket options with Windows constants mapped (`SO_LINGER`, timeouts, `TCP_KEEPALIVE`, ...), `getaddrinfo`/`getnameinfo`/`gethostbyname` with Windows layouts, `inet_*`, byte order, `WSAGetLastError` is `GetLastError` with `WSAE*` codes from `errno`. Imports by name and by ordinal (`#n`, Wine's `ws2_32.spec`). Same policy as `modules::net`: loopback only, `localhost` and IP literals only, wildcard binds narrowed. No overlapped I/O |
| WinHTTP (`host_posix/winhttp.rs`) | Done, offline: handles, options, timeouts, headers, status callbacks (`HANDLE_CLOSING` is delivered on close), `WinHttpCrackUrl`; `WinHttpSendRequest` fails with `ERROR_WINHTTP_CANNOT_CONNECT` and the calls that need a sent request fail with `ERROR_WINHTTP_INCORRECT_HANDLE_STATE`. Nothing leaves the machine |
| Media Foundation (`host_posix/mf.rs`) | `MFStartup`/`MFShutdown`/`MFGetSystemTime` work; `MFCreateAttributes`, `MFCreateMediaType`, `MFCreateSourceResolver` and the source readers fail with `MF_E_UNSUPPORTED_BYTESTREAM_TYPE` and a null object, so a title skips its video paths. No COM objects; AVFoundation is the way to a real reader |
| Audio decoder (`xapu.dll`, `modules::apu`) | Done on macOS: the same module as on Windows, its result event is a `host_posix` event handle |
| XAudio2 (`host_posix/xaudio2.rs`, `mixer.rs`, `coreaudio.rs`) | Done for PCM and float sources: `XAudio2Create` (ordinal 1) returns an `IXAudio2` whose methods are `extern "win64"` behind bridge stubs (`native::host_callbacks`), source/submix/mastering voices, buffer queues with loops, volume, channel volumes, output matrix, frequency ratio, voice callbacks (`OnBufferStart/End`, `OnLoopEnd`, `OnStreamEnd`) from a thread of ours. The mixer is plain Rust over float frames; the output is a trait with an AudioToolbox `AudioQueue` (a small FFI, no crate) and a silent clock when there is no device. Not done: effects, filters, submix mixing (a submix is a gain group), ADPCM/XMA/xWMA sources, engine callbacks |
| Graphics objects and window on macOS | Done (`victory-gpu` native host on Metal, see `docs/metal-backend.md`): every object `D3D12XboxCreateDevice` and its methods hand out passes `object::answer`, which gives it a bridged method table (`object::set_bridge`, set by `apps/victory/src/native_run.rs` from `victory_loader::native`; the table's length sits just before its first slot, `ClearDepthStencilView` is `extern "win64"` for its float). Slots nobody wrote log once and answer `E_NOTIMPL`. The runtime's physical-memory observer forwards to `native::pool_observer()`, fence events to `posix::set_event`. `victory run --execute` opens an `NSWindow` (1920x1080, "VIctory") over the host's `CAMetalLayer` on the main thread (`apps/victory/src/window.rs`, AppKit through `objc2::msg_send!`, a hand-driven event loop) while the title runs on its own threads; closing it exits the process. `--headless` presents into an offscreen layer instead |
| Remaining Win32 call sites in `victory-hle` and `victory-gpu` moved to the seam | Not started |

Run the tests with:

```sh
rustup target add x86_64-apple-darwin                 # once
cargo test -p victory-platform                        # arm64 host: memory, threads, sync
cargo test -p victory-platform -p victory-loader -p victory-hle --target x86_64-apple-darwin   # Rosetta host: adds the probe PE
WINE=... tools/macos/test-wine.sh -p victory-platform # the same conformance tests on the Win32 backend
```

`crates/victory-hle/tests/native_probe.rs` builds `tests/fixtures/native-probe/probe.c` with `tools/macos/build-native-probe.sh` (the gnullvm clang and lld, no CRT), maps it, binds it, runs it, and checks 16 things the probe reports in its exit code. The probe also imports a second fixture, `probe_dll.c` (built beside it as `victory-probe-dll.dll`): the test maps it with `native::dll`, and `probe_dll` checks its `DllMain` and TLS callback order, per-thread static TLS, thread notices, an exception raised in the DLL unwinding through its `__finally` to a handler in the exe (and from the exe to a handler in the DLL), and `GetModuleHandle`/`GetProcAddress` by name. The C++ runtime is not ours to build, so the throw is a Windows exception through `__C_specific_handler`, not `_CxxThrowException` through `__CxxFrameHandler`.

## Function pointers we hand to title code, and title pointers we call

Title code runs with GS = its TEB, ours with GS = the host's TSD, and the two ABIs differ, so every pointer crossing needs a bridge.

* **Objects and method tables.** When an HLE object (first word = method table) is first handed to the title, `victory-hle::title::expose` calls `native::expose_object`, which replaces the table word, in place, with a bridged copy: each slot is a bridge stub for the host function in it (`extern "win64"` ones keep their ABI), null slots stay null, and 64 extra slots answer `E_NOTIMPL` and log the area and slot. A title that calls a slot past the end of one of our tables would otherwise read whatever follows the static (often zero: a jump to address 0). Copies are cached per table address. `this` is unchanged: the title gets the object's own address, so methods find their state as before. The price is that Rust must not call an exposed object through its table word (nothing does outside tests, and `expose` does nothing until an image is mapped, so unit tests are unaffected). Applied to `xgameruntime::Object` (`QueryApiImpl`, DirectStorage objects), GameInput hub, devices and readings, XG texture computers and the ACP. `victory-gpu`'s D3D12 objects exist only on the Windows host path today; the Metal backend must expose its objects the same way.
* **Title callbacks.** `victory-hle::title::call(f, args)` calls a callback pointer: through `call_win64` (GS = TEB) when `f` is in a mapped title image, directly when it is one of ours (tests and internal completions pass Rust functions). Used for XTaskQueue and XAsync callbacks, providers and completion routines, monitors, GameInput device callbacks, game-save enumeration callbacks, `XMemAlloc` hooks and the unhandled-exception filter. Thread-pool, `InitOnce`, atexit, qsort, thread procs, TLS callbacks, SEH handlers already used `call_win64`.

## Which Win32 APIs the loader and HLE depend on

Counts are call sites (a call site can be a test). The macOS column is what the native host uses or will use.

### Memory

| Win32 | Where | macOS |
|-------|-------|-------|
| `VirtualAlloc`, `VirtualFree`, `VirtualProtect`, `VirtualQuery(Ex)` | `victory-loader` (`crash.rs` 18, `masked.rs` 12, `tls.rs`), `victory-hle` `xmem.rs` (31), `dstorage.rs`; `victory-gpu` (`watch.rs` 59, `texture.rs` 14, `host.rs` 13, `shadow.rs` 7, `memory.rs`, `list.rs`, `copystat.rs`, `draw.rs`) | `mach_vm_allocate` (reserve), `mach_vm_protect` (commit and protect), `mach_vm_deallocate`, `mach_vm_region` (query) |
| `VirtualAlloc2` with `MEM_RESERVE_PLACEHOLDER`, `MapViewOfFile3`, `UnmapViewOfFile2`, `CreateFileMappingW` (`SEC_RESERVE`) | `xmem.rs`: `XMemAllocatePhysicalPages`, `XMemMapPhysicalPages` and scatter, `XMemVirtualAlloc` | One Mach memory entry per physical-page pool (`mach_make_memory_entry_64`), mapped with `mach_vm_map` at fixed addresses (`VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE`) |
| `MEM_WRITE_WATCH`, `GetWriteWatch`, guard-page tracking with a vectored handler | `victory-gpu/src/watch.rs` | Not needed once the GPU reads title memory directly (zero-copy). Dirty tracking, if wanted, is `mach_vm_protect` plus a Mach exception handler |
| `FlushInstructionCache` | loader patches | no-op on x86-64; `sys_icache_invalidate` on arm64 |
| `LoadLibraryExW(path)` of the prepared image | `windows.rs` | not used: `native::image` maps the PE itself |

Why the Wine path is slow here: one 64 KiB view per page (`MapViewOfFile3`) makes every `VirtualProtect` cost a wineserver round trip. The platform page pool avoids it, since one pool is one VM object and a protect over a range of mappings is one call (tested in `one_protect_call_covers_many_mappings_of_a_pool`).

### Threads and synchronisation

| Win32 | Where | macOS |
|-------|-------|-------|
| `CreateThread`, `GetCurrentThreadId`, `SuspendThread`, `ResumeThread`, `GetThreadContext`, `SetThreadPriority`, `NtQueryInformationThread`, `Sleep`, `SwitchToThread` | loader (`threads.rs`, `stacks.rs`, `crash.rs`), HLE (`kernel.rs`, `apu.rs`, `audio`, `lifecycle.rs`), GPU (`queue.rs`) | pthreads (through `std::thread` for stack size and name), `pthread_threadid_np`, `thread_suspend` and `thread_get_state` on the Mach thread port for suspend and context, `pthread_setschedparam` |
| `CreateEventW`, `SetEvent`, `ResetEvent`, `CreateSemaphoreW`, `WaitForSingleObject`, `WaitForMultipleObjects`, `CloseHandle`, `DuplicateHandle` | HLE (`lifecycle.rs`, `dstorage.rs`), GPU (`fence.rs`, `host.rs`) | `victory_platform::sync::{Event, Semaphore}` behind a handle table (`host_posix.rs`). `WaitForMultipleObjects` across mixed kinds needs one waiter structure: events and semaphores register on it, files and sockets use `kqueue`, timers use `EVFILT_TIMER` |
| `WaitOnAddress`, `WakeByAddress*` | std and title code | `os_sync_wait_on_address` (macOS 14.4 and later) |
| Critical sections and SRW locks | inline in the title's CRT, over `ntdll` | `RtlEnterCriticalSection` and `RtlAcquireSRWLock*` on `os_unfair_lock` or the wait-on-address primitive; not yet reached |
| DLL thread notices (`DLL_THREAD_ATTACH` through a TLS callback registered in `.CRT$XLY`) | `victory-loader/src/threads.rs` | The Windows loader calls each thread's callbacks for us today. Natively `spawn_title_thread` is the one place threads start in title code, so it must send the notices. Not yet done |

### Files

| Win32 | Where | macOS |
|-------|-------|-------|
| 27 path-taking calls (`CreateFileW/A`, `CreateFile2`, `DeleteFile*`, `FindFirstFile*`, `GetFileAttributes*`, `MoveFileExW`, `CreateDirectoryW`, `GetDiskFreeSpace*` and the C runtime's `fopen`, `_wopen`, `_stat64i32`, ...) | `victory-hle/src/modules/files.rs` rewrites drive paths, then calls the host | `open`, `stat`, `opendir`/`readdir` (or `getattrlistbulk`), `rename`, `statfs`. Paths go through the same drive rewrite; wide to UTF-8 conversion is the new part |
| `ReadFile`, `WriteFile`, `SetFilePointerEx`, `GetFileSizeEx`, `FlushFileBuffers`, overlapped I/O with events or completion ports | `files.rs`, `dstorage.rs`, `storage.rs` | `pread`, `pwrite`, `fcntl(F_NOCACHE)`; overlapped I/O on a worker pool, or `kqueue`/`dispatch_io` for DirectStorage's queues |
| `CreateFileMappingW` on files, `NtQueryVolumeInformationFile` | `dstorage.rs`, `files.rs` | `mmap`, `fstatfs` |
| `CreateHardLink` | blob cache (`victory-gpu`) | `link` (APFS has no 1024-link limit; see `docs/macos.md`) |

### Exceptions: SEH and vectored handlers

| Win32 | Where | macOS |
|-------|-------|-------|
| `AddVectoredExceptionHandler` | `crash.rs` (reporter), `watch.rs` (write-guard faults: `EXCEPTION_ACCESS_VIOLATION`, `EXCEPTION_CONTINUE_EXECUTION`), `masked.rs` (resumes patched stores) | `sigaction(SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGTRAP)` with `SA_SIGINFO | SA_ONSTACK`, translating `ucontext` into a Windows `CONTEXT` and `EXCEPTION_RECORD`. Where signals are too coarse (a fault inside `os_sync`, debugger interplay), a Mach exception port with `EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES` and `thread_get_state` |
| `SetUnhandledExceptionFilter`, `RaiseException` | `kernel.rs`, `crash.rs`, title CRT | Last in the handler chain; `RaiseException` becomes a `longjmp`-free walk of the same chain on the calling thread |
| `RtlLookupFunctionEntry`, `RtlVirtualUnwind`, `RtlCaptureContext`, `RtlAddFunctionTable`, `RtlCaptureStackBackTrace` | `crash.rs` (stack walks), title SEH and C++ exceptions | Pure computation over the image's `.pdata` and `UNWIND_INFO`; the walker in `crash.rs` already does it from bytes and moves over. Windows' dispatch (`RtlDispatchException`, `RtlUnwindEx`, `__C_specific_handler`) must be ours, since the OS no longer does it |

Known Rosetta constraint, from `docs/macos.md`: a fault inside a masked or `vcvtps2ph` store cannot be resumed, so `masked.rs` patches those stores. The same patching applies natively.

### TLS

| Win32 | Where | macOS |
|-------|-------|-------|
| Static TLS (`gs:[0x58]` array, `_tls_index`, TLS directory, TLS callbacks) | `victory-loader/src/tls.rs` redirects exe loads; `victory-pe` reads the directory | `native::teb` builds the array and per-thread blocks from each image's TLS directory and assigns each image its own index, so the redirect is not needed |
| `TlsAlloc`, `TlsGetValue`, `TlsSetValue`, `TlsFree`, `FlsAlloc`... | title and C runtime | `host_posix.rs`: 64 slots at `TEB+0x1480`. `Fls*` and the expansion slots are not done |
| `GetLastError`, `SetLastError` | everywhere | `TEB+0x68` |

### Time

| Win32 | Where | macOS |
|-------|-------|-------|
| `QueryPerformanceCounter` and `Frequency`, `GetTickCount(64)` | HLE `runtime.rs`, `xgameruntime.rs`, `kernel.rs`, GPU `queue.rs` | `mach_absolute_time` with `mach_timebase_info` (or `clock_gettime_nsec_np(CLOCK_UPTIME_RAW)`); `host_posix.rs` uses `Instant` with a 1 GHz counter |
| `GetSystemTimePreciseAsFileTime`, `GetLocalTime` | HLE | `clock_gettime(CLOCK_REALTIME)` and the 1601 epoch offset |
| `Sleep`, waitable timers | HLE | `nanosleep`; `EVFILT_TIMER` for waitable timers |

### Not in these groups, still needed

Window and input (`CreateWindowExW`, message loop, XInput; `lifecycle.rs`, `gameinput.rs`) go to AppKit and `GameController`. Media Foundation video (`media.rs`) would go to AVFoundation. (Sockets, audio and the audio decoder are done: see the table above.) Crypto (`bcrypt`, `crypt32`) goes to CommonCrypto and Security. These are each separate pieces of work, after the core is up.

## The platform seam

`crates/victory-platform` has one API and two backends, chosen by `cfg`. It is plain functions and types, not a trait: nothing needs to be generic over the OS at run time, and the title-facing code calls the same names either way.

```rust
// mem: all unsafe, all return io::Result
page_size() / granularity()               // macOS: the page size; Windows: 64 KiB
reserve(at: Option<usize>, len)           // no access; `at` exactly or fail
alloc(at, len, Protect)                   // reserve and commit in one step
commit(addr, len, Protect)                // inside a reservation
protect(addr, len, Protect) -> Protect    // returns the previous protection
decommit(addr, len)   release(addr, len)  // Windows: the whole reservation
query(addr) -> Option<Region>             // VirtualQuery / mach_vm_region
PagePool::new(len)                        // one OS memory object
pool.map(offset, len, at, Protect)        // map any part of it, anywhere or at `at`
unmap(addr, len)   flush_icache(addr, len)
// thread
spawn(name, stack_size, FnOnce() -> u32) -> Thread    Thread::{id, join}    current_id()    sleep()
// sync
Event::{new(manual, initial), set, reset, wait(timeout)}    Semaphore::{new, release, wait}
wait_on_address(&AtomicU32, expected, timeout)    wake_one    wake_all
```

`Protect` wraps the raw OS value (`PAGE_*` or `VM_PROT_*`) so that a value `VirtualProtect` returned can be passed back as it came; use the constants (`READ`, `READ_WRITE`, `READ_EXECUTE`, ...).

Two documented differences between backends. On Windows `PagePool::map` at an address needs that range free (it makes a placeholder to map over), where on macOS it replaces what is there, as `MAP_FIXED` does. And Windows addresses are 64 KiB granular where macOS ones are page granular (4 KiB under Rosetta, 16 KiB natively); `host_posix::VirtualAlloc` makes up for it by reserving a granule more and giving the ends back.

What moved so far: `windows.rs` (IAT slot writes), `thunks.rs` (import stub block) and `tls.rs` (thunk page, code patching) use `mem`, and so does the physical-page half of `xmem`. The Wine tests of `victory-loader` and `victory-hle` pass unchanged. The rest follows the order of the tables above: `crash.rs` and `masked.rs` (protect and query), then threads and events in HLE and GPU, then `watch.rs`, which the native host drops.

`PagePool` also has: `base()` (the pool's permanent whole mapping; lazy on Windows, where it can fail for a huge pool), `window(offset, len)` (bytes through `base`, or a view of their own when `base` failed), `map_over(offset, len, at, prot)` (map over part of the caller's reservation, a placeholder on Windows), `mem::unmap_over` (back to no access, or a placeholder), `new_lazy` (Windows: reserve only, commit with `mem::commit`) and, on macOS, `memory_entry()`.

Under Wine `xmem` still maps one view per run of consecutive pages the title passes in one call, so a title that maps page by page (scatter, `CopyPageMappings`) still pays one view per 64 KiB. Merging those needs a mapping model that is not page granular, which the write-guard code (`watch.rs`) assumes; left as it was.

## The native loader

### Mapping (`native::image`, any macOS host)

`NativeImage::map` parses the PE (`victory-pe`), reserves `SizeOfImage` at the preferred base (`0x1_4000_0000` for the probe, as for a console title) or anywhere if that is taken, copies the headers and sections, and applies base relocations (`DIR64`, `HIGHLOW`). `finalize` sets page protections from section flags once imports are bound, taking the union where a host page holds two sections. The preferred base only works if it is free; the host binary sits at `0x1_0000_0000` and must stay below about 1 GiB. Nothing here runs title code or depends on the CPU, so the recompiler's arm64 host maps images the same way.

### TEB and GS under Rosetta (`native::teb`)

Title code reads the TEB through `gs:`: `gs:[0x30]` (self), `gs:[0x58]` (static TLS array), `gs:[0x60]` (PEB), `gs:[0x10]` (stack limit), `gs:[0x68]` (last error), `gs:[0x1480+]` (TLS slots). What was found out, by experiment (`M5 Pro`, macOS 27, x86-64 process under Rosetta):

1. Rosetta gives an x86-64 process the macOS ABI: `gs:` is the pthread's thread-specific data (TSD) array, with `pthread_self()` in slot 0. Slots 6, 11 and 12 (the offsets `0x30`, `0x58`, `0x60`) read as zero in a bare process, but 10 and up are libSystem's own pthread keys, 8 bytes each. A dynamic key starts at 258. Writing title data into those slots would collide with libc as soon as it uses them. Slot 1 (`0x08`, stack base) is `errno` and slot 2 (`0x10`, stack limit) is the Mach reply port, which are in use.
2. Rosetta implements the machdep call `thread_fast_set_cthread_self` (syscall `0x3000003`, `rdi` = the new GS base) for x86-64 guests, from which Wine's macOS port gets its own TEB. It works from inline assembly, with no entitlement. A pair of calls costs about 370 ns (190 ns each), measured in a loop.
3. The base to restore is not `pthread_self()`: it is the address of the TSD array inside the pthread structure, `pthread_self() + 0xE0` here on the main and secondary threads alike. Restoring `pthread_self()` itself as the base breaks `pthread_setspecific` and the process dies later in unrelated code. The offset is not an API: the structure also holds a stray word equal to `pthread_self()` at `+0xB0`, which a first version of the search took, and which crashed the process at thread exit. `host_tsd_base` therefore takes the address whose first 12 words equal what `gs:[0..96]` reads.
4. `rdgsbase` was not tried; the base is found by the search above.

Decision: each thread has two GS bases, the host's (pthread TSD) while host code runs and its own TEB while title code runs, swapped at each crossing by the bridge. The alternatives and why they lose:

| Option | Cost | Why not |
|--------|------|---------|
| Write TEB fields into free TSD slots, no swap | none | `0x08`, `0x10` and `0x58`/`0x60` collide with `errno`, the reply port and libSystem keys; `0x1480` (TLS slots) lands among dynamic pthread keys |
| Patch every `gs:` access in the title (as `tls.rs` and `masked.rs` patch) | patch time; the game has about 53,000 `gs:[0x58]` loads alone | Fragile, and the recompiler will make it moot: it lowers `gs:` to a register-relative access |
| Swap per crossing (chosen) | 370 ns per host call and per callback | Simple and exact; every `gs:` offset works. The cost is the first thing the recompiler removes |

At 370 ns a crossing, 100,000 host calls per frame cost about 37 ms. The Wine path pays far more per call (see the profile in the task), and a title calls the host mostly for `Sleep`, events, memory and the console API, not per draw. Cheaper paths for hot calls (a TEB-only function such as `GetLastError` or `TlsGetValue` can be inlined into the stub with no swap) are a next step.

`ThreadTeb` (in `teb.rs`) is a `0x2000`-byte block with the fields above, `StackBase` and `StackLimit` from `pthread_get_stackaddr_np`, `ClientId`, a pointer to the thread's TLS array, a shared fake PEB (`ImageBaseAddress` at `0x10`), and, at `0x1F00` (unused by Windows), the thread's host GS base for the bridge to swap back to. Every host thread that calls into title code gets one on first use (`teb::current`).

### Static TLS

`register_tls` gives each image with a TLS directory the next index, writes it to the image's `_tls_index` (the TLS directory's `AddressOfIndex`), and records the template. Each thread's TEB gets an array `[block_for_image_0, block_for_image_1, ...]`, each block a copy of the template plus zero fill, so `mov rax, gs:[0x58]; mov rax, [rax + _tls_index*8]; add rax, SECREL(var)` works for an exe as for a DLL (the Wine path needs `tls.rs` to redirect exe loads because Windows gives an exe index 0 by rule; here there is no such rule). Images must be registered before threads that use them start; a thread made earlier has no block for a later image. The probe checks that a thread gets a fresh copy (`100`, not the main thread's `101`), through `CreateThread`.

### The calling-convention bridge (`native::bridge`)

Title to host (`victory_bridge_to_host`, reached through a 32-byte stub per import: `mov r10, target; mov r11, context; mov rax, bridge; jmp rax`):

- the stub keeps the Windows return address and shadow space; the bridge saves `rbx`, `rdi`, `rsi`, `r12`-`r14` and `xmm6`-`xmm15` (the Windows ABI makes these the callee's to keep, the System V ABI does not), spills `rcx`, `rdx`, `r8`, `r9`;
- it switches GS to the host base held at `TEB+0x1F00`, loads `rdi, rsi, rdx, rcx, r8, r9` from the four spilled registers and the first two stack arguments, copies stack arguments 7 to 12, and calls the target with `al = 0`;
- it switches GS back to the TEB, restores the saved registers and returns. `rax` and `xmm0` pass through unchanged, since both ABIs return there.

The bridge covers up to 12 integer or pointer arguments. A function with `float` or `double` arguments cannot be remapped without its signature, because the two ABIs number the float registers differently once an integer comes first. Those are written `extern "win64"` and registered with `declare_win64(address)`; they go through `victory_bridge_win64`, which swaps GS and nothing else (up to 8 arguments; floats stay in `xmm0..3`). The probe does this with `ProbeScale(double, int)`. A float return needs nothing.

An unresolved import gets a stub whose context word is its `ImportSite`; the bridge hands the site to the handler in place of the first argument, which logs (`ReturnZero`) or aborts (`Trap`), the same policies as the Wine path. The probe checks the names reach the report and that `ReturnZero` returns 0.

Host to title (`call_win64(fn, &[u64])`): four register arguments, the rest on the stack above 32 bytes of shadow space, 16-byte alignment, GS to the thread's TEB, call, GS back. Used for TLS callbacks, entry points, thread start routines and every callback title code hands the host. It nests: title, host, title again, with an explicit base each time rather than a toggle. A test (`the_bridge_preserves_windows_callee_saved_registers`) runs hand-assembled Windows-ABI code that sets `rdi`, `rsi` and `xmm6`, calls a host function that clobbers all three, and checks they survive.

### Imports and the HLE

`native::bind_imports` takes an `ImportResolver` (the same trait the Wine path uses, so `victory_hle::HleResolver` works unchanged), writes a stub address into each IAT slot (title exports are bound directly), and reports counts. `HostPolicy` on macOS resolves allowed DLLs (`kernel32`, `kernelbase`, `api-ms-win-core-*`) to `victory-hle::host_posix`, which is the first slice of "the host's own Win32, reimplemented on POSIX": `GetLastError`, `SetLastError`, ids, time (`QueryPerformanceCounter`, `GetTickCount(64)`, `Sleep`), `VirtualAlloc/Free/Protect`, `TlsAlloc/Free/GetValue/SetValue`, events, semaphores, `WaitForSingleObject`, `CloseHandle` and `CreateThread` (title threads), over a handle table. Everything else lands in the unimplemented-import stub, named. This module is x86-64 only for now because it reads the TEB; once the recompiler defines its thread context it moves behind a `current_teb()` that both hosts provide.

The HLE functions are `extern "system"`, which is System V on macOS (x86-64 and arm64 alike), so no change to the 100+ existing HLE modules is needed to bridge them. The ones that compile on macOS today (the portable modules: `kernel`, `xgameruntime`, `gameinput`, `d3d12x`, `audio`, `dsp`, `xg`, `rpcrt4`) are in the registry; `files`, `xmem`, `lifecycle`, `net`, `media`, `dstorage`, `apu`, `xsystem` and `fidelityfx` are `cfg(windows)` and are the porting backlog.

## Interfaces for the Metal backend

Goal: `victory-gpu`'s Metal backend wraps title memory pages as `MTLBuffer` objects with no copy and no shadow.

1. **One VM object per physical-page pool.** `XMemAllocatePhysicalPages` should create one `PagePool` per pool (instead of one section with a 64 KiB view per page). A `PagePool` is one Mach memory entry from `mach_make_memory_entry_64(MAP_MEM_NAMED_CREATE)`: anonymous, zero filled, committed on first touch. `XMemMapPhysicalPages` and `XMemVirtualAlloc` map ranges of it into the title's address space with `PagePool::map(offset, len, Some(address), prot)`. Any number of mappings, at any addresses, of any part of it are the same physical pages.
2. **Stable addresses.** A mapping stays at its address until the title unmaps or remaps it. The title's virtual addresses are the CPU's virtual addresses (identity), so a GPU address the title holds is already a pointer. Remapping a range (`XMemMapPhysicalPages` over mapped pages) replaces the pages at that address, which is the moment a backend must drop and rebuild any `MTLBuffer` over it (`title_memory_remapped` in `victory-gpu`).
3. **Alignment.** Map at and in multiples of 16 KiB (`vm_kernel_page_size`, also under Rosetta, where `getpagesize()` says 4 KiB). Checked on an M5 Pro with macOS 27 (a short Objective-C program, not in the repository, built for arm64 and for x86-64 under Rosetta): `newBufferWithBytesNoCopy` over a pool mapping returns a buffer whose `contents` is the same pointer, for the whole mapping, for a 4 KiB-aligned and a 16 KiB-aligned sub-range, and for a mapping of a sub-range of the pool at another address. A write through a buffer over mapping B is seen through mapping A at once (one VM object). Only the 16 KiB multiple is guaranteed to stay valid on other OS versions.
4. **What the backend may assume.** Memory is `MTLStorageModeShared`. CPU and GPU see one copy, so the shadow copies and the copy-back disappear, and so does `victory-gpu/src/watch.rs` (write-guard tracking and its page-tracking lock). Ordering is by command buffers and fences only: a CPU write the title makes before it submits is visible to that submission, and the title's own fences order the rest. Create buffers with `hazardTrackingModeUntracked`; the title already tracks hazards.
5. **Sizes.** `device.maxBufferLength` bounds one `MTLBuffer`; wrap each pool mapping (or each large allocation's mapping) separately and keep an address-to-buffer interval map keyed by the mapping's start. Pool mappings that are contiguous in address are still separate VM entries, so a buffer cannot span two mappings; the backend should ask for allocations large enough that they are one mapping (`XMemVirtualAlloc` of a range is).
6. **Protection.** The CPU side can set a mapping `READ` while a buffer over it is live; `PagePool::map(.., Protect::READ)` makes a read-only alias (for CPU-side guards or for the GPU's read-only use) of pages another mapping writes.

Provided (`victory_hle::modules::xmem_physical`, branch `native/host-mem`):

- Every `XMemAllocatePhysicalPages` makes one pool (`PagePool`). `pools()` lists them as `PoolInfo { id, base, len, memory_entry }`: `base` is the pool's own read-write mapping of all of it, 16 KiB aligned (also under Rosetta), made with the pool and never moved, protected or unmapped while it lives; `memory_entry` is the Mach entry (a send right the pool owns).
- `set_observer(Some(Arc<dyn Observer>))` delivers `pool_created`, `mapped(at, len, &PoolInfo, pool_offset)`, `unmapped(at, len)` and `pool_released`, synchronously and under the page-table lock (no calls back into `xmem`). A backend that starts late wraps `pools()` and then hears the rest. On Windows the same events fire, next to `victory_gpu::title_memory_remapped`.
- Title mappings are `mach_vm_map` of a run of the pool over the title's reservation (`FIXED | OVERWRITE`), one call per run. Unmapping a page leaves no access, and cuts a larger mapping in two without touching the rest (no event for what stays). A title address is an alias only: the buffer over `base` serves every address showing that pool, from the table the events build.
- Two points differ from item 4 above. A pool is not process-lifetime: when the title has freed every page and unmapped every view, `pool_released` fires and `base` is unmapped right after it returns, so the backend must drop its buffer there (a pool kept for good would keep the memory of a freed 6 GiB allocation). And `base` is not itself a title address: the GPU address of a page is its title address, translated through the table.
- Only 64 KiB aligned title addresses are accepted by `XMemMapPhysicalPages`, as on Windows.

## Interfaces for the recompiler

The recompiler produces arm64 code for the title's functions and runs it in an arm64 host process with the same loader, runtime and HLE. What it can rely on:

1. **Guest addresses are host addresses.** `NativeImage::map` is arch-neutral and maps the image at its preferred base (the arm64 host binary must also stay clear of `0x1_4000_0000` and up; use a linker `-image_base` or a custom segment layout if it does not). Title pointers can be passed to HLE functions as they are, and `PagePool` mappings give the same identity for GPU and physical-page memory. Recompiled code is keyed by guest RVA, so the `.pdata` function table, vtables, jump tables, and the IAT stay valid.
2. **Calling an HLE entry point.** Today the registry holds addresses (`registry()` returns `HleExport { name, address }`) of `extern "system"` Rust functions: System V on x86-64, AAPCS64 on arm64, both the host's C ABI. The recompiled caller reads the Windows x64 argument registers from its virtual register file (`rcx, rdx, r8, r9`, then the stack above the 32-byte shadow space), calls the function with them as native integer arguments (`x0..x7`, then the stack), and writes `rax` from `x0` (and `xmm0` from `d0` for a float return). No GS swap, no register saving beyond what recompiled code does for its own state. To do this for floats the recompiler needs each export's signature class; add `sig: &'static str` to `HleExport` (for example `"pp,i->i"`), filled by `hle_export!` from the Rust signature, with the `extern "win64"` exports marked. Until then only integer and pointer signatures can be called.
3. **Imports.** `bind_imports` and `ImportResolver` are reused as they are; the recompiler gives the resolver its own stub generator instead of `StubBlock` (a native call stub for an arm64 host, or a direct branch).
4. **TLS and the TEB.** Recompiled code lowers `gs:[off]` to `[teb_base + off]`, with `teb_base` in a reserved register or in the thread context struct passed to every recompiled function. The layout is the one in `native::teb` (`OFF_*`): `0x30` self, `0x58` TLS array, `0x60` PEB, `0x68` last error, `0x1480` TLS slots, `0x08` and `0x10` stack bounds. `teb::register_tls` and the per-thread block construction (`ThreadTeb::new`) move to be arch-neutral; they have nothing x86-specific except the `host_tsd_base` search, which an arm64 host does not need (there is no second GS base, so the bridge's swap disappears, and a host call is a plain call).
5. **Threads.** Title threads start through `spawn_title_thread`, which the recompiler replaces with a version that allocates the thread's context, sets the stack, and enters recompiled code at the start routine's address.
6. **Exceptions.** A fault in recompiled code arrives as `SIGSEGV` or `SIGBUS` with a host PC. The recompiler gives the host a side table from host PC ranges to guest RIP and a way to rebuild the guest register file (as `Mach` thread state or from the saved context), so the host can run the vectored-handler chain with a Windows `CONTEXT` and `EXCEPTION_RECORD`, and either resume (`EXCEPTION_CONTINUE_EXECUTION`: re-enter at the recompiled block for the faulting guest instruction) or unwind. C++ and SEH unwinding use `.pdata` and `UNWIND_INFO` over the guest context (the walker in `crash.rs` is portable and moves into `native`), and handler and `__finally` addresses are guest addresses of recompiled functions. `RaiseException` and the HLE's own failures enter the same dispatcher.
7. **Self-modifying and patched code.** `masked.rs` patches (stores Rosetta cannot resume) are unneeded under the recompiler, which lowers them itself.

## Exceptions, threads and files on macOS (`native::seh`, `host_posix`)

**Signals, not a Mach exception port.** A SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGTRAP handler (on the thread's alternate stack) tells title code (GS = TEB) from host code (GS = TSD) by `gs:[0x30]`, copies the thread state into a Windows `CONTEXT` and `EXCEPTION_RECORD` on the thread's own stack, and redirects the thread to `victory_exception_entry`. Nothing runs in signal context, there is no second thread or `thread_get_state`, and the faulting registers are exact. The entry switches GS to the host base, then `dispatch` runs the vectored handlers, then walks the frames (`unwind::lookup`/`virtual_unwind`, the x64 unwinder incl. epilogues and chained info) calling each language handler (`__C_specific_handler` is ours: scope tables, `__except` filters, `__finally` via `RtlUnwindEx`), then the unhandled filter. Title handlers run through `call_win64` (GS = TEB, host base restored after); continuing or unwinding ends in `victory_restore_context`, which sets the TEB last. `RtlCaptureContext` and `RaiseException` are Windows-ABI assembly bound with no bridge (`declare_direct`). Access violations carry the read/write flag and address (`info[0]`, `info[1]`); macOS's `si_code` numbers for SIGFPE differ from Linux's. A fault in host code is not dispatched (chained to Rust's handler, then default); that and debugger interplay are what a Mach exception port would add.

The probe must be built with `-fasynchronous-unwind-tables -fexceptions -Xclang -fasync-exceptions` (gnullvm clang supports `__try/__except/__finally`; without async EH clang drops the `__except` around plain loads and stores).

Limits: `RtlUnwindEx` unwinds from the exception being dispatched on the thread (a call from title code outside a dispatch returns); C++ handlers (`__CxxFrameHandler`) are not provided; unwinding stops at host frames (a host function that calls back into title code is not crossed); no stack-overflow handling; `SetThreadContext` is not done.

**Threads.** `spawn_title_thread_with` sends `DLL_THREAD_ATTACH` before and `DLL_THREAD_DETACH` after the start routine to every image registered with `register_thread_callbacks` (`run_entry` registers the main image). `CREATE_SUSPENDED` holds the thread at a gate; once running, `SuspendThread` is `thread_suspend` on the Mach port, retried briefly until the thread is in title code (so it holds no host lock), and `GetThreadContext` is `thread_get_state` (x86 state under Rosetta works). Waits on several objects use one epoch counter that every signal bumps; wait-all takes under a lock and gives back what it took if it cannot take all. Critical sections, SRW locks and condition variables live in the title's own structures over `wait_on_address`. Not done: mutexes (`CreateMutexW`), waitable timers, APCs, `SetThreadContext`, thread priorities.

**Files.** `host_posix/files.rs` mirrors the calls `modules/files.rs` hands to the host's kernel32 on Windows, on `std::fs`: drives are mounted folders (`mount`, `mount_at`), a drive path is resolved under its folder (`..` stops at the root), other paths just get `/` for `\`. The Windows path is untouched. Persistent storage (`storage.rs`) mounts its folder through it on macOS. Overlapped reads complete before returning. The C runtime's `fopen`/`_wopen`/`_stat64` are in `host_posix/ucrt` (below). Not done: `MoveFileEx`, `GetDiskFreeSpace`, `CreateFile2`, file mappings, share modes.

**dstorage on files.** `open_for_reading` calls `host_posix::files::translate`. dstorage signals no Win32 events today (completion is the status array; the fence methods are not implemented), so nothing there needs `sync::signalled`; when fences are added, a host event set by them must be followed by `sync::signalled` so waits on several objects see it. Still open: `FileExt::read_at` on the worker pool, no `kernel_will_write` once the GPU shares memory, `mmap` paths through `victory-platform`.

## Running a title natively (`victory run` on macOS)

`victory run [--unimplemented trap|zero] [--strict] <exe>` maps the PE with `native::NativeImage`, registers its TLS, sets `runtime::set_main_image`/`set_image_path`, binds imports (unresolved ones get logging stubs that act on `UnimplementedPolicy` when *called*; loading never stops, and a per-DLL summary is printed), and stops. `--list-imports` also prints every import as `native (hle|host|title)` or `STUB`; `--execute` mounts `G:` (the title's folder) and `T:` (temp under the data dir), sets the title and console, and starts the main thread with `native::run_entry`. `--list-imports` and no-flag runs never execute title code. The host executable is linked with `-pagezero_size 0x1000` (`apps/victory/build.rs`): the default 4 GiB `__PAGEZERO` is the whole range 32-bit `XMemAlloc` results must come from. Not done: the title's own DLLs (`load_title_dlls` equivalent), the window, GPU and audio.

## Next steps, in order

1. Merging Wine's views for scatter maps. (`xmem` and `dstorage` are ported to macOS; a 32-bit `XMemAlloc` or `XMemVirtualAlloc` finds no address on macOS until the host executable is linked with a small `-pagezero_size`, since the default 4 GiB `__PAGEZERO` is the whole low range; and a committed page without access reports as reserved to `XMemVirtualQuery`.)
2. (done) Exceptions on macOS. Left: C++ exceptions (`__CxxFrameHandler`), a Mach exception port for host-code faults and debuggers, stack overflow.
3. (done) Thread notices, suspension, waits, locks. Left: mutexes, timers, `SetThreadContext`.
4. Files: `MoveFileEx`, disk space.
5. Move the remaining `cfg(windows)` HLE modules to compile on macOS with their Win32 calls behind `victory-platform`: `lifecycle` (window, `NSWindow`), `net`, `apu`/`audio` (CoreAudio), `media`.
6. Cheaper crossings: inline the TEB-only host calls (`GetLastError`, `TlsGetValue`, `GetCurrentThreadId`) in the stubs; batch crossings for hot calls.
7. Add `sig` to `HleExport`, generate bridge stubs from it (float arguments through the System V bridge, a recompiler-facing description).
8. Boot the real title loader path on macOS: `prepare`/`load_title_dlls` equivalents for the title's own DLLs, `runtime::set_main_image`, the `apps/victory` front end on macOS (`victory run --native`).
9. Metal backend against `PagePool`, and the recompiler against `NativeImage` and the bridge's register-file calling rule above.

### The C runtime (`host_posix/ucrt`)

`api-ms-win-crt-*.dll` and `ucrtbase.dll` resolve to Rust functions over libSystem, tried before kernel32's table. `vcruntime140*` and `msvcp140*` are not served by name: the title's own copies (C++ runtime, `__CxxFrameHandler`) are loaded from its folder; only `vcruntime140`'s kernel-table functions (`__C_specific_handler`) fall back to the host when the title ships none. The HLE `exit` runs the `atexit`/`onexit` handlers and flushes streams before ending the process on this path (`abort`/`terminate` do not). A title linked against the dynamic UCRT needs nothing else of it. The rules that matter:

- `wchar_t` is two bytes: wide strings are UTF-16 and the host's `wcs*` is never called. Narrow strings are UTF-8; `mbstowcs` and `wcstombs` convert, and fail with `EILSEQ` on a bad sequence or a lone surrogate.
- `printf` and `scanf` are our own engine (`ucrt/format.rs`), not macOS's. `__stdio_common_v{s,sn,f}{w,}printf[_s]` and `...v{s,f}{w,}scanf` read the Windows `va_list` (a pointer to 8-byte slots, floats as doubles), honour the MSVC option bits (standard `snprintf` behaviour, legacy NUL termination, ISO wide specifiers) and the `_s` buffer rules, and format `%ls %hs %S %C %I64d %I32d %zu %p %a`, `inf`, `-nan(ind)` as the CRT does.
- A function with a `float` or `double` argument is `extern "win64"` and declared to the bridge (`declare_win64`) when looked up; everything else is `extern "system"`. `lldiv` returns through the hidden pointer Windows passes in `rcx`.
- Callbacks (`qsort`, `bsearch`, `_initterm`, `_execute_onexit_table`, `signal`/`raise`, the invalid-parameter handler) are title functions called with `call_win64`. `qsort` is a merge sort over a copy, so an inconsistent comparator cannot panic the host.
- `FILE*` is a leaked 16-byte block the title never reads; behind it is a stream shared with the descriptor `_fileno` returns, over `files::translate` paths (so `G:\` works). Reads are buffered, writes are not. Text mode drops `\r` before `\n` on reads and writes `\n` unchanged.
- `errno` is per thread (`_errno()`), with the Windows numbers (`EDOM` 33, `ERANGE` 34, `STRUNCATE` 80). The math functions set `EDOM` for a NaN from numbers and `ERANGE` for an infinity from finite ones.
- `rand` is the CRT's linear generator, per thread, seed 1: `srand(1)` gives 41, 18467, 6334 as on Windows.
- `argv`, `argc`, the command line and the environment come from the host process, or from `victory_hle::posix::set_command_line` (call it before the title's start-up code runs).

Locale is "C" only (`setlocale` rejects anything else; `localeconv` is the C locale's). A failed invalid-parameter check calls the title's handler if it set one and returns the error; it does not terminate. `_endthread(ex)` is not served (it needs an exit primitive in the loader's thread spawn); `_get_osfhandle`/`_open_osfhandle`, `_ecvt`/`_fcvt`/`_gcvt`, positional `%1$d` formats and the `_mbs*` multibyte-string family are not either. `ucrt::tests::every_ucrt_import_of_a_dry_run_is_served` checks a `--list-imports` output when `VICTORY_UCRT_IMPORTS` names it. The probe (`probe_ucrt` in `tests/fixtures/native-probe/probe.c`) links `ucrtbase.dll` and `api-ms-win-crt-math-l1-1-0.dll` and runs 17 checks under Rosetta.

## Known limits of this slice

- Only x86-64 hosts run title code; `native::image` also builds and tests on arm64.
- `host_posix` (kernel32 side) is a few dozen functions, enough for the probe, not a title; the C runtime is complete enough for a title that links it dynamically.
- `VirtualAlloc` reserves 64 KiB granules at 64 KiB boundaries; `VirtualFree(MEM_RELEASE)` takes a base from `VirtualAlloc` only.
- TLS images must be registered before the threads that use them; thread notices go to every registered image on every title thread.
- A `PagePool::map` over an existing mapping replaces it silently on macOS.
- The relocated-image test picks a free address and maps there a moment later; another thread could take it in between (it has not).
- The probe's `.tls` handling is hand-written (`_tls_used`, `_tls_start`, `_tls_end`), since it links no CRT. A real MinGW or MSVC title brings its own.

## Bring-up diagnostics (native path)

Off by default, environment variables of the host process:

- `VICTORY_TRACE_FAILS=1`: every title-to-host call of a bridged method table (the COM-style objects) whose result in `eax` is negative is logged with the object area, the slot offset, `this` and the title's calling address as an offset into the image (`from`). A method returning a pointer or a count shows up as noise. `=2` logs every such call.
- `VICTORY_TRACE_IMPORTS=1`: the same for every import bound to a host function (name and DLL), whatever the result. Slow (millions of lines in a title's startup).
- `VICTORY_PEEK=rva,rva,...`: at the first-chance exception log, prints those qwords of the faulting image (an operand above `0x10000000` is an absolute address), the qwords `rcx` points at, and a call chain by the unwind tables. Use it with `llvm-objdump` on the title (analysis only).

The first-chance log (`seh::dispatch`) shows a breakpoint's `rip` as the `int3` itself, as Windows x64 does.

Instructions Rosetta does not run are skipped or emulated by the SIGILL path in `seh::signal`: `clwb`/`clflushopt` (skipped), `clzero` (zeroes the line), `monitorx`/`mwaitx` (skipped, the latter yields).

## Console pipeline descriptions (device slots 0x1c8 and 0x218)

Observed at run time (`VICTORY_DUMP_DESC=1` dumps the first descriptions and the bytes behind every pointer and size pair). Grand Theft Auto VI creates most of its pipelines through these two device methods, `(this, desc, iid, out)` with the pipeline state's IID.

* **0x218, compute.** `desc` is a root signature object pointer and one 0x60-byte record: pairs (pointer, size) of the pipeline registers (0x1c bytes), the hash (0x14), the GFX10 code and the metadata, then two pointers and a 64-bit hash. These are the entries of kinds 0, 1, 6 and 7 of the `FSPX` layout `victory_shader::native::fspx_program` reads, so the host builds that part and a container (`pipeline::create_compute_pieces`) and the compute path translates it.
* **0x1c8, graphics.** `desc` is a root signature pointer, then stage slots of 0x160 bytes from +0x08. A slot holds 16-byte (pointer, size) pairs: pair 0 a register image (0x17c to 0x1d8 bytes, starting `08d80540` or `00d80540`), pair 1 a blob that holds an input signature (element count, then rows with the semantic names, "POSITION", "COLOR", "TEXCOORD", as an `ISG1` part does), then up to three (code, metadata) pairs at pairs 2/3, 4/5 and 6/7, which of them filled varies. At slot+0x108 there is a block of host pointers, `0x0401000000000160` (the slot size), a 40-byte hash and `0x8000000000000000`. The metadata is the compute metadata with a 0x10-byte header, `u32 2, u32 size - 0xc, u32 (0x76 seen), u8 0x40, u8 first user-data dword, u8 count of 0x41 records, u8 0`, then the same 32-byte tagged records (0x41 resource, 0x42 view declaration, 0x43 hash). The slot layout is therefore known; what is not: which of the code pairs is the vertex shader, the pixel shader or a variant, how the register image maps to the stage records (`SPI_SHADER_*`, `SPI_PS_INPUT_*`, `DB_SHADER_CONTROL`), where a pixel shader's export patches are, and the graphics state (blend, rasterizer, formats), which is not in the description. Until the translator takes these pieces, `pipeline::create_unread_1c8` hands out a pipeline with no shaders and its draws are dropped.

**The graphics state is device method 0x1b8.** Found by `VICTORY_TRACE_MISSING=<bytes>` (every unwritten slot logs its first calls, `VICTORY_TRACE_MISSING_CALLS=n` more, with its arguments and the bytes behind pointers): the title calls device slot 0x1b8 about 42,000 times at start-up, `(this, base, count, records, iid, out)`, `base` a pipeline 0x1c8 made, `iid` the pipeline state's, and uses the result as the pipeline it binds (`VICTORY_TRACE_BIND=1` counts binds by kind). Each of the `count` (1 to 3) records is 83 dwords, a tag then the D3D12 structure of its kind: 0 `D3D12_DEPTH_STENCIL_DESC`, 1 `D3D12_RASTERIZER_DESC`, 2 `D3D12_BLEND_DESC` (tag 5 is rare and not understood). A record left out means D3D12's default. The register image (pair 0) holds no blend or depth state (it has `CB_COLOR_CONTROL` and the shader registers). Target formats are in no record: a draw takes them from the targets `OMSetRenderTargets` bound, and the topology from `IASetPrimitiveTopology`. `native::console_state` decodes the records into `native::state::Graphics` (`apply`, `graphics_state_for`), `variant.rs` is the device method, and `glue::pipe` makes a console pipeline again for each set of targets. Command lists have no Xbox-specific state methods (every slot the title calls is written), and the title writes no packets into list memory while our lists keep it on the call path. `VICTORY_TRACE_STREAM=1` shows that the stream pipelines (device 0x2b0, PC style) carry all their state, and `VICTORY_DUMP_STATE=1` dumps what the host pointers at the end of each 0x1c8 slot point at (not understood).

Labelling the code pairs (by scanning the code for `exp` and `v_interp` encodings, 12 graphics descriptions, `VICTORY_DUMP_DESC_DIR=<dir>` writes every piece to a file): pair 2/3 (`c0`) is the pixel shader (it has `v_interp`, and `exp` to mrt0 and others), pair 4/5 (`c1`) the vertex shader (`exp pos0` and `exp param`), and pair 6/7 (`c2`) a second vertex shader (more exports, larger: probably the variant that runs with a geometry stage or as a primitive shader). A slot with no `c0` is a pipeline without a pixel shader (depth only). So a description is a root signature and one slot per pipeline, the slots after the first being the next pipelines of an array the title keeps (as for compute, whose descriptions are 0x68 apart). What is still open is the vertex input mapping (the signature blob says which semantics, not which VGPRs), the pixel shader's export patch source, and the register image fields.
