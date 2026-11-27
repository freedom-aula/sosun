//! Which console the title runs on. Titles pick settings (resolution,
//! memory budgets) by it, so the user chooses; Series X unless told.

use std::sync::atomic::{AtomicBool, Ordering};

const GIB: usize = 1 << 30;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Console {
    #[default]
    SeriesX,
    SeriesS,
}

impl Console {
    /// Title memory in bytes: (GPU-optimal, standard). Series X in the
    /// advanced configuration; Series S has one range, counted as standard
    /// (7,776 MiB, the published console-mode figure).
    // shortcut: later system updates gave Series S titles more (amount not
    // published here); Grand Theft Auto VI sizes nothing from this figure
    // (it goes by the device type), so update it if a title does.
    pub const fn title_memory(self) -> (usize, usize) {
        match self {
            Console::SeriesX => (10 * GIB, 7 * GIB / 2),
            Console::SeriesS => (0, 7_776 << 20),
        }
    }

    /// Compute units of the GPU.
    pub const fn compute_units(self) -> u32 {
        match self {
            Console::SeriesX => 52,
            Console::SeriesS => 20,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Console::SeriesX => "Xbox Series X",
            Console::SeriesS => "Xbox Series S",
        }
    }
}

static SERIES_S: AtomicBool = AtomicBool::new(false);

/// Set before the title starts.
pub fn set(console: Console) {
    SERIES_S.store(console == Console::SeriesS, Ordering::Relaxed);
}

pub fn current() -> Console {
    if SERIES_S.load(Ordering::Relaxed) {
        Console::SeriesS
    } else {
        Console::SeriesX
    }
}

/// The host's physical memory, when it can be read; `None` on the targets
/// without the call, where the tighter defaults then do not apply.
pub(crate) fn host_memory() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        victory_platform::mem::physical_memory()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// A Mac whose memory asks for the tighter budget: 16 GB and less.
const SMALL_HOST: u64 = 16 * GIB as u64;

/// The tighter title ceiling a memory-pressured host reports (6 GiB).
const SMALL_HOST_TITLE_MEMORY: usize = 6 * GIB;

/// What the title is told its memory is: [`Console::title_memory`], tightened
/// on a host that cannot afford it. `VICTORY_TITLE_MEM_MB=<mib>` overrides the
/// ceiling outright (0 keeps the console's own figures); without it, a Mac
/// with 16 GB of RAM or less is told 6 GiB of title memory — the 7.6 GiB a
/// Series S reports, plus the host's own mirrors and compiler state, is what
/// took a 16 GB Air to 15.2 GB and a ten-minute load, and a tighter budget is
/// what makes the engine's streamer purge its boot and menu assets once the
/// prologue starts.
pub fn title_memory_budget() -> (usize, usize) {
    static BUDGET: std::sync::OnceLock<(usize, usize)> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        if let Ok(v) = std::env::var("VICTORY_TITLE_MEM_MB")
            && let Ok(mb) = v.trim().parse::<usize>()
            && mb > 0
        {
            return (0, mb << 20);
        }
        let (gpu_optimal, standard) = current().title_memory();
        if host_memory().is_some_and(|m| m <= SMALL_HOST) && gpu_optimal + standard > SMALL_HOST_TITLE_MEMORY {
            let keep_gpu_optimal = gpu_optimal.min(SMALL_HOST_TITLE_MEMORY / 2);
            (keep_gpu_optimal, SMALL_HOST_TITLE_MEMORY - keep_gpu_optimal)
        } else {
            (gpu_optimal, standard)
        }
    })
}
