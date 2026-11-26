//! XAudio2 (`XAudio2_9.dll`, ordinal 1 is `XAudio2Create`) for the native
//! macOS host: the COM objects a title calls, over the mixer, out through
//! CoreAudio.
//!
//! `IXAudio2` and its voices are objects whose vtables hold bridge stubs, so
//! title code calls them directly: every method is `extern "win64"` (floats
//! arrive in their Windows registers) behind a stub that swaps GS. The title's
//! `IXAudio2VoiceCallback` is called from a thread of ours, off the audio
//! thread, with the buffer notices the mixer reports.
//!
//! shortcut: PCM and float sources only (ADPCM, XMA, xWMA fail
//! `CreateSourceVoice`; XMA goes through `AcpHal`), no effects, no
//! `OnVoiceProcessingPass*` or engine callbacks, and the operation sets of
//! `CommitChanges` are applied at once.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use victory_loader::native;

use super::coreaudio::{self, Opener, Output};
use super::lock;
use super::mixer::{
    self, Buffer, Event, Format, MASTER, Mixer, Notice, VoiceId,
};

type HRESULT = i32;

const S_OK: HRESULT = 0;
const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
const XAUDIO2_E_INVALID_CALL: HRESULT = 0x8896_0001_u32 as i32;
const XAUDIO2_END_OF_STREAM: u32 = 0x40;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Source,
    Submix,
    Master,
}

/// What every voice and the engine's output share.
struct Inner {
    mixer: Mutex<Mixer>,
    running: std::sync::atomic::AtomicBool,
    events: Sender<Event>,
    output: Mutex<Option<Box<dyn Output>>>,
    opener: Opener,
}

#[repr(C)]
struct Engine {
    vtable: *const usize,
    refs: AtomicU32,
    inner: Arc<Inner>,
}

#[repr(C)]
struct Voice {
    vtable: *const usize,
    inner: Arc<Inner>,
    id: VoiceId,
    kind: Kind,
    /// Of a source voice.
    format: Format,
}

// Callbacks into the title.

#[cfg(not(test))]
fn deliver(e: Event) {
    // The callback object's vtable: OnVoiceProcessingPassStart, PassEnd,
    // OnStreamEnd, OnBufferStart, OnBufferEnd, OnLoopEnd, OnVoiceError.
    let slot = match e.notice {
        Notice::StreamEnd => 2,
        Notice::BufferStart => 3,
        Notice::BufferEnd => 4,
        Notice::LoopEnd => 5,
    };
    // SAFETY: a title's `IXAudio2VoiceCallback`, as it passed it to
    // `CreateSourceVoice`.
    unsafe {
        let table = (e.callback as *const *const usize).read();
        let method = table.add(slot).read();
        if e.notice == Notice::StreamEnd {
            native::call_win64(method, &[e.callback as u64]);
        } else {
            native::call_win64(method, &[e.callback as u64, e.context as u64]);
        }
    }
}

#[cfg(test)]
static DELIVERED: Mutex<Vec<Event>> = Mutex::new(Vec::new());

#[cfg(test)]
fn deliver(e: Event) {
    lock(&DELIVERED).push(e);
}

// Objects.

/// A table of methods as the title calls them: stubs, which swap GS and the
/// ABI, except in tests, which call the methods themselves.
fn table(methods: &[usize]) -> Vec<usize> {
    if cfg!(test) {
        return methods.to_vec();
    }
    let targets: Vec<(usize, bool)> = methods.iter().map(|&m| (m, true)).collect();
    native::host_callbacks(&targets).expect("allocating the XAudio2 method stubs")
}

macro_rules! methods {
    ($($f:path),* $(,)?) => { [$($f as *const () as usize),*] };
}

static ENGINE_TABLE: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        engine_query_interface,
        engine_add_ref,
        engine_release,
        engine_register,
        engine_unregister,
        create_source_voice,
        create_submix_voice,
        create_mastering_voice,
        start_engine,
        stop_engine,
        commit_changes,
        get_performance_data,
        set_debug_configuration,
    ])
});

/// The slots every voice has (`IXAudio2Voice`, 0 to 18).
macro_rules! voice_methods {
    ($($extra:path),* $(,)?) => {
        methods![
            get_voice_details,
            set_output_voices,
            set_effect_chain,
            enable_effect,
            disable_effect,
            get_effect_state,
            set_effect_parameters,
            get_effect_parameters,
            set_filter_parameters,
            get_filter_parameters,
            set_output_filter_parameters,
            get_output_filter_parameters,
            set_volume,
            get_volume,
            set_channel_volumes,
            get_channel_volumes,
            set_output_matrix,
            get_output_matrix,
            destroy_voice,
            $($extra),*
        ]
    };
}

static SOURCE_TABLE: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&voice_methods![
        source_start,
        source_stop,
        source_submit,
        source_flush,
        source_discontinuity,
        source_exit_loop,
        source_get_state,
        source_set_ratio,
        source_get_ratio,
        source_set_rate,
    ])
});

static SUBMIX_TABLE: LazyLock<Vec<usize>> = LazyLock::new(|| table(&voice_methods![]));

static MASTER_TABLE: LazyLock<Vec<usize>> =
    LazyLock::new(|| table(&voice_methods![master_channel_mask]));

fn new_engine(opener: Opener) -> *mut Engine {
    let (events, received) = channel::<Event>();
    // Ends with the last `Inner`, which holds the only sender.
    std::thread::Builder::new()
        .name("xaudio2-callbacks".into())
        .spawn(move || {
            for e in received {
                deliver(e);
            }
        })
        .expect("starting the XAudio2 callback thread");
    let inner = Arc::new(Inner {
        mixer: Mutex::new(Mixer::new(48_000, 2)),
        running: std::sync::atomic::AtomicBool::new(true),
        events,
        output: Mutex::new(None),
        opener,
    });
    Box::into_raw(Box::new(Engine {
        vtable: ENGINE_TABLE.as_ptr(),
        refs: AtomicU32::new(1),
        inner,
    }))
}

/// `XAudio2Create(IXAudio2**, flags, processor)`.
pub extern "system" fn XAudio2Create(out: *mut *mut Engine, _flags: u32, _processor: u32) -> HRESULT {
    create_with(out, coreaudio::open_default)
}

/// `XAudio2CreateWithVersionInfo(IXAudio2**, flags, processor, version)`.
pub extern "system" fn XAudio2CreateWithVersionInfo(
    out: *mut *mut Engine,
    _flags: u32,
    _processor: u32,
    _version: u32,
) -> HRESULT {
    create_with(out, coreaudio::open_default)
}

fn create_with(out: *mut *mut Engine, opener: Opener) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let engine = new_engine(opener);
    // SAFETY: the title's out parameter.
    unsafe { out.write(engine) };
    S_OK
}

// IUnknown and IXAudio2.

extern "win64" fn engine_query_interface(
    this: *mut Engine,
    _iid: *const c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // Every interface the title asks for (IUnknown and the IXAudio2s) is
    // this object.
    // SAFETY: the title's object and out parameter.
    unsafe {
        (*this).refs.fetch_add(1, Ordering::AcqRel);
        out.write(this.cast());
    }
    S_OK
}

extern "win64" fn engine_add_ref(this: *mut Engine) -> u32 {
    // SAFETY: the title's object.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

extern "win64" fn engine_release(this: *mut Engine) -> u32 {
    // SAFETY: the title's object.
    let left = unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel) } - 1;
    if left == 0 {
        // SAFETY: made by `new_engine`, released for the last time. The
        // output stops with it, before the mixer goes.
        unsafe {
            lock(&(&*this).inner.output).take();
            drop(Box::from_raw(this));
        }
    }
    left
}

extern "win64" fn engine_register(_this: *mut Engine, _callback: *mut c_void) -> HRESULT {
    S_OK
}

extern "win64" fn engine_unregister(_this: *mut Engine, _callback: *mut c_void) {}

extern "win64" fn start_engine(this: *mut Engine) -> HRESULT {
    // SAFETY: the title's object.
    unsafe { (&*this).inner.running.store(true, Ordering::Release) };
    S_OK
}

extern "win64" fn stop_engine(this: *mut Engine) {
    // SAFETY: the title's object.
    unsafe { (&*this).inner.running.store(false, Ordering::Release) };
}

extern "win64" fn commit_changes(_this: *mut Engine, _operation_set: u32) -> HRESULT {
    S_OK
}

extern "win64" fn get_performance_data(_this: *mut Engine, data: *mut u8) {
    if !data.is_null() {
        // SAFETY: an XAUDIO2_PERFORMANCE_DATA (72 bytes).
        unsafe { std::ptr::write_bytes(data, 0, 72) };
    }
}

extern "win64" fn set_debug_configuration(
    _this: *mut Engine,
    _configuration: *const c_void,
    _reserved: *mut c_void,
) {
}

/// `WAVEFORMATEX` (or its extensible form) as a mixer format; PCM and float
/// only.
fn parse_format(wave: *const u8) -> Option<Format> {
    if wave.is_null() {
        return None;
    }
    // SAFETY: the title passes a WAVEFORMATEX, which is 18 bytes at least;
    // the extensible form carries the sub-format at byte 24.
    unsafe {
        let tag = wave.cast::<u16>().read_unaligned();
        let channels = usize::from(wave.add(2).cast::<u16>().read_unaligned());
        let rate = wave.add(4).cast::<u32>().read_unaligned();
        let bits = wave.add(14).cast::<u16>().read_unaligned();
        let sub = if tag == 0xFFFE {
            wave.add(24).cast::<u16>().read_unaligned()
        } else {
            tag
        };
        let float = match sub {
            1 => false,
            3 => true,
            _ => return None,
        };
        (channels > 0 && rate > 0 && (float && bits == 32 || !float && matches!(bits, 8 | 16 | 24 | 32)))
            .then_some(Format { channels, rate, bits, float })
    }
}

/// Where a voice's first send goes: the mastering voice by default, nowhere
/// for an empty list.
fn first_send(inner: &Inner, sends: *const u8) -> Option<VoiceId> {
    let _ = inner;
    if sends.is_null() {
        return Some(MASTER);
    }
    // SAFETY: an XAUDIO2_VOICE_SENDS: a count, then a pointer to descriptors
    // of a flags word and a voice.
    unsafe {
        let count = sends.cast::<u32>().read_unaligned();
        if count == 0 {
            return None;
        }
        let descriptors = sends.add(8).cast::<*const u8>().read_unaligned();
        let voice = descriptors.add(8).cast::<*const Voice>().read_unaligned();
        (!voice.is_null()).then(|| (*voice).id)
    }
}

fn voice_object(inner: &Arc<Inner>, vtable: &'static [usize], id: VoiceId, kind: Kind, format: Format) -> *mut Voice {
    Box::into_raw(Box::new(Voice {
        vtable: vtable.as_ptr(),
        inner: inner.clone(),
        id,
        kind,
        format,
    }))
}

extern "win64" fn create_source_voice(
    this: *mut Engine,
    out: *mut *mut Voice,
    wave: *const u8,
    _flags: u32,
    _max_ratio: f32,
    callback: *mut c_void,
    sends: *const u8,
    _chain: *const c_void,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let Some(format) = parse_format(wave) else {
        return XAUDIO2_E_INVALID_CALL;
    };
    // SAFETY: the title's object.
    let inner = unsafe { &(*this).inner };
    let output = first_send(inner, sends);
    let id = lock(&inner.mixer).add_source(format.channels, format.rate, callback as usize, output);
    // SAFETY: the title's out parameter.
    unsafe { out.write(voice_object(inner, &SOURCE_TABLE, id, Kind::Source, format)) };
    S_OK
}

extern "win64" fn create_submix_voice(
    this: *mut Engine,
    out: *mut *mut Voice,
    channels: u32,
    rate: u32,
    _flags: u32,
    _stage: u32,
    sends: *const u8,
    _chain: *const c_void,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title's object.
    let inner = unsafe { &(*this).inner };
    let output = first_send(inner, sends);
    let id = lock(&inner.mixer).add_submix(output);
    let format = Format {
        channels: channels as usize,
        rate,
        bits: 32,
        float: true,
    };
    // SAFETY: the title's out parameter.
    unsafe { out.write(voice_object(inner, &SUBMIX_TABLE, id, Kind::Submix, format)) };
    S_OK
}

extern "win64" fn create_mastering_voice(
    this: *mut Engine,
    out: *mut *mut Voice,
    channels: u32,
    rate: u32,
    _flags: u32,
    _device: *const u16,
    _chain: *const c_void,
    _category: i32,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title's object.
    let inner = unsafe { &(*this).inner };
    let mut output = lock(&inner.output);
    if output.is_some() {
        return XAUDIO2_E_INVALID_CALL;
    }
    // 0 asks for the device's own; stereo at 48 kHz is every Mac's.
    let (channels, rate) = (if channels == 0 { 2 } else { channels }, if rate == 0 { 48_000 } else { rate });
    {
        let mut mixer = lock(&inner.mixer);
        mixer.channels = channels as usize;
        mixer.rate = rate;
    }
    let weak: Weak<Inner> = Arc::downgrade(inner);
    let render: coreaudio::Render = Box::new(move |out| {
        let Some(inner) = weak.upgrade() else {
            out.fill(0.0);
            return;
        };
        if !inner.running.load(Ordering::Acquire) {
            out.fill(0.0);
            return;
        }
        let events = lock(&inner.mixer).render(out);
        for e in events.into_iter().filter(|e| e.callback != 0) {
            let _ = inner.events.send(e);
        }
    });
    match (inner.opener)(rate, channels, render) {
        Ok(device) => *output = Some(device),
        Err(e) => {
            tracing::warn!(error = %e, "XAudio2: no audio output");
            return E_OUTOFMEMORY;
        }
    }
    drop(output);
    let format = Format {
        channels: channels as usize,
        rate,
        bits: 32,
        float: true,
    };
    // SAFETY: the title's out parameter.
    unsafe { out.write(voice_object(inner, &MASTER_TABLE, MASTER, Kind::Master, format)) };
    S_OK
}

// IXAudio2Voice.

/// What a voice is, by kind.
fn with_voice<R>(
    this: *mut Voice,
    master: impl FnOnce(&mut Mixer) -> R,
    submix: impl FnOnce(&mut mixer::Submix) -> R,
    source: impl FnOnce(&mut mixer::Source) -> R,
) -> Option<R> {
    // SAFETY: the title's voice object.
    let v = unsafe { &*this };
    let mut m = lock(&v.inner.mixer);
    match v.kind {
        Kind::Master => Some(master(&mut m)),
        Kind::Submix => m.submix(v.id).map(submix),
        Kind::Source => m.source(v.id).map(source),
    }
}

extern "win64" fn get_voice_details(this: *mut Voice, details: *mut u32) {
    // SAFETY: the title's voice and an XAUDIO2_VOICE_DETAILS: creation
    // flags, active flags, input channels, input sample rate.
    unsafe {
        let v = &*this;
        let (channels, rate) = if v.kind == Kind::Master {
            let m = lock(&v.inner.mixer);
            (m.channels as u32, m.rate)
        } else {
            (v.format.channels as u32, v.format.rate)
        };
        details.write(0);
        details.add(1).write(0);
        details.add(2).write(channels);
        details.add(3).write(rate);
    }
}

extern "win64" fn set_output_voices(this: *mut Voice, sends: *const u8) -> HRESULT {
    // SAFETY: the title's voice.
    let v = unsafe { &*this };
    let output = first_send(&v.inner, sends);
    let mut m = lock(&v.inner.mixer);
    match v.kind {
        Kind::Source => {
            if let Some(s) = m.source(v.id) {
                s.output = output;
            }
        }
        Kind::Submix => {
            if let Some(s) = m.submix(v.id) {
                s.output = output;
            }
        }
        Kind::Master => return XAUDIO2_E_INVALID_CALL,
    }
    S_OK
}

extern "win64" fn set_effect_chain(_this: *mut Voice, _chain: *const c_void) -> HRESULT {
    S_OK
}
extern "win64" fn enable_effect(_this: *mut Voice, _index: u32, _set: u32) -> HRESULT {
    S_OK
}
extern "win64" fn disable_effect(_this: *mut Voice, _index: u32, _set: u32) -> HRESULT {
    S_OK
}
extern "win64" fn get_effect_state(_this: *mut Voice, _index: u32, enabled: *mut i32) {
    if !enabled.is_null() {
        // SAFETY: the title's out parameter.
        unsafe { enabled.write(0) };
    }
}
extern "win64" fn set_effect_parameters(
    _this: *mut Voice,
    _index: u32,
    _parameters: *const c_void,
    _size: u32,
    _set: u32,
) -> HRESULT {
    S_OK
}
extern "win64" fn get_effect_parameters(
    _this: *mut Voice,
    _index: u32,
    _parameters: *mut c_void,
    _size: u32,
) -> HRESULT {
    S_OK
}
extern "win64" fn set_filter_parameters(_this: *mut Voice, _p: *const c_void, _set: u32) -> HRESULT {
    S_OK
}
extern "win64" fn get_filter_parameters(_this: *mut Voice, _p: *mut c_void) {}
extern "win64" fn set_output_filter_parameters(
    _this: *mut Voice,
    _to: *mut Voice,
    _p: *const c_void,
    _set: u32,
) -> HRESULT {
    S_OK
}
extern "win64" fn get_output_filter_parameters(_this: *mut Voice, _to: *mut Voice, _p: *mut c_void) {}

extern "win64" fn set_volume(this: *mut Voice, volume: f32, _set: u32) -> HRESULT {
    with_voice(
        this,
        |m| m.volume = volume,
        |s| s.volume = volume,
        |s| s.volume = volume,
    );
    S_OK
}

extern "win64" fn get_volume(this: *mut Voice, volume: *mut f32) {
    let v = with_voice(this, |m| m.volume, |s| s.volume, |s| s.volume).unwrap_or(1.0);
    // SAFETY: the title's out parameter.
    unsafe { volume.write(v) };
}

extern "win64" fn set_channel_volumes(
    this: *mut Voice,
    count: u32,
    volumes: *const f32,
    _set: u32,
) -> HRESULT {
    if volumes.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: `count` floats from the title.
    let list = unsafe { std::slice::from_raw_parts(volumes, count as usize) }.to_vec();
    with_voice(this, |_| (), |_| (), |s| {
        if list.len() == s.channels {
            s.channel_volumes = list;
        }
    });
    S_OK
}

extern "win64" fn get_channel_volumes(this: *mut Voice, count: u32, volumes: *mut f32) {
    let list = with_voice(this, |_| Vec::new(), |_| Vec::new(), |s| s.channel_volumes.clone())
        .unwrap_or_default();
    for (i, v) in list.iter().take(count as usize).enumerate() {
        // SAFETY: room for `count` floats.
        unsafe { volumes.add(i).write(*v) };
    }
}

extern "win64" fn set_output_matrix(
    this: *mut Voice,
    _to: *mut Voice,
    source_channels: u32,
    destination_channels: u32,
    levels: *const f32,
    _set: u32,
) -> HRESULT {
    if levels.is_null() {
        return E_INVALIDARG;
    }
    let n = (source_channels * destination_channels) as usize;
    // SAFETY: `source * destination` floats from the title.
    let levels = unsafe { std::slice::from_raw_parts(levels, n) }.to_vec();
    with_voice(this, |_| (), |_| (), |s| {
        if source_channels as usize == s.channels {
            s.matrix = Some(levels);
        }
    });
    S_OK
}

extern "win64" fn get_output_matrix(
    this: *mut Voice,
    _to: *mut Voice,
    source_channels: u32,
    destination_channels: u32,
    levels: *mut f32,
) {
    let matrix = with_voice(
        this,
        |_| None,
        |_| None,
        |s| s.matrix.clone().or_else(|| {
            Some(mixer::default_matrix(s.channels, destination_channels as usize))
        }),
    )
    .flatten()
    .unwrap_or_else(|| mixer::default_matrix(source_channels as usize, destination_channels as usize));
    for (i, l) in matrix
        .iter()
        .take((source_channels * destination_channels) as usize)
        .enumerate()
    {
        // SAFETY: room for `source * destination` floats.
        unsafe { levels.add(i).write(*l) };
    }
}

extern "win64" fn destroy_voice(this: *mut Voice) {
    // SAFETY: made by a `create_*_voice`, destroyed once.
    let v = unsafe { Box::from_raw(this) };
    match v.kind {
        Kind::Master => {
            // The output stops before anything else can pull from it.
            lock(&v.inner.output).take();
        }
        _ => lock(&v.inner.mixer).remove(v.id),
    }
}

// IXAudio2SourceVoice and IXAudio2MasteringVoice.

extern "win64" fn source_start(this: *mut Voice, _flags: u32, _set: u32) -> HRESULT {
    with_voice(this, |_| (), |_| (), |s| s.running = true);
    S_OK
}

extern "win64" fn source_stop(this: *mut Voice, _flags: u32, _set: u32) -> HRESULT {
    with_voice(this, |_| (), |_| (), |s| s.running = false);
    S_OK
}

extern "win64" fn source_submit(this: *mut Voice, buffer: *const u8, _wma: *const c_void) -> HRESULT {
    if buffer.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: an XAUDIO2_BUFFER from the title, and the voice it submits to.
    let (v, fields) = unsafe {
        let r = |o: usize| buffer.add(o).cast::<u32>().read_unaligned();
        (
            &*this,
            (
                r(0),
                r(4) as usize,
                buffer.add(8).cast::<*const u8>().read_unaligned(),
                r(16) as usize,
                r(20) as usize,
                r(24) as usize,
                r(28) as usize,
                r(32),
                buffer.add(40).cast::<usize>().read_unaligned(),
            ),
        )
    };
    let (flags, bytes, data, play_begin, play_length, loop_begin, loop_length, loops, context) = fields;
    let samples = if bytes == 0 {
        Vec::new()
    } else if data.is_null() {
        return E_INVALIDARG;
    } else {
        // SAFETY: `bytes` readable bytes of the title's audio.
        let pcm = unsafe { std::slice::from_raw_parts(data, bytes) };
        match mixer::decode(v.format, pcm) {
            Some(s) => s,
            None => return XAUDIO2_E_INVALID_CALL,
        }
    };
    let frames = samples.len() / v.format.channels;
    let end = if play_length == 0 { frames } else { play_begin + play_length };
    if play_begin > end || end > frames {
        return XAUDIO2_E_INVALID_CALL;
    }
    let (loop_begin, loop_end) = if loops == 0 {
        (0, 0)
    } else if loop_length == 0 {
        (play_begin, end)
    } else {
        (loop_begin, loop_begin + loop_length)
    };
    if loops > 0 && (loop_end > frames || loop_begin >= loop_end) {
        return XAUDIO2_E_INVALID_CALL;
    }
    let buffer = Buffer {
        samples,
        begin: play_begin,
        end,
        loop_begin,
        loop_end,
        loops,
        context,
        end_of_stream: flags & XAUDIO2_END_OF_STREAM != 0,
    };
    with_voice(this, |_| (), |_| (), |s| s.submit(buffer));
    S_OK
}

extern "win64" fn source_flush(this: *mut Voice) -> HRESULT {
    // SAFETY: the title's voice.
    let v = unsafe { &*this };
    let events = with_voice(this, |_| Vec::new(), |_| Vec::new(), |s| s.flush()).unwrap_or_default();
    for e in events.into_iter().filter(|e| e.callback != 0) {
        let _ = v.inner.events.send(e);
    }
    S_OK
}

extern "win64" fn source_discontinuity(_this: *mut Voice) -> HRESULT {
    S_OK
}

extern "win64" fn source_exit_loop(this: *mut Voice, _set: u32) -> HRESULT {
    with_voice(this, |_| (), |_| (), |s| s.exit_loop());
    S_OK
}

extern "win64" fn source_get_state(this: *mut Voice, state: *mut u8, _flags: u32) {
    let (context, queued, played) =
        with_voice(this, |_| (0, 0, 0), |_| (0, 0, 0), |s| s.state()).unwrap_or_default();
    // SAFETY: an XAUDIO2_VOICE_STATE: the context, the buffers queued, the
    // samples played.
    unsafe {
        state.cast::<usize>().write_unaligned(context);
        state.add(8).cast::<u32>().write_unaligned(queued);
        state.add(16).cast::<u64>().write_unaligned(played);
    }
}

extern "win64" fn source_set_ratio(this: *mut Voice, ratio: f32, _set: u32) -> HRESULT {
    with_voice(this, |_| (), |_| (), |s| s.ratio = ratio.clamp(1.0 / 1024.0, 1024.0));
    S_OK
}

extern "win64" fn source_get_ratio(this: *mut Voice, ratio: *mut f32) {
    let r = with_voice(this, |_| 1.0, |_| 1.0, |s| s.ratio).unwrap_or(1.0);
    // SAFETY: the title's out parameter.
    unsafe { ratio.write(r) };
}

extern "win64" fn source_set_rate(this: *mut Voice, rate: u32) -> HRESULT {
    if rate == 0 {
        return E_INVALIDARG;
    }
    with_voice(this, |_| (), |_| (), |s| s.rate = rate);
    S_OK
}

extern "win64" fn master_channel_mask(this: *mut Voice, mask: *mut u32) -> HRESULT {
    // SAFETY: the title's voice and out parameter.
    let channels = lock(&unsafe { &*this }.inner.mixer).channels;
    let m = match channels {
        1 => 0x4,
        2 => 0x3,
        4 => 0x33,
        6 => 0x3F,
        8 => 0x63F,
        _ => 0,
    };
    // SAFETY: as above.
    unsafe { mask.write(m) };
    S_OK
}

/// The function table of `xaudio2_9.dll`.
pub(super) fn lookup(dll: &str, name: &str) -> Option<usize> {
    if dll != "xaudio2_9.dll" {
        return None;
    }
    match name {
        "XAudio2Create" | "#1" => Some(XAudio2Create as *const () as usize),
        "XAudio2CreateWithVersionInfo" | "#7" => {
            Some(XAudio2CreateWithVersionInfo as *const () as usize)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// An output that is the test: it keeps the render function to pull
    /// frames from, as the device would.
    static PULL: Mutex<Option<coreaudio::Render>> = Mutex::new(None);

    struct Held;
    impl Output for Held {}
    impl Drop for Held {
        fn drop(&mut self) {
            lock(&PULL).take();
        }
    }

    fn open_held(_rate: u32, _channels: u32, render: coreaudio::Render) -> io::Result<Box<dyn Output>> {
        *lock(&PULL) = Some(render);
        Ok(Box::new(Held))
    }

    fn pull(frames: usize, channels: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * channels];
        (lock(&PULL).as_mut().expect("an open output"))(&mut out);
        out
    }

    /// The method at `slot` of an object's vtable.
    macro_rules! call {
        ($object:expr, $slot:expr, fn($($t:ty),*) -> $r:ty, $($arg:expr),*) => {{
            // SAFETY: a vtable of `extern "win64"` methods of this signature.
            let f: extern "win64" fn(*mut c_void $(, $t)*) -> $r = unsafe {
                std::mem::transmute(*(*$object.cast::<*const usize>()).add($slot))
            };
            f($object.cast() $(, $arg)*)
        }};
    }

    fn wave(channels: u16, rate: u32, bits: u16, tag: u16) -> Vec<u8> {
        let mut w = Vec::new();
        w.extend_from_slice(&tag.to_le_bytes());
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * u32::from(channels) * u32::from(bits / 8)).to_le_bytes());
        w.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(&0u16.to_le_bytes());
        w
    }

    #[test]
    fn a_title_plays_a_tone_through_the_engine() {
        let _turn = lock(&TURN);
        lock(&DELIVERED).clear();
        let mut engine: *mut Engine = std::ptr::null_mut();
        assert_eq!(create_with(&mut engine, open_held), S_OK);
        let engine = engine.cast::<c_void>();

        // CreateMasteringVoice(&master, 2, 48000, ...), slot 7.
        let mut master: *mut Voice = std::ptr::null_mut();
        assert_eq!(
            call!(engine, 7, fn(*mut *mut Voice, u32, u32, u32, *const u16, *const c_void, i32) -> HRESULT,
                &mut master, 2, 48_000, 0, std::ptr::null(), std::ptr::null(), 0),
            S_OK
        );
        // A second one is refused.
        let mut again: *mut Voice = std::ptr::null_mut();
        assert_eq!(
            call!(engine, 7, fn(*mut *mut Voice, u32, u32, u32, *const u16, *const c_void, i32) -> HRESULT,
                &mut again, 2, 48_000, 0, std::ptr::null(), std::ptr::null(), 0),
            XAUDIO2_E_INVALID_CALL
        );
        let mut mask = 0u32;
        assert_eq!(call!(master.cast::<c_void>(), 19, fn(*mut u32) -> HRESULT, &mut mask), S_OK);
        assert_eq!(mask, 3);

        // CreateSourceVoice: mono, 16-bit, 24 kHz, with a callback object.
        let w = wave(1, 24_000, 16, 1);
        let callback = 0x5000usize as *mut c_void;
        let mut voice: *mut Voice = std::ptr::null_mut();
        assert_eq!(
            call!(engine, 5, fn(*mut *mut Voice, *const u8, u32, f32, *mut c_void, *const u8, *const c_void) -> HRESULT,
                &mut voice, w.as_ptr(), 0, 2.0, callback, std::ptr::null(), std::ptr::null()),
            S_OK
        );
        // ADPCM is not ours.
        let adpcm = wave(1, 24_000, 4, 2);
        let mut none: *mut Voice = std::ptr::null_mut();
        assert_eq!(
            call!(engine, 5, fn(*mut *mut Voice, *const u8, u32, f32, *mut c_void, *const u8, *const c_void) -> HRESULT,
                &mut none, adpcm.as_ptr(), 0, 2.0, callback, std::ptr::null(), std::ptr::null()),
            XAUDIO2_E_INVALID_CALL
        );

        // 50 ms of 1 kHz at 24 kHz: 1200 frames.
        let pcm: Vec<u8> = (0..1200)
            .flat_map(|i| {
                let x = (i as f32 * 1000.0 * std::f32::consts::TAU / 24_000.0).sin();
                ((x * 16000.0) as i16).to_le_bytes()
            })
            .collect();
        let mut buffer = [0u8; 48];
        buffer[0..4].copy_from_slice(&XAUDIO2_END_OF_STREAM.to_le_bytes());
        buffer[4..8].copy_from_slice(&(pcm.len() as u32).to_le_bytes());
        buffer[8..16].copy_from_slice(&(pcm.as_ptr() as usize).to_le_bytes());
        buffer[40..48].copy_from_slice(&0xC0DEusize.to_le_bytes());
        let voice_ptr = voice.cast::<c_void>();
        assert_eq!(call!(voice_ptr, 21, fn(*const u8, *const c_void) -> HRESULT, buffer.as_ptr(), std::ptr::null()), S_OK);
        // Not started: silent, one buffer queued.
        let mut state = [0u8; 24];
        call!(voice_ptr, 25, fn(*mut u8, u32) -> (), state.as_mut_ptr(), 0);
        assert_eq!(u32::from_le_bytes(state[8..12].try_into().unwrap()), 1);
        assert!(pull(480, 2).iter().all(|&x| x == 0.0));
        // Start, half volume, and the device pulls the 50 ms (2400 output frames).
        assert_eq!(call!(voice_ptr, 19, fn(u32, u32) -> HRESULT, 0, 0), S_OK);
        assert_eq!(call!(voice_ptr, 12, fn(f32, u32) -> HRESULT, 0.5, 0), S_OK);
        let mut v = 0.0f32;
        call!(voice_ptr, 13, fn(*mut f32) -> (), &mut v);
        assert_eq!(v, 0.5);
        let mut heard = Vec::new();
        for _ in 0..6 {
            heard.extend(pull(480, 2));
        }
        let left: Vec<f32> = heard.iter().step_by(2).copied().collect();
        let peak = left.iter().fold(0.0f32, |a, x| a.max(x.abs()));
        let expected = 16000.0 / 32768.0 * 0.5;
        assert!((peak - expected).abs() < 0.01, "peak {peak} vs {expected}");
        let periods = left.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        assert_eq!(periods, 50, "a 1 kHz tone of 50 ms");
        // The tail after the buffer is silence, and the title was told.
        assert!(left[2400..].iter().all(|&x| x == 0.0));
        // The notices go out through the channel to the callback thread.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let got: Vec<_> = lock(&DELIVERED)
            .iter()
            .map(|e| (e.notice, e.context, e.callback))
            .collect();
        assert_eq!(
            got,
            [
                (Notice::BufferStart, 0xC0DE, 0x5000),
                (Notice::BufferEnd, 0xC0DE, 0x5000),
                (Notice::StreamEnd, 0, 0x5000)
            ]
        );
        call!(voice_ptr, 25, fn(*mut u8, u32) -> (), state.as_mut_ptr(), 0);
        assert_eq!(u32::from_le_bytes(state[8..12].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(state[16..24].try_into().unwrap()), 1200);

        // Voices and the engine go; the output stops with the master.
        call!(voice_ptr, 18, fn() -> (),);
        call!(master.cast::<c_void>(), 18, fn() -> (),);
        assert!(lock(&PULL).is_none());
        assert_eq!(call!(engine, 2, fn() -> u32,), 0);
    }

    /// The engine tests share the delivered-notice log and the held output.
    static TURN: Mutex<()> = Mutex::new(());

    #[test]
    fn the_engine_is_reference_counted_and_lookups_resolve() {
        let _turn = lock(&TURN);
        let mut engine: *mut Engine = std::ptr::null_mut();
        assert_eq!(create_with(&mut engine, open_held), S_OK);
        let e = engine.cast::<c_void>();
        assert_eq!(call!(e, 1, fn() -> u32,), 2);
        let mut same: *mut c_void = std::ptr::null_mut();
        assert_eq!(call!(e, 0, fn(*const c_void, *mut *mut c_void) -> HRESULT, std::ptr::null(), &mut same), S_OK);
        assert_eq!(same, e);
        assert_eq!(call!(e, 2, fn() -> u32,), 2);
        assert_eq!(call!(e, 2, fn() -> u32,), 1);
        assert_eq!(call!(e, 2, fn() -> u32,), 0);
        assert_eq!(lookup("xaudio2_9.dll", "#1"), lookup("xaudio2_9.dll", "XAudio2Create"));
        assert!(lookup("xaudio2_9.dll", "#1").is_some());
        assert!(lookup("xaudio2_8.dll", "#1").is_none());
        assert_eq!(XAudio2Create(std::ptr::null_mut(), 0, 0), E_INVALIDARG);
    }
}
