//! Windows Spatial Audio (`ISpatialAudioClient`, which a title activates on
//! the default render device): a stream of audio objects, each a mono float
//! buffer with a position and a volume. Static objects are the speaker bed;
//! dynamic objects are panned by their position. Every update pass mixes the
//! objects to stereo into the same ring the plain WASAPI client feeds.
//!
//! shortcut: stereo panning by azimuth only (no distance, height or HRTF),
//! no metadata streams, no notify callbacks; add them when a title needs them.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use super::{
    DEVICE_CHANNELS, E_INVALIDARG, E_NOINTERFACE, E_POINTER, HRESULT, IID_UNKNOWN,
    MIX_RATE, Output, S_OK, STEREO, Shared, Simple, coreaudio, guid, leak, lock, parse_format, put,
    read_guid, signal, simple, table, warn_once,
};

pub const IID_CLIENT: [u8; 16] =
    guid(0xBBF8_E066, 0xAAAA, 0x49BE, [0x9A, 0x4D, 0xFD, 0x2A, 0x85, 0x8E, 0xA2, 0x7F]);
const IID_STREAM: [u8; 16] =
    guid(0xBAB5_F473, 0xB423, 0x477B, [0x85, 0xF5, 0xB5, 0xA3, 0x32, 0xA0, 0x41, 0x53]);
const IID_STREAM_BASE: [u8; 16] =
    guid(0xFEAA_F403, 0xC1D8, 0x450D, [0xAA, 0x05, 0xE0, 0xCC, 0xEE, 0x75, 0x02, 0xA8]);
const IID_OBJECT: [u8; 16] =
    guid(0xDDE2_8967, 0x521B, 0x46E5, [0x8F, 0x00, 0xBD, 0x6F, 0x2B, 0xC8, 0xAB, 0x1D]);
const IID_OBJECT_BASE: [u8; 16] =
    guid(0xCCE0_B8F2, 0x8D4D, 0x4EFB, [0xA8, 0xCF, 0x3D, 0x6E, 0xCF, 0x1C, 0x30, 0xE0]);
const IID_FORMATS: [u8; 16] =
    guid(0xDCDA_A858, 0x895A, 0x4A22, [0xA5, 0xEB, 0x67, 0xBD, 0xA5, 0x06, 0x09, 0x6D]);

const SPTLAUDCLNT_E_OUT_OF_ORDER: HRESULT = 0x8889_0101_u32 as i32;
const SPTLAUDCLNT_E_NO_MORE_OBJECTS: HRESULT = 0x8889_0103_u32 as i32;

const DYNAMIC: u32 = 1;
/// Front left to top back right: the 7.1.4 bed.
const STATIC_MASK: u32 = 0x1FFE;
const MAX_DYNAMIC: u32 = 128;
const PERIOD: usize = (MIX_RATE / 100) as usize;

macro_rules! methods {
    ($($f:path),* $(,)?) => { [$($f as *const () as usize),*] };
}

pub fn client() -> *mut Simple {
    simple(&CLIENT, IID_CLIENT)
}

static CLIENT: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        super::simple_query,
        super::simple_add_ref,
        super::simple_release,
        static_position,
        static_mask,
        max_dynamic,
        format_enumerator,
        max_frames,
        format_supported,
        stream_available,
        activate_stream,
    ])
});

static FORMATS: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        super::simple_query,
        super::simple_add_ref,
        super::simple_release,
        formats_count,
        formats_get,
    ])
});

static STREAM: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        stream_query,
        stream_add_ref,
        stream_release,
        stream_available_objects,
        stream_service,
        stream_start,
        stream_stop,
        stream_reset,
        stream_begin,
        stream_end,
        stream_activate_object,
    ])
});

static OBJECT: LazyLock<Vec<usize>> = LazyLock::new(|| {
    table(&methods![
        object_query,
        object_add_ref,
        object_release,
        object_buffer,
        object_end_of_stream,
        object_is_active,
        object_type,
        object_position,
        object_volume,
    ])
});

/// Where a static object sits (x right, y up, z back), on the unit sphere.
fn static_xyz(kind: u32) -> [f32; 3] {
    let h = std::f32::consts::FRAC_1_SQRT_2;
    let (x, z) = match kind {
        0x2 | 0x200 | 0x2000 => (-h, -h),
        0x4 | 0x400 | 0x4000 => (h, -h),
        0x8 => (0.0, -1.0),
        0x20 => (-1.0, 0.0),
        0x40 => (1.0, 0.0),
        0x80 | 0x800 | 0x8000 => (-h, h),
        0x100 | 0x1000 | 0x10000 => (h, h),
        0x20000 => (0.0, 1.0),
        _ => (0.0, 0.0),
    };
    let y = match kind {
        0x200..=0x1000 => 0.5,
        0x2000..=0x10000 => -0.5,
        _ => 0.0,
    };
    [x, y, z]
}

extern "win64" fn static_position(
    _this: *mut Simple,
    kind: u32,
    x: *mut f32,
    y: *mut f32,
    z: *mut f32,
) -> HRESULT {
    if x.is_null() || y.is_null() || z.is_null() {
        return E_POINTER;
    }
    let p = static_xyz(kind);
    put(x, p[0]);
    put(y, p[1]);
    put(z, p[2])
}

extern "win64" fn static_mask(_this: *mut Simple, out: *mut u32) -> HRESULT {
    put(out, STATIC_MASK)
}

extern "win64" fn max_dynamic(_this: *mut Simple, out: *mut u32) -> HRESULT {
    put(out, MAX_DYNAMIC)
}

extern "win64" fn format_enumerator(_this: *mut Simple, out: *mut *mut c_void) -> HRESULT {
    put(out, simple(&FORMATS, IID_FORMATS).cast())
}

extern "win64" fn max_frames(_this: *mut Simple, _wave: *const u8, out: *mut u32) -> HRESULT {
    put(out, PERIOD as u32)
}

/// Mono float is what objects carry.
fn object_format(wave: *const u8) -> bool {
    parse_format(wave).is_some_and(|f| f.channels == 1 && f.float && f.bits == 32)
}

extern "win64" fn format_supported(_this: *mut Simple, wave: *const u8) -> HRESULT {
    if object_format(wave) { S_OK } else { super::AUDCLNT_E_UNSUPPORTED_FORMAT }
}

extern "win64" fn stream_available(
    _this: *mut Simple,
    _iid: *const c_void,
    _params: *const u8,
) -> HRESULT {
    S_OK
}

extern "win64" fn formats_count(_this: *mut Simple, out: *mut u32) -> HRESULT {
    put(out, 1)
}

/// A `WAVEFORMATEX`: 48 kHz mono float.
extern "win64" fn formats_get(_this: *mut Simple, index: u32, out: *mut *mut u8) -> HRESULT {
    if index != 0 {
        return E_INVALIDARG;
    }
    let mut w = Vec::new();
    w.extend(3u16.to_le_bytes());
    w.extend(1u16.to_le_bytes());
    w.extend(MIX_RATE.to_le_bytes());
    w.extend((MIX_RATE * 4).to_le_bytes());
    w.extend(4u16.to_le_bytes());
    w.extend(32u16.to_le_bytes());
    w.extend(0u16.to_le_bytes());
    put(out, Box::leak(w.into_boxed_slice()).as_mut_ptr())
}

// The stream.

struct ObjectData {
    /// `frames` mono samples, zeroed at the start of each pass.
    samples: Vec<f32>,
    position: [f32; 3],
    volume: f32,
    active: bool,
    /// Frames to play from the end-of-stream mark on, if set.
    last: Option<usize>,
}

struct Mix {
    kind: u32,
    data: Arc<Mutex<ObjectData>>,
}

#[repr(C)]
struct Stream {
    vtable: *const usize,
    refs: AtomicU32,
    shared: Arc<Shared>,
    _output: Box<dyn Output>,
    _clock: Clock,
    objects: Mutex<Vec<Mix>>,
    updating: Arc<AtomicBool>,
    dynamic_used: Arc<AtomicU32>,
    max_dynamic: u32,
}

/// The clock of the title's event (`Shared::tick`), stopped when dropped.
struct Clock {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Clock {
    fn start(shared: Arc<Shared>) -> Clock {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut next = Instant::now();
            while !flag.load(Ordering::Acquire) {
                let (event, wait) = shared.tick();
                if let Some(event) = event {
                    signal(event);
                }
                // A late tick does not bring the next one closer than half a wait.
                next = (next + wait).max(Instant::now() + wait / 2);
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        });
        Clock {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `ActivateSpatialAudioStream(PROPVARIANT*, REFIID, void**)`: the variant
/// holds a `SpatialAudioObjectRenderStreamActivationParams` blob.
extern "win64" fn activate_stream(
    _this: *mut Simple,
    params: *const u8,
    iid: *const c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    let iid = read_guid(iid);
    if out.is_null() || params.is_null() {
        return E_POINTER;
    }
    if iid != IID_STREAM && iid != IID_STREAM_BASE {
        warn_once(format!("ActivateSpatialAudioStream of {iid:02x?}"));
        return E_NOINTERFACE;
    }
    // SAFETY: a VT_BLOB PROPVARIANT (type at 0, size at 8, pointer at 16)
    // holding the 40-byte activation parameters.
    let (format, mask, max, event, size) = unsafe {
        let size = params.add(8).cast::<u32>().read_unaligned();
        let blob = params.add(16).cast::<*const u8>().read_unaligned();
        if blob.is_null() || size < 36 {
            return E_INVALIDARG;
        }
        (
            blob.cast::<*const u8>().read_unaligned(),
            blob.add(8).cast::<u32>().read_unaligned(),
            blob.add(16).cast::<u32>().read_unaligned(),
            blob.add(24).cast::<usize>().read_unaligned(),
            size,
        )
    };
    if !object_format(format) {
        warn_once("spatial stream with an object format other than mono float".into());
        return super::AUDCLNT_E_UNSUPPORTED_FORMAT;
    }
    let shared = Shared::new(STEREO, PERIOD * 5, PERIOD);
    lock(&shared.state).event = event;
    let render = shared.clone();
    let output = coreaudio::open_default(
        MIX_RATE,
        DEVICE_CHANNELS as u32,
        Box::new(move |out| match out {
            // The event has a clock of its own.
            Ok(out) => drop(render.render(out)),
            Err(e) => warn_once(format!("the output failed: {e}")),
        }),
    );
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("WASAPI: no output: {e}");
            return super::AUDCLNT_E_UNSUPPORTED_FORMAT;
        }
    };
    tracing::info!(
        static_mask = mask,
        max_dynamic = max,
        params = size,
        event = event != 0,
        "WASAPI: spatial audio stream activated"
    );
    let stream = leak(Stream {
        vtable: STREAM.as_ptr(),
        refs: AtomicU32::new(1),
        _clock: Clock::start(shared.clone()),
        shared,
        _output: output,
        objects: Mutex::new(Vec::new()),
        updating: Arc::new(AtomicBool::new(false)),
        dynamic_used: Arc::new(AtomicU32::new(0)),
        max_dynamic: max.min(MAX_DYNAMIC),
    });
    put(out, stream.cast())
}

extern "win64" fn stream_query(this: *mut Stream, iid: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let iid = read_guid(iid);
    if out.is_null() {
        return E_POINTER;
    }
    if ![IID_UNKNOWN, IID_STREAM, IID_STREAM_BASE].contains(&iid) {
        warn_once(format!("spatial stream QueryInterface of {iid:02x?}"));
        put(out, std::ptr::null_mut());
        return E_NOINTERFACE;
    }
    // SAFETY: the title's object.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) };
    put(out, this.cast())
}

extern "win64" fn stream_add_ref(this: *mut Stream) -> u32 {
    // SAFETY: the title's object.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

extern "win64" fn stream_release(this: *mut Stream) -> u32 {
    // SAFETY: the title's object.
    let left = unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel) } - 1;
    if left == 0 {
        // SAFETY: made by `activate_stream`, released for the last time:
        // the device closes with it.
        unsafe { drop(Box::from_raw(this)) };
    }
    left
}

extern "win64" fn stream_available_objects(this: *mut Stream, out: *mut u32) -> HRESULT {
    // SAFETY: the title's object.
    let s = unsafe { &*this };
    put(out, s.max_dynamic.saturating_sub(s.dynamic_used.load(Ordering::Acquire)))
}

extern "win64" fn stream_service(_this: *mut Stream, iid: *const c_void, out: *mut *mut c_void) -> HRESULT {
    warn_once(format!("spatial stream GetService of {:02x?}", read_guid(iid)));
    put(out, std::ptr::null_mut());
    E_NOINTERFACE
}

extern "win64" fn stream_start(this: *mut Stream) -> HRESULT {
    // SAFETY: the title's object.
    let s = unsafe { &*this };
    let event = {
        let mut state = lock(&s.shared.state);
        if state.running {
            return super::AUDCLNT_E_NOT_STOPPED;
        }
        state.running = true;
        state.event
    };
    // A period of slack against the device, then the title's first pass.
    s.shared.push(&[0.0; PERIOD * DEVICE_CHANNELS]);
    if event != 0 {
        signal(event);
    }
    S_OK
}

extern "win64" fn stream_stop(this: *mut Stream) -> HRESULT {
    // SAFETY: the title's object.
    lock(&unsafe { &*this }.shared.state).running = false;
    S_OK
}

extern "win64" fn stream_reset(this: *mut Stream) -> HRESULT {
    // SAFETY: the title's object.
    let s = unsafe { &*this };
    let mut state = lock(&s.shared.state);
    if state.running {
        return super::AUDCLNT_E_NOT_STOPPED;
    }
    state.ring = super::Ring::new(DEVICE_CHANNELS);
    S_OK
}

extern "win64" fn stream_begin(this: *mut Stream, available: *mut u32, frames: *mut u32) -> HRESULT {
    if available.is_null() || frames.is_null() {
        return E_POINTER;
    }
    // SAFETY: the title's object.
    let s = unsafe { &*this };
    if s.updating.swap(true, Ordering::AcqRel) {
        return SPTLAUDCLNT_E_OUT_OF_ORDER;
    }
    for o in lock(&s.objects).iter() {
        lock(&o.data).samples.fill(0.0);
    }
    put(available, s.max_dynamic.saturating_sub(s.dynamic_used.load(Ordering::Acquire)));
    put(frames, PERIOD as u32)
}

extern "win64" fn stream_end(this: *mut Stream) -> HRESULT {
    // SAFETY: the title's object.
    let s = unsafe { &*this };
    if !s.updating.swap(false, Ordering::AcqRel) {
        return SPTLAUDCLNT_E_OUT_OF_ORDER;
    }
    let (mixed, ended_dynamic) = {
        let mut objects = lock(&s.objects);
        let (mixed, ended_dynamic) = mix_pass(&objects);
        objects.retain(|o| lock(&o.data).active);
        (mixed, ended_dynamic)
    };
    if ended_dynamic > 0 {
        s.dynamic_used.fetch_sub(ended_dynamic, Ordering::AcqRel);
    }
    // The title's passes are paced by our event; a stray extra one must not
    // grow the queue without bound.
    if s.shared.padding() < PERIOD * 8 {
        s.shared.push(&mixed);
    }
    S_OK
}

extern "win64" fn stream_activate_object(this: *mut Stream, kind: u32, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: the title's object.
    let s = unsafe { &*this };
    if kind == DYNAMIC
        && s.dynamic_used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < s.max_dynamic).then_some(used + 1)
            })
            .is_err()
    {
        return SPTLAUDCLNT_E_NO_MORE_OBJECTS;
    }
    let data = Arc::new(Mutex::new(ObjectData {
        samples: vec![0.0; PERIOD],
        position: [0.0; 3],
        volume: 1.0,
        active: true,
        last: None,
    }));
    lock(&s.objects).push(Mix { kind, data: data.clone() });
    let object = leak(Object {
        vtable: OBJECT.as_ptr(),
        refs: AtomicU32::new(1),
        kind,
        data,
        updating: s.updating.clone(),
        dynamic_used: s.dynamic_used.clone(),
    });
    put(out, object.cast())
}

/// Left and right gain of an object.
fn gains(kind: u32, position: [f32; 3]) -> (f32, f32) {
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
    match kind {
        DYNAMIC => {
            // Azimuth from straight ahead (-z), positive to the right.
            let pan = position[0].atan2(-position[2]).sin().clamp(-1.0, 1.0);
            let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
            (angle.cos(), angle.sin())
        }
        0x10 => (0.35, 0.35),
        k => {
            let x = static_xyz(k)[0];
            let up = if (0x200..=0x1000).contains(&k) { 0.7 } else { 1.0 };
            if x < -0.01 {
                (up, 0.0)
            } else if x > 0.01 {
                (0.0, up)
            } else {
                (H * up, H * up)
            }
        }
    }
}

/// One pass of stereo frames from every active object, plus the count of
/// dynamic objects deactivated by end-of-stream during this pass.
fn mix_pass(objects: &[Mix]) -> (Vec<f32>, u32) {
    let mut out = vec![0.0; PERIOD * DEVICE_CHANNELS];
    let mut ended_dynamic = 0u32;
    for o in objects {
        let mut d = lock(&o.data);
        if !d.active {
            continue;
        }
        let (l, r) = gains(o.kind, d.position);
        let (l, r) = (l * d.volume, r * d.volume);
        let n = d.last.map_or(PERIOD, |n| n.min(PERIOD));
        for (frame, s) in out.chunks_exact_mut(2).zip(&d.samples[..n]) {
            frame[0] += s * l;
            frame[1] += s * r;
        }
        if d.last.is_some() && std::mem::replace(&mut d.active, false) && o.kind == DYNAMIC {
            ended_dynamic += 1;
        }
    }
    (out, ended_dynamic)
}

#[cfg(test)]
fn mix(objects: &[Mix]) -> Vec<f32> {
    mix_pass(objects).0
}

// Objects.

#[repr(C)]
struct Object {
    vtable: *const usize,
    refs: AtomicU32,
    kind: u32,
    data: Arc<Mutex<ObjectData>>,
    updating: Arc<AtomicBool>,
    dynamic_used: Arc<AtomicU32>,
}

extern "win64" fn object_query(this: *mut Object, iid: *const c_void, out: *mut *mut c_void) -> HRESULT {
    let iid = read_guid(iid);
    if out.is_null() {
        return E_POINTER;
    }
    if ![IID_UNKNOWN, IID_OBJECT, IID_OBJECT_BASE].contains(&iid) {
        warn_once(format!("spatial object QueryInterface of {iid:02x?}"));
        put(out, std::ptr::null_mut());
        return E_NOINTERFACE;
    }
    // SAFETY: the title's object.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) };
    put(out, this.cast())
}

extern "win64" fn object_add_ref(this: *mut Object) -> u32 {
    // SAFETY: the title's object.
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

extern "win64" fn object_release(this: *mut Object) -> u32 {
    // SAFETY: the title's object.
    let left = unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel) } - 1;
    if left == 0 {
        // SAFETY: made by `stream_activate_object`, released for the last
        // time; the object stops playing and gives its slot back.
        let o = unsafe { Box::from_raw(this) };
        if std::mem::replace(&mut lock(&o.data).active, false) && o.kind == DYNAMIC {
            o.dynamic_used.fetch_sub(1, Ordering::AcqRel);
        }
    }
    left
}

extern "win64" fn object_buffer(this: *mut Object, buffer: *mut *mut u8, bytes: *mut u32) -> HRESULT {
    if buffer.is_null() || bytes.is_null() {
        return E_POINTER;
    }
    // SAFETY: the title's object.
    let o = unsafe { &*this };
    if !o.updating.load(Ordering::Acquire) {
        return SPTLAUDCLNT_E_OUT_OF_ORDER;
    }
    // The buffer is never resized, so the pointer holds until the pass ends.
    let mut d = lock(&o.data);
    put(bytes, (d.samples.len() * 4) as u32);
    put(buffer, d.samples.as_mut_ptr().cast())
}

extern "win64" fn object_end_of_stream(this: *mut Object, frames: u32) -> HRESULT {
    // SAFETY: the title's object.
    lock(&unsafe { &*this }.data).last = Some(frames as usize);
    S_OK
}

extern "win64" fn object_is_active(this: *mut Object, out: *mut i32) -> HRESULT {
    // SAFETY: the title's object.
    put(out, i32::from(lock(&unsafe { &*this }.data).active))
}

extern "win64" fn object_type(this: *mut Object, out: *mut u32) -> HRESULT {
    // SAFETY: the title's object.
    put(out, unsafe { &*this }.kind)
}

extern "win64" fn object_position(this: *mut Object, x: f32, y: f32, z: f32) -> HRESULT {
    // SAFETY: the title's object.
    lock(&unsafe { &*this }.data).position = [x, y, z];
    S_OK
}

extern "win64" fn object_volume(this: *mut Object, volume: f32) -> HRESULT {
    // SAFETY: the title's object.
    lock(&unsafe { &*this }.data).volume = volume;
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(kind: u32, position: [f32; 3], fill: f32) -> Mix {
        Mix {
            kind,
            data: Arc::new(Mutex::new(ObjectData {
                samples: vec![fill; PERIOD],
                position,
                volume: 1.0,
                active: true,
                last: None,
            })),
        }
    }

    #[test]
    fn objects_mix_to_stereo_by_position() {
        let objects = [
            object(DYNAMIC, [-1.0, 0.0, 0.0], 0.5),
            object(0x4, [0.0; 3], 0.25),
        ];
        let out = mix(&objects);
        assert_eq!(out.len(), PERIOD * 2);
        // Hard left dynamic object plus a front right static one.
        assert!(out[0] > 0.45 && out[0] <= 0.5 + 1e-6);
        assert!((out[1] - 0.25).abs() < 0.05);
    }

    #[test]
    fn an_object_ends_after_its_last_frame() {
        let o = object(DYNAMIC, [0.0, 0.0, -1.0], 1.0);
        lock(&o.data).last = Some(10);
        let out = mix(&[o]);
        assert!(out[2 * 9] > 0.0 && out[2 * 10] == 0.0);
    }

    struct DummyOutput;
    impl Output for DummyOutput {}

    fn test_stream(max_dynamic: u32) -> *mut Stream {
        leak(Stream {
            vtable: STREAM.as_ptr(),
            refs: AtomicU32::new(1),
            shared: Shared::new(STEREO, PERIOD * 5, PERIOD),
            _output: Box::new(DummyOutput),
            _clock: Clock {
                stop: Arc::new(AtomicBool::new(true)),
                thread: None,
            },
            objects: Mutex::new(Vec::new()),
            updating: Arc::new(AtomicBool::new(false)),
            dynamic_used: Arc::new(AtomicU32::new(0)),
            max_dynamic,
        })
    }

    #[test]
    fn end_of_stream_reclaims_dynamic_slots_without_double_decrement() {
        let stream = test_stream(2);
        let mut avail = 0u32;
        let mut frames = 0u32;

        // Activate two dynamic objects up to the limit; a third must fail.
        let mut o1 = std::ptr::null_mut();
        let mut o2 = std::ptr::null_mut();
        let mut o3 = std::ptr::null_mut();
        assert_eq!(stream_activate_object(stream, DYNAMIC, &mut o1), S_OK);
        assert_eq!(stream_activate_object(stream, DYNAMIC, &mut o2), S_OK);
        assert_eq!(
            stream_activate_object(stream, DYNAMIC, &mut o3),
            SPTLAUDCLNT_E_NO_MORE_OBJECTS
        );
        assert_eq!(stream_available_objects(stream, &mut avail), S_OK);
        assert_eq!(avail, 0);

        // End o1 through SetEndOfStream + Begin/End pass; its slot must be reclaimed.
        assert_eq!(stream_begin(stream, &mut avail, &mut frames), S_OK);
        assert_eq!(object_end_of_stream(o1.cast(), 4), S_OK);
        assert_eq!(stream_end(stream), S_OK);
        assert_eq!(stream_available_objects(stream, &mut avail), S_OK);
        assert_eq!(avail, 1);

        // Releasing o1 after SetEndOfStream must not decrement dynamic_used a second time.
        assert_eq!(object_release(o1.cast()), 0);
        assert_eq!(stream_available_objects(stream, &mut avail), S_OK);
        assert_eq!(avail, 1);

        // Releasing o2 without SetEndOfStream reclaims its slot once.
        assert_eq!(object_release(o2.cast()), 0);
        assert_eq!(stream_available_objects(stream, &mut avail), S_OK);
        assert_eq!(avail, 2);

        // A new dynamic object can now be activated again.
        assert_eq!(stream_activate_object(stream, DYNAMIC, &mut o3), S_OK);
        assert_eq!(object_release(o3.cast()), 0);
        assert_eq!(stream_release(stream), 0);
    }

    #[test]
    fn null_out_pointers_fail_without_mutating_state() {
        let stream = test_stream(1);
        let mut avail = 0u32;
        let mut frames = 0u32;
        assert_eq!(stream_begin(stream, std::ptr::null_mut(), &mut frames), E_POINTER);
        assert_eq!(stream_begin(stream, &mut avail, std::ptr::null_mut()), E_POINTER);
        // The stream must not be stuck in the updating state after a failed Begin.
        assert_eq!(stream_begin(stream, &mut avail, &mut frames), S_OK);
        assert_eq!(stream_activate_object(stream, DYNAMIC, std::ptr::null_mut()), E_POINTER);
        assert_eq!(stream_available_objects(stream, &mut avail), S_OK);
        assert_eq!(avail, 1);

        let mut obj = std::ptr::null_mut();
        assert_eq!(stream_activate_object(stream, DYNAMIC, &mut obj), S_OK);
        let mut buf = std::ptr::null_mut();
        let mut bytes = 0u32;
        assert_eq!(object_buffer(obj.cast(), std::ptr::null_mut(), &mut bytes), E_POINTER);
        assert_eq!(object_buffer(obj.cast(), &mut buf, std::ptr::null_mut()), E_POINTER);
        assert_eq!(object_buffer(obj.cast(), &mut buf, &mut bytes), S_OK);
        assert_eq!(stream_end(stream), S_OK);

        let mut f = 0.0f32;
        assert_eq!(
            static_position(std::ptr::null_mut(), 0x2, std::ptr::null_mut(), &mut f, &mut f),
            E_POINTER
        );
        assert_eq!(
            static_position(std::ptr::null_mut(), 0x2, &mut f, std::ptr::null_mut(), &mut f),
            E_POINTER
        );
        assert_eq!(object_release(obj.cast()), 0);
        assert_eq!(stream_release(stream), 0);
    }
}
