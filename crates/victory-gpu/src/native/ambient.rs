//! `VICTORY_AMBIENT=<level>`: a guaranteed-visible fallback for presents (a debug aid, off unless
//! asked for).
//!
//! While the lighting compute passes are still refused, a fully drawn G-buffer reaches the
//! swap chain as pitch black after tone-mapping, and nothing on the screen shows the world was
//! rendered at all. With the switch set, each present shades the near-black pixels of the
//! presented picture by the frame's most-drawn depth target (`level` as the constant ambient,
//! the depth as the shade: nearer is brighter, the console's reversed Z), so the 3D geometry is
//! immediately visible while the lighting shaders are being perfected. Pixels the title lit stay
//! as they are (the overlay, the menus, the movie), and the switch changes no work the host
//! records for the title: the shading happens in the present pass alone.
//!
//! The shading also blends the frame's most-drawn *colour* target in as the albedo (the
//! G-buffer's own surface colours, when a pass drew one), and darkens the near-black pixels at
//! surface silhouettes and slants by the screen-space derivatives of the depth, so the world
//! reads as surfaces and not a flat relief. Which targets to shade by is the frame's most-drawn
//! one, ties by the most recently drawn: the scene's main depth target is drawn to by every
//! world pass, while a shadow map's cascades see far fewer draws. The choice is logged at each
//! present that changes it. Guesses: reversed Z (near 1, far 0), and that a near-black source
//! pixel (all channels under 0.005) is unlit rather than the title's own black.

use std::collections::HashMap;
use std::sync::Mutex;

/// The switch's level, read once: `VICTORY_AMBIENT=<level>`, a fraction in (0, 1].
pub fn level() -> Option<f32> {
    static LEVEL: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| {
        let v = std::env::var("VICTORY_AMBIENT").ok()?;
        let k: f32 = v.trim().parse().ok()?;
        (k > 0.0 && k <= 1.0).then_some(k)
    })
}

/// The depth and colour targets drawn to since the last present: key, draws, and the sequence
/// number of the last draw (ties in draws go to the most recently drawn).
#[derive(Default)]
struct Frame {
    depths: HashMap<usize, (u32, u64)>,
    colors: HashMap<usize, (u32, u64)>,
    seq: u64,
}

fn frame() -> std::sync::MutexGuard<'static, Frame> {
    static FRAME: std::sync::LazyLock<Mutex<Frame>> =
        std::sync::LazyLock::new(|| Mutex::new(Frame::default()));
    FRAME.lock().unwrap_or_else(|e| e.into_inner())
}

/// A draw was encoded with `depth` bound as its depth target (nothing when the binding has none).
/// Called by the native host's draw path (macOS); public so every target builds it.
pub fn note(depth: Option<usize>) {
    let Some(key) = depth else { return };
    let mut f = frame();
    let seq = f.seq;
    f.seq += 1;
    let e = f.depths.entry(key).or_default();
    e.0 += 1;
    e.1 = seq;
}

/// The colour targets a draw was encoded into, its depth target apart: the frame's albedo
/// candidates. Called by the native host's draw path (macOS) with the targets the caller could
/// already read as floats (integer-packed G-buffer targets cannot be sampled by the present
/// fragment); public so every target builds it.
pub fn note_colors(keys: impl Iterator<Item = usize>) {
    let mut f = frame();
    let seq = f.seq;
    f.seq += 1;
    for key in keys {
        let e = f.colors.entry(key).or_default();
        e.0 += 1;
        e.1 = seq;
    }
}

/// What a present shades by: the frame's most-drawn depth target (draws with it) and colour
/// target (draws into it), which resets the tracking. `None` when no draw had that kind of
/// target. Called at each present (macOS).
pub fn choose() -> Chosen {
    let mut f = frame();
    let depths = std::mem::take(&mut f.depths);
    let colors = std::mem::take(&mut f.colors);
    f.seq = 0;
    let pick = |mut m: HashMap<usize, (u32, u64)>| {
        m.drain()
            .max_by_key(|&(key, (draws, seq))| (draws, seq, key))
            .map(|(key, (draws, _))| (key, draws))
    };
    Chosen { depth: pick(depths), color: pick(colors) }
}

/// The frame's choices for one present ([`choose`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chosen {
    /// The most-drawn depth target: the shade.
    pub depth: Option<(usize, u32)>,
    /// The most-drawn colour target: the albedo.
    pub color: Option<(usize, u32)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests own the process-wide frame, so only one runs at a time (as `valu_tests`
    /// does for the host it shares).
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    #[test]
    fn a_level_must_be_a_fraction_above_zero() {
        // The parsing itself, not the cached switch: same shape, other values.
        let ok = |v: &str| v.trim().parse::<f32>().ok().filter(|&k| k > 0.0 && k <= 1.0);
        assert_eq!(ok("0.6"), Some(0.6));
        assert_eq!(ok("1"), Some(1.0));
        assert_eq!(ok("0"), None, "a level of zero would show nothing");
        assert_eq!(ok("1.5"), None);
        assert_eq!(ok("-0.2"), None);
        assert_eq!(ok("bright"), None);
    }

    #[test]
    fn the_most_drawn_depth_wins_and_ties_go_to_the_latest() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        // These use the process-wide frame, so this test owns it: nothing else notes draws.
        assert_eq!(choose(), Chosen { depth: None, color: None }, "nothing was drawn yet");
        note(None);
        assert_eq!(choose(), Chosen { depth: None, color: None }, "a draw without a depth target is no candidate");
        for _ in 0..3 {
            note(Some(0xa));
        }
        for _ in 0..5 {
            note(Some(0xb));
        }
        note(Some(0xc));
        assert_eq!(choose().depth, Some((0xb, 5)), "the most drawn");
        assert_eq!(choose().depth, None, "choosing resets the frame");
    }

    #[test]
    fn equally_drawn_depths_go_to_the_most_recently_drawn() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..2 {
            note(Some(0xa));
            note(Some(0xb));
        }
        note(Some(0xa));
        assert_eq!(choose().depth, Some((0xa, 3)), "drawn the same, drawn later");
    }

    #[test]
    fn the_most_drawn_colour_target_is_the_albedo_and_depth_does_not_candidate() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        // A frame of world passes: a depth target with colour targets drawn beside it.
        for _ in 0..6 {
            note(Some(0x50));
            note_colors([0x60, 0x61].into_iter());
        }
        // A shadow pass: the same depth target, another colour target once.
        note(Some(0x50));
        note_colors([0x70].into_iter());
        // The post pass: the depth-less colour target the present will show.
        note(None);
        note_colors([0x80].into_iter());
        let chosen = choose();
        assert_eq!(chosen.depth, Some((0x50, 7)), "the world's depth target");
        // The two colour targets are drawn equally often; the later one of each pair wins the tie.
        assert_eq!(chosen.color, Some((0x61, 6)), "the most-drawn colour target");
        // Both reset together.
        assert_eq!(choose(), Chosen { depth: None, color: None });
    }

    #[test]
    fn equally_drawn_colour_targets_go_to_the_most_recently_drawn() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        note_colors([0x60].into_iter());
        note_colors([0x61].into_iter());
        assert_eq!(choose().color, Some((0x61, 1)), "drawn the same, drawn later");
    }
}
