//! `VICTORY_AMBIENT=<level>`: a guaranteed-visible fallback for presents (a debug aid, on by
//! default at 0.45).
//!
//! While the lighting compute passes are still refused, a fully drawn G-buffer reaches the
//! swap chain as pitch black after tone-mapping, and nothing on the screen shows the world was
//! rendered at all. Each present shades the near-black pixels of the presented picture by the
//! frame's main depth target (`level` as the constant ambient, the depth as the shade: nearer is
//! brighter, the console's reversed Z), so the 3D geometry is immediately visible while the
//! lighting shaders are being perfected. Pixels the title lit stay as they are (the overlay, the
//! menus, the movie), and the switch changes no work the host records for the title: the shading
//! happens in the present pass alone.
//!
//! The shading also blends the frame's main *colour* target in as the albedo (the G-buffer's own
//! surface colours, when a pass drew one), and darkens the near-black pixels at surface
//! silhouettes and slants by the screen-space derivatives of the depth, so the world reads as
//! surfaces and not a flat relief.
//!
//! Which targets to shade by: the *largest* one drawn to since the last present — the scene's
//! main depth target is the full-resolution one, while a shadow map's cascades and the small
//! intermediates of post passes are smaller — with ties broken by the most draws and then the
//! most recently drawn. A title that presents more than once a frame (the 3D scene first, then a
//! depth-less 2D UI and subtitle pass) must not lose the scene's targets at the second present,
//! so the last depth and colour choices are *kept* across presents that draw no depth of their
//! own, for up to [`KEEP_PRESENTS`] of them: the UI pass shades by the scene pass's targets, and a
//! stretch of pure 2D screens (a menu, a movie) eventually clears the shading again. The choice
//! is logged at each present that changes it. Guesses: reversed Z (near 1, far 0), and that a
//! near-black source pixel (all channels under 0.015) is unlit rather than the title's own black.

use std::collections::HashMap;
use std::sync::Mutex;

/// The default level when `VICTORY_AMBIENT` names none: enough to read the world under the
/// subtitles without washing out the title's own dark scenes.
pub const DEFAULT_LEVEL: f32 = 0.45;

/// The switch's level, read once: `VICTORY_AMBIENT=<level>`, a fraction in (0, 1]. Unset means
/// the default ([`DEFAULT_LEVEL`]); an explicit `0` (or any value that is not such a fraction)
/// turns the shading off.
pub fn level() -> Option<f32> {
    static LEVEL: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var("VICTORY_AMBIENT") {
        Ok(v) => parse_level(Some(v.as_str())),
        Err(_) => Some(DEFAULT_LEVEL),
    })
}

/// [`level`] as a pure function of the variable's value (`None` when it is unset).
pub fn parse_level(v: Option<&str>) -> Option<f32> {
    let v = v?;
    let k: f32 = v.trim().parse().ok()?;
    (k > 0.0 && k <= 1.0).then_some(k)
}

/// How many presents without any depth or colour candidate the last choices survive: a title
/// that presents its 2D UI after its 3D scene keeps the scene's targets for the UI present,
/// while a screen that draws no 3D at all clears the shading after this many presents.
pub const KEEP_PRESENTS: u32 = 4;

/// The depth and colour targets drawn to since the last present: key, pixels, draws, and the
/// sequence number of the last draw (ties go to the most recently drawn).
#[derive(Default)]
struct Frame {
    depths: HashMap<usize, (u64, u32, u64)>,
    colors: HashMap<usize, (u64, u32, u64)>,
    seq: u64,
}

fn frame() -> std::sync::MutexGuard<'static, Frame> {
    static FRAME: std::sync::LazyLock<Mutex<Frame>> =
        std::sync::LazyLock::new(|| Mutex::new(Frame::default()));
    FRAME.lock().unwrap_or_else(|e| e.into_inner())
}

/// The pixels of title texture `key`, as far as its record says (0 when it has none, and 0
/// whole on the targets without the resource records: it weighs nothing in the choice, but
/// still candidates).
fn pixels_of(key: usize) -> u64 {
    #[cfg(target_os = "macos")]
    {
        crate::resource::texture_info(key as crate::object::This)
            .map_or(0, |i| u64::from(i.width) * u64::from(i.height.max(1)))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = key;
        0
    }
}

/// A draw was encoded with `depth` bound as its depth target (nothing when the binding has none).
/// Called by the native host's draw path (macOS); public so every target builds it.
pub fn note(depth: Option<usize>) {
    let Some(key) = depth else { return };
    let mut f = frame();
    let seq = f.seq;
    f.seq += 1;
    let e = f.depths.entry(key).or_insert_with(|| (pixels_of(key), 0, 0));
    e.1 += 1;
    e.2 = seq;
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
        let e = f.colors.entry(key).or_insert_with(|| (pixels_of(key), 0, 0));
        e.1 += 1;
        e.2 = seq;
    }
}

/// The largest entry of `m` (pixels, then draws, then recency), or `None` when it is empty.
fn pick(m: &mut HashMap<usize, (u64, u32, u64)>) -> Option<(usize, u64, u32)> {
    m.drain()
        .max_by_key(|&(key, (pixels, draws, seq))| (pixels, draws, seq, key))
        .map(|(key, (pixels, draws, _))| (key, pixels, draws))
}

/// [`choose`]'s retention rule as a free function (the closure would borrow the kept state
/// twice): a fresh candidate replaces what is held and clears its miss count; without one the
/// held choice stands for [`KEEP_PRESENTS`] presents and is then dropped.
fn kept_or(
    fresh: Option<(usize, u64, u32)>,
    held: &mut Option<(usize, u64, u32)>,
    misses: &mut u32,
) -> Option<(usize, u64, u32)> {
    match fresh {
        Some(f) => {
            *held = Some(f);
            *misses = 0;
            Some(f)
        }
        None if *misses < KEEP_PRESENTS => {
            *misses += 1;
            *held
        }
        None => {
            *held = None;
            None
        }
    }
}

/// The last choices kept across depth-less (or colour-less) presents, and how many such
/// presents have passed: a fresh candidate of its kind resets its counter.
static KEPT: Mutex<Kept> = Mutex::new(Kept::NONE);

/// [`choose`]'s kept choices, as a type of their own (a tuple of Options and counters reads
/// worse than it works).
#[derive(Clone, Copy, Default)]
struct Kept {
    depth: Option<(usize, u64, u32)>,
    color: Option<(usize, u64, u32)>,
    depth_misses: u32,
    color_misses: u32,
}

impl Kept {
    const NONE: Kept = Kept { depth: None, color: None, depth_misses: 0, color_misses: 0 };
}

/// What a present shades by: the largest depth target (pixels, then draws) and colour target of
/// the draws since the last present, which resets the tracking. A present whose draws named no
/// depth keeps the last depth ([`KEEP_PRESENTS`] of them may pass this way), so the depth-less
/// 2D pass that follows the 3D scene presents shaded all the same; the colour choice follows
/// the same rule on its own. `None` when nothing was drawn and nothing is worth keeping.
/// Called at each present (macOS).
pub fn choose() -> Chosen {
    let mut f = frame();
    let mut depths = std::mem::take(&mut f.depths);
    let mut colors = std::mem::take(&mut f.colors);
    f.seq = 0;
    let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    // The guard derefs once: the state moves out as a copy and back when this present is done.
    let mut held = *kept;
    let depth = kept_or(pick(&mut depths), &mut held.depth, &mut held.depth_misses);
    let color = kept_or(pick(&mut colors), &mut held.color, &mut held.color_misses);
    *kept = held;
    Chosen { depth, color }
}

/// The frame's choices for one present ([`choose`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chosen {
    /// The largest depth target: the shade.
    pub depth: Option<(usize, u64, u32)>,
    /// The largest colour target: the albedo.
    pub color: Option<(usize, u64, u32)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests own the process-wide frame and its kept choices, so only one runs at a time
    /// (as `valu_tests` does for the host it shares) and each starts from a clear slate.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn alone() -> std::sync::MutexGuard<'static, ()> {
        let one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        *KEPT.lock().unwrap_or_else(|e| e.into_inner()) = Kept::NONE;
        one
    }

    /// [`level`] with the default filled in: what the present path actually shades by.
    fn level_or_default(v: Option<&str>) -> Option<f32> {
        v.map_or(Some(DEFAULT_LEVEL), |v| parse_level(Some(v)))
    }

    #[test]
    fn a_level_must_be_a_fraction_above_zero() {
        // The parsing itself, not the cached switch: same shape, other values.
        assert_eq!(parse_level(Some("0.6")), Some(0.6));
        assert_eq!(parse_level(Some("1")), Some(1.0));
        assert_eq!(parse_level(Some("0")), None, "a level of zero would show nothing");
        assert_eq!(parse_level(Some("1.5")), None);
        assert_eq!(parse_level(Some("-0.2")), None);
        assert_eq!(parse_level(Some("bright")), None);
        assert_eq!(parse_level(Some("")), None);
        // Unset is the default, not off: only an explicit value can disable the shading.
        assert_eq!(level_or_default(None), Some(DEFAULT_LEVEL));
        assert_eq!(level_or_default(Some("0")), None);
        assert_eq!(level_or_default(Some("0.45")), Some(0.45));
    }

    #[test]
    fn the_largest_depth_wins_and_ties_go_to_the_most_drawn() {
        let _one = alone();
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
        // No texture record exists for these keys, so all weigh zero pixels: the most drawn wins.
        assert_eq!(choose().depth.map(|(k, _, d)| (k, d)), Some((0xb, 5)), "the most drawn");
        // The tracking reset: a fresh frame's draws choose afresh, and only a stretch without
        // any depth clears the shading (the kept choice) again.
        note(Some(0xd));
        assert_eq!(choose().depth.map(|(k, _, _)| k), Some(0xd), "a new frame chooses afresh");
        for _ in 0..KEEP_PRESENTS {
            choose();
        }
        assert_eq!(choose().depth, None, "cleared after enough depth-less presents");
    }

    #[test]
    fn equally_drawn_depths_go_to_the_most_recently_drawn() {
        let _one = alone();
        for _ in 0..2 {
            note(Some(0xa));
            note(Some(0xb));
        }
        note(Some(0xa));
        assert_eq!(choose().depth.map(|(k, _, d)| (k, d)), Some((0xa, 3)), "drawn the same, drawn later");
    }

    #[test]
    fn the_scene_depth_outranks_a_more_drawn_shadow_map_by_pixels() {
        let _one = alone();
        // A shadow map's four cascades draw more often than the scene, but the scene's depth is
        // the full-resolution target: it is the one to shade by. (`pixels_of` reads the texture
        // record; these keys have none, so this checks the ranking order the way `pick` applies
        // it — the pixels first. The next test covers a record-backed pair.)
        let mut depths = HashMap::new();
        depths.insert(0x50, (2_592_000u64, 30u32, 1u64)); // the scene: 1920x1350
        depths.insert(0x60, (1_048_576u64, 90u32, 2u64)); // a shadow atlas: 1024x1024
        assert_eq!(pick(&mut depths).map(|(k, _, _)| k), Some(0x50));
    }

    #[test]
    fn a_depthless_ui_present_keeps_the_scene_targets() {
        let _one = alone();
        // A frame of world passes: a depth target with colour targets drawn beside it.
        for _ in 0..6 {
            note(Some(0x50));
            note_colors([0x60, 0x61].into_iter());
        }
        let scene = choose();
        assert_eq!(scene.depth.map(|(k, _, d)| (k, d)), Some((0x50, 6)), "the world's depth target");
        assert_eq!(scene.color.map(|(k, _, d)| (k, d)), Some((0x61, 6)), "the most-drawn colour target");
        // The post and UI passes: no depth at all, a colour target of the UI's own.
        note(None);
        note_colors([0x80].into_iter());
        let ui = choose();
        assert_eq!(ui.depth.map(|(k, _, _)| k), Some(0x50), "the scene's depth is kept for the UI present");
        assert_eq!(ui.color.map(|(k, _, d)| (k, d)), Some((0x80, 1)), "the UI's own colour target is the newer choice");
        // A pure 2D stretch clears the shading after KEEP_PRESENTS depth-less presents. The UI
        // present above was the first of them, so the kept depth survives this loop's first
        // KEEP_PRESENTS - 1 presents and is gone at the next.
        for (n, still) in (1..=KEEP_PRESENTS).map(|n| (n, n < KEEP_PRESENTS)) {
            note(None);
            note_colors([0x80].into_iter());
            let gone = choose();
            if still {
                assert_eq!(gone.depth.map(|(k, _, _)| k), Some(0x50), "kept through depth-less present {n}");
            } else {
                assert_eq!(gone.depth, None, "cleared after {n} depth-less presents");
            }
        }
        // Both reset together once 3D draws again: the kept choice is the new frame's own.
        note(Some(0x51));
        assert_eq!(choose().depth.map(|(k, _, _)| k), Some(0x51));
        note(Some(0x52));
        assert_eq!(choose().depth.map(|(k, _, _)| k), Some(0x52), "the next frame's choice replaces it");
    }

    #[test]
    fn equally_drawn_colour_targets_go_to_the_most_recently_drawn() {
        let _one = alone();
        note_colors([0x60].into_iter());
        note_colors([0x61].into_iter());
        assert_eq!(choose().color.map(|(k, _, d)| (k, d)), Some((0x61, 1)), "drawn the same, drawn later");
    }
}
