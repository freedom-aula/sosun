//! Where the mixer's frames go: an audio output behind a trait, with
//! CoreAudio (an AudioToolbox `AudioQueue`, called through a small FFI of
//! our own: no crate is added) and a silent one that keeps time when there is
//! no device.
//!
//! An [`Output`] asks for frames: the render function is called on the
//! device's thread with a buffer of interleaved 32-bit floats to fill, and
//! stops being called when the output is dropped.

use std::ffi::c_void;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Fills a buffer of interleaved float frames.
pub type Render = Box<dyn FnMut(&mut [f32]) + Send>;

/// An open output: it plays until it is dropped.
pub trait Output: Send {}

/// Opens an output of `channels` at `rate` that pulls frames from `render`.
pub type Opener = fn(rate: u32, channels: u32, render: Render) -> io::Result<Box<dyn Output>>;

/// CoreAudio, or the silent clock if it cannot open.
pub fn open_default(rate: u32, channels: u32, render: Render) -> io::Result<Box<dyn Output>> {
    // The render function is moved into whichever opens; keep it shared
    // until one has.
    let shared = Arc::new(std::sync::Mutex::new(render));
    let for_device = shared.clone();
    let device: Render = Box::new(move |out| (for_device.lock().unwrap_or_else(|e| e.into_inner()))(out));
    match CoreAudio::open(rate, channels, device) {
        Ok(output) => Ok(Box::new(output)),
        Err(e) => {
            tracing::warn!(error = %e, "no audio device: sound is mixed and dropped");
            let silent: Render = Box::new(move |out| (shared.lock().unwrap_or_else(|e| e.into_inner()))(out));
            Ok(Box::new(Silent::open(rate, channels, silent)))
        }
    }
}

// AudioToolbox's AudioQueue.

type OSStatus = i32;
type AudioQueueRef = *mut c_void;

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioQueueBuffer {
    capacity: u32,
    data: *mut c_void,
    byte_size: u32,
    user_data: *mut c_void,
    packet_description_capacity: u32,
    packet_descriptions: *mut c_void,
    packet_description_count: u32,
}

type OutputCallback = extern "C" fn(*mut c_void, AudioQueueRef, *mut AudioQueueBuffer);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(
        format: *const AudioStreamBasicDescription,
        callback: OutputCallback,
        user: *mut c_void,
        run_loop: *mut c_void,
        run_loop_mode: *const c_void,
        flags: u32,
        queue: *mut AudioQueueRef,
    ) -> OSStatus;
    fn AudioQueueAllocateBuffer(
        queue: AudioQueueRef,
        bytes: u32,
        buffer: *mut *mut AudioQueueBuffer,
    ) -> OSStatus;
    fn AudioQueueEnqueueBuffer(
        queue: AudioQueueRef,
        buffer: *mut AudioQueueBuffer,
        packets: u32,
        descriptions: *const c_void,
    ) -> OSStatus;
    fn AudioQueueStart(queue: AudioQueueRef, start_time: *const c_void) -> OSStatus;
    fn AudioQueueStop(queue: AudioQueueRef, immediate: u8) -> OSStatus;
    fn AudioQueueDispose(queue: AudioQueueRef, immediate: u8) -> OSStatus;
}

/// `'lpcm'`, linear PCM; float, packed.
const FORMAT_LINEAR_PCM: u32 = 0x6C70_636D;
const FLAGS_FLOAT_PACKED: u32 = 1 | 8;
/// Frames per buffer (about 21 ms at 48 kHz) and buffers in flight.
const BUFFER_FRAMES: u32 = 1024;
const BUFFERS: usize = 3;

/// What the queue's callback reaches: boxed, so its address is stable.
struct Shared {
    render: Render,
    channels: u32,
    /// Set before the queue is stopped, so a late callback does nothing.
    closing: AtomicBool,
}

pub struct CoreAudio {
    queue: AudioQueueRef,
    shared: *mut Shared,
}

// SAFETY: the queue is thread-safe and `shared` is only touched by the
// callback and by `drop`, after the queue is disposed.
unsafe impl Send for CoreAudio {}
impl Output for CoreAudio {}

extern "C" fn refill(user: *mut c_void, queue: AudioQueueRef, buffer: *mut AudioQueueBuffer) {
    // SAFETY: `user` is our `Shared`, alive until the queue is disposed;
    // `buffer` is one of ours.
    unsafe {
        let shared = &mut *user.cast::<Shared>();
        let buffer = &mut *buffer;
        let samples = std::slice::from_raw_parts_mut(
            buffer.data.cast::<f32>(),
            (BUFFER_FRAMES * shared.channels) as usize,
        );
        if shared.closing.load(Ordering::Acquire) {
            samples.fill(0.0);
        } else {
            (shared.render)(samples);
        }
        buffer.byte_size = BUFFER_FRAMES * shared.channels * 4;
        AudioQueueEnqueueBuffer(queue, buffer, 0, std::ptr::null());
    }
}

impl CoreAudio {
    pub fn open(rate: u32, channels: u32, render: Render) -> io::Result<CoreAudio> {
        let fail = |what: &str, status: OSStatus| {
            io::Error::other(format!("{what} failed with OSStatus {status}"))
        };
        let format = AudioStreamBasicDescription {
            sample_rate: f64::from(rate),
            format_id: FORMAT_LINEAR_PCM,
            format_flags: FLAGS_FLOAT_PACKED,
            bytes_per_packet: channels * 4,
            frames_per_packet: 1,
            bytes_per_frame: channels * 4,
            channels_per_frame: channels,
            bits_per_channel: 32,
            reserved: 0,
        };
        let shared = Box::into_raw(Box::new(Shared {
            render,
            channels,
            closing: AtomicBool::new(false),
        }));
        let mut queue: AudioQueueRef = std::ptr::null_mut();
        // SAFETY: a valid format, our callback and its user data.
        let status = unsafe {
            AudioQueueNewOutput(
                &format,
                refill,
                shared.cast(),
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                &mut queue,
            )
        };
        let free = |queue: AudioQueueRef| {
            // SAFETY: made above; nothing runs on it yet or any more.
            unsafe {
                if !queue.is_null() {
                    AudioQueueDispose(queue, 1);
                }
                drop(Box::from_raw(shared));
            }
        };
        if status != 0 {
            free(std::ptr::null_mut());
            return Err(fail("AudioQueueNewOutput", status));
        }
        for _ in 0..BUFFERS {
            let mut buffer: *mut AudioQueueBuffer = std::ptr::null_mut();
            // SAFETY: the queue just made.
            let status = unsafe {
                AudioQueueAllocateBuffer(queue, BUFFER_FRAMES * channels * 4, &mut buffer)
            };
            if status != 0 {
                free(queue);
                return Err(fail("AudioQueueAllocateBuffer", status));
            }
            // Primed with silence-or-sound by the same path as every refill.
            refill(shared.cast(), queue, buffer);
        }
        // SAFETY: the queue just made.
        let status = unsafe { AudioQueueStart(queue, std::ptr::null()) };
        if status != 0 {
            free(queue);
            return Err(fail("AudioQueueStart", status));
        }
        Ok(CoreAudio { queue, shared })
    }
}

impl Drop for CoreAudio {
    fn drop(&mut self) {
        // SAFETY: our queue and state; after the dispose no callback runs.
        unsafe {
            (*self.shared).closing.store(true, Ordering::Release);
            AudioQueueStop(self.queue, 1);
            AudioQueueDispose(self.queue, 1);
            drop(Box::from_raw(self.shared));
        }
    }
}

/// No device: frames are pulled at the pace of the clock and dropped, so the
/// title's voices still advance and its callbacks still come.
pub struct Silent {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Output for Silent {}

impl Silent {
    pub fn open(rate: u32, channels: u32, mut render: Render) -> Silent {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let chunk = (rate / 100).max(1) as usize; // 10 ms
        let thread = std::thread::spawn(move || {
            let mut buffer = vec![0.0f32; chunk * channels as usize];
            let mut next = Instant::now();
            while !flag.load(Ordering::Acquire) {
                render(&mut buffer);
                next += Duration::from_millis(10);
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
            }
        });
        Silent {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Silent {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn the_device_opens_pulls_frames_and_closes() {
        let pulled = Arc::new(AtomicUsize::new(0));
        let count = pulled.clone();
        let render: Render = Box::new(move |out| {
            // A 440 Hz tone, as a title's mixer would give it.
            for (i, frame) in out.chunks_exact_mut(2).enumerate() {
                let x = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.01;
                frame.fill(x);
            }
            count.fetch_add(out.len() / 2, Ordering::Relaxed);
        });
        match CoreAudio::open(48_000, 2, render) {
            Ok(device) => {
                // The priming buffers are pulled at once; the device pulls
                // more as it plays them.
                assert!(pulled.load(Ordering::Relaxed) >= 3 * 1024);
                std::thread::sleep(Duration::from_millis(200));
                drop(device);
                let after = pulled.load(Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(100));
                // Closed: nothing pulls any more.
                assert_eq!(pulled.load(Ordering::Relaxed), after);
            }
            // A machine with no output device (CI) says so; the silent
            // clock takes over, and that is tested next.
            Err(e) => eprintln!("no CoreAudio output here: {e}"),
        }
    }

    #[test]
    fn the_silent_output_keeps_time() {
        let pulled = Arc::new(AtomicUsize::new(0));
        let count = pulled.clone();
        let silent = Silent::open(
            48_000,
            2,
            Box::new(move |out| {
                count.fetch_add(out.len() / 2, Ordering::Relaxed);
            }),
        );
        std::thread::sleep(Duration::from_millis(300));
        drop(silent);
        let frames = pulled.load(Ordering::Relaxed);
        // About 14,400 frames in 300 ms.
        assert!((9_600..=19_200).contains(&frames), "{frames}");
        let open = open_default(48_000, 2, Box::new(|_| {})).unwrap();
        drop(open);
    }
}
