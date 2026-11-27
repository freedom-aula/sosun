//! The console's audio DSP (`chad.dll`): streams that convolve blocks of
//! float samples with an impulse response (reverb), or run other block
//! transforms, on a coprocessor.
//!
//! A title connects once with a memory block for the DSP, activates
//! streams, and submits one command per block (gains, an input and an
//! output buffer). Activation hands back a status block whose second word is
//! the ticket of the last completed command and whose fourth is its result;
//! the title checks it one block later and treats a late command as an
//! overrun. Commands complete when submitted here.
//!
//! The transforms themselves are not computed from the title's own impulse
//! response: the hardware takes it already in its own partitioned
//! frequency-domain layout, which is not known, so `StreamDesc::ir` is not
//! read. With `VICTORY_GUESS_DSP=1`, a convolution stream instead gets a
//! short synthetic decaying-noise tail (see `synthetic_ir`), carried across
//! blocks so it does not reset at every block boundary; this is a CPU
//! fallback approximation of reverb coloration, not the title's actual
//! effect, and without the variable set the output is silence as before.
//! Other block transforms stay silent either way: their shape is unknown.
// shortcut: convolution streams get one fixed synthetic decay instead of the
// title's impulse response, behind VICTORY_GUESS_DSP=1 since it is unverified
// against a real run; other transform kinds stay silent regardless. Upgrade
// once the hardware's partitioned IR layout is documented.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::Mutex;

use super::xasync::E_INVALIDARG;
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;

const CONVOLUTION: u32 = 1;
/// Option bits that need a stereo stream, and one convolution refuses.
const STEREO_OPTIONS: u32 = 2 | 8;
const NOT_FOR_CONVOLUTION: u32 = 4;

pub struct Dsp {
    max_streams: u32,
    active: Mutex<u32>,
}

/// What activation describes.
#[repr(C, packed)]
pub struct StreamDesc {
    kind: u32,
    options: u32,
    frames: u32,
    channels: u32,
    ir_floats: u32,
    ir: *const f32,
}

/// The status block a stream's owner polls.
#[repr(C)]
pub struct Status {
    submitted: u32,
    completed: u32,
    reserved: u32,
    result: HRESULT,
}

pub struct Stream {
    dsp: *mut Dsp,
    frames: u32,
    channels: u32,
    kind: u32,
    ir: Vec<f32>,
    /// The last `ir.len() - 1` frames of input, interleaved, carried into the
    /// next block so the synthetic tail does not restart at every boundary.
    history: Vec<f32>,
    /// Reused across `XDspSubmitCommand` calls to merge `history` and the
    /// title's input without allocating on the audio path each block.
    scratch: Vec<f32>,
    status: Box<Status>,
}

/// Taps of the synthetic reverb tail: a fixed decaying-noise burst,
/// deterministic so the same stream always gets the same tail. About 100 ms
/// at 48 kHz: an early-reflection-length coloration, well short of a real
/// room's decay, but as far as a per-sample scalar convolution goes without
/// ballooning into an FFT-based engine.
const TAPS: usize = 4800;

fn synthetic_ir(len: usize) -> Vec<f32> {
    let mut seed = 0x9E37_79B9_u32;
    (0..len)
        .map(|k| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let noise = (seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
            let decay = (-(k as f32) / (len as f32 / 5.0)).exp();
            noise * decay
        })
        .collect()
}

/// Convolves one interleaved block against `ir` per channel, scaled by that
/// channel's gain (a guess: as likely to be `[dry, wet]`, in which case a
/// mono stream's second gain is never read). `merged` is `hist_frames`
/// frames of history (zero for the first block of a stream) followed by
/// the current block's `frames` frames, so the tail is continuous across
/// block boundaries rather than truncated at each one; `out` holds only
/// the current block's `frames` frames.
fn convolve_merged(
    merged: &[f32],
    hist_frames: usize,
    channels: usize,
    ir: &[f32],
    gains: [f32; 2],
    out: &mut [f32],
) {
    let frames = out.len() / channels;
    for c in 0..channels {
        let gain = gains[c];
        for n in 0..frames {
            let base = hist_frames + n;
            let mut acc = 0.0f32;
            for (k, &h) in ir.iter().enumerate() {
                if k > base {
                    break;
                }
                acc += merged[(base - k) * channels + c] * h;
            }
            out[n * channels + c] = acc * gain;
        }
    }
}

/// One block of work.
#[repr(C)]
pub struct Command {
    gains: [f32; 2],
    input: *const f32,
    output: *mut f32,
    reserved: u64,
}

pub extern "system" fn XDspConnectWithMaximumStreamLimit(
    base: *mut c_void,
    base_length: u32,
    ir_seconds: u32,
    max_streams: u32,
    max_channels: u32,
    out: *mut *mut Dsp,
) -> HRESULT {
    if base.is_null()
        || !(0x1_0000..=0x1F40_0000).contains(&base_length)
        || ir_seconds > 0x80
        || max_streams > 0x100
        || max_channels > 2
    {
        return E_INVALIDARG;
    }
    if out.is_null() {
        return E_POINTER;
    }
    tracing::info!(
        base_length,
        ir_seconds,
        max_streams,
        max_channels,
        "audio DSP connected"
    );
    let dsp = Box::new(Dsp {
        max_streams,
        active: Mutex::new(0),
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(dsp)) };
    S_OK
}

pub extern "system" fn XDspDisconnect(dsp: *mut Dsp) -> HRESULT {
    if dsp.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `XDspConnectWithMaximumStreamLimit`; disconnected
    // once, after its streams.
    drop(unsafe { Box::from_raw(dsp) });
    S_OK
}

/// Whether the synthetic convolution tail (module docs) is turned on; off by
/// default since it is unverified against a real run, leaving the title's
/// reverb silent as before. Checked fresh each call, not cached, so tests
/// can toggle it.
fn guess_dsp() -> bool {
    std::env::var_os("VICTORY_GUESS_DSP").is_some()
}

/// Logs the leading floats of a convolution stream's own impulse response at
/// activation, to help settle what format `StreamDesc::ir` is actually in
/// (see native-host.md). Off by default; reads at most 8 floats, bounded by
/// `ir_floats`.
fn trace_ir(desc: &StreamDesc) {
    if desc.kind != CONVOLUTION
        || desc.ir.is_null()
        || std::env::var_os("VICTORY_TRACE_DSP").is_none()
    {
        return;
    }
    let ir_floats = desc.ir_floats;
    let len = (ir_floats as usize).min(8);
    // SAFETY: `len` is bounded by the title's own `ir_floats` count.
    let head = unsafe { std::slice::from_raw_parts(desc.ir, len) };
    tracing::info!(ir_floats, ?head, "audio DSP stream's own impulse response");
}

fn valid(desc: &StreamDesc) -> bool {
    let (kind, options, frames, channels) = (desc.kind, desc.options, desc.frames, desc.channels);
    let shape = if kind == CONVOLUTION {
        (1..=2).contains(&channels)
            && matches!(frames, 0x200 | 0x400)
            && options & NOT_FOR_CONVOLUTION == 0
    } else {
        channels == 1 && matches!(frames, 0x200 | 0x400 | 0x800)
    };
    shape && (options & STEREO_OPTIONS == 0 || channels == 2)
}

pub extern "system" fn XDspActivate(
    dsp: *mut Dsp,
    desc: *const StreamDesc,
    status: *mut *const Status,
    stream: *mut *mut Stream,
) -> HRESULT {
    if status.is_null() || stream.is_null() {
        return E_POINTER;
    }
    if dsp.is_null() || desc.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes its description.
    let desc = unsafe { &*desc };
    if !valid(desc) {
        return E_INVALIDARG;
    }
    let (kind, options, frames, channels) = (desc.kind, desc.options, desc.frames, desc.channels);
    tracing::debug!(
        kind,
        options,
        frames,
        channels,
        "audio DSP stream activated"
    );
    trace_ir(desc);
    {
        // SAFETY: a connection from `XDspConnectWithMaximumStreamLimit`.
        let dsp = unsafe { &*dsp };
        let mut active = dsp.active.lock().unwrap_or_else(|e| e.into_inner());
        if *active >= dsp.max_streams {
            return E_OUTOFMEMORY;
        }
        *active += 1;
    }
    let ir = if kind == CONVOLUTION {
        synthetic_ir(TAPS)
    } else {
        Vec::new()
    };
    let history = vec![0.0; ir.len().saturating_sub(1) * channels as usize];
    let new = Box::new(Stream {
        dsp,
        frames: desc.frames,
        channels: desc.channels,
        kind,
        ir,
        history,
        scratch: Vec::new(),
        status: Box::new(Status {
            submitted: 0,
            completed: 0,
            reserved: 0,
            result: S_OK,
        }),
    });
    // SAFETY: checked non-null.
    unsafe {
        status.write(&*new.status);
        stream.write(Box::into_raw(new));
    }
    S_OK
}

pub extern "system" fn XDspDeactivate(stream: *mut Stream) -> HRESULT {
    if stream.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `XDspActivate`; deactivated once.
    let stream = unsafe { Box::from_raw(stream) };
    // SAFETY: the connection outlives its streams.
    let dsp = unsafe { &*stream.dsp };
    *dsp.active.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
    S_OK
}

/// Completes the command at once: a convolution stream with an input gets
/// the synthetic reverb tail (see module docs); everything else stays
/// silence.
pub extern "system" fn XDspSubmitCommand(
    stream: *mut Stream,
    command: *const Command,
    ticket: *mut u32,
) -> HRESULT {
    if stream.is_null() || command.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a stream from `XDspActivate`, the title's command.
    let (stream, command) = unsafe { (&mut *stream, &*command) };
    if !command.output.is_null() {
        let samples = (stream.frames * stream.channels) as usize;
        if guess_dsp() && stream.kind == CONVOLUTION && !command.input.is_null() {
            let hist_frames = stream.history.len() / stream.channels as usize;
            stream.scratch.clear();
            stream.scratch.extend_from_slice(&stream.history);
            // SAFETY: the title's input buffer holds one block of the
            // stream. This borrow ends with `extend_from_slice`, before
            // `out` below, in case the two pointers alias.
            stream
                .scratch
                .extend_from_slice(unsafe { std::slice::from_raw_parts(command.input, samples) });
            // SAFETY: the title's output buffer holds one block of the stream.
            let out = unsafe { std::slice::from_raw_parts_mut(command.output, samples) };
            convolve_merged(
                &stream.scratch,
                hist_frames,
                stream.channels as usize,
                &stream.ir,
                command.gains,
                out,
            );
            let keep = stream.history.len();
            let tail_start = stream.scratch.len() - keep;
            stream
                .history
                .copy_from_slice(&stream.scratch[tail_start..]);
        } else {
            // SAFETY: the title's output buffer holds one block of the stream.
            unsafe { std::ptr::write_bytes(command.output, 0, samples) };
        }
    }
    let status = &mut *stream.status;
    let next = status.submitted.wrapping_add(1);
    // SAFETY: the status block is read by title code on other threads.
    unsafe {
        std::ptr::write_volatile(&mut status.submitted, next);
        std::ptr::write_volatile(&mut status.result, S_OK);
        std::ptr::write_volatile(&mut status.completed, next);
    }
    if !ticket.is_null() {
        // SAFETY: optional out parameter.
        unsafe { ticket.write(next) };
    }
    S_OK
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "dsp",
        dlls: &["chad.dll"],
        exports: vec![
            hle_export!(XDspConnectWithMaximumStreamLimit),
            hle_export!(XDspDisconnect),
            hle_export!(XDspActivate),
            hle_export!(XDspDeactivate),
            hle_export!(XDspSubmitCommand),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convolution_stream_as_the_title_drives_it() {
        // SAFETY: no other test reads VICTORY_GUESS_DSP.
        unsafe { std::env::set_var("VICTORY_GUESS_DSP", "1") };
        let mut base = vec![0u8; 0x10_0000];
        let mut dsp = std::ptr::null_mut();
        assert_eq!(
            XDspConnectWithMaximumStreamLimit(
                base.as_mut_ptr().cast(),
                0x10_0000,
                24,
                0x80,
                1,
                &mut dsp
            ),
            S_OK
        );
        let ir = vec![0f32; 0x800];
        // Stereo convolution with option 2, as the title asks for.
        let desc = StreamDesc {
            kind: CONVOLUTION,
            options: 2,
            frames: 0x400,
            channels: 2,
            ir_floats: 0x800,
            ir: ir.as_ptr(),
        };
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);
        let mut output = vec![1f32; 0x800];
        let input = vec![0.5f32; 0x800];
        let command = Command {
            gains: [1.0, 1.0],
            input: input.as_ptr(),
            output: output.as_mut_ptr(),
            reserved: 0,
        };
        let mut ticket = 0;
        assert_eq!(XDspSubmitCommand(stream, &command, &mut ticket), S_OK);
        // SAFETY: the stream's status block.
        let (completed, result) = unsafe { ((*status).completed, (*status).result) };
        assert_eq!((completed, result), (ticket, S_OK));
        // A convolution stream with real input no longer returns digital
        // silence: it gets the synthetic reverb tail.
        assert!(output.iter().any(|&s| s != 0.0));
        assert_ne!(output, vec![1f32; 0x800]);
        assert_eq!(XDspDeactivate(stream), S_OK);

        // Mono streams refuse stereo options; transforms are mono only.
        let bad = StreamDesc {
            channels: 1,
            ..desc
        };
        assert_eq!(
            XDspActivate(dsp, &bad, &mut status, &mut stream),
            E_INVALIDARG
        );
        let transform = StreamDesc {
            kind: 2,
            options: 0,
            channels: 1,
            ir_floats: 0,
            ir: std::ptr::null(),
            ..desc
        };
        assert_eq!(
            XDspActivate(dsp, &transform, &mut status, &mut stream),
            S_OK
        );
        assert_eq!(XDspDeactivate(stream), S_OK);
        assert_eq!(XDspDisconnect(dsp), S_OK);
        // SAFETY: set at the top of this test; nothing else reads it.
        unsafe { std::env::remove_var("VICTORY_GUESS_DSP") };
    }

    #[test]
    fn convolve_merged_is_a_causal_fir_per_channel_scaled_by_gain() {
        // Stereo, no history (hist_frames 0), 3 frames/channel: interleaved
        // L0 R0 L1 R1 L2 R2.
        let merged = [1.0, 2.0, 0.0, 0.0, 0.0, 0.0];
        let ir = [1.0, 0.5, 0.25];
        let mut out = [0.0; 6];
        convolve_merged(&merged, 0, 2, &ir, [2.0, 3.0], &mut out);
        // Left channel is [1,0,0] convolved with ir, times gain 2: [1, 0.5, 0.25] * 2.
        // Right channel is [2,0,0] convolved with ir, times gain 3: [2, 1, 0.5] * 3.
        assert_eq!(out, [2.0, 6.0, 1.0, 3.0, 0.5, 1.5]);
    }

    #[test]
    fn convolve_merged_reads_history_as_the_frames_before_the_block() {
        // Mono, one frame of history (5.0), then the block [0.0, 0.0].
        let merged = [5.0, 0.0, 0.0];
        let ir = [1.0, 0.5, 0.25];
        let mut out = [0.0; 2];
        convolve_merged(&merged, 1, 1, &ir, [1.0, 0.0], &mut out);
        // n=0 reaches one frame back into history: 0*1 + 5*0.5 = 2.5.
        // n=1 reaches two frames back: 0*1 + 0*0.5 + 5*0.25 = 1.25.
        assert_eq!(out, [2.5, 1.25]);
    }

    #[test]
    fn a_block_split_matches_one_convolution_over_the_whole_signal() {
        // The point of carrying history across calls: splitting one signal
        // into two blocks and convolving them in sequence, carrying the
        // tail, must give the same result as convolving it in one go. This
        // is the property that stops a reverb tail from clicking at every
        // block boundary.
        let ir = synthetic_ir(5);
        let channels = 1;
        let whole = [1.0, 0.3, -0.2, 0.0, 0.5, 0.1, -0.4, 0.2];
        let gains = [1.0, 0.0];

        let mut reference = [0.0; 8];
        convolve_merged(&whole, 0, channels, &ir, gains, &mut reference);

        let (first, second) = whole.split_at(4);
        let mut history = vec![0.0; ir.len() - 1];
        let mut out1 = [0.0; 4];
        let mut scratch: Vec<f32> = history.clone();
        scratch.extend_from_slice(first);
        convolve_merged(&scratch, history.len(), channels, &ir, gains, &mut out1);
        let keep = history.len();
        history.copy_from_slice(&scratch[scratch.len() - keep..]);

        let mut out2 = [0.0; 4];
        scratch.clear();
        scratch.extend_from_slice(&history);
        scratch.extend_from_slice(second);
        convolve_merged(&scratch, history.len(), channels, &ir, gains, &mut out2);

        let streamed: Vec<f32> = out1.iter().chain(out2.iter()).copied().collect();
        assert_eq!(streamed, reference);
    }

    #[test]
    fn synthetic_ir_is_deterministic_and_decays() {
        let a = synthetic_ir(16);
        let b = synthetic_ir(16);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        // The tail dies out: the back half carries much less energy than the front half.
        let front: f32 = a[..8].iter().map(|s| s * s).sum();
        let back: f32 = a[8..].iter().map(|s| s * s).sum();
        assert!(back < front);
    }
}
