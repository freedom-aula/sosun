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
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long long u64;
typedef void *ptr;

#define IMPORT __declspec(dllimport)

/* xgameruntime.dll: console API, answered by victory-hle's registry. */
IMPORT int XGameRuntimeInitialize(void);
IMPORT int QueryApiImpl(const u8 *provider, const u8 *iid, ptr *out);

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

/* Exceptions. */
IMPORT ptr AddVectoredExceptionHandler(u32, ptr);
IMPORT u32 RemoveVectoredExceptionHandler(ptr);
IMPORT void RaiseException(u32, u32, u32, const u64 *);
IMPORT void DebugBreak(void);

/* Threads and synchronisation. */
IMPORT u32 SuspendThread(ptr);
IMPORT u32 ResumeThread(ptr);
IMPORT int GetThreadContext(ptr, ptr);
IMPORT u32 WaitForMultipleObjects(u32, const ptr *, int, u32);
IMPORT int SwitchToThread(void);
IMPORT void InitializeCriticalSection(ptr);
IMPORT void EnterCriticalSection(ptr);
IMPORT int TryEnterCriticalSection(ptr);
IMPORT void LeaveCriticalSection(ptr);
IMPORT void AcquireSRWLockExclusive(ptr);
IMPORT void AcquireSRWLockShared(ptr);
IMPORT u8 TryAcquireSRWLockExclusive(ptr);
IMPORT u8 TryAcquireSRWLockShared(ptr);
IMPORT void ReleaseSRWLockExclusive(ptr);
IMPORT void ReleaseSRWLockShared(ptr);
IMPORT int SleepConditionVariableCS(ptr, ptr, u32);
IMPORT void WakeConditionVariable(ptr);

/* Files. */
IMPORT ptr CreateFileW(const u16 *, u32, u32, ptr, u32, u32, ptr);
IMPORT int ReadFile(ptr, ptr, u32, u32 *, ptr);
IMPORT int WriteFile(ptr, const void *, u32, u32 *, ptr);
IMPORT u32 SetFilePointer(ptr, int, int *, u32);
IMPORT int SetFilePointerEx(ptr, long long, long long *, u32);
IMPORT u32 GetFileSize(ptr, u32 *);
IMPORT u32 GetFileAttributesW(const u16 *);
IMPORT ptr FindFirstFileW(const u16 *, ptr);
IMPORT int FindNextFileW(ptr, ptr);
IMPORT int FindClose(ptr);

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
static volatile u32 thread_attach_notices, thread_detach_notices;

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
    else if (reason == 2)
        thread_attach_notices++;
    else if (reason == 3)
        thread_detach_notices++;
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

/* ---- exceptions ---------------------------------------------------- */

struct exception_record {
    u32 code, flags;
    struct exception_record *chain;
    ptr address;
    u32 count;
    u64 info[15];
};
struct exception_pointers {
    struct exception_record *record;
    ptr context;
};

enum {
    E_AV_CAUGHT = 1u << 0,
    E_AV_DETAILS = 1u << 1,
    E_GS_IN_FILTER = 1u << 2,
    E_FINALLY = 1u << 3,
    E_DEEP = 1u << 4,
    E_RAISE = 1u << 5,
    E_RAISE_ARGS = 1u << 6,
    E_DIVIDE = 1u << 7,
    E_CONTINUE = 1u << 8,
    E_VEH_FIXES = 1u << 9,
    E_VEH_GS = 1u << 10,
    E_VEH_WRITE = 1u << 11,
    E_UNHANDLED_SKIPPED = 1u << 12,
    E_AFTER = 1u << 13,
    E_ORDER = 1u << 14,
    E_BREAK = 1u << 15,
    E_BREAK_CONTINUE = 1u << 16,
};
#define EXC_ALL 0x1FFFFu

static volatile int *volatile null_pointer = (volatile int *)8;
static volatile int zero;
static volatile u32 seen_code, seen_kind;
static volatile u64 seen_address, seen_teb;
static volatile u32 finally_runs, sequence, finally_at, except_at;
static u64 teb_at_start;

static int record_filter(struct exception_pointers *p)
{
    seen_code = p->record->code;
    seen_kind = (u32)p->record->info[0];
    seen_address = p->record->info[1];
    seen_teb = read_gs(0x30);
    return 1; /* EXCEPTION_EXECUTE_HANDLER */
}

static int continue_filter(struct exception_pointers *p)
{
    (void)p;
    return -1; /* EXCEPTION_CONTINUE_EXECUTION */
}

/* Faults several frames below the __try that catches it. */
static void __attribute__((noinline)) fault_deep(int depth)
{
    volatile int local = depth;
    if (depth > 0)
        fault_deep(depth - 1);
    else
        *null_pointer = local;
    local = 0;
}

static void __attribute__((noinline)) fault_with_finally(void)
{
    __try {
        fault_deep(3);
    } __finally {
        finally_runs++;
        finally_at = ++sequence;
    }
}

static u8 *veh_page;
static volatile u32 veh_calls, veh_wrote, veh_gs_ok;

/* A vectored handler (title code): makes the page writable and retries. */
static int veh(struct exception_pointers *p)
{
    veh_calls++;
    veh_gs_ok = read_gs(0x30) == teb_at_start;
    if (p->record->code == 0xC0000005 && p->record->info[0] == 1 &&
        (u8 *)p->record->info[1] >= veh_page && (u8 *)p->record->info[1] < veh_page + 4096) {
        u32 old;
        veh_wrote = 1;
        VirtualProtect(veh_page, 4096, 4, &old);
        return -1;
    }
    return 0; /* EXCEPTION_CONTINUE_SEARCH */
}

u32 probe_seh(ptr unused)
{
    u32 ok = 0;
    (void)unused;
    teb_at_start = read_gs(0x30);

    /* An access violation caught by __try/__except, with its details. */
    int caught = 0;
    seen_code = 0;
    __try {
        *null_pointer = 1;
    } __except (record_filter((struct exception_pointers *)_exception_info())) {
        caught = 1;
    }
    if (caught)
        ok |= E_AV_CAUGHT;
    if (seen_code == 0xC0000005 && seen_kind == 1 && seen_address == 8)
        ok |= E_AV_DETAILS;
    if (seen_teb == teb_at_start && read_gs(0x30) == teb_at_start)
        ok |= E_GS_IN_FILTER;

    /* A fault three frames down, a __finally in between, then the __except. */
    finally_runs = 0;
    caught = 0;
    __try {
        fault_with_finally();
    } __except (1) {
        caught = 1;
        except_at = ++sequence;
    }
    if (caught && finally_runs == 1)
        ok |= E_FINALLY;
    if (caught)
        ok |= E_DEEP;
    if (finally_at != 0 && except_at > finally_at)
        ok |= E_ORDER;

    /* RaiseException with parameters. */
    caught = 0;
    seen_code = 0;
    u64 args[2] = {0x1234, 0x5678};
    __try {
        RaiseException(0xE0000001, 0, 2, args);
    } __except (record_filter((struct exception_pointers *)_exception_info())) {
        caught = 1;
    }
    if (caught && seen_code == 0xE0000001)
        ok |= E_RAISE;
    if (caught && seen_kind == 0x1234 && seen_address == 0x5678)
        ok |= E_RAISE_ARGS;

    /* An integer divide by zero. */
    caught = 0;
    seen_code = 0;
    __try {
        zero = 10 / zero;
    } __except (record_filter((struct exception_pointers *)_exception_info())) {
        caught = 1;
    }
    if (caught && seen_code == 0xC0000094)
        ok |= E_DIVIDE;

    /* A filter that continues: RaiseException returns to its caller. */
    int after = 0;
    __try {
        RaiseException(0xE0000002, 0, 0, 0);
        after = 1;
    } __except (continue_filter((struct exception_pointers *)_exception_info())) {
        after = 2;
    }
    if (after == 1)
        ok |= E_CONTINUE;

    /* A vectored handler fixes a read-only page and the store goes on. */
    veh_page = VirtualAlloc(0, 4096, 0x3000, 2);
    ptr handle = AddVectoredExceptionHandler(1, (ptr)veh);
    if (veh_page && handle) {
        veh_page[16] = 42;
        if (veh_page[16] == 42 && veh_wrote)
            ok |= E_VEH_FIXES | E_VEH_WRITE;
        if (veh_gs_ok)
            ok |= E_VEH_GS;
        /* It is passed over for exceptions it declines. */
        caught = 0;
        __try {
            *null_pointer = 1;
        } __except (1) {
            caught = 1;
        }
        if (caught && veh_calls >= 2)
            ok |= E_UNHANDLED_SKIPPED;
        RemoveVectoredExceptionHandler(handle);
        VirtualFree(veh_page, 0, 0x8000);
    }
    /* DebugBreak raises EXCEPTION_BREAKPOINT, which __except takes... */
    caught = 0;
    seen_code = 0;
    __try {
        DebugBreak();
    } __except (record_filter((struct exception_pointers *)_exception_info())) {
        caught = 1;
    }
    if (caught && seen_code == 0x80000003)
        ok |= E_BREAK;
    /* ...and a filter that continues returns from it. */
    after = 0;
    __try {
        DebugBreak();
        after = 1;
    } __except (continue_filter((struct exception_pointers *)_exception_info())) {
        after = 2;
    }
    if (after == 1)
        ok |= E_BREAK_CONTINUE;

    /* Everything above left the thread able to carry on. */
    if (read_gs(0x30) == teb_at_start)
        ok |= E_AFTER;
    return ok;
}

/* ---- threads and synchronisation ------------------------------------ */

enum {
    T_ATTACH = 1u << 0,
    T_DETACH = 1u << 1,
    T_SUSPENDED_START = 1u << 2,
    T_RESUME_RUNS = 1u << 3,
    T_COUNTS = 1u << 4,
    T_STOPS = 1u << 5,
    T_CONTEXT = 1u << 6,
    T_CONTINUES = 1u << 7,
    T_WAIT_ANY = 1u << 8,
    T_WAIT_ANY_TIMEOUT = 1u << 9,
    T_WAIT_ALL = 1u << 10,
    T_WAIT_ALL_KEEPS = 1u << 11,
    T_CS = 1u << 12,
    T_CS_TRY = 1u << 13,
    T_SRW = 1u << 14,
    T_SRW_TRY = 1u << 15,
    T_CV = 1u << 16,
    T_CV_TIMEOUT = 1u << 17,
    T_SWITCH = 1u << 18,
};

static volatile u32 ran_flag, spin_stop;
static volatile u64 spin_count;

static u32 flag_worker(ptr param)
{
    ran_flag = 1;
    if (param)
        SetEvent(param);
    return 0;
}

static u32 __attribute__((noinline)) spin_worker(ptr param)
{
    (void)param;
    while (!spin_stop)
        spin_count++;
    return 0;
}

static u32 __attribute__((noinline)) after_spin_worker(void)
{
    return 0;
}

/* A CONTEXT is 0x4D0 bytes, 16-aligned. */
static u8 context_buffer[0x4D0] __attribute__((aligned(16)));

u32 probe_threads(ptr unused)
{
    u32 ok = 0;
    (void)unused;

    /* Thread notices reach the TLS callback on the new thread. */
    u32 a0 = thread_attach_notices, d0 = thread_detach_notices;
    ptr t = CreateThread(0, 0, (ptr)flag_worker, 0, 0, 0);
    WaitForSingleObject(t, 5000);
    CloseHandle(t);
    if (thread_attach_notices >= a0 + 1)
        ok |= T_ATTACH;
    if (thread_detach_notices >= d0 + 1)
        ok |= T_DETACH;

    /* CREATE_SUSPENDED: nothing runs until ResumeThread. */
    ran_flag = 0;
    ptr done = CreateEventW(0, 1, 0, 0);
    t = CreateThread(0, 0, (ptr)flag_worker, done, 4, 0);
    Sleep(40);
    if (!ran_flag)
        ok |= T_SUSPENDED_START;
    u32 prev = ResumeThread(t);
    if (prev == 1 && WaitForSingleObject(done, 5000) == 0 && ran_flag)
        ok |= T_RESUME_RUNS;
    WaitForSingleObject(t, 5000);
    CloseHandle(t);

    /* Suspend counts nest. */
    ptr t2 = CreateThread(0, 0, (ptr)flag_worker, 0, 4, 0);
    u32 s1 = SuspendThread(t2), s2 = SuspendThread(t2);
    u32 r1 = ResumeThread(t2), r2 = ResumeThread(t2), r3 = ResumeThread(t2);
    if (s1 == 1 && s2 == 2 && r1 == 3 && r2 == 2 && r3 == 1)
        ok |= T_COUNTS;
    WaitForSingleObject(t2, 5000);
    CloseHandle(t2);

    /* Suspend a thread spinning in title code, read its registers. */
    spin_stop = 0;
    spin_count = 0;
    ptr sp = CreateThread(0, 0, (ptr)spin_worker, 0, 0, 0);
    Sleep(30);
    SuspendThread(sp);
    u64 c1 = spin_count;
    Sleep(30);
    if (c1 > 0 && spin_count == c1)
        ok |= T_STOPS;
    for (u32 i = 0; i < sizeof context_buffer; i++)
        context_buffer[i] = 0;
    *(u32 *)(context_buffer + 0x30) = 0x10000B;
    if (GetThreadContext(sp, context_buffer)) {
        u64 rip = *(u64 *)(context_buffer + 0xF8), rsp = *(u64 *)(context_buffer + 0x98);
        if (rip >= (u64)spin_worker && rip < (u64)after_spin_worker && rsp > 0x10000)
            ok |= T_CONTEXT;
    }
    ResumeThread(sp);
    Sleep(30);
    if (spin_count > c1)
        ok |= T_CONTINUES;
    spin_stop = 1;
    WaitForSingleObject(sp, 5000);
    CloseHandle(sp);
    CloseHandle(done);
    return ok;
}

static u8 cs_area[64] __attribute__((aligned(16)));
static u8 srw_area[16] __attribute__((aligned(16)));
static u8 cv_area[16] __attribute__((aligned(16)));

/* Bounds on waits for another thread to get scheduled at all: loose for
   slow hosts, where scheduling a thread can take seconds. Waits that must
   time out stay short: a time-out cannot come early on a monotonic clock. */
enum { THREAD_SCHEDULED_MS = 30000 };
static volatile u32 shared_counter, cv_ready, cv_seen;
static ptr consumer_done;

static u32 cs_worker(ptr param)
{
    for (int i = 0; i < 20000; i++) {
        if (param) {
            EnterCriticalSection(cs_area);
            shared_counter++;
            LeaveCriticalSection(cs_area);
        } else {
            AcquireSRWLockExclusive(srw_area);
            shared_counter++;
            ReleaseSRWLockExclusive(srw_area);
        }
    }
    return 0;
}

static u32 try_from_other(ptr param)
{
    (void)param;
    shared_counter = (u32)TryEnterCriticalSection(cs_area);
    if (shared_counter)
        LeaveCriticalSection(cs_area);
    return 0;
}

static u32 consumer(ptr param)
{
    (void)param;
    EnterCriticalSection(cs_area);
    while (!cv_ready)
        SleepConditionVariableCS(cv_area, cs_area, 0xFFFFFFFF);
    cv_seen = 1;
    LeaveCriticalSection(cs_area);
    SetEvent(consumer_done);
    return 0;
}

static u32 late_setter(ptr param)
{
    Sleep(40);
    SetEvent(param);
    return 0;
}

static u32 delayed_flag(ptr param)
{
    (void)param;
    Sleep(20);
    ran_flag = 1;
    return 0;
}

u32 probe_sync(ptr unused)
{
    u32 ok = 0;
    (void)unused;

    /* WaitForMultipleObjects, any. */
    ptr e[2] = {CreateEventW(0, 0, 0, 0), CreateEventW(0, 0, 0, 0)};
    if (WaitForMultipleObjects(2, e, 0, 20) == 0x102)
        ok |= T_WAIT_ANY_TIMEOUT;
    SetEvent(e[1]);
    if (WaitForMultipleObjects(2, e, 0, 1000) == 1 && WaitForSingleObject(e[1], 0) == 0x102)
        ok |= T_WAIT_ANY;

    /* All: one signalled is not enough, and is not consumed by the try. */
    SetEvent(e[0]);
    u32 r = WaitForMultipleObjects(2, e, 1, 30);
    if (r == 0x102 && WaitForSingleObject(e[0], 0) == 0)
        ok |= T_WAIT_ALL_KEEPS;
    SetEvent(e[0]);
    ptr th = CreateThread(0, 0, (ptr)late_setter, e[1], 0, 0);
    if (WaitForMultipleObjects(2, e, 1, THREAD_SCHEDULED_MS) == 0)
        ok |= T_WAIT_ALL;
    WaitForSingleObject(th, THREAD_SCHEDULED_MS);
    CloseHandle(th);
    CloseHandle(e[0]);
    CloseHandle(e[1]);

    /* Critical sections: mutual exclusion, recursion, TryEnter. */
    InitializeCriticalSection(cs_area);
    EnterCriticalSection(cs_area);
    EnterCriticalSection(cs_area);
    LeaveCriticalSection(cs_area);
    LeaveCriticalSection(cs_area);
    shared_counter = 0;
    ptr w[2];
    w[0] = CreateThread(0, 0, (ptr)cs_worker, (ptr)1, 0, 0);
    w[1] = CreateThread(0, 0, (ptr)cs_worker, (ptr)1, 0, 0);
    WaitForMultipleObjects(2, w, 1, THREAD_SCHEDULED_MS);
    if (shared_counter == 40000)
        ok |= T_CS;
    CloseHandle(w[0]);
    CloseHandle(w[1]);
    /* Keep ownership until the other thread has actually tried. A holder
       sleeping for 80 ms could release before that thread was scheduled. */
    EnterCriticalSection(cs_area);
    shared_counter = 0xFFFFFFFF;
    ptr tr = CreateThread(0, 0, (ptr)try_from_other, 0, 0, 0);
    u32 tried = WaitForSingleObject(tr, THREAD_SCHEDULED_MS);
    u32 rejected = tr && tried == 0 && shared_counter == 0;
    LeaveCriticalSection(cs_area);
    CloseHandle(tr);
    /* Do not reuse shared_counter if a broken TryEnter left a worker live. */
    if (!tr || tried != 0)
        return ok;
    /* A stub that always refuses TryEnter must not pass the check either. */
    if (TryEnterCriticalSection(cs_area)) {
        if (rejected)
            ok |= T_CS_TRY;
        LeaveCriticalSection(cs_area);
    }

    /* SRW locks. */
    shared_counter = 0;
    w[0] = CreateThread(0, 0, (ptr)cs_worker, 0, 0, 0);
    w[1] = CreateThread(0, 0, (ptr)cs_worker, 0, 0, 0);
    WaitForMultipleObjects(2, w, 1, THREAD_SCHEDULED_MS);
    if (shared_counter == 40000)
        ok |= T_SRW;
    CloseHandle(w[0]);
    CloseHandle(w[1]);
    AcquireSRWLockShared(srw_area);
    u8 shared_again = TryAcquireSRWLockShared(srw_area);
    u8 exclusive_while_shared = TryAcquireSRWLockExclusive(srw_area);
    ReleaseSRWLockShared(srw_area);
    ReleaseSRWLockShared(srw_area);
    u8 exclusive_after = TryAcquireSRWLockExclusive(srw_area);
    ReleaseSRWLockExclusive(srw_area);
    if (shared_again && !exclusive_while_shared && exclusive_after)
        ok |= T_SRW_TRY;

    /* Condition variable: a consumer sleeps until told. */
    consumer_done = CreateEventW(0, 1, 0, 0);
    ptr c = CreateThread(0, 0, (ptr)consumer, 0, 0, 0);
    Sleep(30);
    EnterCriticalSection(cs_area);
    cv_ready = 1;
    LeaveCriticalSection(cs_area);
    WakeConditionVariable(cv_area);
    if (WaitForSingleObject(consumer_done, THREAD_SCHEDULED_MS) == 0 && cv_seen)
        ok |= T_CV;
    CloseHandle(c);
    CloseHandle(consumer_done);
    EnterCriticalSection(cs_area);
    SetLastError(0);
    int slept = SleepConditionVariableCS(cv_area, cs_area, 10);
    LeaveCriticalSection(cs_area);
    if (!slept && GetLastError() == 1460)
        ok |= T_CV_TIMEOUT;

    /* SwitchToThread while another thread gets on. */
    ran_flag = 0;
    ptr d = CreateThread(0, 0, (ptr)delayed_flag, 0, 0, 0);
    u64 t0 = GetTickCount64();
    while (!ran_flag && GetTickCount64() < t0 + THREAD_SCHEDULED_MS)
        SwitchToThread();
    if (ran_flag)
        ok |= T_SWITCH;
    WaitForSingleObject(d, THREAD_SCHEDULED_MS);
    CloseHandle(d);
    return ok;
}

/* ---- files ------------------------------------------------------------ */

enum {
    F_OPEN = 1u << 0,
    F_SIZE = 1u << 1,
    F_READ = 1u << 2,
    F_SEEK = 1u << 3,
    F_SEEK_END = 1u << 4,
    F_FIND = 1u << 5,
    F_FIND_END = 1u << 6,
    F_WRITE = 1u << 7,
    F_ATTR = 1u << 8,
    F_MISSING = 1u << 9,
};

static u32 wlen(const u16 *s)
{
    u32 n = 0;
    while (s[n])
        n++;
    return n;
}

static void wcopy(u16 *to, const u16 *from, const char *tail)
{
    u32 n = wlen(from);
    for (u32 i = 0; i < n; i++)
        to[i] = from[i];
    while (*tail)
        to[n++] = (u16)*tail++;
    to[n] = 0;
}

static u8 find_data[592] __attribute__((aligned(8)));

/* `dir` is a wide path of a folder holding hello.txt ("hello, victory\n"). */
u32 probe_files(const u16 *dir)
{
    u32 ok = 0;
    u16 path[600];
    wcopy(path, dir, "/hello.txt");
    ptr f = CreateFileW(path, 0x80000000u, 1, 0, 3, 0x80, 0);
    if (f != (ptr)-1)
        ok |= F_OPEN;
    if (f != (ptr)-1 && GetFileSize(f, 0) == 15)
        ok |= F_SIZE;
    u8 buf[32];
    u32 n = 0;
    if (f != (ptr)-1 && ReadFile(f, buf, 5, &n, 0) && n == 5 && buf[0] == 'h' && buf[4] == 'o')
        ok |= F_READ;
    if (f != (ptr)-1 && SetFilePointer(f, 7, 0, 0) == 7 && ReadFile(f, buf, 8, &n, 0) && n == 8 &&
        buf[0] == 'v' && buf[7] == '\n')
        ok |= F_SEEK;
    long long end = 0;
    if (f != (ptr)-1 && SetFilePointerEx(f, 0, &end, 2) && end == 15 &&
        ReadFile(f, buf, 4, &n, 0) && n == 0)
        ok |= F_SEEK_END;
    if (f != (ptr)-1)
        CloseHandle(f);

    wcopy(path, dir, "/*.txt");
    ptr find = FindFirstFileW(path, find_data);
    if (find != (ptr)-1 && *(u16 *)(find_data + 44) == 'h' && *(u32 *)(find_data + 32) == 15)
        ok |= F_FIND;
    if (find != (ptr)-1 && !FindNextFileW(find, find_data) && GetLastError() == 18)
        ok |= F_FIND_END;
    if (find != (ptr)-1)
        FindClose(find);

    wcopy(path, dir, "/written.bin");
    ptr w = CreateFileW(path, 0x40000000u, 0, 0, 2, 0x80, 0);
    u32 written = 0;
    if (w != (ptr)-1 && WriteFile(w, "abcd", 4, &written, 0) && written == 4)
        ok |= F_WRITE;
    if (w != (ptr)-1)
        CloseHandle(w);

    if (GetFileAttributesW(dir) & 0x10)
        ok |= F_ATTR;
    wcopy(path, dir, "/nothing-here");
    if (GetFileAttributesW(path) == 0xFFFFFFFFu && GetLastError() == 2 &&
        CreateFileW(path, 0x80000000u, 1, 0, 3, 0x80, 0) == (ptr)-1 && GetLastError() == 2)
        ok |= F_MISSING;
    return ok;
}

/* ---- a DLL the title ships ------------------------------------------- */

/* victory-probe-dll.dll (probe_dll.c): mapped from the title's folder by
 * the loader, not provided by the host. */
IMPORT u32 DllProbeState(void);
IMPORT u32 DllProbeAdd(u32, u32);
IMPORT u32 DllProbeTls(u32);
IMPORT u32 DllProbeThreadAttaches(void);
IMPORT u32 DllProbeTlsThreadAttaches(void);
IMPORT void DllProbeThrow(u32);
IMPORT u32 DllProbeFinallyRuns(void);
IMPORT u32 DllProbeCatchInside(void);
IMPORT u32 DllProbeCall(u32 (*)(void));
IMPORT ptr GetModuleHandleA(const char *);
IMPORT ptr GetProcAddress(ptr, const char *);

enum {
    D_ATTACH = 1u << 0,
    D_STATIC = 1u << 1,
    D_CALL = 1u << 2,
    D_TLS_MAIN = 1u << 3,
    D_TLS_THREAD = 1u << 4,
    D_THREAD_NOTICE = 1u << 5,
    D_TLS_CALLBACK_THREAD = 1u << 6,
    D_THROW_CROSS = 1u << 7,
    D_FINALLY = 1u << 8,
    D_CATCH_INSIDE = 1u << 9,
    D_CATCH_BELOW = 1u << 10,
    D_PROC = 1u << 11,
};
#define DLL_ALL 0xFFFu

static volatile u32 dll_worker_tls;
static ptr dll_worker_done;

static u32 dll_worker(ptr param)
{
    (void)param;
    dll_worker_tls = DllProbeTls(5);
    SetEvent(dll_worker_done);
    return 0;
}

static u32 dll_raiser(void)
{
    RaiseException(0xE000000A, 0, 0, 0);
    return 0;
}

/* Everything the DLL has that the executable does not. Run on a fresh
 * thread (its own TLS). */
u32 probe_dll(ptr unused)
{
    u32 ok = 0;
    (void)unused;

    /* DllMain and the TLS callback ran before this, in that order, with
     * static TLS in place, and were told the executable imports the DLL. */
    u32 state = DllProbeState();
    if ((state & 0xB) == 0xB)
        ok |= D_ATTACH;
    if (state & 4)
        ok |= D_STATIC;
    if (DllProbeAdd(20, 22) == 42)
        ok |= D_CALL;

    if (DllProbeTls(1) == 501)
        ok |= D_TLS_MAIN;
    dll_worker_done = CreateEventW(0, 1, 0, 0);
    ptr thread = CreateThread(0, 0, (ptr)dll_worker, 0, 0, 0);
    if (thread && WaitForSingleObject(dll_worker_done, 5000) == 0) {
        /* The worker has its own copy (500 + 5), ours is unchanged. */
        if (dll_worker_tls == 505 && DllProbeTls(0) == 501)
            ok |= D_TLS_THREAD;
        WaitForSingleObject(thread, 5000);
    }
    if (thread)
        CloseHandle(thread);
    CloseHandle(dll_worker_done);
    /* The DLL heard of the thread (DllMain) and so did its TLS callback. */
    if (DllProbeThreadAttaches() >= 1)
        ok |= D_THREAD_NOTICE;
    if (DllProbeTlsThreadAttaches() >= 1)
        ok |= D_TLS_CALLBACK_THREAD;

    /* An exception raised three frames down in the DLL, past its __finally,
     * is caught here, in another image. */
    u32 finally_before = DllProbeFinallyRuns();
    int caught = 0;
    seen_code = 0;
    __try {
        DllProbeThrow(0xE0000007);
    } __except (record_filter((struct exception_pointers *)_exception_info())) {
        caught = 1;
    }
    if (caught && seen_code == 0xE0000007 && seen_kind == 0xD11)
        ok |= D_THROW_CROSS;
    if (DllProbeFinallyRuns() == finally_before + 1)
        ok |= D_FINALLY;

    if (DllProbeCatchInside() == 1)
        ok |= D_CATCH_INSIDE;
    /* Raised here, caught in the DLL's frame above ours. */
    if (DllProbeCall(dll_raiser) == 1)
        ok |= D_CATCH_BELOW;

    /* The title finds the DLL by name, as the Windows loader would. */
    ptr module = GetModuleHandleA("victory-probe-dll.dll");
    ptr add = module ? GetProcAddress(module, "DllProbeAdd") : 0;
    if (add && add == (ptr)DllProbeAdd && ((u32(*)(u32, u32))add)(1, 2) == 3)
        ok |= D_PROC;
    return ok;
}

/* The DLL forwards SetError to KERNEL32.SetLastError. Use distinct values to
 * check both the static import and the address returned to the title. */
IMPORT void SetError(u32);
u32 probe_forwarders(ptr unused)
{
    (void)unused;
    u32 ok = 0;
    ptr module = GetModuleHandleA("victory-probe-dll.dll");
    SetError(41);
    if (GetLastError() == 41)
        ok |= 1;
    if (!module)
        return ok;
    void (*set_error)(u32) = (void (*)(u32))GetProcAddress(module, "SetError");
    if (set_error) {
        set_error(42);
        if (GetLastError() == 42)
            ok |= 2;
    }
    if (GetProcAddress(module, "AddAlias") == (ptr)DllProbeAdd)
        ok |= 4;
    SetLastError(0);
    if (!GetProcAddress(module, "Loop") && GetLastError() == 127)
        ok |= 8;
    SetLastError(0);
    if (!GetProcAddress(module, "Missing") && GetLastError() == 127)
        ok |= 16;
    int (*initialize)(void) = (int (*)(void))GetProcAddress(module, "Initialize");
    if (initialize && initialize() == 0)
        ok |= 32;
    return ok;
}

/* ---- module handles of system DLLs -------------------------------------- */

IMPORT ptr GetModuleHandleW(const u16 *);
IMPORT u32 GetModuleFileNameW(ptr, u16 *, u32);
IMPORT int FreeLibrary(ptr);

enum {
    M_KERNEL32 = 1u << 0,
    M_APISET = 1u << 1,
    M_STABLE = 1u << 2,
    M_HEADER = 1u << 3,
    M_SLEEPCV = 1u << 4,
    M_WAKEALL = 1u << 5,
    M_TICK_CALL = 1u << 6,
    M_MISSING = 1u << 7,
    M_FILENAME = 1u << 8,
    M_FREE = 1u << 9,
    M_ORDINAL_MISSING = 1u << 10,
    M_ALL = (1u << 11) - 1,
};

/* What a VC runtime start-up does: GetModuleHandleW for an API set and for
 * kernel32, then GetProcAddress for the condition-variable functions. */
u32 probe_modules(ptr unused)
{
    (void)unused;
    u32 ok = 0;
    ptr k32 = GetModuleHandleW((const u16 *)L"kernel32.dll");
    ptr set = GetModuleHandleW((const u16 *)L"api-ms-win-core-synch-l1-2-0.dll");
    if (k32)
        ok |= M_KERNEL32;
    if (set && set != k32)
        ok |= M_APISET;
    if (k32 && GetModuleHandleW((const u16 *)L"KERNEL32.DLL") == k32)
        ok |= M_STABLE;
    if (k32 && (((u64)k32 & 0xFFFF) == 0) && ((const char *)k32)[0] == 'M' && ((const char *)k32)[1] == 'Z') {
        u32 pe = *(const u32 *)((const char *)k32 + 0x3C);
        if (*(const u32 *)((const char *)k32 + pe) == 0x00004550u)
            ok |= M_HEADER;
    }
    if (set && GetProcAddress(set, "SleepConditionVariableCS"))
        ok |= M_SLEEPCV;
    if (set && GetProcAddress(set, "WakeAllConditionVariable"))
        ok |= M_WAKEALL;
    u64 (*tick)(void) = k32 ? (u64(*)(void))GetProcAddress(k32, "GetTickCount64") : 0;
    if (tick) {
        u64 t0 = tick();
        Sleep(20);
        if (tick() >= t0 + 10 && GetTickCount64() >= t0 + 10)
            ok |= M_TICK_CALL;
    }
    SetLastError(0);
    if (k32 && !GetProcAddress(k32, "NoSuchVictoryFunction") && GetLastError() == 127)
        ok |= M_MISSING;
    u16 name[64];
    u32 n = k32 ? GetModuleFileNameW(k32, name, 64) : 0;
    if (n == 32 && name[0] == 'C' && name[3] == 'W' && name[n - 1] == 'l')
        ok |= M_FILENAME;
    if (k32 && FreeLibrary(k32))
        ok |= M_FREE;
    if (k32 && !GetProcAddress(k32, (const char *)(u64)0x7FFE))
        ok |= M_ORDINAL_MISSING;
    return ok;
}

/* ---- HLE objects called through their method tables ---------------------- */

/* The task-queue provider's id (the provider and its own interface id). */
static const u8 TASK_QUEUE_ID[16] = {0xcb, 0x7d, 0x3b, 0x07, 0xcf, 0x1f, 0x30, 0x40,
                                     0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28};

enum {
    V_QUERY = 1u << 0,
    V_ADDREF = 1u << 1,
    V_CREATE = 1u << 2,
    V_SUBMIT = 1u << 3,
    V_DISPATCH = 1u << 4,
    V_CB_RAN = 1u << 5,
    V_CB_TEB = 1u << 6,
    V_SPARE = 1u << 7,
    V_CLOSE = 1u << 8,
};

static volatile u32 vt_ran;
static volatile u64 vt_context;
static volatile u32 vt_teb_ok;

/* Run by the host (XTaskQueueDispatch) on this thread: title code called
 * from our runtime, so GS must be the TEB again. */
static void vt_callback(ptr context, u8 canceled)
{
    u64 teb = read_gs(0x30);
    vt_teb_ok = teb != 0 && read_gs(0x30) == *(u64 *)(teb + 0x30);
    vt_context = (u64)context;
    vt_ran += canceled ? 100 : 1;
}

u32 probe_vtable(ptr unused)
{
    (void)unused;
    u32 ok = 0;
    ptr object = 0;
    if (QueryApiImpl(TASK_QUEUE_ID, TASK_QUEUE_ID, &object) != 0 || !object)
        return 0;
    ok |= V_QUERY;
    /* The method table the title got is read by title code at [object]. */
    void **vtable = *(void ***)object;
    if (((u32(*)(ptr))vtable[1])(object) == 1)
        ok |= V_ADDREF;
    ptr queue = 0;
    if (((int (*)(ptr, u32, u32, ptr *))vtable[0x60 / 8])(object, 0, 0, &queue) == 0 && queue)
        ok |= V_CREATE;
    if (!queue)
        return ok;
    if (((int (*)(ptr, ptr, u32, ptr, ptr))vtable[0x90 / 8])(object, queue, 0, (ptr)0x5150, (ptr)vt_callback) == 0)
        ok |= V_SUBMIT;
    if (((u8(*)(ptr, ptr, u32, u32))vtable[0x80 / 8])(object, queue, 0, 1000))
        ok |= V_DISPATCH;
    if (vt_ran == 1 && vt_context == 0x5150)
        ok |= V_CB_RAN;
    if (vt_teb_ok)
        ok |= V_CB_TEB;
    /* A slot past the end of our table answers E_NOTIMPL instead of
     * jumping into whatever follows it. */
    if (((int (*)(ptr))vtable[0x100 / 8])(object) == (int)0x80004001u)
        ok |= V_SPARE;
    ((void (*)(ptr, ptr))vtable[0x88 / 8])(object, queue);
    ok |= V_CLOSE;
    return ok;
}

/* ---- the Universal C runtime ------------------------------------------- */

/* ucrtbase.dll and api-ms-win-crt-math-l1-1-0.dll: victory-hle's host_posix::ucrt. */
IMPORT u64 strlen(const char *);
IMPORT int strcmp(const char *, const char *);
IMPORT int _stricmp(const char *, const char *);
IMPORT const char *strstr(const char *, const char *);
IMPORT ptr memcpy(ptr, const void *, u64);
IMPORT ptr memset(ptr, int, u64);
IMPORT int memcmp(const void *, const void *, u64);
IMPORT u64 wcslen(const u16 *);
IMPORT double strtod(const char *, char **);
IMPORT long strtol(const char *, char **, int);
IMPORT int atoi(const char *);
IMPORT char *_itoa(int, char *, int);
IMPORT void qsort(ptr, u64, u64, int (*)(const void *, const void *));
IMPORT ptr bsearch(const void *, const void *, u64, u64, int (*)(const void *, const void *));
IMPORT int __stdio_common_vsprintf(u64, char *, u64, const char *, ptr, char *);
IMPORT int __stdio_common_vswprintf(u64, u16 *, u64, const u16 *, ptr, char *);
IMPORT int __stdio_common_vsscanf(u64, const char *, u64, const char *, ptr, char *);
IMPORT ptr fopen(const char *, const char *);
IMPORT int fclose(ptr);
IMPORT u64 fwrite(const void *, u64, u64, ptr);
IMPORT u64 fread(ptr, u64, u64, ptr);
IMPORT int fseek(ptr, long, int);
IMPORT long long _ftelli64(ptr);
IMPORT int remove(const char *);
IMPORT int _stat64(const char *, ptr);
IMPORT int _access(const char *, int);
IMPORT ptr malloc(u64);
IMPORT ptr calloc(u64, u64);
IMPORT ptr realloc(ptr, u64);
IMPORT void free(ptr);
IMPORT ptr _aligned_malloc(u64, u64);
IMPORT void _aligned_free(ptr);
IMPORT u64 _msize(ptr);
IMPORT int _gmtime64_s(ptr, const long long *);
IMPORT u64 strftime(char *, u64, const char *, const void *);
IMPORT int rand(void);
IMPORT void srand(u32);
IMPORT int *_errno(void);
IMPORT int *__p___argc(void);
IMPORT void _initterm(void (**)(void), void (**)(void));
IMPORT int _initialize_onexit_table(ptr);
IMPORT int _register_onexit_function(ptr, void (*)(void));
IMPORT int _execute_onexit_table(ptr);
IMPORT ptr signal(int, void (*)(int));
IMPORT int raise(int);
IMPORT u64 _beginthreadex(ptr, u32, u32 (*)(ptr), ptr, u32, u32 *);

IMPORT double sqrt(double);
IMPORT double pow(double, double);
IMPORT double fmod(double, double);
IMPORT double ldexp(double, int);
IMPORT double frexp(double, int *);
IMPORT float floorf(float);
IMPORT float sinf(float);
IMPORT int _finite(double);

enum {
    U_STR = 1u << 0,
    U_MATH = 1u << 1,
    U_CONV = 1u << 2,
    U_SORT = 1u << 3,
    U_FMT = 1u << 4,
    U_WFMT = 1u << 5,
    U_FILE = 1u << 6,
    U_HEAP = 1u << 7,
    U_TIME = 1u << 8,
    U_INIT = 1u << 9,
    U_ONEXIT = 1u << 10,
    U_RAND = 1u << 11,
    U_SIGNAL = 1u << 12,
    U_THREAD = 1u << 13,
    U_STAT = 1u << 14,
    U_SCAN = 1u << 15,
    U_ERRNO = 1u << 16,
    U_ALL = (1u << 17) - 1,
};

static int ucrt_sprintf(char *buf, u64 n, const char *fmt, ...)
{
    __builtin_va_list ap;
    __builtin_va_start(ap, fmt);
    int r = __stdio_common_vsprintf(2, buf, n, fmt, 0, ap);
    __builtin_va_end(ap);
    return r;
}

static int ucrt_swprintf(u16 *buf, u64 n, const u16 *fmt, ...)
{
    __builtin_va_list ap;
    __builtin_va_start(ap, fmt);
    int r = __stdio_common_vswprintf(2, buf, n, fmt, 0, ap);
    __builtin_va_end(ap);
    return r;
}

static int ucrt_sscanf(const char *in, const char *fmt, ...)
{
    __builtin_va_list ap;
    __builtin_va_start(ap, fmt);
    int r = __stdio_common_vsscanf(0, in, (u64)-1, fmt, 0, ap);
    __builtin_va_end(ap);
    return r;
}

static int same(const char *a, const char *b) { return strcmp(a, b) == 0; }

static int by_value(const void *a, const void *b)
{
    int x = *(const int *)a, y = *(const int *)b;
    return x < y ? -1 : x > y;
}

static int init_calls, init_order;
static void init_a(void) { init_calls++; init_order = init_order * 10 + 1; }
static void init_b(void) { init_calls++; init_order = init_order * 10 + 2; }
static void (*init_table[3])(void) = { init_a, 0, init_b };

static int exit_order;
static void exit_a(void) { exit_order = exit_order * 10 + 1; }
static void exit_b(void) { exit_order = exit_order * 10 + 2; }

static int signal_seen;
static void on_signal(int sig) { signal_seen = sig; }

static u32 thread_value;
static u32 ucrt_thread(ptr arg)
{
    thread_value = (u32)(u64)arg + 1;
    return 7;
}

/* `dir` is a narrow path of a folder the probe may write in. */
u32 probe_ucrt(const char *dir)
{
    u32 ok = 0;
    char buf[256];

    if (strlen("hello") == 5 && strcmp("a", "b") < 0 && _stricmp("AbC", "aBc") == 0 &&
        strstr("hello", "ll") && !strstr("hello", "z") && wcslen((const u16 *)L"wide") == 4) {
        u8 a[8], b[8];
        memset(a, 7, 8);
        memcpy(b, a, 8);
        if (memcmp(a, b, 8) == 0)
            ok |= U_STR;
    }

    /* Windows ABI: the doubles arrive in xmm0/xmm1 and the int in edx. */
    int e = 0;
    double m = frexp(48.0, &e);
    if (pow(2.0, 10.0) == 1024.0 && fmod(7.5, 2.0) == 1.5 && ldexp(1.5, 4) == 24.0 &&
        sqrt(16.0) == 4.0 && floorf(2.7f) == 2.0f && _finite(1.5) && !_finite(1e308 * 10) &&
        m == 0.75 && e == 6 && sinf(0.0f) == 0.0f)
        ok |= U_MATH;

    char *end;
    if (strtod("3.5xyz", &end) == 3.5 && *end == 'x' && strtol("-42", 0, 10) == -42 &&
        atoi("123") == 123 && same(_itoa(255, buf, 16), "ff"))
        ok |= U_CONV;

    int v[6] = { 5, 3, 9, 1, 7, 3 };
    qsort(v, 6, 4, by_value);
    int key = 7;
    int *hit = (int *)bsearch(&key, v, 6, 4, by_value);
    if (v[0] == 1 && v[1] == 3 && v[2] == 3 && v[5] == 9 && hit == &v[4])
        ok |= U_SORT;

    int n = ucrt_sprintf(buf, sizeof buf, "%d|%s|%.2f|%I64d|%x", 42, "ok", 3.14159, -5LL, 255);
    if (n == 16 && same(buf, "42|ok|3.14|-5|ff"))
        ok |= U_FMT;

    u16 wbuf[32];
    int wn = ucrt_swprintf(wbuf, 32, (const u16 *)L"%ls=%d", (const u16 *)L"k", 9);
    if (wn == 3 && wbuf[0] == 'k' && wbuf[1] == '=' && wbuf[2] == '9' && wbuf[3] == 0)
        ok |= U_WFMT;

    int i1 = 0;
    double d1 = 0;
    char word[16];
    if (ucrt_sscanf("12 3.5 word", "%d %lf %s", &i1, &d1, word) == 3 && i1 == 12 && d1 == 3.5 &&
        same(word, "word"))
        ok |= U_SCAN;

    char path[300];
    ucrt_sprintf(path, sizeof path, "%s/ucrt.bin", dir);
    ptr f = fopen(path, "wb");
    if (f && fwrite("abcdef", 1, 6, f) == 6) {
        fclose(f);
        f = fopen(path, "rb");
        char got[4] = { 0 };
        if (f && fseek(f, 2, 0) == 0 && fread(got, 1, 3, f) == 3 && same(got, "cde") &&
            _ftelli64(f) == 5)
            ok |= U_FILE;
        if (f)
            fclose(f);
    }
    u8 st[56];
    if (_stat64(path, st) == 0 && *(long long *)(st + 24) == 6 && (*(u16 *)(st + 6) & 0x8000) &&
        _access(path, 0) == 0 && remove(path) == 0 && _access(path, 0) == -1)
        ok |= U_STAT;

    u8 *p = (u8 *)malloc(100);
    ptr z = calloc(4, 8);
    ptr al = _aligned_malloc(64, 64);
    if (p && z && al && ((u64)al & 63) == 0 && ((u64)p & 15) == 0) {
        memset(p, 1, 100);
        p = (u8 *)realloc(p, 1000);
        if (p && p[99] == 1 && _msize(p) >= 1000 && *(u64 *)z == 0)
            ok |= U_HEAP;
    }
    free(p);
    free(z);
    _aligned_free(al);

    long long t = 31536000; /* 1971-01-01 */
    int tm[9];
    char date[32];
    if (_gmtime64_s(tm, &t) == 0 && tm[5] == 71 && tm[4] == 0 && tm[3] == 1 &&
        strftime(date, sizeof date, "%Y-%m-%d", tm) == 10 && same(date, "1971-01-01"))
        ok |= U_TIME;

    /* Title function pointers, called by the host: null entries are skipped. */
    _initterm(init_table, init_table + 3);
    if (init_calls == 2 && init_order == 12)
        ok |= U_INIT;

    u64 table[3] = { 0, 0, 0 };
    _initialize_onexit_table(table);
    _register_onexit_function(table, exit_a);
    _register_onexit_function(table, exit_b);
    _execute_onexit_table(table);
    if (exit_order == 21) /* last registered runs first */
        ok |= U_ONEXIT;

    srand(1);
    int r1 = rand(), r2 = rand();
    if (r1 == 41 && r2 == 18467)
        ok |= U_RAND;

    signal(2, on_signal);
    if (raise(2) == 0 && signal_seen == 2)
        ok |= U_SIGNAL;

    u32 tid = 0;
    u64 th = _beginthreadex(0, 0, ucrt_thread, (ptr)41, 0, &tid);
    if (th && WaitForSingleObject((ptr)th, 5000) == 0 && thread_value == 42 && *__p___argc() >= 1)
        ok |= U_THREAD;
    if (th)
        CloseHandle((ptr)th);

    *_errno() = 0;
    double bad = sqrt(-1.0);
    if (bad != bad && *_errno() == 33)
        ok |= U_ERRNO;
    return ok;
}
