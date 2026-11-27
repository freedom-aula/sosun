//! WASAPI shared-mode output for the native macOS host: the COM objects a
//! title reaches through `CoCreateInstance(MMDeviceEnumerator)` (device
//! enumerator, device, property store, `IAudioClient`, `IAudioRenderClient`,
//! `IAudioClock`), over a ring that CoreAudio pulls from.
//!
//! The title is told the device mixes 48 kHz float stereo and may initialise
//! any PCM or float format; what it releases is converted to float, folded to
//! the device's two channels and queued. The client's event is signalled
//! every period while it runs, as Windows does, and also when it starts.
//! `VICTORY_WASAPI_WAV=<file>` writes what reaches the device as a float
//! WAV (a debugging aid).
//!
//! shortcut: shared mode only, one render endpoint and no capture, no
//! `IAudioStreamVolume`/`ISimpleAudioVolume`, no device notifications; add
//! them when a title asks (each unknown request is logged once).
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::{HashSet, VecDeque};
use std::ffi::c_void;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use victory_gpu::clock;

use super::coreaudio::{self, Opener, Output};
use super::lock;
use super::mixer::{self, Format};
use super::xaudio2::{parse_format, table};

mod spatial;

type HRESULT = i32;

const S_OK: HRESULT = 0;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
const E_NOTFOUND: HRESULT = 0x8007_0490_u32 as i32;
const E_ACCESSDENIED: HRESULT = 0x8007_0005_u32 as i32;
const AUDCLNT_E_NOT_INITIALIZED: HRESULT = 0x8889_0001_u32 as i32;
const AUDCLNT_E_ALREADY_INITIALIZED: HRESULT = 0x8889_0002_u32 as i32;
const AUDCLNT_E_NOT_STOPPED: HRESULT = 0x8889_0005_u32 as i32;
const AUDCLNT_E_BUFFER_TOO_LARGE: HRESULT = 0x8889_0006_u32 as i32;
const AUDCLNT_E_OUT_OF_ORDER: HRESULT = 0x8889_0007_u32 as i32;
const AUDCLNT_E_UNSUPPORTED_FORMAT: HRESULT = 0x8889_0008_u32 as i32;
const AUDCLNT_E_INVALID_SIZE: HRESULT = 0x8889_0009_u32 as i32;
const AUDCLNT_E_BUFFER_OPERATION_PENDING: HRESULT = 0x8889_000B_u32 as i32;
const AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED: HRESULT = 0x8889_000E_u32 as i32;
const AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED: HRESULT = 0x8889_001F_u32 as i32;

const SHARE_MODE_EXCLUSIVE: u32 = 1;
const STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;
const STREAMFLAGS_EVENTCALLBACK: u32 = 0x0004_0000;
const BUFFERFLAGS_SILENT: u32 = 2;

/// What the device mixes, and what CoreAudio is opened at.
const MIX_RATE: u32 = 48_000;
const DEVICE_CHANNELS: usize = 2;
const STEREO: Format = Format { channels: 2, rate: MIX_RATE, bits: 32, float: true };
/// 10 ms, in 100 ns units.
const DEFAULT_PERIOD: i64 = 100_000;

const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6],
        d[7],
    ]
}

const CLSID_ENUMERATOR: [u8; 16] =
    guid(0xBCDE_0395, 0xE52F, 0x467C, [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E]);
const IID_UNKNOWN: [u8; 16] = guid(0, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
const IID_ENUMERATOR: [u8; 16] =
    guid(0xA956_64D2, 0x9614, 0x4F35, [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6]);
const IID_DEVICE: [u8; 16] =
    guid(0xD666_063F, 0x1587, 0x4E43, [0x81, 0xF1, 0xB9, 0x48, 0xE8, 0x07, 0x36, 0x3F]);
const IID_COLLECTION: [u8; 16] =
    guid(0x0BD7_A1BE, 0x7A1A, 0x44DB, [0x83, 0x97, 0xCC, 0x53, 0x92, 0x38, 0x7B, 0x5E]);
const IID_PROPERTY_STORE: [u8; 16] =
    guid(0x886D_8EEB, 0x8CF2, 0x4446, [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99]);
const IID_CLIENT: [u8; 16] =
    guid(0x1CB9_AD4C, 0xDBFA, 0x4C32, [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2]);
const IID_CLIENT2: [u8; 16] =
    guid(0x7267_78CD, 0xF60A, 0x4EDA, [0x82, 0xDE, 0xE4, 0x76, 0x10, 0xCD, 0x78, 0xAA]);
const IID_CLIENT3: [u8; 16] =
    guid(0x7ED4_EE07, 0x8E67, 0x4CD4, [0x8C, 0x1A, 0x2B, 0x7A, 0x59, 0x87, 0xAD, 0x42]);
const IID_RENDER: [u8; 16] =
    guid(0xF294_ACFC, 0x3146, 0x4483, [0xA7, 0xBF, 0xAD, 0xDC, 0xA7, 0xC2, 0x60, 0xE2]);
const IID_CLOCK: [u8; 16] =
    guid(0xCD63_314F, 0x3FBA, 0x4A1B, [0x81, 0x2C, 0xEF, 0x96, 0x35, 0x87, 0x28, 0xE7]);
/// `PKEY_Device_FriendlyName` and its siblings share this format id.
const FMTID_DEVICE: [u8; 16] =
    guid(0xA45C_254E, 0xDF1C, 0x4EFD, [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0]);
const FMTID_INTERFACE: [u8; 16] =
    guid(0x026E_516E, 0xB814, 0x414B, [0x83, 0xCD, 0x85, 0x6D, 0x6F, 0xEF, 0x48, 0x22]);
const FMTID_ENGINE: [u8; 16] =
    guid(0xF19F_064D, 0x082C, 0x4E27, [0xBC, 0x73, 0x68, 0x82, 0xA1, 0xBB, 0x8E, 0x4C]);
const SUBTYPE_FLOAT: [u8; 16] =
    guid(3, 0, 0x0010, [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71]);

fn read_guid(p: *const c_void) -> [u8; 16] {
    let mut g = [0u8; 16];
    if !p.is_null() {
        // SAFETY: a 16-byte GUID from the title.
        g.copy_from_slice(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), 16) });
    }
    g
}

fn warn_once(what: String) {
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    if lock(&SEEN).get_or_insert_with(HashSet::new).insert(what.clone()) {
        tracing::warn!("WASAPI: {what}");
    }
}

/// Memory the title may "free" with `CoTaskMemFree`, which here frees
/// nothing: it lives on.
fn leak<T: 'static>(v: T) -> *mut T {
    Box::into_raw(Box::new(v))
}

fn wide(s: &str) -> *mut u16 {
    Box::leak(s.encode_utf16().chain([0]).collect::<Vec<_>>().into_boxed_slice()).as_mut_ptr()
}

fn put<T>(out: *mut T, v: T) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: the title's out parameter.
    unsafe { out.write_unaligned(v) };
    S_OK
}

// The ring and what feeds it.

/// Frames of interleaved floats, in device channels.
pub struct Ring {
    samples: VecDeque<f32>,
    channels: usize,
}

impl Ring {
    pub fn new(channels: usize) -> Ring {
        Ring { samples: VecDeque::new(), channels }
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.samples.extend(samples);
    }

    /// Fills `out` from the queue, silence for what is missing; returns the
    /// frames that were real.
    pub fn pull(&mut self, out: &mut [f32]) -> usize {
        let n = out.len().min(self.samples.len()) / self.channels * self.channels;
        for (o, s) in out.iter_mut().zip(self.samples.drain(..n)) {
            *o = s;
        }
        out[n..].fill(0.0);
        n / self.channels
    }
}

/// Folds `n`-channel frames (Windows channel order) to stereo.
fn fold(src: &[f32], n: usize, out: &mut Vec<f32>) {
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
    let w: &[(f32, f32)] = match n {
        1 => &[(1.0, 1.0)],
        2 => &[(1.0, 0.0), (0.0, 1.0)],
        3 => &[(1.0, 0.0), (0.0, 1.0), (0.0, 0.0)],
        4 => &[(1.0, 0.0), (0.0, 1.0), (H, 0.0), (0.0, H)],
        5 => &[(1.0, 0.0), (0.0, 1.0), (H, H), (H, 0.0), (0.0, H)],
        6 => &[(1.0, 0.0), (0.0, 1.0), (H, H), (0.0, 0.0), (H, 0.0), (0.0, H)],
        7 => &[(1.0, 0.0), (0.0, 1.0), (H, H), (0.0, 0.0), (0.5, 0.5), (H, 0.0), (0.0, H)],
        _ => &[
            (1.0, 0.0),
            (0.0, 1.0),
            (H, H),
            (0.0, 0.0),
            (H, 0.0),
            (0.0, H),
            (H, 0.0),
            (0.0, H),
        ],
    };
    let (sl, sr) = w.iter().fold((0.0, 0.0), |(l, r), &(a, b)| (l + a, r + b));
    let gain = 1.0 / sl.max(sr).max(1.0);
    for frame in src.chunks_exact(n) {
        let (mut l, mut r) = (0.0, 0.0);
        for (s, &(a, b)) in frame.iter().zip(w) {
            l += s * a;
            r += s * b;
        }
        out.extend([l * gain, r * gain]);
    }
}

struct Wav {
    file: File,
    frames: u64,
}

impl Wav {
    fn create(path: &str) -> Option<Wav> {
        let mut wav = Wav { file: File::create(path).ok()?, frames: 0 };
        wav.header().ok()?;
        Some(wav)
    }

    /// Writes valid sizes at any time: runs end by being killed.
    fn header(&mut self) -> std::io::Result<()> {
        let data = u32::try_from(self.frames * 8).unwrap_or(u32::MAX - 36);
        let mut h = Vec::new();
        h.extend(b"RIFF");
        h.extend((36 + data).to_le_bytes());
        h.extend(b"WAVEfmt ");
        h.extend(16u32.to_le_bytes());
        h.extend(3u16.to_le_bytes());
        h.extend(2u16.to_le_bytes());
        h.extend(MIX_RATE.to_le_bytes());
        h.extend((MIX_RATE * 8).to_le_bytes());
        h.extend(8u16.to_le_bytes());
        h.extend(32u16.to_le_bytes());
        h.extend(b"data");
        h.extend(data.to_le_bytes());
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&h)?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }
}

struct State {
    ring: Ring,
    running: bool,
    /// The title's event handle.
    event: usize,
    since_event: usize,
    /// Frames the device has played, for `IAudioClock`.
    position: u64,
    /// Frames the title released and their peak since the last log line.
    released: u64,
    peak: f32,
    logged: Instant,
    wav: Option<Wav>,
}

/// What the title's objects and CoreAudio's thread share.
pub struct Shared {
    state: Mutex<State>,
    format: Format,
    buffer_frames: usize,
    period: usize,
    /// The buffer the title writes into between `GetBuffer` and
    /// `ReleaseBuffer`, and how many frames it asked for.
    staging: Mutex<(Vec<u8>, Option<usize>)>,
}

impl Shared {
    pub fn new(format: Format, buffer_frames: usize, period: usize) -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(State {
                ring: Ring::new(DEVICE_CHANNELS),
                running: false,
                event: 0,
                since_event: 0,
                position: 0,
                released: 0,
                peak: 0.0,
                logged: Instant::now(),
                wav: std::env::var("VICTORY_WASAPI_WAV").ok().and_then(|p| Wav::create(&p)),
            }),
            format,
            buffer_frames,
            period,
            staging: Mutex::new((vec![0; buffer_frames * format.frame_bytes()], None)),
        })
    }

    pub fn padding(&self) -> usize {
        lock(&self.state).ring.frames()
    }

    pub fn get_buffer(&self, frames: usize) -> Result<*mut u8, HRESULT> {
        let mut staging = lock(&self.staging);
        if staging.1.is_some() {
            return Err(AUDCLNT_E_OUT_OF_ORDER);
        }
        if frames > self.buffer_frames.saturating_sub(self.padding()) {
            return Err(AUDCLNT_E_BUFFER_TOO_LARGE);
        }
        staging.1 = Some(frames);
        Ok(staging.0.as_mut_ptr())
    }

    pub fn release(&self, frames: usize, flags: u32) -> HRESULT {
        let mut staging = lock(&self.staging);
        let Some(asked) = staging.1.take() else {
            return AUDCLNT_E_OUT_OF_ORDER;
        };
        if frames > asked {
            staging.1 = Some(asked);
            return AUDCLNT_E_INVALID_SIZE;
        }
        let f = self.format;
        let stereo = if flags & BUFFERFLAGS_SILENT != 0 {
            vec![0.0; frames * DEVICE_CHANNELS]
        } else {
            let bytes = &staging.0[..frames * f.frame_bytes()];
            let samples = mixer::decode(f, bytes).unwrap_or_else(|| vec![0.0; frames * f.channels]);
            let mut out = Vec::with_capacity(frames * DEVICE_CHANNELS);
            fold(&samples, f.channels, &mut out);
            out
        };
        drop(staging);
        self.push(&stereo);
        S_OK
    }

    /// Queues stereo frames for the device.
    pub fn push(&self, stereo: &[f32]) {
        let mut state = lock(&self.state);
        state.released += (stereo.len() / DEVICE_CHANNELS) as u64;
        state.peak = stereo.iter().fold(state.peak, |p, s| p.max(s.abs()));
        state.ring.push(stereo);
    }

    /// One tick of the clock of a title's event: the event to set, if the
    /// queue has room for a period, and the time to the next tick.
    ///
    /// The device cannot be that clock. CoreAudio takes frames in cycles of
    /// its own (512 on the Mac this was measured on), so it asks for 10 ms
    /// buffers 10.7 ms apart, with two back to back every 160 ms. An event
    /// set from the device gave a title one signal for those two periods,
    /// and one for any stretch it slept through, so the queue ran dry and
    /// those periods played as silence. Ticks come a period apart, as on
    /// Windows, and half a period apart while the queue is short: a title
    /// that fell behind catches up, and is never given two passes back to
    /// back (the second of two such passes came back silent when a sound
    /// was starting).
    pub fn tick(&self) -> (Option<usize>, Duration) {
        let state = lock(&self.state);
        let frames = state.ring.frames();
        let event = (state.running && state.event != 0 && frames < self.buffer_frames)
            .then_some(state.event);
        let period =
            Duration::from_micros(self.period as u64 * 1_000_000 / u64::from(self.format.rate));
        let short = frames + 2 * self.period < self.buffer_frames;
        (event, if short { period / 2 } else { period })
    }

    /// CoreAudio wants `out` (stereo floats). Returns the title's event
    /// handle if a period has passed.
    pub fn render(&self, out: &mut [f32]) -> Option<usize> {
        let mut state = lock(&self.state);
        if !state.running {
            out.fill(0.0);
            return None;
        }
        let frames = out.len() / DEVICE_CHANNELS;
        state.ring.pull(out);
        state.position += frames as u64;
        if let Some(wav) = &mut state.wav {
            let bytes: Vec<u8> = out.iter().flat_map(|s| s.to_le_bytes()).collect();
            if wav.file.write_all(&bytes).is_ok() {
                wav.frames += frames as u64;
            }
        }
        if state.logged.elapsed().as_secs() >= 1 {
            if state.released > 0 {
                tracing::info!(
                    frames = state.released,
                    peak = state.peak,
                    padding = state.ring.frames(),
                    "WASAPI: render client released frames in the last second"
                );
            }
            (state.released, state.peak, state.logged) = (0, 0.0, Instant::now());
            if let Some(wav) = &mut state.wav {
                let _ = wav.header();
            }
        }
        state.since_event += frames;
        if state.since_event >= self.period {
            state.since_event %= self.period;
            return (state.event != 0).then_some(state.event);
        }
        None
    }
}

fn signal(event: usize) {
    super::SetEvent(event as *mut c_void);
}

// Stateless objects: the enumerator, a device, a collection of one and a
// property store.

#[repr(C)]
struct Simple {
    vtable: *const usize,
    refs: AtomicU32,
    iid: [u8; 16],
}

fn simple(vtable: &'static LazyLock<Vec<usize>>, iid: [u8; 16]) -> *mut Simple {
    leak(Simple { vtable: vtable.as_ptr(), refs: AtomicU32::new(1), iid })
}

macro_rules! methods {
    ($($f:path),* $(,)?) => { [$($f as *const () as usize),*] };
}

static ENUMERATOR: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        simple_query,
        simple_add_ref,
        simple_release,
        enum_endpoints,
        default_endpoint,
        get_device,
        register_notification,
        register_notification,
    ])
});
static DEVICE: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        simple_query,
        simple_add_ref,
        simple_release,
        device_activate,
        device_properties,
        device_id,
        device_state,
    ])
});
static COLLECTION: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        simple_query,
        simple_add_ref,
        simple_release,
        collection_count,
        collection_item
    ])
});
static EMPTY_COLLECTION: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        simple_query,
        simple_add_ref,
        simple_release,
        empty_collection_count,
        empty_collection_item
    ])
});
static PROPERTIES: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        simple_query,
        simple_add_ref,
        simple_release,
        properties_count,
        properties_at,
        properties_value,
        properties_set,
        properties_commit,
    ])
});

/// `CoCreateInstance` for the one class this module implements; `None` for
/// any other.
pub fn create(class: *const c_void, iid: *const c_void, out: *mut *mut c_void) -> Option<HRESULT> {
    if read_guid(class) != CLSID_ENUMERATOR {
        return None;
    }
    let iid = read_guid(iid);
    if out.is_null() {
        return Some(E_POINTER);
    }
    if iid != IID_UNKNOWN && iid != IID_ENUMERATOR {
        warn_once(format!("MMDeviceEnumerator asked for interface {iid:02x?}"));
        return Some(E_NOINTERFACE);
    }
    // SAFETY: the title's out parameter.
    unsafe { out.write(simple(&ENUMERATOR, IID_ENUMERATOR).cast()) };
    Some(S_OK)
}

extern "win64" fn simple_query(
    this: *mut Simple,
    iid: *const c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    let iid = read_guid(iid);
    // SAFETY: the title's object.
    let me = unsafe { &*this };
    if out.is_null() {
        return E_POINTER;
    }
    if iid != IID_UNKNOWN && iid != me.iid {
        warn_once(format!("QueryInterface of {:02x?} for {iid:02x?}", me.iid));
        // SAFETY: the title's out parameter.
        unsafe { out.write(std::ptr::null_mut()) };
        return E_NOINTERFACE;
    }
    me.refs.fetch_add(1, Ordering::AcqRel);
    // SAFETY: as above.
    unsafe { out.write(this.cast()) };
    S_OK
}

extern "win64" fn simple_add_ref(this: *mut Simple) -> u32 {
    // SAFETY: the title's object.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

/// The object is kept on purpose: a few words, and a title that releases
/// once too often must not crash us.
extern "win64" fn simple_release(this: *mut Simple) -> u32 {
    // SAFETY: the title's object.
    unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel).saturating_sub(1) }
}

/// Writes a new object at `out`, allocating only if `out` is non-null.
fn give(out: *mut *mut c_void, make: impl FnOnce() -> *mut Simple) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    put(out, make().cast())
}

const DATA_FLOW_CAPTURE: u32 = 1;
const DATA_FLOW_ALL: u32 = 2;
const DEVICE_STATE_ACTIVE: u32 = 1;

extern "win64" fn enum_endpoints(
    _this: *mut Simple,
    flow: u32,
    mask: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    if flow > DATA_FLOW_ALL {
        put(out, std::ptr::null_mut());
        return E_INVALIDARG;
    }
    let vtable = if flow == DATA_FLOW_CAPTURE || mask & DEVICE_STATE_ACTIVE == 0 {
        &EMPTY_COLLECTION
    } else {
        &COLLECTION
    };
    give(out, || simple(vtable, IID_COLLECTION))
}

extern "win64" fn default_endpoint(
    _this: *mut Simple,
    flow: u32,
    _role: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    if flow != 0 {
        warn_once(format!("default endpoint requested for data flow {flow} (no capture device)"));
        put(out, std::ptr::null_mut());
        return E_NOTFOUND;
    }
    give(out, || simple(&DEVICE, IID_DEVICE))
}

extern "win64" fn get_device(
    _this: *mut Simple,
    _id: *const u16,
    out: *mut *mut c_void,
) -> HRESULT {
    give(out, || simple(&DEVICE, IID_DEVICE))
}

extern "win64" fn register_notification(_this: *mut Simple, _callback: *mut c_void) -> HRESULT {
    S_OK
}

extern "win64" fn collection_count(_this: *mut Simple, count: *mut u32) -> HRESULT {
    put(count, 1)
}

extern "win64" fn collection_item(
    _this: *mut Simple,
    index: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    if index != 0 {
        return E_INVALIDARG;
    }
    give(out, || simple(&DEVICE, IID_DEVICE))
}

extern "win64" fn empty_collection_count(_this: *mut Simple, count: *mut u32) -> HRESULT {
    put(count, 0)
}

extern "win64" fn empty_collection_item(
    _this: *mut Simple,
    _index: u32,
    _out: *mut *mut c_void,
) -> HRESULT {
    E_INVALIDARG
}

extern "win64" fn device_activate(
    _this: *mut Simple,
    iid: *const c_void,
    _context: u32,
    _params: *const c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    let iid = read_guid(iid);
    if out.is_null() {
        return E_POINTER;
    }
    if iid == spatial::IID_CLIENT {
        return put(out, spatial::client().cast());
    }
    if ![IID_CLIENT, IID_CLIENT2, IID_CLIENT3].contains(&iid) {
        warn_once(format!("IMMDevice::Activate of {iid:02x?}"));
        put(out, std::ptr::null_mut());
        return E_NOINTERFACE;
    }
    let client = leak(Client {
        vtable: CLIENT.as_ptr(),
        refs: AtomicU32::new(1),
        active: Mutex::new(None),
    });
    put(out, client.cast())
}

extern "win64" fn device_properties(
    _this: *mut Simple,
    _access: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    give(out, || simple(&PROPERTIES, IID_PROPERTY_STORE))
}

extern "win64" fn device_id(_this: *mut Simple, out: *mut *mut u16) -> HRESULT {
    put(out, wide("{0.0.0.00000000}.{00000000-0000-0000-0000-000000000001}"))
}

extern "win64" fn device_state(_this: *mut Simple, state: *mut u32) -> HRESULT {
    put(state, 1) // DEVICE_STATE_ACTIVE
}

extern "win64" fn properties_count(_this: *mut Simple, count: *mut u32) -> HRESULT {
    put(count, 0)
}

extern "win64" fn properties_at(_this: *mut Simple, _index: u32, _key: *mut u8) -> HRESULT {
    E_INVALIDARG
}

/// A `PROPVARIANT` is 24 bytes: the type at 0, the payload at 8.
extern "win64" fn properties_value(_this: *mut Simple, key: *const u8, value: *mut u8) -> HRESULT {
    if key.is_null() || value.is_null() {
        return E_POINTER;
    }
    let fmtid = read_guid(key.cast());
    // SAFETY: a PROPERTYKEY (16 bytes and a pid) and a PROPVARIANT.
    unsafe {
        let pid = key.add(16).cast::<u32>().read_unaligned();
        std::ptr::write_bytes(value, 0, 24);
        if fmtid == FMTID_DEVICE || fmtid == FMTID_INTERFACE {
            value.cast::<u16>().write_unaligned(31); // VT_LPWSTR
            value.add(8).cast::<*mut u16>().write_unaligned(wide("Default Output"));
        } else if fmtid == FMTID_ENGINE && pid == 0 {
            // `PKEY_AudioEngine_DeviceFormat`: the mix format.
            value.cast::<u16>().write_unaligned(0x41); // VT_BLOB
            value.add(8).cast::<u32>().write_unaligned(40);
            value.add(16).cast::<*mut u8>().write_unaligned(mix_format());
        } else {
            warn_once(format!("property {fmtid:02x?} #{pid} (answered empty)"));
        }
    }
    S_OK
}

extern "win64" fn properties_set(
    _this: *mut Simple,
    _key: *const u8,
    _value: *const u8,
) -> HRESULT {
    E_ACCESSDENIED
}

extern "win64" fn properties_commit(_this: *mut Simple) -> HRESULT {
    S_OK
}

/// The shared-mode mix format, as a `WAVEFORMATEXTENSIBLE` (40 bytes).
fn mix_format() -> *mut u8 {
    let mut w = Vec::with_capacity(40);
    w.extend(0xFFFEu16.to_le_bytes());
    w.extend((DEVICE_CHANNELS as u16).to_le_bytes());
    w.extend(MIX_RATE.to_le_bytes());
    w.extend((MIX_RATE * DEVICE_CHANNELS as u32 * 4).to_le_bytes());
    w.extend((DEVICE_CHANNELS as u16 * 4).to_le_bytes());
    w.extend(32u16.to_le_bytes());
    w.extend(22u16.to_le_bytes()); // cbSize
    w.extend(32u16.to_le_bytes()); // valid bits
    w.extend(3u32.to_le_bytes()); // front left and right
    w.extend(SUBTYPE_FLOAT);
    Box::leak(w.into_boxed_slice()).as_mut_ptr()
}

// IAudioClient.

struct Active {
    shared: Arc<Shared>,
    /// Closes the device with the client.
    _output: Box<dyn Output>,
    event_mode: bool,
}

#[repr(C)]
struct Client {
    vtable: *const usize,
    refs: AtomicU32,
    active: Mutex<Option<Active>>,
}

static CLIENT: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        client_query,
        client_add_ref,
        client_release,
        client_initialize,
        client_buffer_size,
        client_latency,
        client_padding,
        client_format_supported,
        client_mix_format,
        client_period,
        client_start,
        client_stop,
        client_reset,
        client_set_event,
        client_service,
        // IAudioClient2.
        client_offload,
        client_set_properties,
        client_buffer_limits,
        // IAudioClient3.
        client_engine_period,
        client_current_period,
        client_initialize_shared,
    ])
});

static RENDER: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![sub_query, sub_add_ref, sub_release, render_get_buffer, render_release_buffer])
});
static CLOCK: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        sub_query,
        sub_add_ref,
        sub_release,
        clock_frequency,
        clock_position,
        clock_characteristics,
    ])
});

extern "win64" fn client_query(
    this: *mut Client,
    iid: *const c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    let iid = read_guid(iid);
    if out.is_null() {
        return E_POINTER;
    }
    if ![IID_UNKNOWN, IID_CLIENT, IID_CLIENT2, IID_CLIENT3].contains(&iid) {
        warn_once(format!("IAudioClient::QueryInterface of {iid:02x?}"));
        put(out, std::ptr::null_mut());
        return E_NOINTERFACE;
    }
    // SAFETY: the title's object.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) };
    put(out, this.cast())
}

extern "win64" fn client_add_ref(this: *mut Client) -> u32 {
    // SAFETY: the title's object.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

extern "win64" fn client_release(this: *mut Client) -> u32 {
    // SAFETY: the title's object.
    let left = unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel) } - 1;
    if left == 0 {
        // SAFETY: made by `device_activate`, released for the last time:
        // the device closes with it.
        unsafe { drop(Box::from_raw(this)) };
    }
    left
}

fn rate_ok(f: &Format) -> bool {
    (8_000..=384_000).contains(&f.rate) && f.channels <= 8
}

extern "win64" fn client_initialize(
    this: *mut Client,
    mode: u32,
    flags: u32,
    duration: i64,
    _periodicity: i64,
    wave: *const u8,
    _session: *const c_void,
) -> HRESULT {
    initialize(this, mode, flags, duration, wave, coreaudio::open_default)
}

fn initialize(
    this: *mut Client,
    mode: u32,
    flags: u32,
    duration: i64,
    wave: *const u8,
    open: Opener,
) -> HRESULT {
    // SAFETY: the title's object.
    let mut active = lock(&unsafe { &*this }.active);
    if active.is_some() {
        return AUDCLNT_E_ALREADY_INITIALIZED;
    }
    if mode == SHARE_MODE_EXCLUSIVE {
        return AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED;
    }
    if flags & STREAMFLAGS_LOOPBACK != 0 {
        warn_once("loopback capture is not supported".into());
        return AUDCLNT_E_UNSUPPORTED_FORMAT;
    }
    let Some(format) = parse_format(wave).filter(rate_ok) else {
        warn_once("Initialize with an unsupported format".into());
        return AUDCLNT_E_UNSUPPORTED_FORMAT;
    };
    let period = (format.rate / 100) as usize;
    let requested =
        (duration.max(0) as u128 * u128::from(format.rate)).div_ceil(10_000_000) as usize;
    let frames = requested.max(period * 2);
    let shared = Shared::new(format, frames, period);
    let render = shared.clone();
    let output = open(
        format.rate,
        DEVICE_CHANNELS as u32,
        Box::new(move |out| match out {
            Ok(out) => {
                if let Some(event) = render.render(out) {
                    signal(event);
                }
            }
            Err(e) => warn_once(format!("the output failed: {e}")),
        }),
    );
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("WASAPI: no output: {e}");
            return AUDCLNT_E_UNSUPPORTED_FORMAT;
        }
    };
    tracing::info!(
        rate = format.rate,
        channels = format.channels,
        bits = format.bits,
        float = format.float,
        buffer_frames = frames,
        period,
        event = flags & STREAMFLAGS_EVENTCALLBACK != 0,
        "WASAPI: audio client initialised"
    );
    *active = Some(Active {
        shared,
        _output: output,
        event_mode: flags & STREAMFLAGS_EVENTCALLBACK != 0,
    });
    S_OK
}

/// The initialised client's shared state.
fn shared_of(this: *mut Client) -> Result<Arc<Shared>, HRESULT> {
    // SAFETY: the title's object.
    lock(&unsafe { &*this }.active)
        .as_ref()
        .map(|a| a.shared.clone())
        .ok_or(AUDCLNT_E_NOT_INITIALIZED)
}

extern "win64" fn client_buffer_size(this: *mut Client, out: *mut u32) -> HRESULT {
    match shared_of(this) {
        Ok(s) => put(out, s.buffer_frames as u32),
        Err(e) => e,
    }
}

extern "win64" fn client_latency(this: *mut Client, out: *mut i64) -> HRESULT {
    match shared_of(this) {
        Ok(s) => put(out, (s.period as i64 * 10_000_000) / i64::from(s.format.rate)),
        Err(e) => e,
    }
}

extern "win64" fn client_padding(this: *mut Client, out: *mut u32) -> HRESULT {
    match shared_of(this) {
        Ok(s) => put(out, s.padding() as u32),
        Err(e) => e,
    }
}

extern "win64" fn client_format_supported(
    _this: *mut Client,
    mode: u32,
    wave: *const u8,
    closest: *mut *mut u8,
) -> HRESULT {
    if !closest.is_null() {
        put(closest, std::ptr::null_mut());
    }
    if mode == SHARE_MODE_EXCLUSIVE {
        return AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED;
    }
    if parse_format(wave).is_some_and(|f| rate_ok(&f)) {
        S_OK
    } else {
        AUDCLNT_E_UNSUPPORTED_FORMAT
    }
}

extern "win64" fn client_mix_format(_this: *mut Client, out: *mut *mut u8) -> HRESULT {
    put(out, mix_format())
}

extern "win64" fn client_period(
    _this: *mut Client,
    default: *mut i64,
    minimum: *mut i64,
) -> HRESULT {
    if !default.is_null() {
        put(default, DEFAULT_PERIOD);
    }
    if !minimum.is_null() {
        put(minimum, 30_000);
    }
    S_OK
}

extern "win64" fn client_start(this: *mut Client) -> HRESULT {
    let s = match shared_of(this) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let event = {
        let mut state = lock(&s.state);
        if state.running {
            return AUDCLNT_E_NOT_STOPPED;
        }
        state.running = true;
        state.event
    };
    // Windows signals the event at once, so a title that waits on it begins.
    if event != 0 {
        signal(event);
    }
    S_OK
}

extern "win64" fn client_stop(this: *mut Client) -> HRESULT {
    match shared_of(this) {
        Ok(s) => {
            lock(&s.state).running = false;
            S_OK
        }
        Err(e) => e,
    }
}

extern "win64" fn client_reset(this: *mut Client) -> HRESULT {
    let s = match shared_of(this) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if lock(&s.staging).1.is_some() {
        return AUDCLNT_E_BUFFER_OPERATION_PENDING;
    }
    let mut state = lock(&s.state);
    if state.running {
        return AUDCLNT_E_NOT_STOPPED;
    }
    state.ring = Ring::new(DEVICE_CHANNELS);
    state.position = 0;
    S_OK
}

extern "win64" fn client_set_event(this: *mut Client, event: usize) -> HRESULT {
    // SAFETY: the title's object.
    let active = lock(&unsafe { &*this }.active);
    let Some(active) = active.as_ref() else {
        return AUDCLNT_E_NOT_INITIALIZED;
    };
    if !active.event_mode {
        return AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED;
    }
    lock(&active.shared.state).event = event;
    S_OK
}

extern "win64" fn client_service(
    this: *mut Client,
    iid: *const c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    let iid = read_guid(iid);
    let shared = match shared_of(this) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let vtable = if iid == IID_RENDER {
        RENDER.as_ptr()
    } else if iid == IID_CLOCK {
        CLOCK.as_ptr()
    } else {
        warn_once(format!("IAudioClient::GetService of {iid:02x?}"));
        put(out, std::ptr::null_mut());
        return E_NOINTERFACE;
    };
    let sub = leak(Sub { vtable, refs: AtomicU32::new(1), iid, shared });
    put(out, sub.cast())
}

extern "win64" fn client_offload(_this: *mut Client, out: *mut i32) -> HRESULT {
    put(out, 0)
}

extern "win64" fn client_set_properties(_this: *mut Client, _properties: *const c_void) -> HRESULT {
    S_OK
}

extern "win64" fn client_buffer_limits(
    _this: *mut Client,
    _wave: *const u8,
    _event: i32,
    _min: *mut i64,
    _max: *mut i64,
) -> HRESULT {
    E_NOTIMPL
}

extern "win64" fn client_engine_period(
    _this: *mut Client,
    _wave: *const u8,
    default: *mut u32,
    fundamental: *mut u32,
    minimum: *mut u32,
    maximum: *mut u32,
) -> HRESULT {
    for p in [default, minimum, maximum] {
        if !p.is_null() {
            put(p, MIX_RATE / 100);
        }
    }
    put(fundamental, 480)
}

extern "win64" fn client_current_period(
    _this: *mut Client,
    wave: *mut *mut u8,
    period: *mut u32,
) -> HRESULT {
    if wave.is_null() || period.is_null() {
        return E_POINTER;
    }
    put(wave, mix_format());
    put(period, MIX_RATE / 100)
}

extern "win64" fn client_initialize_shared(_this: *mut Client) -> HRESULT {
    warn_once("IAudioClient3::InitializeSharedAudioStream is not implemented".into());
    E_NOTIMPL
}

// IAudioRenderClient and IAudioClock.

#[repr(C)]
struct Sub {
    vtable: *const usize,
    refs: AtomicU32,
    iid: [u8; 16],
    shared: Arc<Shared>,
}

extern "win64" fn sub_query(this: *mut Sub, iid: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let iid = read_guid(iid);
    // SAFETY: the title's object.
    let me = unsafe { &*this };
    if out.is_null() {
        return E_POINTER;
    }
    if iid != IID_UNKNOWN && iid != me.iid {
        warn_once(format!("QueryInterface of {:02x?} for {iid:02x?}", me.iid));
        put(out, std::ptr::null_mut());
        return E_NOINTERFACE;
    }
    me.refs.fetch_add(1, Ordering::AcqRel);
    put(out, this.cast())
}

extern "win64" fn sub_add_ref(this: *mut Sub) -> u32 {
    // SAFETY: the title's object.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

extern "win64" fn sub_release(this: *mut Sub) -> u32 {
    // SAFETY: the title's object.
    let left = unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel) } - 1;
    if left == 0 {
        // SAFETY: made by `client_service`, released for the last time.
        unsafe { drop(Box::from_raw(this)) };
    }
    left
}

extern "win64" fn render_get_buffer(this: *mut Sub, frames: u32, out: *mut *mut u8) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: the title's object.
    match unsafe { &*this }.shared.get_buffer(frames as usize) {
        Ok(p) => put(out, p),
        Err(e) => e,
    }
}

extern "win64" fn render_release_buffer(this: *mut Sub, frames: u32, flags: u32) -> HRESULT {
    // SAFETY: the title's object.
    unsafe { &*this }.shared.release(frames as usize, flags)
}

extern "win64" fn clock_frequency(this: *mut Sub, out: *mut u64) -> HRESULT {
    // SAFETY: the title's object.
    put(out, u64::from(unsafe { &*this }.shared.format.rate))
}

extern "win64" fn clock_position(this: *mut Sub, position: *mut u64, qpc: *mut u64) -> HRESULT {
    if position.is_null() {
        return E_POINTER;
    }
    // SAFETY: the title's object.
    let played = lock(&unsafe { &*this }.shared.state).position;
    if !qpc.is_null() {
        // 100 ns units.
        let now = u128::from(clock::counter()) * 10_000_000 / u128::from(clock::counter_frequency());
        put(qpc, now as u64);
    }
    put(position, played)
}

extern "win64" fn clock_characteristics(_this: *mut Sub, out: *mut u32) -> HRESULT {
    put(out, 1) // AUDIOCLOCK_CHARACTERISTIC_FIXED_FREQ
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO_F32: Format = Format { channels: 2, rate: 48_000, bits: 32, float: true };

    /// A title writes stereo float frames through GetBuffer and ReleaseBuffer.
    fn write(shared: &Shared, frames: &[f32]) {
        let n = frames.len() / 2;
        let p = shared.get_buffer(n).unwrap();
        // SAFETY: the staging buffer holds the frames asked for.
        unsafe { std::ptr::copy_nonoverlapping(frames.as_ptr().cast::<u8>(), p, frames.len() * 4) };
        assert_eq!(shared.release(n, 0), S_OK);
    }

    #[test]
    fn frames_released_come_out_of_the_device_in_order() {
        let shared = Shared::new(STEREO_F32, 960, 480);
        let tone: Vec<f32> = (0..2 * 480).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        write(&shared, &tone);
        assert_eq!(shared.padding(), 480);
        // Stopped: silence, and the frames wait.
        let mut out = vec![1.0; 2 * 240];
        assert!(shared.render(&mut out).is_none());
        assert!(out.iter().all(|&s| s == 0.0));
        {
            let mut state = lock(&shared.state);
            state.running = true;
            state.event = 7;
        }
        let mut heard = Vec::new();
        let mut events = 0;
        for _ in 0..2 {
            let mut out = vec![0.0; 2 * 240];
            events += usize::from(shared.render(&mut out).is_some());
            heard.extend(out);
        }
        assert_eq!(heard, tone);
        assert_eq!(shared.padding(), 0);
        assert_eq!(events, 1); // one period (480 frames) passed
        // Drained: silence.
        let mut out = vec![1.0; 2 * 100];
        shared.render(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    /// The spatial stream's loop, on a clock of microseconds: the device takes
    /// frames in cycles of 512 and the title adds a period each time it finds
    /// its event set. Returns the frames of silence played and the shortest
    /// time between two passes.
    fn play(oversleeps: bool) -> (usize, u64) {
        const PERIOD: usize = 480;
        let shared = Shared::new(STEREO_F32, 5 * PERIOD, PERIOD);
        {
            let mut state = lock(&shared.state);
            state.running = true;
            state.event = 7;
        }
        let (mut tick, mut cycle) = (0, 0);
        let (mut set, mut owed, mut silent) = (false, 0, 0);
        let (mut last, mut closest) = (None, u64::MAX);
        while tick.min(cycle) < 10_000_000_u64 {
            let now = tick.min(cycle);
            if now == tick {
                let (event, wait) = shared.tick();
                set |= event.is_some();
                tick += wait.as_micros() as u64;
            }
            // 15 ms asleep in every second.
            let asleep = oversleeps && now % 1_000_000 < 15_000;
            if set && !asleep {
                set = false;
                shared.push(&[0.25; 2 * PERIOD]);
                closest = closest.min(last.map_or(u64::MAX, |l| now - l));
                last = Some(now);
            }
            if now == cycle {
                owed += 512;
                while owed >= PERIOD {
                    owed -= PERIOD;
                    let mut out = vec![0.0; 2 * PERIOD];
                    shared.render(&mut out);
                    if now >= 100_000 {
                        silent += out.iter().filter(|&&s| s == 0.0).count() / 2;
                    }
                }
                cycle += 10_667;
            }
            assert!(shared.padding() <= 5 * PERIOD);
        }
        (silent, closest)
    }

    #[test]
    fn the_event_clock_feeds_a_device_that_takes_frames_in_its_own_cycles() {
        let (silent, closest) = play(false);
        assert_eq!(silent, 0, "frames of silence played");
        assert!(closest >= 5_000, "two passes {closest} microseconds apart");
    }

    #[test]
    fn a_title_that_wakes_late_catches_up_instead_of_leaving_silence() {
        assert_eq!(play(true).0, 0, "frames of silence played");
    }

    #[test]
    fn the_buffer_limits_what_the_title_may_write() {
        let shared = Shared::new(STEREO_F32, 100, 48);
        write(&shared, &[0.25; 2 * 80]);
        assert_eq!(shared.get_buffer(30), Err(AUDCLNT_E_BUFFER_TOO_LARGE));
        shared.get_buffer(20).unwrap();
        assert_eq!(shared.get_buffer(1), Err(AUDCLNT_E_OUT_OF_ORDER));
        assert_eq!(shared.release(21, 0), AUDCLNT_E_INVALID_SIZE);
        assert_eq!(shared.release(20, BUFFERFLAGS_SILENT), S_OK);
        assert_eq!(shared.padding(), 100);
    }

    #[test]
    fn surround_folds_to_stereo_and_pcm16_converts() {
        let mut out = Vec::new();
        fold(&[0.5, 0.25, 0.0, 0.0, 0.0, 0.0], 6, &mut out);
        assert!(out[0] > 0.1 && out[1] > 0.05 && out[0] > out[1]);
        let mut out = Vec::new();
        fold(&[0.5], 1, &mut out);
        assert_eq!(out, [0.5, 0.5]);
        let f = Format { channels: 2, rate: 44_100, bits: 16, float: false };
        let shared = Shared::new(f, 100, 44);
        let p = shared.get_buffer(1).unwrap();
        // SAFETY: one 16-bit stereo frame fits.
        unsafe {
            p.cast::<i16>().write(16384);
            p.cast::<i16>().add(1).write(-16384);
        }
        shared.release(1, 0);
        lock(&shared.state).running = true;
        let mut out = [0.0; 2];
        shared.render(&mut out);
        assert_eq!(out, [0.5, -0.5]);
    }

    #[test]
    fn the_ring_returns_what_it_was_given() {
        let mut ring = Ring::new(2);
        ring.push(&[0.1, 0.2, 0.3, 0.4]);
        let mut out = [9.0; 6];
        assert_eq!(ring.pull(&mut out), 2);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);
        assert_eq!(ring.frames(), 0);
    }

    #[test]
    fn enum_endpoints_respects_data_flow_and_state_mask() {
        let mut col = std::ptr::null_mut();
        assert_eq!(enum_endpoints(std::ptr::null_mut(), 0, DEVICE_STATE_ACTIVE, &mut col), S_OK);
        let mut count = 99u32;
        // SAFETY: `col` is a `Simple` whose vtable is `COLLECTION`.
        let vt = unsafe { (*(col as *const Simple)).vtable };
        // SAFETY: slot 3 of `COLLECTION` is `collection_count`.
        let get_count: extern "win64" fn(*mut Simple, *mut u32) -> HRESULT =
            unsafe { std::mem::transmute(*vt.add(3)) };
        assert_eq!(get_count(col.cast(), &mut count), S_OK);
        assert_eq!(count, 1);

        // Capture endpoints and inactive masks produce an empty collection.
        for (flow, mask) in [(DATA_FLOW_CAPTURE, DEVICE_STATE_ACTIVE), (0, 0x2)] {
            let mut empty = std::ptr::null_mut();
            assert_eq!(enum_endpoints(std::ptr::null_mut(), flow, mask, &mut empty), S_OK);
            // SAFETY: `empty` is a `Simple` whose vtable is `EMPTY_COLLECTION`.
            let vt = unsafe { (*(empty as *const Simple)).vtable };
            // SAFETY: slot 3 of `EMPTY_COLLECTION` is `empty_collection_count`.
            let get_count: extern "win64" fn(*mut Simple, *mut u32) -> HRESULT =
                unsafe { std::mem::transmute(*vt.add(3)) };
            // SAFETY: slot 4 of `EMPTY_COLLECTION` is `empty_collection_item`.
            let get_item: extern "win64" fn(*mut Simple, u32, *mut *mut c_void) -> HRESULT =
                unsafe { std::mem::transmute(*vt.add(4)) };
            let mut n = 99u32;
            let mut dev = std::ptr::null_mut();
            assert_eq!(get_count(empty.cast(), &mut n), S_OK);
            assert_eq!(n, 0);
            assert_eq!(get_item(empty.cast(), 0, &mut dev), E_INVALIDARG);
        }
        assert_eq!(
            enum_endpoints(std::ptr::null_mut(), 3, DEVICE_STATE_ACTIVE, &mut col),
            E_INVALIDARG
        );
        assert!(col.is_null());
        assert_eq!(
            enum_endpoints(std::ptr::null_mut(), 0, DEVICE_STATE_ACTIVE, std::ptr::null_mut()),
            E_POINTER
        );
    }

    #[test]
    fn current_period_populates_format_and_render_get_buffer_guards_null_out() {
        let mut wave = std::ptr::null_mut();
        let mut period = 0u32;
        assert_eq!(
            client_current_period(std::ptr::null_mut(), std::ptr::null_mut(), &mut period),
            E_POINTER
        );
        assert_eq!(
            client_current_period(std::ptr::null_mut(), &mut wave, std::ptr::null_mut()),
            E_POINTER
        );
        assert_eq!(client_current_period(std::ptr::null_mut(), &mut wave, &mut period), S_OK);
        assert!(!wave.is_null());
        assert_eq!(period, MIX_RATE / 100);
        let fmt = parse_format(wave).expect("valid mix format");
        assert_eq!((fmt.channels, fmt.rate, fmt.bits, fmt.float), (2, MIX_RATE, 32, true));

        // A null out pointer on GetBuffer must not leave staging locked.
        let shared = Shared::new(STEREO_F32, 100, 48);
        let sub = leak(Sub {
            vtable: RENDER.as_ptr(),
            refs: AtomicU32::new(1),
            iid: IID_RENDER,
            shared,
        });
        assert_eq!(render_get_buffer(sub, 10, std::ptr::null_mut()), E_POINTER);
        let mut ptr = std::ptr::null_mut();
        assert_eq!(render_get_buffer(sub, 10, &mut ptr), S_OK);
        assert!(!ptr.is_null());
        assert_eq!(render_release_buffer(sub, 10, 0), S_OK);
        assert_eq!(sub_release(sub), 0);
    }
}
