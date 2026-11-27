/* A stand-in for a DLL a title ships (the C++ runtime, Bink, ...), written by
 * us, for the native macOS host's tests. The native probe (probe.c) imports it.
 *
 * Freestanding x86-64 PE DLL: no CRT. It has what a shipped DLL has that the
 * executable does not: a DllMain that hears of processes and threads, its own
 * static TLS and TLS callback, unwind tables of its own, and frames that an
 * exception raised in it unwinds through on its way to a handler in another
 * image (a C++ throw across a DLL boundary does the same, with
 * __CxxFrameHandler in place of __C_specific_handler; the Windows C++
 * runtime is not ours to build here).
 *
 * Built by tools/macos/build-native-probe.sh. It is NOT part of any cargo
 * build.
 */
typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long long u64;
typedef void *ptr;

#define IMPORT __declspec(dllimport)
#define EXPORT __declspec(dllexport)

IMPORT void RaiseException(u32, u32, u32, const u64 *);

/* What DllMain and the TLS callback saw, in the order it happened. */
enum {
    S_TLS_ATTACH_FIRST = 1u << 0, /* the TLS callback heard PROCESS_ATTACH before DllMain */
    S_MAIN_ATTACH = 1u << 1,
    S_STATIC_LOAD = 1u << 2, /* reserved != 0: the executable imports this DLL */
    S_MAIN_HAS_TLS = 1u << 3, /* DllMain ran with its static TLS in place */
};
static volatile u32 state;
static volatile u32 thread_attaches, thread_detaches;
static volatile u32 tls_thread_attaches;
static volatile u32 finally_runs;

/* Static TLS: the template (500) is copied to each thread. */
__declspec(thread) u32 dll_tls = 500;

static void tls_callback(ptr module, u32 reason, ptr reserved)
{
    (void)module;
    (void)reserved;
    if (reason == 1 && !(state & S_MAIN_ATTACH))
        state |= S_TLS_ATTACH_FIRST;
    else if (reason == 2)
        tls_thread_attaches++;
}

unsigned int _tls_index = 0;
__attribute__((section(".tls"))) char _tls_start = 0;
__attribute__((section(".tls$ZZZ"))) char _tls_end = 0;
__attribute__((section(".CRT$XLB"), used)) void (*tls_callbacks[])(ptr, u32, ptr) = {tls_callback, 0};

struct tls_directory {
    u64 start, end, index, callbacks;
    u32 zero_fill, characteristics;
};
__attribute__((section(".rdata$T"), used)) const struct tls_directory _tls_used = {
    (u64)&_tls_start, (u64)&_tls_end, (u64)&_tls_index, (u64)tls_callbacks, 0, 0,
};

int DllMain(ptr module, u32 reason, ptr reserved)
{
    (void)module;
    if (reason == 1) {
        state |= S_MAIN_ATTACH;
        if (reserved)
            state |= S_STATIC_LOAD;
        /* The process-attach call is on a thread with its TLS block. */
        if (dll_tls == 500)
            state |= S_MAIN_HAS_TLS;
    } else if (reason == 2) {
        thread_attaches++;
    } else if (reason == 3) {
        thread_detaches++;
    }
    return 1;
}

EXPORT u32 DllProbeState(void)
{
    return state;
}

EXPORT u32 DllProbeAdd(u32 a, u32 b)
{
    return a + b;
}

/* Adds to this thread's copy of the static TLS variable. */
EXPORT u32 DllProbeTls(u32 add)
{
    dll_tls += add;
    return dll_tls;
}

EXPORT u32 DllProbeThreadAttaches(void)
{
    return thread_attaches;
}

EXPORT u32 DllProbeTlsThreadAttaches(void)
{
    return tls_thread_attaches;
}

/* Raises with a __finally in between: the handler is in whoever called. */
static void __attribute__((noinline)) raise_below(u32 code, u32 depth)
{
    if (depth > 0) {
        raise_below(code, depth - 1);
    } else {
        u64 args[1] = {0xD11};
        RaiseException(code, 0, 1, args);
    }
}

EXPORT void DllProbeThrow(u32 code)
{
    __try {
        raise_below(code, 2);
    } __finally {
        finally_runs++;
    }
}

EXPORT u32 DllProbeFinallyRuns(void)
{
    return finally_runs;
}

/* A handler inside the DLL, over the DLL's own tables. */
EXPORT u32 DllProbeCatchInside(void)
{
    u32 caught = 0;
    __try {
        raise_below(0xE0000009, 1);
    } __except (1) {
        caught = 1;
    }
    return caught;
}

/* Runs a callback of the caller's inside a __try of the DLL's: an exception
 * from the executable's code reaches the DLL's handler through the
 * executable's frames. */
EXPORT u32 DllProbeCall(u32 (*callback)(void))
{
    u32 caught = 0;
    __try {
        callback();
    } __except (1) {
        caught = 1;
    }
    return caught;
}
