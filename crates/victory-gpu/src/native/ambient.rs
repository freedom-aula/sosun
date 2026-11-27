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
//! Which depth target to shade by is the frame's most-drawn one, ties by the most recently
//! drawn: the scene's main depth target is drawn to by every world pass, while a shadow map's
//! cascades see far fewer draws. The choice is logged at each present that changes it. Guesses:
//! reversed Z (near 1, far 0), and that a near-black source pixel (all channels under 0.02) is
//! unlit rather than the title's own black.

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

/// The depth targets drawn to since the last present: key, draws, and the sequence number of the
/// last draw (ties in draws go to the most recently drawn).
#[derive(Default)]
struct Frame {
    depths: HashMap<usize, (u32, u64)>,
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

/// The most-drawn depth target since the last present (draws with it), which resets the tracking.
/// `None` when no draw had a depth target. Called at each present (macOS).
pub fn choose() -> Option<(usize, u32)> {
    let mut f = frame();
    let depths = std::mem::take(&mut f.depths);
    f.seq = 0;
    depths
        .into_iter()
        .max_by_key(|&(key, (draws, seq))| (draws, seq, key))
        .map(|(key, (draws, _))| (key, draws))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // These use the process-wide frame, so this test owns it: nothing else notes draws.
        assert_eq!(choose(), None, "nothing was drawn yet");
        note(None);
        assert_eq!(choose(), None, "a draw without a depth target is no candidate");
        for _ in 0..3 {
            note(Some(0xa));
        }
        for _ in 0..5 {
            note(Some(0xb));
        }
        note(Some(0xc));
        assert_eq!(choose(), Some((0xb, 5)), "the most drawn");
        assert_eq!(choose(), None, "choosing resets the frame");
    }

    #[test]
    fn equally_drawn_depths_go_to_the_most_recently_drawn() {
        for _ in 0..2 {
            note(Some(0xa));
            note(Some(0xb));
        }
        note(Some(0xa));
        assert_eq!(choose(), Some((0xa, 3)), "drawn the same, drawn later");
    }
}
