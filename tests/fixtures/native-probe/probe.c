/* A stand-in for a title, written by us, for the native macOS host's tests.
 *
 * A freestanding x86-64 PE exe: no CRT, no libc. It exercises what title code
 * needs from the host: imports (HLE, kernel32 and test-only), base
 * relocations, static TLS (gs:[0x58]), the TEB and PEB (gs:[0x30]), TLS
 * callbacks, stack arguments, floating-point arguments and a callback from the
 * host into title code. Each check that passes sets one bit of the exit code;
 * probe_main returns PROBE_ALL when everything worked.
 *
 * Built by tools/macos/build-native-probe.sh. It is NOT part of any cargo
 * build.
 */
typedef unsigned char u8;
typedef unsigned int u32;
typedef unsigned long long u64;
typedef void *ptr;

#define IMPORT __declspec(dllimport)

/* xgameruntime.dll: console API, answered by victory-hle's registry. */
IMPORT int XGameRuntimeInitialize(void);

/* kernel32.dll: the host's own on Windows, victory-hle's POSIX versions here. */
IMPORT u32 GetLastError(void);
IMPORT void SetLastError(u32);
IMPORT u32 GetCurrentThreadId(void);
IMPORT ptr VirtualAlloc(ptr, u64, u32, u32);
IMPORT int VirtualProtect(ptr, u64, u32, u32 *);
IMPORT int VirtualFree(ptr, u64, u32);
IMPORT int QueryPerformanceCounter(u64 *);
IMPORT u64 GetTickCount64(void);
IMPORT void Sleep(u32);
IMPORT ptr CreateEventW(ptr, int, int, ptr);
IMPORT int SetEvent(ptr);
IMPORT u32 WaitForSingleObject(ptr, u32);
IMPORT int CloseHandle(ptr);
IMPORT ptr CreateThread(ptr, u64, ptr, ptr, u32, u32 *);
IMPORT u32 TlsAlloc(void);
IMPORT ptr TlsGetValue(u32);
IMPORT int TlsSetValue(u32, ptr);

/* victory-probe-host.dll: provided by the test itself. */
IMPORT u64 ProbeSum8(u64, u64, u64, u64, u64, u64, u64, u64);
IMPORT double ProbeScale(double, int);
IMPORT u64 ProbeCall(u64 (*callback)(u64, u64), u64 a, u64 b);
IMPORT u64 ProbeNotImplemented(u64);

extern char __ImageBase;

enum {
    C_HLE = 1u << 0,
    C_TEB = 1u << 1,
    C_PEB = 1u << 2,
    C_TLS_MAIN = 1u << 3,
    C_TLS_THREAD = 1u << 4,
    C_VIRTUAL = 1u << 5,
    C_LAST_ERROR = 1u << 6,
    C_TIME = 1u << 7,
    C_STACK_ARGS = 1u << 8,
    C_FLOAT = 1u << 9,
    C_CALLBACK = 1u << 10,
    C_EVENT = 1u << 11,
    C_TLS_CALLBACK = 1u << 12,
    C_RELOCS = 1u << 13,
    C_TLS_API = 1u << 14,
    C_THREAD_ID = 1u << 15,
};
#define PROBE_ALL 0xFFFFu

/* Static TLS: the template (100) is copied to each thread. */
__declspec(thread) u32 tls_counter = 100;

/* Base relocations: pointers stored in the image. */
static u32 data_word = 77;
static u32 *const data_pointer = &data_word;
static u64 callback_table[2];

static u32 tls_callback_saw_attach;

static u64 read_gs(u32 offset)
{
    u64 v;
    __asm__ volatile("movq %%gs:(%1), %0" : "=r"(v) : "r"((u64)offset));
    return v;
}

static u32 read_gs32(u32 offset)
{
    u32 v;
    __asm__ volatile("movl %%gs:(%1), %0" : "=r"(v) : "r"((u64)offset));
    return v;
}

static u64 title_callback(u64 a, u64 b)
{
    return a * 1000 + b + tls_counter;
}

static ptr worker_done;
static volatile u32 worker_tls_after;
static volatile u32 worker_last_error;

static u32 worker(ptr param)
{
    tls_counter += 5;
    worker_tls_after = tls_counter;
    worker_last_error = GetLastError();
    SetEvent(worker_done);
    return tls_counter + (u32)(u64)param;
}

/* TLS callback: DLL_PROCESS_ATTACH (1) arrives before the entry point. */
static void tls_callback(ptr module, u32 reason, ptr reserved)
{
    (void)module;
    (void)reserved;
    if (reason == 1)
        tls_callback_saw_attach = 1;
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

u32 probe_main(ptr unused)
{
    u32 ok = 0;
    (void)unused;

    if (XGameRuntimeInitialize() == 0)
        ok |= C_HLE;

    u64 teb = read_gs(0x30);
    if (teb != 0 && read_gs(0x30) == *(u64 *)(teb + 0x30))
        ok |= C_TEB;
    u64 peb = read_gs(0x60);
    if (peb != 0 && *(u64 *)(peb + 0x10) == (u64)&__ImageBase)
        ok |= C_PEB;

    if (tls_counter == 100) {
        tls_counter++;
        if (tls_counter == 101)
            ok |= C_TLS_MAIN;
    }

    ptr event = CreateEventW(0, 1, 0, 0);
    worker_done = event;
    SetLastError(1234);
    u32 tid = 0;
    ptr thread = CreateThread(0, 0, (ptr)worker, (ptr)7, 0, &tid);
    if (thread && WaitForSingleObject(event, 5000) == 0)
        ok |= C_EVENT;
    if (thread) {
        WaitForSingleObject(thread, 5000);
        CloseHandle(thread);
    }
    /* The worker saw its own copy (100 + 5) and its own last error (0). */
    if (worker_tls_after == 105 && tls_counter == 101)
        ok |= C_TLS_THREAD;
    if (GetLastError() == 1234 && worker_last_error == 0)
        ok |= C_LAST_ERROR;
    if (tid != 0 && tid != GetCurrentThreadId())
        ok |= C_THREAD_ID;
    CloseHandle(event);

    u8 *page = VirtualAlloc(0, 65536, 0x3000, 4);
    if (page) {
        page[0] = 9;
        page[65535] = 8;
        u32 old = 0;
        if (VirtualProtect(page, 65536, 2, &old) && old == 4 && page[0] == 9 && page[65535] == 8)
            ok |= C_VIRTUAL;
        VirtualFree(page, 0, 0x8000);
    }

    u64 t0 = 0, t1 = 0;
    QueryPerformanceCounter(&t0);
    u64 tick0 = GetTickCount64();
    Sleep(15);
    QueryPerformanceCounter(&t1);
    if (t1 > t0 && GetTickCount64() >= tick0 + 10)
        ok |= C_TIME;

    if (ProbeSum8(1, 2, 3, 4, 5, 6, 7, 8) == 36)
        ok |= C_STACK_ARGS;
    if (ProbeScale(1.5, 4) == 6.0)
        ok |= C_FLOAT;
    callback_table[0] = (u64)title_callback;
    callback_table[1] = (u64)&worker;
    if (ProbeCall((u64 (*)(u64, u64))callback_table[0], 3, 4) == 3000 + 4 + 101)
        ok |= C_CALLBACK;

    if (tls_callback_saw_attach)
        ok |= C_TLS_CALLBACK;
    if (*data_pointer == 77 && callback_table[0] != 0)
        ok |= C_RELOCS;

    u32 slot = TlsAlloc();
    if (slot != 0xFFFFFFFFu && TlsSetValue(slot, (ptr)0x1234) && TlsGetValue(slot) == (ptr)0x1234)
        ok |= C_TLS_API;

    return ok;
}

/* Calls an import nobody implements: the host must name it, not crash. */
u32 probe_missing(ptr unused)
{
    (void)unused;
    return (u32)ProbeNotImplemented(42);
}
