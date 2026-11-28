//! The XAudio2 mixer: what the engine's voices add up to, as 32-bit float
//! frames for an output device. No device, no COM, no title code in here, so
//! it is checked with synthetic tones.
//!
//! Source voices queue buffers (decoded to float when submitted, so the
//! title's memory is free again at once) and play them, resampled by linear
//! interpolation to the device rate, with their loops, volume, per-channel
//! volumes and output matrix. A voice's sends lead to the mastering voice or
//! through submix voices, which here are gain groups: they carry a volume and
//! an output, not their own mixing, effects or rate. A voice whose send chain
//! does not reach the mastering voice (a reverb or DSP bus the host does not
//! mix, an unconnected submix) is never dropped for that: it passes its dry
//! signal straight to the mastering voice at full gain, so dialogue routed
//! through an effect bus stays audible while the effect itself is silent.
//!
//! shortcut: no effect chains, no filters, no multiple sends per voice (the
//! first send counts), and a submix has no channels or rate of its own; add
//! them when a title is seen using them.

use std::collections::{HashMap, VecDeque};

/// Voice ids; the mastering voice is [`MASTER`].
pub type VoiceId = u32;
pub const MASTER: VoiceId = 0;

/// `XAUDIO2_LOOP_INFINITE`.
pub const LOOP_INFINITE: u32 = 255;

/// How the title's PCM is laid out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Format {
    pub channels: usize,
    pub rate: u32,
    pub bits: u16,
    pub float: bool,
}

impl Format {
    pub fn frame_bytes(&self) -> usize {
        self.channels * usize::from(self.bits / 8)
    }
}

/// The title's bytes as interleaved floats, or `None` for a layout we do not
/// decode.
pub fn decode(format: Format, bytes: &[u8]) -> Option<Vec<f32>> {
    let width = usize::from(format.bits / 8);
    let samples = bytes.chunks_exact(width);
    Some(match (format.float, format.bits) {
        (true, 32) => samples
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
        (false, 8) => samples.map(|b| (f32::from(b[0]) - 128.0) / 128.0).collect(),
        (false, 16) => samples
            .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
            .collect(),
        (false, 24) => samples
            .map(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0)
            .collect(),
        (false, 32) => samples
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0)
            .collect(),
        _ => return None,
    })
}

/// What the title is told, once the mixing lock is let go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    BufferStart,
    BufferEnd,
    LoopEnd,
    StreamEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub voice: VoiceId,
    pub notice: Notice,
    /// The buffer's `pContext`.
    pub context: usize,
}

pub struct Buffer {
    /// Interleaved, `channels` per frame.
    pub samples: Vec<f32>,
    /// The frames played: `[begin, end)`.
    pub begin: usize,
    pub end: usize,
    /// The loop, `[loop_begin, loop_end)`, repeated `loops` times.
    pub loop_begin: usize,
    pub loop_end: usize,
    pub loops: u32,
    pub context: usize,
    pub end_of_stream: bool,
}

pub struct Source {
    id: VoiceId,
    pub channels: usize,
    pub rate: u32,
    queue: VecDeque<Buffer>,
    /// In frames of the front buffer.
    position: f64,
    announced: bool,
    pub running: bool,
    pub volume: f32,
    pub channel_volumes: Vec<f32>,
    /// `levels[out * channels + source]`.
    pub matrix: Option<Vec<f32>>,
    pub ratio: f32,
    pub played: u64,
    pub output: Option<VoiceId>,
}

impl Source {
    /// Queues a buffer.
    pub fn submit(&mut self, buffer: Buffer) {
        if self.queue.is_empty() {
            self.position = buffer.begin as f64;
            self.announced = false;
        }
        self.queue.push_back(buffer);
    }

    /// Drops the queued buffers but the one playing (all of them when the
    /// voice is stopped), as `FlushSourceBuffers`: they end as played.
    pub fn flush(&mut self) -> Vec<Event> {
        let keep = usize::from(self.running && !self.queue.is_empty());
        self.queue
            .drain(keep..)
            .map(|b| Event {
                voice: self.id,
                notice: Notice::BufferEnd,
                context: b.context,
            })
            .collect()
    }

    fn step(&self, out_rate: u32) -> f64 {
        f64::from(self.ratio) * f64::from(self.rate) / f64::from(out_rate)
    }

    /// Source frames still needed to play this many output frames.
    pub fn frames_required(&self, mut frames: usize, out_rate: u32) -> usize {
        if !self.running {
            return 0;
        }
        let step = self.step(out_rate);
        for (i, b) in self.queue.iter().enumerate() {
            let position = if i == 0 {
                self.position
            } else {
                b.begin as f64
            };
            if b.loops == LOOP_INFINITE {
                return 0;
            }
            let end = if b.loops == 0 {
                b.end as f64
            } else {
                b.end.max(b.loop_begin) as f64
                    + (b.loop_end - b.loop_begin) as f64 * f64::from(b.loops)
            };
            frames = frames.saturating_sub(((end - position) / step).ceil() as usize);
            if frames == 0 {
                return 0;
            }
        }
        (frames as f64 * step).ceil() as usize
    }

    /// `ExitLoop`: the front buffer plays on to its end.
    pub fn exit_loop(&mut self) {
        if let Some(b) = self.queue.front_mut() {
            b.loops = 0;
        }
    }

    /// The context of the buffer playing, the number queued, and the frames
    /// played so far.
    pub fn state(&self) -> (usize, u32, u64) {
        let into_front = self
            .queue
            .front()
            .map_or(0, |b| (self.position as usize).saturating_sub(b.begin));
        (
            self.queue.front().map_or(0, |b| b.context),
            self.queue.len() as u32,
            self.played + into_front as u64,
        )
    }
}

pub struct Submix {
    pub volume: f32,
    pub output: Option<VoiceId>,
}

pub struct Mixer {
    pub rate: u32,
    pub channels: usize,
    pub volume: f32,
    sources: HashMap<VoiceId, Source>,
    submixes: HashMap<VoiceId, Submix>,
    next: VoiceId,
}

impl Mixer {
    pub fn new(rate: u32, channels: usize) -> Self {
        Mixer {
            rate,
            channels,
            volume: 1.0,
            sources: HashMap::new(),
            submixes: HashMap::new(),
            next: 1,
        }
    }

    pub fn add_source(&mut self, channels: usize, rate: u32, output: Option<VoiceId>) -> VoiceId {
        let id = self.next;
        self.next += 1;
        self.sources.insert(
            id,
            Source {
                id,
                channels,
                rate,
                queue: VecDeque::new(),
                position: 0.0,
                announced: false,
                running: false,
                volume: 1.0,
                channel_volumes: vec![1.0; channels],
                matrix: None,
                ratio: 1.0,
                played: 0,
                output,
            },
        );
        id
    }

    pub fn add_submix(&mut self, output: Option<VoiceId>) -> VoiceId {
        let id = self.next;
        self.next += 1;
        self.submixes.insert(
            id,
            Submix {
                volume: 1.0,
                output,
            },
        );
        id
    }

    pub fn remove(&mut self, id: VoiceId) {
        self.sources.remove(&id);
        self.submixes.remove(&id);
    }

    pub fn source(&mut self, id: VoiceId) -> Option<&mut Source> {
        self.sources.get_mut(&id)
    }

    pub fn submix(&mut self, id: VoiceId) -> Option<&mut Submix> {
        self.submixes.get_mut(&id)
    }

    /// The gain from a voice's output to the speakers (0 if the chain does
    /// not end at the mastering voice).
    fn chain_gain(&self, mut to: Option<VoiceId>) -> f32 {
        let mut gain = 1.0;
        for _ in 0..8 {
            match to {
                Some(MASTER) => return gain * self.volume,
                Some(id) => match self.submixes.get(&id) {
                    Some(s) => {
                        gain *= s.volume;
                        to = s.output;
                    }
                    None => return 0.0,
                },
                None => return 0.0,
            }
        }
        0.0
    }

    /// Adds `out.len() / channels` frames of every running voice into `out`
    /// (which is cleared first), and returns what happened to the buffers.
    ///
    /// A voice whose send chain does not reach the mastering voice is mixed
    /// dry at unity instead of being dropped: the reverb and DSP buses the
    /// host cannot yet compute must not take the dialogue with them.
    pub fn render(&mut self, out: &mut [f32]) -> Vec<Event> {
        out.fill(0.0);
        let frames = out.len() / self.channels;
        let mut events = Vec::new();
        let gains: HashMap<VoiceId, f32> = self
            .sources
            .iter()
            .map(|(&id, s)| {
                let chained = self.chain_gain(s.output);
                (id, if chained == 0.0 { 1.0 } else { chained })
            })
            .collect();
        for (id, s) in &mut self.sources {
            if s.running && !s.queue.is_empty() {
                mix(
                    s,
                    gains[id],
                    self.rate,
                    self.channels,
                    out,
                    frames,
                    &mut events,
                );
            }
        }
        events
    }

    /// The voices running with buffers queued, and the buffers waiting among them: what a
    /// log line needs to say whether the graph is carrying anything.
    pub fn running(&self) -> (usize, usize) {
        (
            self.sources
                .values()
                .filter(|s| s.running && !s.queue.is_empty())
                .count(),
            self.sources
                .values()
                .map(|s| if s.running { s.queue.len() } else { 0 })
                .sum(),
        )
    }
}

/// The down-mix gain of source channel `c` into the left and right of a
/// stereo mix: the Windows channel order (front left and right, front
/// center, LFE, then back and side surrounds). The front center — where a
/// game's dialogue lives — feeds both at 0.707, the surrounds fold into their
/// own side, and the LFE is kept at half gain (a subwoofer channel a stereo
/// pair cannot place, but that carries bass the mix would otherwise lose).
fn surround_gains(c: usize) -> (f32, f32) {
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;
    match c {
        0 => (1.0, 0.0), // front left
        1 => (0.0, 1.0), // front right
        2 => (H, H),     // front center: dialogue
        3 => (0.5, 0.5), // LFE
        4 => (H, 0.0),   // back left
        5 => (0.0, H),   // back right
        6 => (H, 0.0),   // side left
        7 => (0.0, H),   // side right
        _ => (H, H),     // height and beyond: the middle
    }
}

/// The levels from `source` channels to `out` channels: an identity where
/// they line up, a mono source to every output, a stereo one to a mono
/// output halved, and a surround source (three channels or more: 5.1, 7.1)
/// folded down with its front center — the dialogue channel — mixed into
/// left and right rather than dropped.
pub fn default_matrix(source: usize, out: usize) -> Vec<f32> {
    let mut m = vec![0.0; source * out];
    for o in 0..out {
        for c in 0..source {
            m[o * source + c] = match (source, out) {
                (1, _) => 1.0,
                (2, 1) => 0.5,
                (_, 1) => {
                    // Surround to mono: the center dominates, the fronts halve
                    // as the stereo pair does, the rest fold at half gain.
                    match c {
                        2 => 1.0,
                        0 | 1 => std::f32::consts::FRAC_1_SQRT_2,
                        _ => 0.5,
                    }
                }
                _ if source > 2 => {
                    let (l, r) = surround_gains(c);
                    if o == 0 { l } else { r }
                }
                _ => f32::from(o == c),
            };
        }
    }
    m
}

fn mix(
    s: &mut Source,
    gain: f32,
    out_rate: u32,
    out_channels: usize,
    out: &mut [f32],
    frames: usize,
    events: &mut Vec<Event>,
) {
    let matrix = s
        .matrix
        .clone()
        .filter(|m| m.len() == s.channels * out_channels)
        .unwrap_or_else(|| default_matrix(s.channels, out_channels));
    let step = s.step(out_rate);
    let mut frame = 0;
    while frame < frames && settle(s, events) {
        let c = s.channels;
        let b = s.queue.front().unwrap();
        let i = s.position as usize;
        let t = (s.position - i as f64) as f32;
        let last = b.end.saturating_sub(1);
        let (a, z) = (i.min(last), (i + 1).min(last));
        for o in 0..out_channels {
            let mut sum = 0.0;
            for ch in 0..c {
                let level = matrix[o * c + ch] * s.channel_volumes.get(ch).copied().unwrap_or(1.0);
                if level != 0.0 {
                    let x0 = b.samples[a * c + ch];
                    let x1 = b.samples[z * c + ch];
                    sum += (x0 + (x1 - x0) * t) * level;
                }
            }
            out[frame * out_channels + o] += sum * s.volume * gain;
        }
        s.position += step;
        frame += 1;
    }
    // A buffer used up by the last frame ends now, not a pass later.
    settle(s, events);
}

/// Announces the front buffer, loops it or ends it, until a frame can be
/// read from it; false when nothing is left to play.
fn settle(s: &mut Source, events: &mut Vec<Event>) -> bool {
    loop {
        let Some(b) = s.queue.front_mut() else {
            return false;
        };
        if !s.announced {
            s.announced = true;
            events.push(Event {
                voice: s.id,
                notice: Notice::BufferStart,
                context: b.context,
            });
        }
        let limit = if b.loops > 0 { b.loop_end } else { b.end } as f64;
        if s.position < limit {
            return true;
        }
        if b.loops > 0 && b.loop_end > b.loop_begin {
            let span = b.loop_end - b.loop_begin;
            s.position -= span as f64;
            s.played += span as u64;
            if b.loops != LOOP_INFINITE {
                b.loops -= 1;
            }
            events.push(Event {
                voice: s.id,
                notice: Notice::LoopEnd,
                context: b.context,
            });
            continue;
        }
        let done = s.queue.pop_front().unwrap();
        s.played += (done.end - done.begin) as u64;
        events.push(Event {
            voice: s.id,
            notice: Notice::BufferEnd,
            context: done.context,
        });
        match s.queue.front() {
            Some(next) => {
                s.position = next.begin as f64;
                s.announced = false;
            }
            None => {
                if done.end_of_stream {
                    events.push(Event {
                        voice: s.id,
                        notice: Notice::StreamEnd,
                        context: 0,
                    });
                }
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// `seconds` of a sine, as the title would hand it over.
    fn tone(freq: f32, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (i as f32 * freq * std::f32::consts::TAU / rate as f32).sin() * 0.5)
            .collect()
    }

    fn buffer(samples: Vec<f32>, channels: usize, context: usize) -> Buffer {
        let frames = samples.len() / channels;
        Buffer {
            samples,
            begin: 0,
            end: frames,
            loop_begin: 0,
            loop_end: 0,
            loops: 0,
            context,
            end_of_stream: false,
        }
    }

    /// Upward zero crossings, which for a sine count its periods.
    fn periods(left: impl Iterator<Item = f32>) -> usize {
        let v: Vec<f32> = left.collect();
        v.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count()
    }

    #[test]
    fn pass_shortfall_counts_play_regions_loops_and_the_front_cursor() {
        let mut mixer = Mixer::new(RATE, 1);
        let voice = mixer.add_source(1, RATE, Some(MASTER));
        let source = mixer.source(voice).unwrap();
        let mut front = buffer(vec![1.0; 10], 1, 0);
        front.begin = 2;
        front.loop_begin = 3;
        front.loop_end = 5;
        front.loops = 2;
        source.submit(front);
        let mut next = buffer(vec![1.0; 6], 1, 1);
        next.begin = 4;
        source.submit(next);
        assert_eq!(source.frames_required(16, RATE), 0); // Stopped.
        source.running = true;
        assert_eq!(source.frames_required(16, RATE), 2);
        mixer.render(&mut [0.0; 4]);
        let source = mixer.source(voice).unwrap();
        assert_eq!(source.frames_required(16, RATE), 6);
        source.queue.front_mut().unwrap().loops = LOOP_INFINITE;
        assert_eq!(source.frames_required(16, RATE), 0);
    }

    #[test]
    fn a_synthetic_tone_comes_out_of_the_mixer() {
        let mut m = Mixer::new(RATE, 2);
        let v = m.add_source(1, RATE, Some(MASTER));
        // 100 ms of 440 Hz mono 16-bit, through the title's own byte layout.
        let pcm: Vec<u8> = tone(440.0, RATE, 4800)
            .iter()
            .flat_map(|x| ((x * 32767.0) as i16).to_le_bytes())
            .collect();
        let format = Format {
            channels: 1,
            rate: RATE,
            bits: 16,
            float: false,
        };
        assert_eq!(format.frame_bytes(), 2);
        let samples = decode(format, &pcm).unwrap();
        assert_eq!(samples.len(), 4800);
        m.source(v).unwrap().submit(buffer(samples, 1, 7));
        // Not started: silence.
        let mut out = vec![1.0; 480 * 2];
        assert!(m.render(&mut out).is_empty());
        assert!(out.iter().all(|&x| x == 0.0));
        m.source(v).unwrap().running = true;
        let mut all = Vec::new();
        let mut events = Vec::new();
        for _ in 0..10 {
            events.extend(m.render(&mut out));
            all.extend_from_slice(&out);
        }
        // Mono to both channels, 0.5 peak, 44 periods in 100 ms.
        let left = || all.iter().step_by(2).copied();
        let right = || all.iter().skip(1).step_by(2).copied();
        assert!(left().zip(right()).all(|(l, r)| l == r));
        let peak = left().fold(0.0f32, |a, x| a.max(x.abs()));
        assert!((0.49..=0.51).contains(&peak), "peak {peak}");
        assert_eq!(periods(left()), 44);
        // Told when it began and when it was used up.
        let notices: Vec<_> = events.iter().map(|e| (e.notice, e.context)).collect();
        assert_eq!(notices, [(Notice::BufferStart, 7), (Notice::BufferEnd, 7)]);
        assert_eq!(m.source(v).unwrap().state(), (0, 0, 4800));
    }

    #[test]
    fn rate_volume_loops_and_submixes_shape_the_sound() {
        let mut m = Mixer::new(RATE, 2);
        // A 24 kHz voice is played at twice its length in frames.
        let slow = m.add_source(1, 24_000, Some(MASTER));
        m.source(slow)
            .unwrap()
            .submit(buffer(tone(440.0, 24_000, 2400), 1, 1));
        m.source(slow).unwrap().running = true;
        let mut out = vec![0.0; 2 * 4800];
        let events = m.render(&mut out);
        let nonzero = out.iter().step_by(2).filter(|&&x| x != 0.0).count();
        assert!((4790..=4800).contains(&nonzero), "{nonzero}");
        assert_eq!(periods(out.iter().step_by(2).copied()), 44);
        assert!(events.iter().any(|e| e.notice == Notice::BufferEnd));

        // A looped buffer repeats and says so; a send to a submix at half
        // volume halves it again, and the master volume does too.
        let mut m = Mixer::new(RATE, 2);
        let sub = m.add_submix(Some(MASTER));
        m.submix(sub).unwrap().volume = 0.5;
        m.volume = 0.5;
        let v = m.add_source(1, RATE, Some(sub));
        let mut b = buffer(vec![1.0; 100], 1, 3);
        b.loop_end = 100;
        b.loops = 2;
        b.end_of_stream = true;
        m.source(v).unwrap().submit(b);
        m.source(v).unwrap().running = true;
        let mut out = vec![0.0; 2 * 400];
        let events = m.render(&mut out);
        // Three passes of 100 frames at 0.25, then nothing.
        assert!(out[..600].iter().all(|&x| (x - 0.25).abs() < 1e-6));
        assert!(out[600..].iter().all(|&x| x == 0.0));
        let notices: Vec<_> = events.iter().map(|e| e.notice).collect();
        assert_eq!(
            notices,
            [
                Notice::BufferStart,
                Notice::LoopEnd,
                Notice::LoopEnd,
                Notice::BufferEnd,
                Notice::StreamEnd
            ]
        );
        assert!(events.iter().all(|e| e.voice == v));

        // An output matrix and channel volumes: left only, quietly.
        let mut m = Mixer::new(RATE, 2);
        let v = m.add_source(2, RATE, Some(MASTER));
        let s = m.source(v).unwrap();
        s.matrix = Some(vec![1.0, 0.0, 0.0, 0.0]); // out 0 takes source 0
        s.channel_volumes = vec![0.5, 1.0];
        s.running = true;
        s.submit(buffer(vec![1.0; 2 * 10], 2, 0));
        let mut out = vec![0.0; 2 * 4];
        m.render(&mut out);
        assert_eq!(out, [0.5, 0.0, 0.5, 0.0, 0.5, 0.0, 0.5, 0.0]);
    }

    #[test]
    fn flushing_and_ending_a_loop() {
        let mut m = Mixer::new(RATE, 1);
        let v = m.add_source(1, RATE, Some(MASTER));
        let s = m.source(v).unwrap();
        for ctx in 1..=3 {
            s.submit(buffer(vec![0.1; 10], 1, ctx));
        }
        // Stopped: everything goes.
        let flushed = s.flush();
        assert_eq!(
            flushed.iter().map(|e| e.context).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        for ctx in 1..=3 {
            s.submit(buffer(vec![0.1; 10], 1, ctx));
        }
        s.running = true;
        // Running: the one playing stays.
        let flushed = s.flush();
        assert_eq!(
            flushed.iter().map(|e| e.context).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(s.state().1, 1);
        let mut looped = buffer(vec![0.2; 10], 1, 9);
        looped.loop_end = 10;
        looped.loops = LOOP_INFINITE;
        s.flush();
        s.queue.clear();
        s.submit(looped);
        s.exit_loop();
        let mut out = vec![0.0; 30];
        let events = m.render(&mut out);
        assert!(
            events
                .iter()
                .any(|e| e.notice == Notice::BufferEnd && e.context == 9)
        );
        assert!(!events.iter().any(|e| e.notice == Notice::LoopEnd));
    }

    #[test]
    fn pcm_decodes_in_every_width() {
        let f = |bits, float| Format {
            channels: 1,
            rate: RATE,
            bits,
            float,
        };
        assert_eq!(
            decode(f(8, false), &[0, 128, 255]).unwrap(),
            [-1.0, 0.0, 127.0 / 128.0]
        );
        assert_eq!(
            decode(f(16, false), &[0, 0x80, 0xFF, 0x7F]).unwrap()[0],
            -1.0
        );
        assert_eq!(decode(f(24, false), &[0, 0, 0x40]).unwrap(), [0.5]);
        assert_eq!(decode(f(32, false), &[0, 0, 0, 0x80]).unwrap(), [-1.0]);
        assert_eq!(decode(f(32, true), &0.25f32.to_le_bytes()).unwrap(), [0.25]);
        assert!(decode(f(12, false), &[0, 0]).is_none());
    }

    #[test]
    fn played_samples_stay_monotonic_and_accurate_across_loops() {
        let mut m = Mixer::new(RATE, 1);
        let v = m.add_source(1, RATE, Some(MASTER));
        let mut b = buffer(vec![0.5; 100], 1, 42);
        b.loop_begin = 20;
        b.loop_end = 70;
        b.loops = 2;
        m.source(v).unwrap().submit(b);
        m.source(v).unwrap().running = true;

        let mut prev = 0u64;
        let mut out = [0.0; 25];
        for _ in 0..10 {
            m.render(&mut out);
            let played = m.source(v).unwrap().state().2;
            assert!(
                played >= prev,
                "samples played regressed: {prev} -> {played}"
            );
            prev = played;
        }
        // 100 base frames + 2 loops of 50 frames = 200 frames total.
        assert_eq!(m.source(v).unwrap().state(), (0, 0, 200));
    }

    /// The Windows 5.1 layout: front left and right, front center, LFE, back
    /// left and right.
    const H: f32 = std::f32::consts::FRAC_1_SQRT_2;

    #[test]
    fn surround_downmix_carries_the_center_dialogue_channel() {
        // The matrix of a 5.1 voice into stereo: rows are outputs, columns
        // the source channels in Windows order (FL, FR, FC, LFE, BL, BR).
        let m = default_matrix(6, 2);
        let row = |o: usize| m[o * 6..o * 6 + 6].to_vec();
        assert_eq!(
            row(0),
            [1.0, 0.0, H, 0.5, H, 0.0],
            "left takes FL, the center at 0.707, LFE at 0.5, back left at 0.707"
        );
        assert_eq!(
            row(1),
            [0.0, 1.0, H, 0.5, 0.0, H],
            "right takes FR, the center at 0.707, LFE at 0.5, back right at 0.707"
        );
        // 7.1 adds side left and right (channels 6 and 7) to their own sides.
        let m = default_matrix(8, 2);
        assert_eq!(m[6], H, "side left feeds left");
        assert_eq!(m[7], 0.0, "side right does not");
        assert_eq!(m[8 + 6], 0.0, "side left does not feed right");
        assert_eq!(m[8 + 7], H, "side right feeds right");
        assert_eq!(m[8 + 2], H, "the 7.1 center still feeds right");
        // Stereo and mono sources keep their plain matrices.
        assert_eq!(default_matrix(2, 2), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(default_matrix(1, 2), [1.0, 1.0]);
        assert_eq!(default_matrix(2, 1), [0.5, 0.5]);
        // A surround voice into mono keeps its center above everything else.
        let m = default_matrix(6, 1);
        assert_eq!(m[2], 1.0, "the center dominates mono");
        assert_eq!(m[0], H, "front left folds at 0.707");
    }

    #[test]
    fn a_51_dialogue_voice_is_audible_in_stereo() {
        // What the title actually does: a six-channel voice (dialogue on the
        // center, channel 2) into the stereo mastering voice. With the old
        // identity matrix the center never reached a speaker.
        let mut m = Mixer::new(RATE, 2);
        let v = m.add_source(6, RATE, Some(MASTER));
        // One frame: FL=0, FR=0, FC=1, LFE=0, BL=0, BR=0 — pure center.
        m.source(v)
            .unwrap()
            .submit(buffer(vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0], 6, 0));
        m.source(v).unwrap().running = true;
        let mut out = vec![0.0; 2];
        m.render(&mut out);
        assert!(
            (out[0] - H).abs() < 1e-6,
            "left carries the center at 0.707, not {}",
            out[0]
        );
        assert!(
            (out[1] - H).abs() < 1e-6,
            "right carries the center at 0.707, not {}",
            out[1]
        );
        // A back-left-only frame stays out of the right speaker.
        m.source(v)
            .unwrap()
            .submit(buffer(vec![0.0, 0.0, 0.0, 0.0, 1.0, 0.0], 6, 1));
        m.source(v).unwrap().running = true;
        let mut out = vec![0.0; 2];
        m.render(&mut out);
        assert!((out[0] - H).abs() < 1e-6, "back left folds into left");
        assert_eq!(out[1], 0.0, "back left never reaches right");
    }

    #[test]
    fn a_voice_into_a_dead_submix_passes_its_dry_signal_to_the_master() {
        // A reverb bus the host does not mix: a submix whose own output goes
        // nowhere (an empty send list, or a chain that never reaches the
        // mastering voice). The voice into it must still be heard.
        let mut m = Mixer::new(RATE, 2);
        let dead = m.add_submix(None);
        let v = m.add_source(1, RATE, Some(dead));
        m.source(v).unwrap().submit(buffer(vec![0.5; 10], 1, 0));
        m.source(v).unwrap().running = true;
        let mut out = vec![0.0; 2 * 4];
        m.render(&mut out);
        assert!(
            out.iter().all(|&x| (x - 0.5).abs() < 1e-6),
            "dry at unity, not silence"
        );

        // A chain that runs out before the mastering voice (a submix loop)
        // must not hang or zero the voice either.
        let mut m = Mixer::new(RATE, 1);
        let a = m.add_submix(None);
        let b = m.add_submix(Some(a));
        m.submix(a).unwrap().output = Some(b);
        let v = m.add_source(1, RATE, Some(a));
        m.source(v).unwrap().submit(buffer(vec![0.25; 4], 1, 0));
        m.source(v).unwrap().running = true;
        let mut out = vec![0.0; 4];
        m.render(&mut out);
        assert!(
            out.iter().all(|&x| (x - 0.25).abs() < 1e-6),
            "a looping chain falls back to dry"
        );

        // A live chain still applies its submix volume: the dry fallback is
        // only for chains that reach nothing.
        let mut m = Mixer::new(RATE, 1);
        let sub = m.add_submix(Some(MASTER));
        m.submix(sub).unwrap().volume = 0.5;
        let v = m.add_source(1, RATE, Some(sub));
        m.source(v).unwrap().submit(buffer(vec![1.0; 4], 1, 0));
        m.source(v).unwrap().running = true;
        let mut out = vec![0.0; 4];
        m.render(&mut out);
        assert!(
            out.iter().all(|&x| (x - 0.5).abs() < 1e-6),
            "the live submix still halves"
        );
    }

    #[test]
    fn an_infinitely_looped_buffer_plays_forever_and_announces_its_loops() {
        // The title screen's music: one buffer, LoopCount = 255 (infinite). It
        // must still be playing minutes later, and every pass over the loop
        // region fires OnLoopEnd so the title can resubmit around it.
        let mut m = Mixer::new(RATE, 2);
        let v = m.add_source(1, RATE, Some(MASTER));
        let mut b = buffer(vec![0.5; 100], 1, 7);
        b.loop_begin = 10;
        b.loop_end = 90;
        b.loops = LOOP_INFINITE;
        m.source(v).unwrap().submit(b);
        m.source(v).unwrap().running = true;

        let mut out = vec![0.0; 2 * 100];
        let mut loops = 0;
        let mut silence_after_sound = false;
        // Far longer than the buffer: eighty passes over the loop region.
        for _ in 0..80 {
            out.fill(0.0);
            let events = m.render(&mut out);
            loops += events
                .iter()
                .filter(|e| e.notice == Notice::LoopEnd)
                .count();
            let sounded = out.iter().any(|&x| x != 0.0);
            if !sounded && loops > 0 {
                silence_after_sound = true;
            }
            assert!(sounded, "the looped buffer is still playing");
        }
        assert!(
            loops >= 80,
            "every pass over the loop region announces itself: {loops}"
        );
        assert!(!silence_after_sound);
        // Its state never reports the queue empty.
        assert_eq!(m.source(v).unwrap().state().1, 1, "the buffer stays queued");
        // An infinite loop never needs more data submitted.
        assert_eq!(m.source(v).unwrap().frames_required(100, RATE), 0);
    }
}
