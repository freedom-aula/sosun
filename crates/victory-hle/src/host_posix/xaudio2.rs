//! XAudio2 (`XAudio2_9.dll`, ordinal 1 is `XAudio2Create`) for the native
//! macOS host: the COM objects a title calls, over the mixer, out through
//! CoreAudio.
//!
//! `IXAudio2` and its voices are objects whose vtables hold bridge stubs, so
//! title code calls them directly: every method is `extern "win64"` (floats
//! arrive in their Windows registers) behind a stub that swaps GS. Pass
//! callbacks run on the audio thread. Buffer notices go to a separate worker.
//!
//! shortcut: PCM and float sources only, add other formats when a title needs them through XAudio2.
//! shortcut: operation sets apply at once, defer them when a title needs atomic CommitChanges.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, LazyLock, Mutex};

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
const XAUDIO2_E_DEVICE_INVALIDATED: HRESULT = 0x8896_0004_u32 as i32;
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
    callbacks: Mutex<Callbacks>,
    pass_done: Condvar,
    events: Sender<Event>,
    output: Mutex<OutputState>,
    opener: Opener,
}

enum OutputState {
    Closed,
    Busy,
    Open(Box<dyn Output>),
}

struct Callbacks {
    engines: Vec<Arc<Callback>>,
    voices: BTreeMap<VoiceId, (Arc<Callback>, usize)>,
    running: bool,
    in_pass: bool,
    device_invalidated: bool,
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

/// A snapshot keeps this gate alive. Cancellation prevents new calls and
/// waits for any caller already using the title's callback object.
struct Callback {
    ptr: usize,
    state: Mutex<CallbackState>,
    idle: Condvar,
}

#[derive(Default)]
struct CallbackState {
    cancelled: bool,
    in_flight: usize,
}

impl Callback {
    fn new(ptr: usize) -> Arc<Self> {
        Arc::new(Self {
            ptr,
            state: Mutex::default(),
            idle: Condvar::new(),
        })
    }

    fn cancel(&self) {
        let mut state = lock(&self.state);
        state.cancelled = true;
        drop(self.idle.wait_while(state, |state| state.in_flight != 0));
    }

    fn call(&self, slot: usize, arg: Option<usize>) {
        {
            let mut state = lock(&self.state);
            if state.cancelled {
                return;
            }
            state.in_flight += 1;
        }
        // SAFETY: the title's callback object is valid until it is unregistered
        // or its voice is destroyed. Both wait for active calls before returning.
        unsafe {
            let table = (self.ptr as *const *const usize).read();
            let method = table.add(slot).read();
            #[cfg(not(test))]
            match arg {
                Some(arg) => native::call_win64(method, &[self.ptr as u64, arg as u64]),
                None => native::call_win64(method, &[self.ptr as u64]),
            };
            #[cfg(test)]
            match arg {
                Some(arg) => std::mem::transmute::<usize, extern "win64" fn(usize, usize)>(method)(
                    self.ptr, arg,
                ),
                None => std::mem::transmute::<usize, extern "win64" fn(usize)>(method)(self.ptr),
            }
        }
        let mut state = lock(&self.state);
        state.in_flight -= 1;
        if state.in_flight == 0 {
            self.idle.notify_all();
        }
    }
}

fn deliver(callback: &Callback, e: Event) {
    // shortcut: PCM and float decoding errors return from SubmitSourceBuffer, dispatch OnVoiceError when a decoder can fail during mixing.
    // The callback object's vtable: OnVoiceProcessingPassStart, PassEnd,
    // OnStreamEnd, OnBufferStart, OnBufferEnd, OnLoopEnd, OnVoiceError.
    let slot = match e.notice {
        Notice::StreamEnd => 2,
        Notice::BufferStart => 3,
        Notice::BufferEnd => 4,
        Notice::LoopEnd => 5,
    };
    callback.call(slot, (e.notice != Notice::StreamEnd).then_some(e.context));
}

impl Inner {
    fn close_output(&self) {
        let output = {
            let mut output = lock(&self.output);
            if !matches!(*output, OutputState::Open(_)) {
                return;
            }
            std::mem::replace(&mut *output, OutputState::Busy)
        };
        if let OutputState::Open(output) = output {
            drop(output);
        }
        *lock(&self.output) = OutputState::Closed;
    }

    fn render(&self, out: &mut [f32]) {
        let engines = {
            let mut callbacks = lock(&self.callbacks);
            if !callbacks.running {
                out.fill(0.0);
                return;
            }
            callbacks.in_pass = true;
            callbacks.engines.clone()
        };
        for callback in &engines {
            callback.call(0, None); // OnProcessingPassStart
        }

        let voices = lock(&self.callbacks).voices.clone();
        for (&id, (callback, frame_bytes)) in &voices {
            let bytes = {
                let mut mixer = lock(&self.mixer);
                let (frames, rate) = (out.len() / mixer.channels, mixer.rate);
                mixer.source(id).map_or(0, |source| {
                    source
                        .frames_required(frames, rate)
                        .saturating_mul(*frame_bytes)
                        .min(u32::MAX as usize)
                })
            };
            callback.call(0, Some(bytes)); // OnVoiceProcessingPassStart
        }

        let events = lock(&self.mixer).render(out);
        for e in events {
            let _ = self.events.send(e);
        }
        for (callback, _) in voices.values() {
            callback.call(1, None); // OnVoiceProcessingPassEnd
        }
        for callback in &engines {
            callback.call(1, None); // OnProcessingPassEnd
        }
        lock(&self.callbacks).in_pass = false;
        self.pass_done.notify_all();
    }

    fn output_failed(&self, error: std::io::Error) {
        tracing::warn!(%error, "XAudio2: audio output failed");
        let engines = {
            let mut callbacks = lock(&self.callbacks);
            if callbacks.device_invalidated {
                return;
            }
            callbacks.running = false;
            callbacks.device_invalidated = true;
            callbacks.engines.clone()
        };
        // shortcut: only StartEngine reports device invalidation after OnCriticalError, gate other HRESULT methods if a title uses the failed engine.
        for callback in &engines {
            callback.call(2, Some(XAUDIO2_E_DEVICE_INVALIDATED as u32 as usize)); // OnCriticalError
        }
    }
}

// Objects.

/// A table of methods as the title calls them: stubs, which swap GS and the
/// ABI, except in tests, which call the methods themselves.
pub(super) fn table(methods: &[usize]) -> Vec<usize> {
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
    let inner = Arc::new(Inner {
        mixer: Mutex::new(Mixer::new(48_000, 2)),
        callbacks: Mutex::new(Callbacks {
            engines: Vec::new(),
            voices: BTreeMap::new(),
            running: true,
            in_pass: false,
            device_invalidated: false,
        }),
        pass_done: Condvar::new(),
        events,
        output: Mutex::new(OutputState::Closed),
        opener,
    });
    let weak = Arc::downgrade(&inner);
    // Ends with the last `Inner`, which holds the only sender.
    std::thread::Builder::new()
        .name("xaudio2-callbacks".into())
        .spawn(move || {
            for e in received {
                let Some(inner) = weak.upgrade() else { break };
                let callback = lock(&inner.callbacks)
                    .voices
                    .get(&e.voice)
                    .map(|(c, _)| c.clone());
                if let Some(callback) = callback {
                    deliver(&callback, e);
                }
            }
        })
        .expect("starting the XAudio2 callback thread");
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
    tracing::info!("XAudio2: engine created");
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
            (&*this).inner.close_output();
            drop(Box::from_raw(this));
        }
    }
    left
}

extern "win64" fn engine_register(this: *mut Engine, callback: *mut c_void) -> HRESULT {
    if callback.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title's engine.
    let mut callbacks = lock(&unsafe { &*this }.inner.callbacks);
    let ptr = callback as usize;
    if !callbacks.engines.iter().any(|c| c.ptr == ptr) {
        callbacks.engines.push(Callback::new(ptr));
    }
    S_OK
}

extern "win64" fn engine_unregister(this: *mut Engine, callback: *mut c_void) {
    let removed = {
        // SAFETY: the title's engine.
        let mut callbacks = lock(&unsafe { &*this }.inner.callbacks);
        callbacks
            .engines
            .iter()
            .position(|c| c.ptr == callback as usize)
            .map(|i| callbacks.engines.remove(i))
    };
    if let Some(removed) = removed {
        removed.cancel();
    }
}

extern "win64" fn start_engine(this: *mut Engine) -> HRESULT {
    // SAFETY: the title's object.
    let mut callbacks = lock(&unsafe { &*this }.inner.callbacks);
    if callbacks.device_invalidated {
        return XAUDIO2_E_DEVICE_INVALIDATED;
    }
    callbacks.running = true;
    S_OK
}

extern "win64" fn stop_engine(this: *mut Engine) {
    // SAFETY: the title's object.
    let inner = &unsafe { &*this }.inner;
    let mut callbacks = lock(&inner.callbacks);
    callbacks.running = false;
    drop(
        inner
            .pass_done
            .wait_while(callbacks, |callbacks| callbacks.in_pass),
    );
}

extern "win64" fn commit_changes(_this: *mut Engine, _operation_set: u32) -> HRESULT {
    S_OK
}

extern "win64" fn get_performance_data(_this: *mut Engine, data: *mut u8) {
    if !data.is_null() {
        // SAFETY: XAUDIO2_PERFORMANCE_DATA contains two UINT64s and twelve UINT32s.
        unsafe { std::ptr::write_bytes(data, 0, 64) };
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
pub(super) fn parse_format(wave: *const u8) -> Option<Format> {
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
        let descriptors = sends.add(4).cast::<*const u8>().read_unaligned();
        if descriptors.is_null() {
            return None;
        }
        let voice = descriptors.add(4).cast::<*const Voice>().read_unaligned();
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
    let id = lock(&inner.mixer).add_source(format.channels, format.rate, output);
    // SAFETY: the title's out parameter.
    unsafe { out.write(voice_object(inner, &SOURCE_TABLE, id, Kind::Source, format)) };
    if !callback.is_null() {
        lock(&inner.callbacks)
            .voices
            .insert(id, (Callback::new(callback as usize), format.frame_bytes()));
    }
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
    {
        let mut output = lock(&inner.output);
        if !matches!(*output, OutputState::Closed) {
            return XAUDIO2_E_INVALID_CALL;
        }
        *output = OutputState::Busy;
    }
    // 0 asks for the device's own; stereo at 48 kHz is every Mac's.
    let (channels, rate) = (if channels == 0 { 2 } else { channels }, if rate == 0 { 48_000 } else { rate });
    {
        let mut mixer = lock(&inner.mixer);
        mixer.channels = channels as usize;
        mixer.rate = rate;
    }
    let weak = Arc::downgrade(inner);
    let render: coreaudio::Render = Box::new(move |pull| match (weak.upgrade(), pull) {
        (Some(inner), Ok(out)) => inner.render(out),
        (Some(inner), Err(error)) => inner.output_failed(error),
        (None, Ok(out)) => out.fill(0.0),
        (None, Err(_)) => (),
    });
    match (inner.opener)(rate, channels, render) {
        Ok(device) => *lock(&inner.output) = OutputState::Open(device),
        Err(e) => {
            *lock(&inner.output) = OutputState::Closed;
            tracing::warn!(error = %e, "XAudio2: no audio output");
            return E_OUTOFMEMORY;
        }
    }
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
        details.cast::<[u32; 4]>().write_unaligned([0, 0, channels, rate]);
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
    let v = unsafe { &*this };
    match v.kind {
        Kind::Master => {
            // The output stops before anything else can pull from it.
            v.inner.close_output();
        }
        _ => {
            let removed = lock(&v.inner.callbacks).voices.remove(&v.id);
            if let Some((callback, _)) = removed {
                callback.cancel();
            }
            lock(&v.inner.mixer).remove(v.id);
        }
    }
    // SAFETY: no callback can use the voice after cancellation.
    unsafe { drop(Box::from_raw(this)) };
}

// IXAudio2SourceVoice and IXAudio2MasteringVoice.

extern "win64" fn source_start(this: *mut Voice, _flags: u32, _set: u32) -> HRESULT {
    tracing::info!("XAudio2: source voice started");
    with_voice(this, |_| (), |_| (), |s| s.running = true);
    S_OK
}

extern "win64" fn source_stop(this: *mut Voice, _flags: u32, _set: u32) -> HRESULT {
    with_voice(this, |_| (), |_| (), |s| s.running = false);
    S_OK
}

extern "win64" fn source_submit(this: *mut Voice, buffer: *const u8, _wma: *const c_void) -> HRESULT {
    static SUBMITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SUBMITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        tracing::info!(n, "XAudio2: buffers submitted");
    }
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
                buffer.add(36).cast::<usize>().read_unaligned(),
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
    if loops > 0 && (loop_end > end || loop_end <= play_begin || loop_begin >= loop_end) {
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
    for e in events {
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
        state.add(12).cast::<u64>().write_unaligned(played);
    }
}

extern "win64" fn source_set_ratio(this: *mut Voice, ratio: f32, _set: u32) -> HRESULT {
    if !ratio.is_finite() || ratio <= 0.0 {
        return E_INVALIDARG;
    }
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
    use std::sync::atomic::AtomicPtr;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

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
        let mut render = lock(&PULL).take().expect("an open output");
        render(Ok(&mut out));
        *lock(&PULL) = Some(render);
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

    #[derive(Debug, PartialEq, Eq)]
    enum Observed {
        EngineStart(usize),
        EngineEnd(usize),
        CriticalError(usize, usize),
        VoiceStart(usize, usize),
        VoiceEnd(usize),
        Notice(Notice, usize),
    }

    #[repr(C)]
    struct TestCallback<'a> {
        table: *const usize,
        log: &'a Mutex<Vec<Observed>>,
        id: usize,
        voice: AtomicPtr<Voice>,
    }

    impl<'a> TestCallback<'a> {
        fn new(table: &'a [usize], log: &'a Mutex<Vec<Observed>>, id: usize) -> Self {
            Self {
                table: table.as_ptr(),
                log,
                id,
                voice: AtomicPtr::new(std::ptr::null_mut()),
            }
        }

        fn ptr(&self) -> *mut c_void {
            (self as *const Self).cast_mut().cast()
        }

        fn from_ptr(this: usize) -> &'a Self {
            // SAFETY: the test keeps the object alive while registered.
            unsafe { &*(this as *const Self) }
        }
    }

    extern "win64" fn engine_pass_start(this: usize) {
        let cb = TestCallback::from_ptr(this);
        lock(cb.log).push(Observed::EngineStart(cb.id));
    }

    extern "win64" fn engine_pass_end(this: usize) {
        let cb = TestCallback::from_ptr(this);
        lock(cb.log).push(Observed::EngineEnd(cb.id));
    }

    extern "win64" fn critical_error(this: usize, error: usize) {
        let cb = TestCallback::from_ptr(this);
        lock(cb.log).push(Observed::CriticalError(cb.id, error));
    }

    extern "win64" fn voice_pass_start(this: usize, bytes: usize) {
        let cb = TestCallback::from_ptr(this);
        lock(cb.log).push(Observed::VoiceStart(cb.id, bytes));
        let voice = cb.voice.load(Ordering::Relaxed);
        if !voice.is_null() {
            assert_eq!(call!(voice, 12, fn(f32, u32) -> HRESULT, 0.25, 0), S_OK);
        }
    }

    extern "win64" fn voice_pass_end(this: usize) {
        let cb = TestCallback::from_ptr(this);
        lock(cb.log).push(Observed::VoiceEnd(cb.id));
    }

    extern "win64" fn stream_end(this: usize) {
        lock(TestCallback::from_ptr(this).log).push(Observed::Notice(Notice::StreamEnd, 0));
    }

    extern "win64" fn buffer_start(this: usize, context: usize) {
        lock(TestCallback::from_ptr(this).log).push(Observed::Notice(Notice::BufferStart, context));
    }

    extern "win64" fn buffer_end(this: usize, context: usize) {
        lock(TestCallback::from_ptr(this).log).push(Observed::Notice(Notice::BufferEnd, context));
    }

    fn voice_callback_table() -> [usize; 7] {
        let mut table = [0; 7];
        table[..5].copy_from_slice(&methods![
            voice_pass_start,
            voice_pass_end,
            stream_end,
            buffer_start,
            buffer_end
        ]);
        table
    }

    #[test]
    fn a_title_plays_a_tone_through_the_engine() {
        let _turn = lock(&TURN);
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

        // Route the source through an explicit packed send to the mastering voice.
        let mut descriptor = [0u8; 12];
        descriptor[4..12].copy_from_slice(&(master as usize).to_le_bytes());
        let mut sends = [0u8; 12];
        sends[..4].copy_from_slice(&1u32.to_le_bytes());
        sends[4..12].copy_from_slice(&(descriptor.as_ptr() as usize).to_le_bytes());

        // CreateSourceVoice: mono, 16-bit, 24 kHz, with a callback object.
        let w = wave(1, 24_000, 16, 1);
        let log = Mutex::new(Vec::new());
        let table = voice_callback_table();
        let callback = TestCallback::new(&table, &log, 0);
        let mut voice: *mut Voice = std::ptr::null_mut();
        assert_eq!(
            call!(engine, 5, fn(*mut *mut Voice, *const u8, u32, f32, *mut c_void, *const u8, *const c_void) -> HRESULT,
                &mut voice, w.as_ptr(), 0, 2.0, callback.ptr(), sends.as_ptr(), std::ptr::null()),
            S_OK
        );
        // A packed XAUDIO2_VOICE_DETAILS can start on an odd address.
        #[repr(align(4))]
        struct Details([u8; 17]);
        let mut details = Details([0xA5; 17]);
        call!(voice, 0, fn(*mut u32) -> (), details.0[1..].as_mut_ptr().cast());
        assert_eq!(details.0[0], 0xA5);
        assert_eq!(&details.0[1..9], &[0; 8]);
        assert_eq!(&details.0[9..13], &1u32.to_le_bytes());
        assert_eq!(&details.0[13..], &24_000u32.to_le_bytes());
        // ADPCM is not ours.
        let adpcm = wave(1, 24_000, 4, 2);
        let mut none: *mut Voice = std::ptr::null_mut();
        assert_eq!(
            call!(engine, 5, fn(*mut *mut Voice, *const u8, u32, f32, *mut c_void, *const u8, *const c_void) -> HRESULT,
                &mut none, adpcm.as_ptr(), 0, 2.0, callback.ptr(), std::ptr::null(), std::ptr::null()),
            XAUDIO2_E_INVALID_CALL
        );

        // 50 ms of 1 kHz at 24 kHz: 1200 frames.
        let pcm: Vec<u8> = (0..1200)
            .flat_map(|i| {
                let x = (i as f32 * 1000.0 * std::f32::consts::TAU / 24_000.0).sin();
                ((x * 16000.0) as i16).to_le_bytes()
            })
            .collect();
        const CONTEXT: usize = 0x0000_1234_5678_C0DE;
        let mut buffer = [0u8; 44];
        buffer[0..4].copy_from_slice(&XAUDIO2_END_OF_STREAM.to_le_bytes());
        buffer[4..8].copy_from_slice(&(pcm.len() as u32).to_le_bytes());
        buffer[8..16].copy_from_slice(&(pcm.as_ptr() as usize).to_le_bytes());
        buffer[36..44].copy_from_slice(&CONTEXT.to_le_bytes());
        let voice_ptr = voice.cast::<c_void>();
        assert_eq!(call!(voice_ptr, 21, fn(*const u8, *const c_void) -> HRESULT, buffer.as_ptr(), std::ptr::null()), S_OK);
        // Not started: silent, one buffer queued.
        let mut state = [0xA5u8; 20 + 4];
        call!(voice_ptr, 25, fn(*mut u8, u32) -> (), state.as_mut_ptr(), 0);
        assert_eq!(usize::from_le_bytes(state[..8].try_into().unwrap()), CONTEXT);
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
        let notices = lock(&log);
        let got: Vec<_> = notices
            .iter()
            .filter(|e| matches!(e, Observed::Notice(..)))
            .collect();
        assert_eq!(
            got,
            [
                &Observed::Notice(Notice::BufferStart, CONTEXT),
                &Observed::Notice(Notice::BufferEnd, CONTEXT),
                &Observed::Notice(Notice::StreamEnd, 0)
            ]
        );
        drop(notices);
        call!(voice_ptr, 25, fn(*mut u8, u32) -> (), state.as_mut_ptr(), 0);
        assert_eq!(u32::from_le_bytes(state[8..12].try_into().unwrap()), 0);
        assert_eq!(&state[20..], &[0xA5; 4]);
        assert_eq!(u64::from_le_bytes(state[12..20].try_into().unwrap()), 1200);

        // Voices and the engine go; the output stops with the master.
        call!(voice_ptr, 18, fn() -> (),);
        call!(master.cast::<c_void>(), 18, fn() -> (),);
        assert!(lock(&PULL).is_none());
        assert_eq!(call!(engine, 2, fn() -> u32,), 0);
    }

    #[test]
    fn processing_passes_bracket_mixing_and_respect_engine_state() {
        let _turn = lock(&TURN);
        let engine = new_engine(open_held);
        let log = Mutex::new(Vec::new());
        let engine_table = methods![engine_pass_start, engine_pass_end, critical_error];
        let a = TestCallback::new(&engine_table, &log, 1);
        let b = TestCallback::new(&engine_table, &log, 2);
        let unknown = TestCallback::new(&engine_table, &log, 3);
        for cb in [&a, &b, &a] {
            assert_eq!(call!(engine, 3, fn(*mut c_void) -> HRESULT, cb.ptr()), S_OK);
        }
        call!(engine, 4, fn(*mut c_void) -> (), unknown.ptr());

        let mut master = std::ptr::null_mut();
        assert_eq!(
            create_mastering_voice(
                engine,
                &mut master,
                2,
                48_000,
                0,
                std::ptr::null(),
                std::ptr::null(),
                0
            ),
            S_OK
        );
        let voice_table = voice_callback_table();
        let empty = TestCallback::new(&voice_table, &log, 3);
        let fed = TestCallback::new(&voice_table, &log, 4);
        let wave = wave(1, 24_000, 16, 1);
        let source = |cb: &TestCallback<'_>| {
            let mut voice = std::ptr::null_mut();
            assert_eq!(
                create_source_voice(
                    engine,
                    &mut voice,
                    wave.as_ptr(),
                    0,
                    2.0,
                    cb.ptr(),
                    std::ptr::null(),
                    std::ptr::null()
                ),
                S_OK
            );
            assert_eq!(source_start(voice, 0, 0), S_OK);
            voice
        };
        let starved = source(&empty);
        let fed_voice = source(&fed);
        fed.voice.store(fed_voice, Ordering::Relaxed);
        let pcm = [16_384i16; 480]; // Two output passes at a 2:1 resampling ratio.
        let mut buffer = [0; 44];
        buffer[4..8].copy_from_slice(&(size_of_val(&pcm) as u32).to_le_bytes());
        buffer[8..16].copy_from_slice(&(pcm.as_ptr() as usize).to_le_bytes());
        assert_eq!(
            source_submit(fed_voice, buffer.as_ptr(), std::ptr::null()),
            S_OK
        );
        let take_log = || {
            let mut events = std::mem::take(&mut *lock(&log));
            events.retain(|e| !matches!(e, Observed::Notice(..)));
            events
        };

        assert!(pull(480, 2).iter().all(|&sample| sample == 0.125));
        use Observed::*;
        assert_eq!(
            take_log(),
            [
                EngineStart(1),
                EngineStart(2),
                VoiceStart(3, 480),
                VoiceStart(4, 0),
                VoiceEnd(3),
                VoiceEnd(4),
                EngineEnd(1),
                EngineEnd(2)
            ]
        );

        call!(engine, 9, fn() -> (),);
        assert!(pull(480, 2).iter().all(|&sample| sample == 0.0));
        assert!(take_log().is_empty());
        call!(engine, 4, fn(*mut c_void) -> (), a.ptr());
        call!(engine, 4, fn(*mut c_void) -> (), a.ptr());
        assert_eq!(call!(engine, 8, fn() -> HRESULT,), S_OK);
        assert!(pull(480, 2).iter().all(|&sample| sample == 0.125));
        assert_eq!(
            take_log(),
            [
                EngineStart(2),
                VoiceStart(3, 480),
                VoiceStart(4, 0),
                VoiceEnd(3),
                VoiceEnd(4),
                EngineEnd(2)
            ]
        );

        let mut render = lock(&PULL).take().unwrap();
        render(Err(io::Error::other("simulated output failure")));
        *lock(&PULL) = Some(render);
        assert_eq!(
            take_log(),
            [CriticalError(
                2,
                XAUDIO2_E_DEVICE_INVALIDATED as u32 as usize
            )]
        );
        assert_eq!(start_engine(engine), XAUDIO2_E_DEVICE_INVALIDATED);
        assert!(pull(480, 2).iter().all(|&sample| sample == 0.0));
        assert!(take_log().is_empty());

        destroy_voice(starved);
        destroy_voice(fed_voice);
        destroy_voice(master);
        engine_unregister(engine, b.ptr());
        assert_eq!(engine_release(engine), 0);
    }

    #[test]
    fn destroying_a_voice_waits_for_its_buffer_callback() {
        static BLOCK: Mutex<Option<(Sender<()>, Receiver<()>)>> = Mutex::new(None);

        extern "win64" fn blocked_buffer_end(_this: usize, _context: usize) {
            let (entered, resume) = lock(&BLOCK).take().unwrap();
            entered.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(5)).unwrap();
        }

        let engine = new_engine(open_held);
        let table = [0, 0, 0, 0, blocked_buffer_end as *const () as usize, 0, 0];
        let callback = table.as_ptr();
        let wave = wave(1, 48_000, 16, 1);
        let mut voice = std::ptr::null_mut();
        assert_eq!(
            create_source_voice(
                engine,
                &mut voice,
                wave.as_ptr(),
                0,
                2.0,
                (&raw const callback).cast_mut().cast(),
                std::ptr::null(),
                std::ptr::null()
            ),
            S_OK
        );
        let (entered, began) = channel();
        let (resume, wait) = channel();
        *lock(&BLOCK) = Some((entered, wait));
        assert_eq!(
            source_submit(voice, [0; 44].as_ptr(), std::ptr::null()),
            S_OK
        );
        assert_eq!(source_flush(voice), S_OK);
        began.recv_timeout(Duration::from_secs(2)).unwrap();

        let (destroyed, done) = channel();
        let ptr = voice as usize;
        let destroyer = std::thread::spawn(move || {
            destroy_voice(ptr as *mut Voice);
            destroyed.send(()).unwrap();
        });
        let early = done.recv_timeout(Duration::from_millis(50));
        resume.send(()).unwrap();
        destroyer.join().unwrap();
        assert_eq!(early, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
        assert_eq!(engine_release(engine), 0);
    }

    /// The engine tests share the held output.
    static TURN: Mutex<()> = Mutex::new(());

    #[test]
    fn the_engine_is_reference_counted_and_lookups_resolve() {
        let _turn = lock(&TURN);
        let mut engine: *mut Engine = std::ptr::null_mut();
        assert_eq!(create_with(&mut engine, open_held), S_OK);
        let e = engine.cast::<c_void>();
        let mut performance = [0xA5; 64 + 8];
        call!(e, 11, fn(*mut u8) -> (), performance.as_mut_ptr());
        assert_eq!(&performance[..64], &[0; 64]);
        assert_eq!(&performance[64..], &[0xA5; 8]);
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

    #[test]
    fn invalid_ratios_loop_bounds_and_null_send_descriptors_are_rejected() {
        let engine = new_engine(open_held);
        let w = wave(1, 48_000, 16, 1);
        // XAUDIO2_VOICE_SENDS with SendCount=1 and pSends=NULL must not dereference NULL.
        let mut bad_sends = [0u8; 12];
        bad_sends[0..4].copy_from_slice(&1u32.to_le_bytes());
        let mut voice = std::ptr::null_mut();
        assert_eq!(
            create_source_voice(
                engine,
                &mut voice,
                w.as_ptr(),
                0,
                2.0,
                std::ptr::null_mut(),
                bad_sends.as_ptr(),
                std::ptr::null()
            ),
            S_OK
        );

        // Non-finite and non-positive ratios fail with E_INVALIDARG and preserve the current ratio.
        assert_eq!(source_set_ratio(voice, 1.5, 0), S_OK);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -0.5] {
            assert_eq!(source_set_ratio(voice, bad, 0), E_INVALIDARG);
        }
        let mut ratio = 0.0f32;
        source_get_ratio(voice, &mut ratio);
        assert_eq!(ratio, 1.5);

        // Loop regions outside [play_begin, play_end] are refused.
        let pcm = [0i16; 100];
        let mut buf = [0u8; 44];
        buf[4..8].copy_from_slice(&(size_of_val(&pcm) as u32).to_le_bytes());
        buf[8..16].copy_from_slice(&(pcm.as_ptr() as usize).to_le_bytes());
        buf[16..20].copy_from_slice(&50u32.to_le_bytes()); // PlayBegin = 50
        buf[20..24].copy_from_slice(&40u32.to_le_bytes()); // PlayLength = 40 (end = 90)
        buf[24..28].copy_from_slice(&10u32.to_le_bytes()); // LoopBegin = 10
        buf[28..32].copy_from_slice(&20u32.to_le_bytes()); // LoopLength = 20 (loop_end = 30 <= play_begin)
        buf[32..36].copy_from_slice(&1u32.to_le_bytes()); // LoopCount = 1
        assert_eq!(
            source_submit(voice, buf.as_ptr(), std::ptr::null()),
            XAUDIO2_E_INVALID_CALL
        );
        // LoopEnd past PlayEnd is also refused.
        buf[24..28].copy_from_slice(&60u32.to_le_bytes());
        buf[28..32].copy_from_slice(&35u32.to_le_bytes()); // loop_end = 95 > 90
        assert_eq!(
            source_submit(voice, buf.as_ptr(), std::ptr::null()),
            XAUDIO2_E_INVALID_CALL
        );

        destroy_voice(voice);
        assert_eq!(engine_release(engine), 0);
    }
}
