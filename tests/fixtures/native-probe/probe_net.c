/* The probe's second half, for the native macOS host's network, media and
 * audio imports: Winsock (by ordinal, as titles import it), WinHTTP,
 * Media Foundation and XAudio2 (ordinal 1). Built into the same exe as
 * probe.c by tools/macos/build-native-probe.sh. Each check that passes sets
 * one bit; probe_netaudio returns NET_ALL when everything worked.
 *
 * The XAudio2 calls go through COM vtables of host stubs, with a float
 * argument in a Windows register and eight arguments on the stack.
 */
typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long long u64;
typedef void *ptr;

#define IMPORT __declspec(dllimport)

IMPORT void Sleep(u32);
IMPORT u32 GetLastError(void);

/* ws2_32.dll, by ordinal (probe-ws2.def). */
IMPORT int WSAStartup(u16, ptr);
IMPORT int WSACleanup(void);
IMPORT int WSAGetLastError(void);
IMPORT u64 socket(int, int, int);
IMPORT int closesocket(u64);
IMPORT int bind(u64, const void *, int);
IMPORT int listen(u64, int);
IMPORT int connect(u64, const void *, int);
IMPORT u64 accept(u64, void *, int *);
IMPORT int getsockname(u64, void *, int *);
IMPORT int send(u64, const void *, int, int);
IMPORT int recv(u64, void *, int, int);
IMPORT int select(int, void *, void *, void *, const void *);
IMPORT u16 htons(u16);
IMPORT int ioctlsocket(u64, u32, u32 *);

/* winhttp.dll. */
IMPORT ptr WinHttpOpen(const u16 *, u32, const u16 *, const u16 *, u32);
IMPORT ptr WinHttpConnect(ptr, const u16 *, u16, u32);
IMPORT ptr WinHttpOpenRequest(ptr, const u16 *, const u16 *, const u16 *, const u16 *, const u16 **, u32);
IMPORT int WinHttpSendRequest(ptr, const u16 *, u32, ptr, u32, u32, u64);
IMPORT int WinHttpCloseHandle(ptr);

/* mfplat.dll. */
IMPORT int MFStartup(u32, u32);
IMPORT int MFShutdown(void);
IMPORT int MFCreateSourceResolver(ptr *);

/* xaudio2_9.dll, ordinal 1 (probe-xaudio2.def). */
IMPORT int XAudio2Create(ptr *, u32, u32);

enum {
    N_STARTUP = 1u << 0,
    N_BOUND = 1u << 1,
    N_CONNECT = 1u << 2,
    N_ECHO = 1u << 3,
    N_SELECT = 1u << 4,
    N_HTTP_SETUP = 1u << 5,
    N_HTTP_REFUSED = 1u << 6,
    N_MF = 1u << 7,
    N_XA_CREATE = 1u << 8,
    N_XA_VOICES = 1u << 9,
    N_XA_FLOAT = 1u << 10,
    N_XA_PLAYED = 1u << 11,
};
#define NET_ALL 0xFFFu

/* memset (which the compiler emits for larger initialisers) comes from the import library of
   ucrtbase-probe, as probe.c's. */

static ptr vslot(ptr object, int slot)
{
    return (*(ptr **)object)[slot];
}

struct fd_set64 {
    u32 count;
    u64 sockets[64];
};

static const u16 W_AGENT[] = {'p', 0};
static const u16 W_HOST[] = {'e', 'x', 'a', 'm', 'p', 'l', 'e', '.', 'c', 'o', 'm', 0};
static const u16 W_GET[] = {'G', 'E', 'T', 0};
static const u16 W_PATH[] = {'/', 0};

u32 probe_netaudio(ptr unused)
{
    (void)unused;
    u32 ok = 0;

    /* Winsock: a loopback echo on one thread (a listener's backlog takes the
     * connection before accept). */
    u8 wsadata[408];
    if (WSAStartup(0x0202, wsadata) == 0)
        ok |= N_STARTUP;
    u64 server = socket(2, 1, 6);
    u8 any[16] = {2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0};
    u8 name[16];
    int name_len = 16;
    if (server != ~0ull && bind(server, any, 16) == 0 && listen(server, 1) == 0 &&
        getsockname(server, name, &name_len) == 0 && name_len == 16 && name[4] == 127)
        ok |= N_BOUND;
    u8 loop[16] = {2, 0, name[2], name[3], 127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0};
    u64 client = socket(2, 1, 6);
    if (connect(client, loop, 16) == 0)
        ok |= N_CONNECT;
    struct fd_set64 ready = {1, {server}};
    u32 timeout[2] = {2, 0};
    if (select(0, &ready, 0, 0, timeout) == 1 && ready.count == 1 && ready.sockets[0] == server)
        ok |= N_SELECT;
    u64 peer = accept(server, 0, 0);
    char buf[8] = {0};
    if (peer != ~0ull && send(client, "ping", 4, 0) == 4 && recv(peer, buf, 8, 0) == 4 &&
        buf[0] == 'p' && buf[3] == 'g' && send(peer, "pong", 4, 0) == 4 &&
        recv(client, buf, 8, 0) == 4 && buf[1] == 'o')
        ok |= N_ECHO;
    /* Off the machine is refused, with Winsock's own error. */
    u8 faraway[16] = {2, 0, 0, 80, 8, 8, 8, 8, 0, 0, 0, 0, 0, 0, 0, 0};
    u64 outside = socket(2, 1, 6);
    int refused = connect(outside, faraway, 16) == -1 && WSAGetLastError() == 10051;
    closesocket(outside);
    closesocket(peer);
    closesocket(client);
    closesocket(server);
    WSACleanup();
    if (!refused)
        ok &= ~N_CONNECT;

    /* WinHTTP: set up fine, never sent. */
    ptr session = WinHttpOpen(W_AGENT, 0, 0, 0, 0);
    ptr connection = session ? WinHttpConnect(session, W_HOST, 443, 0) : 0;
    ptr request = connection ? WinHttpOpenRequest(connection, W_GET, W_PATH, 0, 0, 0, 0x800000) : 0;
    if (request)
        ok |= N_HTTP_SETUP;
    if (request && WinHttpSendRequest(request, 0, 0, 0, 0, 0, 0) == 0 && GetLastError() == 12029)
        ok |= N_HTTP_REFUSED;
    if (request)
        WinHttpCloseHandle(request);
    if (connection)
        WinHttpCloseHandle(connection);
    if (session)
        WinHttpCloseHandle(session);

    /* Media Foundation: starts, makes nothing. */
    ptr resolver = (ptr)1;
    if (MFStartup(0x20070, 0) == 0 && MFCreateSourceResolver(&resolver) == (int)0xC00D36C4 &&
        resolver == 0 && MFShutdown() == 0)
        ok |= N_MF;

    /* XAudio2. */
    ptr engine = 0;
    if (XAudio2Create(&engine, 0, 1) != 0 || !engine)
        return ok;
    ok |= N_XA_CREATE;
    ptr master = 0, voice = 0;
    int made = ((int (*)(ptr, ptr *, u32, u32, u32, const u16 *, ptr, int))vslot(engine, 7))(
        engine, &master, 2, 48000, 0, 0, 0, 0);
    /* mono 16-bit 24 kHz PCM; the callback is none. */
    u16 wave[9] = {1, 1, 24000, 0, 48000, 0, 2, 16, 0};
    made = made == 0 && ((int (*)(ptr, ptr *, const void *, u32, float, ptr, ptr, ptr))vslot(engine, 5))(
                            engine, &voice, wave, 0, 2.0f, 0, 0, 0) == 0;
    if (made)
        ok |= N_XA_VOICES;
    if (made) {
        float volume = 0.0f;
        ((int (*)(ptr, float, u32))vslot(voice, 12))(voice, 0.25f, 0);
        ((void (*)(ptr, float *))vslot(voice, 13))(voice, &volume);
        if (volume == 0.25f)
            ok |= N_XA_FLOAT;
        /* 480 frames of a ramp (20 ms at 24 kHz), then played to the end. */
        static short pcm[480];
        for (int i = 0; i < 480; i++)
            pcm[i] = (short)((i % 48) * 300 - 7000);
        u32 buffer[12] = {0x40, 960, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0};
        *(ptr *)(buffer + 2) = pcm;
        int submitted = ((int (*)(ptr, const void *, ptr))vslot(voice, 21))(voice, buffer, 0) == 0;
        /* XAUDIO2_VOICE_STATE, packed to 1 byte as in xaudio2.h: SamplesPlayed at 12. */
        u8 state[24];
        ((void (*)(ptr, void *, u32))vslot(voice, 25))(voice, state, 0);
        int queued_first = *(u32 *)(state + 8) == 1;
        ((int (*)(ptr, u32, u32))vslot(voice, 19))(voice, 0, 0);
        Sleep(300);
        ((void (*)(ptr, void *, u32))vslot(voice, 25))(voice, state, 0);
        if (submitted && queued_first && *(u32 *)(state + 8) == 0 && *(u32 *)(state + 12) == 480 &&
            *(u32 *)(state + 16) == 0)
            ok |= N_XA_PLAYED;
        ((void (*)(ptr))vslot(voice, 18))(voice);
    }
    if (master)
        ((void (*)(ptr))vslot(master, 18))(master);
    ((u32 (*)(ptr))vslot(engine, 2))(engine);
    return ok;
}

/* d3d12_xs.dll: the console device, made by the host's graphics objects and called through the
 * methods of its (bridged) table: four register arguments, a fence made with an interface id and
 * an out pointer, a slot nobody wrote. */
IMPORT int D3D12XboxCreateDevice(ptr, const u32 *, const void *, ptr *);

u32 probe_d3d12(ptr unused)
{
    (void)unused;
    static const u8 IID_DEVICE[16] = {0xf1, 0x19, 0x98, 0x18, 0xb6, 0x1d, 0x57, 0x4b,
                                      0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7};
    static const u8 IID_FENCE[16] = {0xcf, 0x3d, 0x75, 0x0a, 0xd8, 0xc4, 0x91, 0x4b,
                                     0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76};
    u32 ok = 0;
    u32 params[16] = {0x20004};
    ptr device = 0;
    if (D3D12XboxCreateDevice(0, params, IID_DEVICE, &device) == 0 && device)
        ok |= 1;
    if (!device)
        return ok;
    if (((u32(*)(ptr))vslot(device, 7))(device) == 1) /* GetNodeCount */
        ok |= 2;
    u32 options[16] = {0};
    if (((int(*)(ptr, u32, ptr, u32))vslot(device, 13))(device, 0, options, 64) == 0 &&
        options[9] == 40) /* CheckFeatureSupport: GPU address bits */
        ok |= 4;
    ptr fence = 0;
    if (((int(*)(ptr, u64, u32, const void *, ptr *))vslot(device, 36))(device, 5, 0, IID_FENCE,
                                                                       &fence) == 0 &&
        fence)
        ok |= 8; /* CreateFence */
    if (fence && ((u64(*)(ptr))vslot(fence, 8))(fence) == 5) /* GetCompletedValue */
        ok |= 16;
    ptr same = 0;
    if (fence && ((int(*)(ptr, const void *, ptr *))vslot(fence, 7))(fence, IID_DEVICE, &same) == 0 &&
        same == device) /* GetDevice */
        ok |= 32;
    /* A slot nobody wrote answers E_NOTIMPL (and logs), and so does one past the table. */
    if (((int(*)(ptr))vslot(device, 30))(device) == (int)0x80004001)
        ok |= 64;
    if (((int(*)(ptr))vslot(device, 170))(device) == (int)0x80004001)
        ok |= 128;
    if (fence)
        ((u32(*)(ptr))vslot(fence, 2))(fence);
    return ok;
}
