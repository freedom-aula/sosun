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
| XAudio2 (`host_posix/xaudio2.rs`, `mixer.rs`, `coreaudio.rs`) | `XAudio2Create` (ordinal 1) returns a bridged `extern "win64"` `IXAudio2`. PCM and float source voices support buffer queues, loops, volume, channel volumes, output matrices and frequency ratios. The float mixer runs roughly 10 ms passes on the CoreAudio `AudioQueue` thread or the silent clock. Each pass calls registered engine start callbacks in order, source voice start callbacks with the byte shortfall, then voice and engine end callbacks after mixing. `StopEngine` pauses passes and `StartEngine` resumes them. Buffer, loop and stream notices still use the callback worker. A runtime queue enqueue failure raises `OnCriticalError`. `OnVoiceError` awaits a decoder that can fail during mixing. Effects, filters, full submix mixing and ADPCM/XMA/xWMA sources remain future work. |
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
- `VICTORY_PROFILE=<secs>`: times every title-to-host call (imports and the methods of bridged objects: count, total and longest, by `rdtsc` in the bridge) and logs the 40 with the longest total every that many seconds. Time inside a call includes waiting for the host's locks and any title code it calls back, so methods that are long everywhere (all of `0x118`, `0x428`, `0xc0` at about 150 ms) point at a lock someone else holds (here the native host's `TITLE` lock, held by `sync_textures`).
- `VICTORY_TRACE_IMPORTS=1`: the same for every import bound to a host function (name and DLL), whatever the result. Slow (millions of lines in a title's startup).
- `VICTORY_KEYS=<seconds>:<key>,...`: on macOS the only input there is (the host source has no keyboard or pads yet): each key is held for 0.2 s that long after `GameInputCreate`, and drives the stand-in pad (`Enter` is A, `Esc` is Menu; letters, digits, `Tab`, `Back` and the arrows as in the README). A headless run needs it to get past the title's engagement screen (below); `VICTORY_KEYS=$(seq 4 2 80 | sed 's/$/:Enter/' | paste -sd, -)` presses Enter every 2 s.
- `VICTORY_DUMP_THREADS=0`: the thread dump (and `VICTORY_DUMP_MEM`, `VICTORY_FIND_QWORD`) is taken when the title first hits an `int3` (its asserts), from that thread, instead of after a time.
- `VICTORY_FIND_QWORD=<hex>,<hex>,...` (with `VICTORY_DUMP_THREADS`): logs every aligned qword of resident readable memory equal to one of the values, with the 0x100 bytes from it; `@<hex>` stands for the qword at that address. Finds heap objects by their method table (data only).
- `VICTORY_PEEK=rva,rva,...`: at the first-chance exception log, prints those qwords of the faulting image (an operand above `0x10000000` is an absolute address), the qwords `rcx` points at, and a call chain by the unwind tables. Use it with `llvm-objdump` on the title (analysis only).

The first-chance log (`seh::dispatch`) shows a breakpoint's `rip` as the `int3` itself, as Windows x64 does.

Instructions Rosetta does not run are skipped or emulated by the SIGILL path in `seh::signal`: `clwb`/`clflushopt` (skipped; a whole `clwb [p]; add p, 0x40; cmp p, limit; jbe/jb` flush loop is skipped in one signal, since the title flushes buffers of many megabytes a line at a time), `clzero` (zeroes the line, or the whole clearing loop), `monitorx`/`mwaitx` (skipped, the latter yields), and the SSE4a `insertq`/`extrq` (run on the saved XMM registers; the title uses `insertq` once its world loads).

## Console pipeline descriptions (device slots 0x1c8 and 0x218)

Observed at run time (`VICTORY_DUMP_DESC=1` dumps the first descriptions and the bytes behind every pointer and size pair). Grand Theft Auto VI creates most of its pipelines through these two device methods, `(this, desc, iid, out)` with the pipeline state's IID.

* **0x218, compute.** `desc` is a root signature object pointer and one 0x60-byte record: pairs (pointer, size) of the pipeline registers (0x1c bytes), the hash (0x14), the GFX10 code and the metadata, then two pointers and a 64-bit hash. These are the entries of kinds 0, 1, 6 and 7 of the `FSPX` layout `victory_shader::native::fspx_program` reads, so the host builds that part and a container (`pipeline::create_compute_pieces`) and the compute path translates it.
* **0x1c8, graphics.** `desc` is a root signature pointer, then stage slots of 0x160 bytes from +0x08. A slot holds 16-byte (pointer, size) pairs: pair 0 a register image (0x17c to 0x1d8 bytes, starting `08d80540` or `00d80540`), pair 1 a blob that holds an input signature (element count, then rows with the semantic names, "POSITION", "COLOR", "TEXCOORD", as an `ISG1` part does), then up to three (code, metadata) pairs at pairs 2/3, 4/5 and 6/7, which of them filled varies. At slot+0x108 there is a block of host pointers, `0x0401000000000160` (the slot size), a 40-byte hash and `0x8000000000000000`. The metadata is the compute metadata with a 0x10-byte header, `u32 2, u32 size - 0xc, u32 (0x76 seen), u8 0x40, u8 first user-data dword, u8 count of 0x41 records, u8 0`, then the same 32-byte tagged records (0x41 resource, 0x42 view declaration, 0x43 hash). The slot layout is therefore known; what is not: which of the code pairs is the vertex shader, the pixel shader or a variant, how the register image maps to the stage records (`SPI_SHADER_*`, `SPI_PS_INPUT_*`, `DB_SHADER_CONTROL`), where a pixel shader's export patches are, and the graphics state (blend, rasterizer, formats), which is not in the description. Until the translator takes these pieces, `pipeline::create_unread_1c8` hands out a pipeline with no shaders and its draws are dropped.

**The graphics state is device method 0x1b8.** Found by `VICTORY_TRACE_MISSING=<bytes>` (every unwritten slot logs its first calls, `VICTORY_TRACE_MISSING_CALLS=n` more, with its arguments and the bytes behind pointers): the title calls device slot 0x1b8 about 42,000 times at start-up, `(this, base, count, records, iid, out)`, `base` a pipeline 0x1c8 made, `iid` the pipeline state's, and uses the result as the pipeline it binds (`VICTORY_TRACE_BIND=1` counts binds by kind). Each of the `count` (1 to 3) records is 83 dwords, a tag then the D3D12 structure of its kind: 0 `D3D12_DEPTH_STENCIL_DESC`, 1 `D3D12_RASTERIZER_DESC`, 2 `D3D12_BLEND_DESC` (tag 5 is rare and not understood). A record left out means D3D12's default. The register image (pair 0) holds no blend or depth state (it has `CB_COLOR_CONTROL` and the shader registers). Target formats are in no record: a draw takes them from the targets `OMSetRenderTargets` bound, and the topology from `IASetPrimitiveTopology`. `native::console_state` decodes the records into `native::state::Graphics` (`apply`, `graphics_state_for`), `variant.rs` is the device method, and `glue::pipe` makes a console pipeline again for each set of targets. Command lists have no Xbox-specific state methods (every slot the title calls is written), and the title writes no packets into list memory while our lists keep it on the call path. `VICTORY_TRACE_STREAM=1` shows that the stream pipelines (device 0x2b0, PC style) carry all their state, and `VICTORY_DUMP_STATE=1` dumps what the host pointers at the end of each 0x1c8 slot point at (not understood).

Labelling the code pairs (by scanning the code for `exp` and `v_interp` encodings, 12 graphics descriptions, `VICTORY_DUMP_DESC_DIR=<dir>` writes every piece to a file): pair 2/3 (`c0`) is the pixel shader (it has `v_interp`, and `exp` to mrt0 and others), pair 4/5 (`c1`) the vertex shader (`exp pos0` and `exp param`), and pair 6/7 (`c2`) a second vertex shader (more exports, larger: probably the variant that runs with a geometry stage or as a primitive shader). A slot with no `c0` is a pipeline without a pixel shader (depth only). So a description is a root signature and one slot per pipeline, the slots after the first being the next pipelines of an array the title keeps (as for compute, whose descriptions are 0x68 apart). What is still open is the vertex input mapping (the signature blob says which semantics, not which VGPRs), the pixel shader's export patch source, and the register image fields.

## Guessed graphics pipelines (`VICTORY_GUESS_GRAPHICS=1`)

Without ground truth for the 0x1c8 register image, `VICTORY_GUESS_GRAPHICS=1` builds pipelines from what the code and metadata say; without it nothing changes (pipelines have no shaders, draws are dropped). Every item is a guess:

* **Which pieces.** Pixel shader: pair 2/3. Vertex shader: pair 4/5, or pair 6/7 when 4/5 is empty (most descriptions: a large pixel shader, empty 4/5, the vertex shader in 6/7). Only the first slot of a description is read. Container: `victory_shader::emit::graphics::container` (an `FSPG` part, code and metadata, plus `ISG1`/`OSG1` made from the inference below), read by `native_programs`.
* **Metadata.** v2 records start at +0x10 and share `native::metadata_resources` with compute (kind 7). Root constants (root type 0, observed as b500, the sampler index) are one SGPR; a `SamplerHeap` parameter is added one SGPR after the last declared user data (pixel shaders only; s9 constant, s11 heap, s12 primitive mask).
* **Vertex shaders** fetch for themselves: v0 is the vertex id, `s[0:1]` the table of 16-byte vertex buffer descriptors (`s_load` before `buffer_load`/`tbuffer_load` with `idxen`), base vertex and instance (s2, s3) read 0. The translator turns such a load into a stage-in element `VB<F|U|N><n>` (float, uint, unorm8x4; n channels) of index `slot << 16 | byte offset`; the host maps those to attributes of vertex buffer `slot`, with the stride the title's `IASetVertexBuffers` gave (the `system_gaps` placeholders keep the converter's numbering by signature position). One index per vertex: no per-instance streams. Other formats are refused. Outputs: `exp param<n>` is `TEXCOORD<n>` of register n (channels as enabled), `SV_Position` last.
* **Pixel shaders.** Attribute n is `TEXCOORD<n>` (identity with the vertex parameters), channels as read; flat when only `v_interp_mov` reads it. `SPI_PS_INPUT_ADDR` is inferred from the VGPRs read on entry: perspective centre (bit 1) when it interpolates, then position x.. front face (bits 8-12). The register image was not decoded, so no field was checked against these. Colour exports `exp mrt<n>` go to `SV_Target<n>`; a compressed export is taken as FP16_ABGR (two halves per dword, as `v_cvt_pkrtz_f16_f32` makes them); no export patches. Wave size is 32 when the code names `exec_lo`/`vcc_lo`.
* **Root layout.** Each stage is converted with the host's layout of its arguments (`Binding::layout`: user data inline, then root views, then the page table), as compute is; with its immediate samplers as static samplers.
* **State.** Blend from the register image (`Graphics::guess_from`, below); no culling, depth less-equal with writes, formats from the targets bound at the draw, strides from the vertex buffers bound; the host pipeline is made at the draw (`Pipe::Guess`, cached per formats and strides). Topology 17 (rectangle list) is drawn as a triangle list, so half of each rectangle: the fourth corner needs a geometry stage.
* **Vertex user data.** If a v2 vertex stage has a nonzero first user-data dword and a 0x41 register index below it, try those record indices relative to the header's start. Accept that layout only when it resolves an otherwise unbound address to a declared table or root view and agrees with existing root-address uses. The title's root slot and descriptor table offsets come from the record. Absolute indices remain the default until the register image is decoded.
* **Pixel primitive mask.** Scalar masks and shifts may feed M0 for interpolation. When the resulting value and SCC have no other use, the inferred primitive-mask SGPR anchors the existing POPS collision-id mapping in the next SGPR.
* **Computed pixel buffers.** Scalar and vector dword loads through linear descriptors with a known control word use the pool page table at the computed address. The descriptor supplies the stride, record count and bounds mode. The Metal host binds that table for the fragment stage, as for compute. The translator requires proof that the descriptor's swizzle flags are clear.
* **Missing vertex stage.** When both vertex piece pairs are empty, skip the slot. A vertex-only slot can still draw depth. Mesh pipelines still need their stage registers decoded. Pixel shaders with opaque lane masks, system inputs the signature lacks, and vertex fetches with other formats are refused (see the frame log).
* **Debugging.** `cargo run -p victory-shader --example disasm` (`VICTORY_PIECE=<piece>`) disassembles a dumped code piece; `--example guess` (`VICTORY_STAGE=vs|ps VICTORY_CODE VICTORY_META`) runs the guessed translation on a pair.

### Why guessed draws leave the target black (findings)

Tools (all off by default): `VICTORY_TRACE_TARGETS=1` logs per present which title textures were drawn to or cleared (address, format, size) and which was presented, plus texture copies and resolves the host drops; `VICTORY_DUMP_TARGETS=<dir>,<seconds>` writes the 8-bit colour targets drawn since the last present as PPMs; `VICTORY_DEBUG_PS_RED=1` makes every translated pixel shader write red, `VICTORY_DEBUG_VS_TRI=1` makes every translated vertex shader position a triangle covering the target (translator debugging, `emit/lower.rs`).

* The first frames are 3840x2160, one MRT draw per frame into the swapchain buffer, a second RGBA8 target and a depth target; no texture copy or resolve was ever dropped, no UAV or compute work touches them before frame ~700. The presented buffer is the one drawn to.
* Host textures are separate host allocations (`Textures::realize`), not views of pool memory; only sampled textures are uploaded from title memory, render targets never are.
* With the real shaders every target dumps all black. With `VICTORY_DEBUG_PS_RED` and `VICTORY_DEBUG_VS_TRI` together, a target dumps solid red: pipeline, targets, formats, viewport (3840x2160) and the draw path work. With only `VICTORY_DEBUG_PS_RED` it stays black (the real vertex shader's triangle covers no pixel); with only `VICTORY_DEBUG_VS_TRI` it stays black (the real pixel shader writes black or transparent there). So two things remain: the vertex shader's position (its matrix comes from a constant buffer read through a descriptor table, likely zeros) and the pixel shader's sample (its texture and sampler).
* The title draws rectangle lists (topology 17) from vertex buffers of 24-byte vertices (float3 position, unorm8x4 colour, float2 uv): fullscreen rectangles in clip space (-1, 1). Only the half-rectangle triangle is drawn; a proper expansion is not done.
* `descriptor_table_buffers_decode_into_metal_heap` also fails on c542e9d (title address not in a mapped pool): not caused by this work.

#### The fullscreen quad draws (trace of what they bind)

`VICTORY_TRACE_TARGETS=1` also logs, for the first draws with vertex buffers, the root parameters and the views of their descriptor tables (`native/host.rs`, `trace_draw`). Observed at start-up (frames 2 to ~700), per frame three fullscreen draws (a 3-vertex triangle with no vertex buffer, and rectangle lists of 24-byte vertices) into 3840x2160 RGBA8 targets:

* The descriptor tables hold the title's own view records (`vVIE` tag, view index, generation; 32 bytes per entry), not GFX10 descriptors. For the quad's pipeline the table is three entries: a placed depth view (R32, format 39), a 2D SRV of a 3840x2160 RGBA8 render target (the previous pass's output), and a 480-byte constant buffer whose first rows are the identity matrix. The pipeline's metadata offsets (SRV at 0 and 8, CBV at 16, in dwords) are these entries at the host's 32-byte step, so the vertex shader's matrix is the identity and the binding is right; the SGPR-above-s1-reads-0 guess does not zero anything here.
* The sampled render target and the dumped targets are all zero: every pass of these frames samples a target that only these passes (or clears to black) write. No mesh dispatch and no compute work were seen in these frames (dispatch and draw counters agree), so nothing ever puts content into that chain. The black is therefore consistent with a real black input, not with a binding error found so far; the scene is drawn by pipelines this path does not run (mesh dispatches are dropped on the native host) or later than the frames examined.

#### Rectangle lists, dropped-command counters, variants (guessed path)

* Rectangle lists (topology 17) of a guessed pipeline are drawn as instances of four vertices (a triangle strip): the vertex shader is translated again with `Options::rect_list`, which reads each fetched attribute for the rectangle's three corners (stage-in elements of corner 0 to 2, index `corner << 28 | slot << 16 | offset`, instance-stepped with three times the bound stride) and picks a corner by vertex id, the fourth being the two other corners less the right-angle one (so only affine vertex shaders are right; a uint attribute takes a stored corner; see "Rectangle-list corners" below). Indexed rectangle lists are dropped.
* The 64-frame log line counts dropped command kinds: `mesh`, `mesh_indirect`, `indirect`, `texture_copies`. Frames 2 to ~1000 show 0 of each: no mesh dispatches, indirect executions, texture copies or resolves, in the boot frames. The Bink DLL (`bink2scarlett.dll`) is loaded at start-up, but no compute dispatch or texture copy happens before frame ~695 (so no GPU or upload-based video decode before then); calls into its exports are title code and are not counted.
* State variants (device slot 0x1b8, `native/gfx-state`) of a guessed base inherit its shaders; their decoded blend/raster/depth state replaces the guessed defaults, and `Graphics::for_draw` adds the inferred vertex layout and bound targets. With them every draw of the boot frames has a host pipeline (`no_pipeline=0`), still all black through frame ~1000.
* Keep large-frame code out of `PresentX`'s call chain (`queue.rs`, `glue::present`): enlarging its stack frame crashed the title under Rosetta ("unexpectedly need to EmulateForward"); the draw-counter log and the present trace are `#[inline(never)]` helpers for that reason.

#### The sampled target's title memory, and the compute at frame ~600

* `VICTORY_DUMP_TITLE=<dir>,<s>,<s>` reads the title memory behind each 8-bit colour target drawn to, whole, and detiles it into a scratch texture. At 20 s and 120 s every one is entirely zero (33,423,360 bytes, 0 non-zero), including the 3840x2160 RGBA8 target the quad passes sample. So the black chain is not a CPU, DMA or DirectStorage write the host misses.
* Render-target textures are nevertheless watched now (`textures.rs`, the scanner of `native/perf`): a change of their title memory after its first sighting is uploaded.
* The first compute dispatch (frame ~580) is groups [5,5,33] of 8x8x1 writing a 33x33x(33) R16G16B16A16_FLOAT unordered view (`VICTORY_TRACE_TARGETS=1` logs dispatches): a colour-grading LUT, not video or a splash.

#### The int3 at image+0x4dcbc9d (analysis, `native/perf`)

* It is an overflow assert in an append routine (`0x4dcbc40`: used + length past a 0x18000-byte capture buffer), reached when a profiling scope ends (`0x4e529a0`, started in `0x5ab12e0` with a 32 KiB stack buffer and the scope hash `0xf0a244ef`). The scope is one iteration of the main thread's boot wait loop (`0x21b7070`): update the main loop (`0x21b7970`), `Sleep(33)`, wait 100 ms on a semaphore (`0x5ab12e0`), test again. Each iteration appends 0x6c0 bytes, so the buffer holds 56 and the 57th asserts (about 8 s). It is not a clock limit: running QPC and the tick counts at half speed (`VICTORY_CLOCK_SLOW=2`) moves nothing.
* The loop ends when `0x41e8c0(&0x1478ac9c8, [0x1478acba0])` is true: record `[0x1478acba0]` (4) of a ring of 0x50-byte records at `0x1478ac9c8` has a non-zero dword at +0x18 (`0x1478acb20`). Other threads stamp their own record's first dword each time they poll (the two counters that grow 1000 a second are such stamps). `VICTORY_TRAP_WRITES=<page>,<addr>,...` (the page is kept read-only; title writes fault, are logged with the title call chain and resumed) shows nothing ever writes `0x1478acb20` or the other records' +0x18, and the cell `0x1478acba0` only once, at static initialisation (`0x222db2a`). A host write would have crashed on the read-only page. So the code that sets the flag never runs here, or sets it before the trap starts and something clears it.
* Ruled out as the flag's writer: frame statistics (device 0x288 now answers; they feed a different ring at `0x1475de4b0`), GPU list writes (all 2.6k land in pools, none in the image), fences, DirectStorage, and the shared semaphores (the two semaphores of the loop are never released; they are only sleeps).
* Diagnostics added: `VICTORY_WATCH_MEM`, `VICTORY_DUMP_MEM`, `VICTORY_DUMP_STACK`, `VICTORY_DUMP_REGS`, `VICTORY_TRAP_WRITES`, `VICTORY_TRACE_HANDLES`, `VICTORY_WATCH_HANDLE`, `VICTORY_TRACE_WRITES`, `VICTORY_CLOCK_SLOW`.
* The "mark ready" function is `0x145862370(this = ring, &cell)`: if the ring parent gate (`0x1458623e0` on `[ring+0x1a0]`, null for this ring) allows, it stores the tick (`0x144e4ba80`) into `[ring + cell*0x50 + 0x18]` when `[ring+0x190] == cell`. Its callers are `0x140dcbc08`, `0x141cfc349`, `0x1421b741e`, `0x1421b751d`, `0x142207863`, `0x142355d6d`, `0x1427ed3bb`, `0x14289e5b2`, `0x1429df27f`, `0x1458622b4`; those passing another object (`0x147bd2140`, cell `0x147bd2318`) are other rings of the same class. For ring `0x1478ac9c8` the only caller is `0x142355d6d`, in slot 12 (`0x142355d40`) of the class whose vtable is `0x1461b2790` (constructor `0x145a8fbe0`); slot 13 (`0x142355dd0`) resets the ring. Slot 12 is never called in a run (the write trap sees no store); who calls it, and on what event, is not found yet. The title app state is not it: its state byte (`0x1471b9048`, context `0x1471b8c78`) is already 2 (running) at 10 s, and delivering the app-state and constrained-change routines the title registers (`0x142217c70`, `0x142217c90`, later `0x1455f8720`) with "running" did not set the flag (the state routine blocks waiting for a handshake bit, by design).
* Frame count or time: presents delayed by 100 ms (`VICTORY_PRESENT_DELAY_MS=100`) still hit the int3 at the same wall time (about 18 s) at frame 64-128 instead of 384, so the boot wait runs on time (its iterations), not on frames. Bink: `movies.rpf` is opened (title RPF device, `CreateFileW`) but never read, by `ReadFile` or DirectStorage (`VICTORY_TRACE_FILES=1`, `VICTORY_TRACE_DSTORAGE_FILES=1`), and no XAudio2 engine, voice or buffer is created before the int3, so the wait is before the boot movie starts, not a stalled movie. `VICTORY_TRACE_CALLBACKS=1` lists every title code address passed to a host import or method and not called by the host when `VICTORY_DUMP_THREADS=<s>` fires (7,655 registered, 38 called in the first 10 s; most are timestamps in structures, so read it with a filter for names such as Register, Create and Set).

* **Found (native/perf):** the wait is the title's engagement screen, a front-end state waiting for a button press. The state object (method table `0x1461b2790`, inside a wrapper with table `0x1461bc490`, state name hash `0xbaab6434`, found on the heap at the int3 with `VICTORY_FIND_QWORD=1461b2790` and `VICTORY_DUMP_THREADS=0`) has run slot 13 (enter): its listener at +0x80 points at itself and is registered for UI event 0xe of the event hub `[0x147f00518]` with handler `0x142355ef0`, which takes menu actions (up, down, toggle, accept). Slot 12 (exit, which marks ring `0x1478ac9c8` ready) is then left to the player accepting (inferred: not traced to the call). The macOS input source had no keys and no pads, and `VICTORY_KEYS` was Windows-only, so nothing ever accepted; under Wine the runs pressed Enter (`VICTORY_KEYS`). With Enter pressed (`VICTORY_KEYS`, now served on macOS) the wait ends, the main thread leaves the wait for the frame loop at `0x1421b7530` (it runs while state `0xbaab6434` is current), the world loads (`core_restofworld.rpf`), and the run reaches frame ~1400 at about 53 s. After it, in the same run: `insertq` at `0x1402e7ff1` (now emulated), a texture view with more levels than its texture (Metal asserts; views are now cut to the texture), the render thread in `clwb` flush loops at one signal per line (now one signal per loop), and then a Metal command buffer GPU time-out at about 53 s (`kIOGPUCommandBufferCallbackErrorTimeout`, a fence stuck 4 behind) that stops presents: the next problem, on the GPU side.

#### What the boot waits on (20 to 50 s runs, `VICTORY_PROFILE`, `VICTORY_TRACE_FAILS`)

* The boot is not waiting on sign-in, store, packages or a missing file: the title's GDK calls are `InitializeApiImplEx` and GameInput only, no failed file open is logged, nothing polls an XAsync block. Calls by time: `WaitForSingleObject` (70k calls in 18 s over many threads), `Sleep`, `d3d12` slot 0x1c8 (pipeline creation, 23k), 0x1b8 (state variants, 41k), `qsort`, string compares.
* The one long poll is DirectStorage: the title loads about 9,500 requests (`G:\data\ttdb\...ttpf`, the `.rpf` archives) through one queue and polls entry 0 of its status array in a `Sleep(1)` loop (20,000 polls in 20 s, `dstorage status array` slot 0x18 returning `E_PENDING`). The queue is not stuck (`VICTORY_*` log of its lanes: reads 3,361 falling to 27, no failures, `held 0`), it drains at about 20 MB/s with workers 90% idle (the title submits in small batches); it finishes at about 30 s into a run (frame ~770 on the fast build). Right after it, the draw count jumps from 3 to about 21 per frame and new pipelines appear: that is the first content-producing work, and with the shader fixes merged it encodes (frame 1024: recorded 5749, encoded 5747).
* The title then hits its int3 (the perf agent's) at frame ~850 to ~1030, before a frame dump of the new work could be taken.

#### Extra vertex outputs, periodic dumps, first content draws

* Guess: a pixel shader's `TEXCOORD` n that the vertex shader does not write is declared as an (undefined) vertex output (`Options::extra_params`, made at the draw by `Shader::translate_extra`), as the converter refuses a pipeline whose fragment inputs the vertex stage lacks. After it every draw of the boot has a host pipeline (`no_pipeline=0` to frame ~900).
* `VICTORY_DUMP_TARGETS=<dir>,<seconds>,<every>` dumps repeatedly (files `t<n>_<second>.ppm`).
* First content draws (frame ~830 on, ~20 per frame into a 3840x2160 RGBA8 target with depth, after the DirectStorage load): with the real shaders the target stays black; with `VICTORY_DEBUG_PS_RED=1` a red band covers the bottom ~9% of the target, its right edge slanting (a ground-like surface seen from a camera: real geometry reaching the screen through the real vertex shaders). So the vertex path draws; the real pixel shaders write black there (probably sampling textures not streamed in yet, not examined).

#### The content draws (`VICTORY_TRACE_CONTENT=<dir>[,<seconds>[,<format>]]`)

* The ~20 draws per frame after the DirectStorage load (frame ~830) sample no texture at all: each has a descriptor table of constant buffers only (64 or 1536 bytes, 62 to 460 non-zero bytes: colours such as (1,1,1,1) and (0.976,...), scale and offset values like 3.05e-5, 0.748 and -0.92) and two root constants [0, 0]. They look like a batch of screen-space quads (UI, a fade or a mask pass), not world geometry.
* The boot passes (3 per frame) sample the 3840x2160 RGBA8 target these draws write (`0x73c7520000`), which is all zero including its alpha: so everything downstream stays black as long as these draws write (0,0,0,0).
* With the real pixel shaders the target and its alpha are 0 everywhere; with every pixel shader forced to (1,0,0,1) a band shows, so coverage, blend, depth and formats let pixels through. The cause is the real pixel shaders' output (their math, an input the vertex shader supplies, or the undefined extra vertex outputs), not yet identified.
* `D32_FLOAT_S8X24_UINT` (DXGI 19) now maps to the host's depth-stencil format; before, the scene's depth target had no host texture, so its draws ran without depth.

#### Rectangle corner order, a content pixel shader, and the depth suspect

* Rectangle lists come as top-left, bottom-left, bottom-right (observed in the title's vertex data), so the missing corner is `v0 + v2 - v1`, and the strip is v0, v1, missing, v2 so both triangles wind alike (the first try, `v1 + v2 - v0` last, covered half the target: the second triangle was back-face culled by the console's rasterizer state). With every pixel shader forced red the rectangle passes now cover 100% of the target.
* A content pixel shader (`825a5674`): `rgb = clamp(attr0.xyz * attr2.yzw + attr1.yzw)`, `a = clamp(attr0.w * attr3.x + attr2.x) * attr1.x`, one compressed FP16 `exp mrt0`, no kill. Its vertex shader (`2927a7de`) exports param0 = vertex colour (unorm8x4), param1 = (an R8 byte at offset 7, then constants), params 2 and 3 constants, position = a matrix from the constant buffer times the vertex's float2 at offset 8 (values near +-32768, scale 3.05e-5). The vertices are 16-byte, indexed (u16, 30 to 4500 indices per draw) with white or dark colours, the constants near (1,1,1,1): so the expected output is vertex colour, i.e. white UI geometry, and nothing in the shaders explains zero.
* Forcing the vertex parameters 0 to 3 to 1.0 (`VICTORY_DEBUG_VS_PARAM=0,1,2,3`) with the real pixel shader still gives black targets, and with the pixel shader forced red the content draws showed a band only in the run before the depth-stencil format mapping (DXGI 19) was added, not after. So the prime suspect is the depth test now active on these draws (their depth state comes from 0x1b8 variants or the guessed default less-equal; a reversed-Z title cleared to 0 with a less-equal test passes nothing). A `VICTORY_DEBUG_NO_DEPTH=1` run was inconclusive (all dumps black, boot passes included: the run probably died early).

#### Split pixel inputs, depth, video (guessed path)

* The merged `lower.rs` splits a pixel input read two ways (perspective and flat) onto a fresh semantic index. That index was the "texcoord4" the vertex shader never wrote; declaring it as an undefined output made every such input read zero, which is why the content pixel shader (`rgb = colour * constant + constant` with the constant inputs flat) wrote (0,0,0,0). `HostShader::splits` now reports (original, new) indices and `Options::copy_params` makes the vertex shader write the new index with the original's values (`Guessed` passes them at the draw).
* No Bink video before frame ~1000: the draws of frames 695 to 830 sample no texture that changes (the boot passes sample the 4K target, the content draws sample none), and the only compute is the LUT.
* Debug switches added: `VICTORY_DEBUG_PS=white|alpha`, `VICTORY_DEBUG_PS_INPUTS` (not wired after the merge), `VICTORY_DEBUG_NO_DEPTH`, `VICTORY_DEBUG_NO_DEPTH_ATTACH`, `VICTORY_DEBUG_VS_PARAM`. `MTL_DEBUG_LAYER=1` overflows the title threads' stacks (a Metal call on a title thread), so it cannot be used; command buffers that fail now log `metal: command buffer failed`.

#### The logo, and why the content draws are invisible (constant buffer reads zero in their vertex shader)

* Debug switches `VICTORY_DEBUG_PS=in<N>` (raw pixel input N as the colour), `VICTORY_DEBUG_ONLY_INDEXED=1` (skip the non-indexed boot passes, so the content target holds the content draws alone) and `VICTORY_DEBUG_VS_POS=1` (vertex positions from the raw vertex xy times 1/32768 instead of the constant-buffer matrix).
* Content draws alone, real constant buffers: nothing is drawn even with every pixel shader forced red, so their vertex positions are degenerate. With `VICTORY_DEBUG_VS_POS=1` they draw: a dark background quad and then a large white vector logo (a "Y" crossing an ellipse and an "X", the first content the title shows), filling the target at that scale (the real matrix scales it down and places it).
* The vertex shader's constant-buffer dwords read zero on the GPU although the title memory holds them at submission (`VICTORY_TRACE_CONTENT`): `in2` (param 2 = s11..s14, expected (0,1,1,1)) is black; `in1.x` (the vertex's R8 byte) is correct. The boot passes' constant buffers (an identity matrix, 480 bytes) read fine with the same machinery, so the open difference is these content draws' tables (1536-byte buffers in a table at heap index ~680k to 980k, two root constants [0,0]), or the memory being reused before the GPU reads it. (Resolved below.)

#### First real frame: two causes of the zero constant buffers (resolved)

* **Root layout.** Guessed render pipelines gave the converter the host's root layout (`Binding::layout`) only for stages that read title memory. Without it the converter makes the user-data block a root CBV, a pointer in the argument buffer, while `translated_args` writes the dwords inline; so every guessed vertex and pixel shader read its user data (descriptor table offsets, root constants) through the address formed by user-data dwords 0 and 1, here 0. Seen by converting the translated vertex shader both ways and reading the AIR (`llvm-dis` on the bitcode in the metallib): without a root signature it loads a pointer from the argument buffer, with one it reads dword 8 inline. Compute pipelines always had the layout. Now render stages get it too. A stage with immediate samplers needs them in the layout (the converter refuses it otherwise, "Resource SMP ... not referenced by the root signature"): `RootLayout::static_samplers` carries the decoded `StaticSampler`s (`s<n>`, space 0) and `msc.rs` passes them as the root signature's static samplers. With it the boot passes' pixel shader (`9406103f`, which samples the content target) runs, and the presented image shows the menus.
* **Copied descriptor records.** The title copies view records into its shader-visible heap with plain stores, which no device call reports: at submission the host heap entry of a content table was all zero (read back from the Metal heap) while the title's record named the constant buffer. `NativeHost::remirror_tables` now checks, at each draw and dispatch, the first 16 entries of every bound table against the record each was last mirrored from (`NativeHeap::mirrored`) and mirrors the ones that changed (once per record). Shortcut: entries past the 16th are not checked, and views of resources the host has not realized yet are not made there.
* Ruled out on the way: the pool view of the constant buffer equals the title's bytes (no aliasing problem for the low, sub-4 GiB linear allocator), no mirror errors for it, the table's heap is the heap bound, residency (`VICTORY_METAL_NO_RESIDENCY_SETS=1` changes nothing).
* With both, a normal run (no debug switch) draws the content target: a dark (32, 32, 44) background and the white button prompts "ACCOUNT PICKER (Y)  SCREEN NARRATION (OFF) (X)  PLAYER (A)" at the bottom right, placed and scaled by the real matrix (the "logo" under `VICTORY_DEBUG_VS_POS=1` was these glyphs at 1/32768 scale). With static samplers in the layout the presented buffer shows the same frames (scratchpad `presented-finish-setup.png`).
* Texture formats added for the menus: DXGI 2 (`R32G32B32A32_FLOAT`), 56 (`R16_UNORM`), 61 (`R8_UNORM`) and 65 (`A8_UNORM`, a 2048x2048 UI atlas). Views of them had no host texture before. With them, runs reach the legal-notices menu (about 90 s with `VICTORY_KEYS`). Still open: the body font's x-height-only letters (a c e m n o r s u v w x z) draw as filled boxes, while letters with ascenders or descenders are right. The title never calls `ClearDepthStencilView`, and stencil-only clears would be dropped anyway (`glue::clear_dsv` acts on flag 1 only), so stencil-then-cover text was the first suspect; ruled out since: no pipeline enables stencil (`VICTORY_TRACE_PIPES=1` logs each guessed pipeline's depth, stencil and blend state), and the atlas reaches the host (below). The cause was the blend state (below).
* Debug runs: the content draws start about 5 s before the boot-wait breakpoint, so dump every second (`VICTORY_DUMP_TARGETS=<dir>,30,1` with `VICTORY_TRACE_TARGETS=1`, which the dumps need) and check that the target was drawn before the last dump. `VICTORY_DEBUG_PS=in<N>` reads the original input row, which a split input may have moved: a black result does not mean the vertex shader wrote 0.

#### Green movie frames: textures written after their first upload (resolved)

* The boot movie (Bink, about 40 to 55 s with `VICTORY_KEYS`) is one draw a frame into a 3840x2160 sRGB target (DXGI 29), sampling three `R8_UNORM` planes (Y 3840x2160, U and V 1920x1080), three sets of them in turn. Each plane was realized and uploaded blank (all zero) about 3 s before the title decoded into it; the scanner then saw the change one round later, and by then a round over every watched texture took about 2 s (900 textures, 2.4 GB). So the first frames of each set showed zero planes, which convert to solid (0, 193, 0), and later frames an old set: the green screens and flicker.
* Fix: a texture whose memory changed after its first upload, or that was blank at it, is hot for the next 600 submissions: each submission probes 8 pages spread over its memory (`textures::probe`) and syncs it when they changed. A movie's planes, rewritten whole, are caught at the submission that samples them. A hot texture of at most 4 MiB that changed more than once (a glyph cache) is probed whole instead, so a few new glyphs are seen at the next submission too; one written once after a blank upload (streaming) is sampled, as probing all of those whole cost frames. Shortcut: a small write into a sampled texture still waits for the scanner's round. With it, no dump from 36 to 55 s has a green pixel and the movie plays (scratchpad `movie_t3_41_c655e0000.png`).
* The UI atlas (A8 2048x2048) is not affected: it is blank until the title draws its first text, and is uploaded within 0.1 s of that.
* `VICTORY_TRACE_CONTENT=<dir>,<seconds>,<format>` traces only the draws into render targets of that title format, boot passes included (the movie's draws are skipped otherwise), and writes 8-bit textures (A8, R8) as grey PPMs: the title's memory once (`<key>_title_<draw>.ppm`) and the host texture at each draw (`<key>_host.ppm`).
* Dumps of the presented image are read without waiting for the GPU: some show the top of the frame black in 1024-pixel steps while the movie target is complete. That is the read racing the draw, not a rendering fault.

#### Text drawn as boxes: blend from the 0x1c8 register image (resolved)

* The text pixel shaders (`ac967781...` and the body font's) write the vertex colour unmultiplied and the atlas coverage in alpha, so they need blending: guessed pipelines had none, and every glyph quad wrote a solid box into the UI target's colour while its alpha held the right glyph. The glyphs that looked right were the ones whose quads later draws covered. The title draws text with the base pipeline 0x1c8 made, not one of the variants 0x1b8 makes (43,000 of those at boot, few ever bound), so the state is in the base's register image.
* Pair 0 of a description slot is the register image (0x17c to 0x208 bytes). Decoded from 23,000 images: word 1 is 4 or 6, and the eight `CB_BLEND<n>_CONTROL` registers start at byte 0xd0 or 0x124 (every image has DISABLE_ROP3, bit 31, in all eight, and 0xffffffff after them; then three words and the eight `SX_MRT<n>_BLEND_OPT`, which agree: 0x06000600 is "blend disabled"). The text pipeline's is `e5010504`, straight alpha. Common values: `80000000` (off), `e5010504`, `f1010f01` (dual source), `e5040504`, `e0010161` (max). `Graphics::guess_from` reads them into the guessed state (hardware factors are D3D12's less one; combine functions add, subtract, min, max, reverse subtract), and variants' blend records apply over it.
* With it the legal-notices screen is right: text, the purple gradient background and the faded art behind it (scratchpad `legal-notices-blend.png`); the white block and stepped black areas were opaque quads too. Still guessed: culling, depth, stencil and the write mask (the word before the blend registers is not the target mask: it is 0 for the text pipeline).
* Two fixes found on the way: title threads get at least 8 MiB of stack (`host_posix::MIN_STACK`; the title's 1 MiB overflowed while the host built a Metal pipeline at the first guessed draw), and a released pipeline object's host pipelines are forgotten (`glue::pipeline_released`), as a new object at a freed one's address got the old one's pipeline.
* `VICTORY_DUMP_DESC_DIR=<dir>` (with `VICTORY_GUESS_GRAPHICS=1`) writes each guessed pipeline's register image as `regs_<vs>_<ps>_<address>.bin`.

#### Dropped work is logged loudly

Every kind of work the native host drops now logs one `warn!` the first time it
happens ("native host drops work: ..."), and each drop still counts in the
periodic "native draws so far" line. The kinds are: a draw or dispatch with no
host pipeline (refused or not translated), a topology the host does not draw,
an indexed rectangle list, a guessed mesh pipeline, mesh and indirect mesh
dispatches, texture copies and resolves, and indirect executions (one warning
per signature). A texture in a format the host cannot realize warns once per
format value, with its size and flags. No switch is needed; `RUST_LOG` filters
them as usual.

#### Rectangle-list corners and the Brightness logo (open)

* The right-angle corner is no longer assumed to be v1: the first two-channel-or-wider float fetch (the position) picks it as the corner opposite the longest side, and the strip is p, r, p + q - r, q (r the right angle). With the title's data (top-left, bottom-left, bottom-right) this is the old rule. A value two corners share is copied to the fourth exactly, not through float arithmetic (packed or NaN bits survive).
* A rectangle-list draw whose pipeline is not guessed (a described pipeline) is drawn as a plain strip without its fourth corner, and now warns once.
* `VICTORY_TRACE_RECTS=1` logs the first three rectangle-list draws of each pipeline with the first three vertices as floats.
* The Brightness logo (half a rectangle, the cut along one diagonal) is a guessed pipeline (`vertex 511484f3`: float3 position, unorm8x4 colour at 12, float2 uv at 16, 24-byte vertices) drawing one rectangle at (+-0.2, +-0.325) with corners top-left, bottom-left, bottom-right, which is the order the rule expects. Its matrices flip y, so the visible half is the strip's first triangle (which holds the computed corner), and the second triangle is lost. Both triangles wind alike, so culling does not explain it; the next suspects are the guessed depth state (less-equal with writes, `VICTORY_DEBUG_NO_DEPTH`) and the pixel shader on the second triangle. Not settled: the game runs needed were stopped.

#### Raster, depth, stencil and write mask from the register image

In all 23,022 images the registers just before the eight `CB_BLEND<n>_CONTROL` sit at the same
offsets back from them (the field layouts below match the values seen):

| Bytes before the blend block | Register | Seen |
|---|---|---|
| 0x30 | `PA_SU_SC_MODE_CNTL` | 0x201a40 no cull (11,341), 0x201a42 cull back (11,621), 0x201a41 cull front (58), 0x201a46 cull back with clockwise front (2) |
| 0x2c, 0x28 | constant 0x8, 0x1000 | |
| 0x24 to 0x1c | floats (polygon offset: 0.003, 29.0, 16.0, 1.0) | not used yet |
| 0x18 | `DB_DEPTH_CONTROL` | 0x7007f0 depth off (6,895), 0x7007e2 test GREATER_EQUAL without writes (4,999), 0x7007e7 GREATER_EQUAL with writes and stencil (3,325), 0x700796 LESS with writes, 0x7007b6 LESS_EQUAL with writes, 0x2002f1 stencil EQUAL only |
| 0x14 | `DB_STENCIL_CONTROL` | 0 (keep), 0x30030 pass REPLACE, 0x80080 pass increment-wrap |
| 0x10, 0xc | `DB_STENCILREFMASK`, `_BF` | 0x100ff00 (read mask 0xff, write mask 0), 0x1ffff00 (both 0xff) |
| 0x4 | `CB_TARGET_MASK` | 0xffffffff, 0 (depth only), 0xf, 0x3ffff |

`Graphics::guess_from` now reads all of these over the guessed defaults: cull (FACE 0 = counter-clockwise front), depth enable, write and function (hardware + 1 = D3D12), stencil enable, read and write masks, front and back ops and functions (the back face's only with BACKFACE_ENABLE), and each target's write mask. Most depth tests are GREATER_EQUAL (reversed Z), where the guess was LESS_EQUAL with writes. The polygon offset floats are not mapped. `VICTORY_GUESS_BLEND_ONLY=1` keeps the earlier blend-only reading, for comparison. A run past setup to the loading screen gave the same presented images, byte for byte, as without this change. The setup screens were not checked: runs now skip them, probably because the profile is saved.

#### Texture formats the host lacked

The title's texture formats are DXGI numbers (`resource::pc_format` only renames the console's depth views 118 to 120). The first warnings ("a title texture format the host does not represent") named ten of them; once those were mapped, the run reached more textures and named fifteen more. All of them but two now map to a Metal format of the same element size (`descriptors::format`, tested against `resource::element` in `host_formats_have_the_title_formats_element_size`):

| DXGI | Format | First texture seen (flags: 1 render target, 2 depth, 4 unordered access) | Metal |
|---|---|---|---|
| 42 | R32_UINT | 1280x720, unordered access (a compute output) | R32Uint |
| 26 | R11G11B10_FLOAT | 1280x720, unordered access (a compute output) | RG11B10Float |
| 67 | R9G9B9E5_SHAREDEXP | 1280x720, read only | RGB9E5Float |
| 16 | R32G32_FLOAT | 160x90, unordered access | RG32Float |
| 62 | R8_UINT | 640x360, render target and unordered access | R8Uint |
| 34, 35 | R16G16_FLOAT, R16G16_UNORM | 256x256, 127x128, render targets | RG16Float, RG16Unorm |
| 24 | R10G10B10A2_UNORM | 32x32 | RGB10A2Unorm |
| 30 | R8G8B8A8_UINT | 1x1 | RGBA8Uint |
| 99, 72, 78 | BC7, BC1, BC3 UNORM_SRGB | 512x512, 32x32, 512x512 | the `_sRGB` BC formats |
| 53 | R16_TYPELESS | 2048x8192 depth (a shadow map) | Depth16Unorm (R16Unorm when not depth) |
| 55 | D16_UNORM | (its depth views) | Depth16Unorm |
| 3, 17, 36, 51, 57 | R32G32B32A32, R32G32, R16G16, R8G8, R16 UINT | 128x128 to 2560x1440 | the matching Uint formats |
| 11, 13 | R16G16B16A16_UNORM, _SNORM | 128x4, 256x256 | RGBA16Unorm, RGBA16Snorm |
| 49, 50 | R8G8_TYPELESS, _UNORM | 16x16, 64x64 | RG8Unorm |
| 54 | R16_FLOAT | 512x512 | R16Float |

Integer render targets never blend (Metal refuses a pipeline that blends one), whatever the state says. Not mapped yet: B5G5R5A1_UNORM (86, 1024x4096 render target) and B4G4R4A4_UNORM (115, 2560x1440 render target). Metal's packed 16-bit formats (BGR5A1Unorm, ABGR4Unorm) need their bit order checked against DXGI's before they are used. Before the story loads, no draw or dispatch binds any of these textures (the targets traced are formats 28, 29 and 19), so which shaders write and read them is still to be seen. With the mapping, the presented images up to the loading screen look the same.
