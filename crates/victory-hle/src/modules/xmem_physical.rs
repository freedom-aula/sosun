//! `xmem.dll`'s physical pages, over `victory-platform`'s [`PagePool`]; on
//! Windows and on macOS.
//!
//! On the console a title allocates 64 KiB physical pages and maps them into
//! its own reservations, at more than one address if it likes. Here each
//! allocation is one pool, its pages are 64 KiB slices of it, and a mapping
//! is a view of consecutive slices in place of what the title's reservation
//! holds there:
//!
//! * macOS: the pool is one Mach memory entry, mapped at title addresses
//!   with `mach_vm_map` (one call for a run, however many pages), and once,
//!   for good, at [`PoolInfo::base`], where our own writes (DirectStorage)
//!   go and a GPU backend wraps the pool as one buffer. Nothing is ever a
//!   placeholder; an unmapped title page is no access.
//! * Windows (and Wine): the pool is a section, and a mapping is one view
//!   of a run of consecutive slices in place of a placeholder, which is what
//!   the title's reservation of 64 KiB pages is.
//!
//! The GPU side hears of mappings and pools through an [`Observer`] (on
//! Windows it is also told through `victory_gpu`'s change tracking, as
//! before).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::{BTreeMap, HashMap, hash_map::Entry};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use victory_platform::mem::{self, PagePool, Protect, Window};

#[cfg(windows)]
use super::xmem::{commit_refused, on_wine};

pub(super) const PAGE: usize = 64 << 10;

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: Grand Theft Auto VI keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;

/// What a GPU backend needs to know of a pool: where the whole of it is
/// mapped, for good while the pool lives, and what stands behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolInfo {
    /// A number for the pool, never reused.
    pub id: usize,
    /// The pool's own mapping of all of it, read-write, 16 KiB aligned, which
    /// is never moved, protected or unmapped until [`Observer::pool_released`]
    /// (0 on Windows when the host would not map it).
    pub base: usize,
    pub len: usize,
    /// macOS: the Mach memory entry (a send right the pool owns), which
    /// `mach_vm_map` can map elsewhere too.
    pub memory_entry: Option<u32>,
}

/// Told, synchronously and with the title's page table locked (so it must
/// not call back into `xmem`), of what happens to pools and their mappings.
/// The GPU layer keeps its `title page -> (pool buffer, offset)` table from
/// these (what `title_memory_remapped` is on Windows).
pub trait Observer: Send + Sync {
    /// A pool was allocated (`XMemAllocatePhysicalPages`), before any page of
    /// it is handed out.
    fn pool_created(&self, _pool: &PoolInfo) {}
    /// Every page of the pool is freed and unmapped: drop what wraps
    /// [`PoolInfo::base`], which is unmapped when this returns.
    fn pool_released(&self, _pool: &PoolInfo) {}
    /// `[at, at + len)` of title memory now shows `[offset, offset + len)` of
    /// the pool (what it showed before is gone, see `unmapped`), before the
    /// title can use the address.
    fn mapped(&self, _at: usize, _len: usize, _pool: &PoolInfo, _offset: usize) {}
    /// `[at, at + len)` of title memory shows no pool any more.
    fn unmapped(&self, _at: usize, _len: usize) {}
}

static OBSERVER: Mutex<Option<Arc<dyn Observer>>> = Mutex::new(None);

/// Sets who hears of pools and mappings (`None`: nobody), returning who did.
pub fn set_observer(observer: Option<Arc<dyn Observer>>) -> Option<Arc<dyn Observer>> {
    std::mem::replace(
        &mut *OBSERVER.lock().unwrap_or_else(|e| e.into_inner()),
        observer,
    )
}

fn observer() -> Option<Arc<dyn Observer>> {
    OBSERVER.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The pools that exist.
static POOLS: Mutex<BTreeMap<usize, PoolInfo>> = Mutex::new(BTreeMap::new());

/// Every pool that exists now, oldest first: a backend that starts late
/// wraps these, then hears of the rest.
pub fn pools() -> Vec<PoolInfo> {
    POOLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .copied()
        .collect()
}

/// One allocation of physical pages.
struct Pool {
    pool: PagePool,
    info: PoolInfo,
}

impl Pool {
    fn new(size: usize) -> std::io::Result<Pool> {
        // Pages are committed when first mapped or written (`commit`) where
        // the host can be asked to (Grand Theft Auto VI asks for 6.1 GiB at
        // once, which a busy Windows host often cannot commit: error 1450).
        // Under Wine a committed section is a sparse file, free until
        // touched, and committing a reserved one page by page costs a
        // wineserver request each that scans every range committed so far:
        // it is committed whole there. On macOS pages are committed when
        // first touched whichever way.
        #[cfg(windows)]
        let pool = (if on_wine() {
            PagePool::new(size)
        } else {
            PagePool::new_lazy(size)
        })?;
        #[cfg(not(windows))]
        let pool = PagePool::new(size)?;
        static IDS: AtomicUsize = AtomicUsize::new(1);
        let info = PoolInfo {
            id: IDS.fetch_add(1, Ordering::Relaxed),
            base: pool.base().map_or(0, |b| b as usize),
            len: pool.len(),
            #[cfg(target_os = "macos")]
            memory_entry: Some(pool.memory_entry()),
            #[cfg(not(target_os = "macos"))]
            memory_entry: None,
        };
        POOLS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(info.id, info);
        if let Some(observer) = observer() {
            observer.pool_created(&info);
        }
        Ok(Pool { pool, info })
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        POOLS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.info.id);
        if let Some(observer) = observer() {
            observer.pool_released(&self.info);
        }
    }
}

/// Where a page lives, or what a view starts with: a pool and a page index
/// in it.
#[derive(Clone)]
struct Slice {
    pool: Arc<Pool>,
    index: usize,
}

#[derive(Default)]
struct Physical {
    next_page: usize,
    pages: HashMap<usize, Slice>,
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
    /// Where each slice (pool, page index) is mapped: the addresses of
    /// its page in every view showing it (the GPU side's change tracking
    /// follows writes through one view to the others, `aliases`).
    shown: HashMap<(usize, usize), Addresses>,
}

enum Addresses {
    One(usize),
    Two([usize; 2]),
    // shortcut: three or more mappings use a Vec, use shared storage if that becomes common.
    Many(Vec<usize>),
}

impl Addresses {
    fn as_slice(&self) -> &[usize] {
        match self {
            Self::One(at) => std::slice::from_ref(at),
            Self::Two(addresses) => addresses,
            Self::Many(addresses) => addresses,
        }
    }

    fn push(&mut self, at: usize) {
        match self {
            Self::One(first) => *self = Self::Two([*first, at]),
            Self::Two([first, second]) => *self = Self::Many(vec![*first, *second, at]),
            Self::Many(addresses) => addresses.push(at),
        }
    }

    /// Returns whether this was the last address.
    fn remove(&mut self, at: usize) -> bool {
        match self {
            Self::One(first) => *first == at,
            Self::Two([first, second]) => {
                if *first == at {
                    *self = Self::One(*second);
                } else if *second == at {
                    *self = Self::One(*first);
                }
                false
            }
            Self::Many(addresses) => {
                addresses.retain(|&shown| shown != at);
                addresses.is_empty()
            }
        }
    }
}

impl Slice {
    /// The key of page `k` of the slices from this one on in `shown`.
    fn key(&self, k: usize) -> (usize, usize) {
        (Arc::as_ptr(&self.pool) as usize, self.index + k)
    }
}

/// Every address of a title view showing a page that title memory at
/// `[at, at + len)` shows, except the page's own address: pushed to `out`
/// as (page address, other address). The GPU side's change tracking
/// (`victory_gpu::watch`) marks writes there too.
pub fn aliases(at: usize, len: usize, out: &mut Vec<(usize, usize)>) {
    if len == 0 {
        return;
    }
    let physical = physical();
    let end = at.saturating_add(len);
    let first = physical
        .views
        .range(..at)
        .next_back()
        .filter(|(base, (size, _))| *base + size > at);
    for (&base, (size, slice)) in first.into_iter().chain(physical.views.range(at..end)) {
        let (from, to) = (at.max(base), end.min(base + size));
        for page in (from / PAGE..to.div_ceil(PAGE)).map(|p| p * PAGE) {
            let k = (page - base) / PAGE;
            if let Some(addresses) = physical.shown.get(&slice.key(k)) {
                out.extend(
                    addresses
                        .as_slice()
                        .iter()
                        .copied()
                        .filter(|&a| a != page)
                        .map(|a| (page, a)),
                );
            }
        }
    }
}

/// The addresses of title views showing `pages` (page numbers): writes into
/// them through our own views must be reported there.
fn shown_at(pages: &[usize]) -> Vec<usize> {
    let physical = physical();
    pages
        .iter()
        .filter_map(|p| physical.pages.get(p))
        .filter_map(|slice| physical.shown.get(&slice.key(0)).map(Addresses::as_slice))
        .flatten()
        .copied()
        .collect()
}

/// Reports writes into physical pages through our own views at every title
/// view showing them (DirectStorage's page destinations).
fn report_written(shown: &[usize]) {
    for &page in shown {
        #[cfg(test)]
        tests::REPORTED.with(|r| r.borrow_mut().push(page));
        #[cfg(windows)]
        drop(victory_gpu::kernel_will_write(page, PAGE));
        #[cfg(not(windows))]
        let _ = page; // the GPU reads title memory itself
    }
}

static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(|| {
    Mutex::new(Physical {
        // Page numbers are opaque to titles; 0 stays unused.
        next_page: 0x10_0000,
        ..Default::default()
    })
});

fn physical() -> MutexGuard<'static, Physical> {
    // shortcut: fixed-address VM changes and observer updates share this lock, use range locks if
    // unrelated concurrent mappings contend. Readers must not see stale mappings after a change.
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> i32 {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    let Some(size) = n.checked_mul(PAGE).filter(|&s| s > 0) else {
        return 0;
    };
    let pool = match Pool::new(size) {
        Ok(pool) => Arc::new(pool),
        Err(error) => {
            tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
            // SAFETY: checked non-null.
            unsafe { count.write(0) };
            return 0;
        }
    };
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    let mut physical = physical();
    let first = physical.next_page;
    physical.next_page += n * FRAMES;
    for index in 0..n {
        let slice = Slice {
            pool: pool.clone(),
            index,
        };
        let number = first + index * FRAMES;
        physical.pages.insert(number, slice);
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(number) };
    }
    1
}

/// How many of `pages` are no allocated page (diagnostics: a DirectStorage
/// destination that could not be written).
pub fn unknown_pages(pages: &[usize]) -> usize {
    let physical = physical();
    pages
        .iter()
        .filter(|p| !physical.pages.contains_key(p))
        .count()
}

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> i32 {
    if pages.is_null() {
        return 0;
    }
    let mut physical = physical();
    for i in 0..n {
        // SAFETY: the title passes `n` page numbers.
        physical.pages.remove(&unsafe { *pages.add(i) });
    }
    1
}

/// Backs the reserved `[base, base + len)` (a multiple of the page size) with
/// a pool of its own, as committed `XMemVirtualAlloc` memory is on the
/// console: physical pages the GPU reaches, here through the pool table.
/// macOS: the GPU backend resolves title addresses by pool only.
#[cfg(not(windows))]
pub(super) fn back_with_pool(base: usize, len: usize) -> bool {
    let Ok(pool) = Pool::new(len) else {
        return false;
    };
    let slice = Slice {
        pool: Arc::new(pool),
        index: 0,
    };
    map(&mut physical(), base, len, slice)
}

/// Gives back what [`back_with_pool`] mapped, before the range is released.
#[cfg(not(windows))]
pub(super) fn release_pool_backing(base: usize, len: usize) {
    unmap(&mut physical(), base, len);
}

/// Consecutive slices of one pool, committed and writable: through the
/// pool's own mapping of all of it, or a view of their own if the host would
/// not map all of it.
struct View {
    window: Window,
    len: usize,
    /// Keeps the pool, and with it the mapping, while this lives.
    _pool: Arc<Pool>,
}

impl View {
    /// `n` slices from `slice` on.
    fn map(slice: &Slice, n: usize) -> Option<View> {
        let (offset, len) = (slice.index * PAGE, n * PAGE);
        let pool = slice.pool.clone();
        let window = pool.pool.window(offset, len).ok()?;
        let view = View {
            window,
            len,
            _pool: pool,
        };
        commit(view.at(), len).then_some(view)
    }

    fn at(&self) -> usize {
        self.window.as_mut_ptr() as usize
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view holds `len` committed read-write bytes, ours
        // alone while `self` lives (other views of the slices only alias
        // them, as the console's do).
        unsafe { std::slice::from_raw_parts_mut(self.at() as *mut u8, self.len) }
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one pool: each run's first
/// slice and its length in pages. One look at the page table, which the
/// title's own mapping calls share with every DirectStorage worker. `None`
/// if a page is unknown or `pages` ends early.
fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let physical = physical();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let slice = physical.pages.get(page)?;
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.pool, &slice.pool) && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// Through each pool's own mapping ([`View`]), one run of consecutive
/// pages at a time. False if a page is unknown or the host fails.
pub fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let Some(mut view) = View::map(slice, *n) else {
            return false;
        };
        let k = (n * PAGE - within).min(data.len() - done);
        view.bytes()[within..within + k].copy_from_slice(&data[done..done + k]);
        (within, done) = (0, done + k);
    }
    // Where the pages show once written (see `PageWindow`).
    report_written(&shown_at(
        pages
            .get(offset / PAGE..(offset + data.len()).div_ceil(PAGE))
            .unwrap_or(&[]),
    ));
    true
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy.
pub struct PageWindow {
    view: View,
    within: usize,
    len: usize,
    /// The pages (page numbers) the bytes reach: the title views showing
    /// them when the window is done are told. Looked up then, not when the
    /// window opens: a decode takes milliseconds, and the title may map a
    /// page at another address meanwhile (its GPU heap); a copy that took
    /// the page there before the bytes landed would never hear of them. An
    /// address that stops showing a page meanwhile was told by the unmap.
    pages: Vec<usize>,
}

impl Drop for PageWindow {
    fn drop(&mut self) {
        report_written(&shown_at(&self.pages));
    }
}

impl PageWindow {
    pub fn bytes(&mut self) -> &mut [u8] {
        let (within, len) = (self.within, self.len);
        &mut self.view.bytes()[within..within + len]
    }
}

/// A window on bytes `[offset, offset + len)` of `pages` when they are
/// consecutive pages of one allocation (how titles usually hand them out);
/// `None` otherwise (pages of several allocations, out of order, or
/// unknown, or a host failure), which `write_physical` then handles or
/// reports.
pub fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    Some(PageWindow {
        view: View::map(slice, *n)?,
        within: offset % PAGE,
        len,
        pages: pages
            .get(offset / PAGE..(offset + len).div_ceil(PAGE))
            .unwrap_or(&[])
            .to_vec(),
    })
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    // A title reserves 64 KiB pages on 64 KiB boundaries (Windows insists).
    if !base.is_multiple_of(PAGE) {
        return std::ptr::null_mut();
    }
    let mut physical = physical();
    if !pages.is_null() {
        // SAFETY: the title passes `n` page numbers.
        let pages = unsafe { std::slice::from_raw_parts(pages, n) };
        if pages.iter().zip((base..).step_by(PAGE)).all(|(page, at)| {
            physical
                .pages
                .get(page)
                .is_some_and(|slice| shows(&physical, at, slice))
        }) {
            return address;
        }
    }
    unmap(&mut physical, base, n * PAGE);
    if pages.is_null() {
        return address;
    }
    // SAFETY: the title passes `n` page numbers.
    let pages = unsafe { std::slice::from_raw_parts(pages, n) };
    let mut i = 0;
    while i < n {
        let Some(first) = physical.pages.get(&pages[i]).cloned() else {
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return std::ptr::null_mut();
        };
        // One view per run of consecutive slices of one pool.
        let run = 1 + pages[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(page, k)| {
                physical.pages.get(page).is_some_and(|s| {
                    Arc::ptr_eq(&s.pool, &first.pool) && s.index == first.index + k
                })
            })
            .count();
        if !map(&mut physical, base + i * PAGE, run * PAGE, first) {
            return std::ptr::null_mut();
        }
        i += run;
    }
    address
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (Grand Theft Auto VI passes 0 for addresses to clear).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> i32 {
    if addresses.is_null() {
        return 0;
    }
    let mut i = 0;
    while i < n {
        // SAFETY: the title passes `n` addresses and, unless null, `n` page numbers.
        let (address, page) = unsafe {
            (
                *addresses.add(i),
                if pages.is_null() || *pages.add(i) == 0 {
                    std::ptr::null()
                } else {
                    pages.add(i)
                },
            )
        };
        // shortcut: keep Windows scatter views separate, batch if the destination is known to
        // be one placeholder. Even contiguous addresses can be separate placeholders today.
        #[cfg(target_os = "macos")]
        if !address.is_null()
            && (address as usize).is_multiple_of(PAGE)
            && !page.is_null()
            && i + 1 < n
        {
            // SAFETY: both lists have `n - i` entries remaining.
            let (destinations, numbers) = unsafe {
                (
                    std::slice::from_raw_parts(addresses.add(i), n - i),
                    std::slice::from_raw_parts(page, n - i),
                )
            };
            if let Some((run, mapped)) = map_fresh_scatter(destinations, numbers) {
                if !mapped {
                    return 0;
                }
                i += run;
                continue;
            }
        }
        if XMemMapPhysicalPages(address, 1, page).is_null() {
            return 0;
        }
        i += 1;
    }
    1
}

/// Maps consecutive, previously unbacked scatter destinations with one Mach call.
#[cfg(target_os = "macos")]
fn map_fresh_scatter(addresses: &[*mut c_void], pages: &[usize]) -> Option<(usize, bool)> {
    let base = addresses[0] as usize;
    // shortcut: replacements and clears stay page by page, batch them if we can preserve
    // unchanged protection and the scatter call's ordered partial results on failure.
    if base.checked_add(PAGE) != Some(addresses[1] as usize)
        || pages[0].checked_add(FRAMES) != Some(pages[1])
    {
        return None;
    }
    let mut physical = physical();
    if view_at(&physical, base).is_some() {
        return None;
    }
    let next_view = physical
        .views
        .range(base..)
        .next()
        .map_or(usize::MAX, |(&at, _)| at);
    let first = physical.pages.get(&pages[0])?;
    let run = 1 + addresses[1..]
        .iter()
        .zip(&pages[1..])
        .zip(1..)
        .take_while(|&((&at, page), k)| {
            let at = at as usize;
            base.checked_add(k * PAGE) == Some(at)
                && at.checked_add(PAGE).is_some_and(|end| end <= next_view)
                && *page != 0
                && physical.pages.get(page).is_some_and(|s| {
                    Arc::ptr_eq(&s.pool, &first.pool) && s.index == first.index + k
                })
        })
        .count();
    if run < 2 {
        return None;
    }
    // Keep reading one entry at a time if the operation itself replaces either input list.
    let end = base.checked_add(run * PAGE)?;
    let inputs = [addresses.as_ptr() as usize, pages.as_ptr() as usize];
    if inputs
        .into_iter()
        .any(|start| start < end && base < start + std::mem::size_of_val(pages))
    {
        return None;
    }
    let first = first.clone();
    let slice_at = |k| Slice {
        pool: first.pool.clone(),
        index: first.index + k,
    };
    // If the host rejects the run, retry one page at a time so a failing scatter keeps
    // its successful prefix. Each destination retains its own view and observer events.
    if host_map(base, run * PAGE, &first).is_err() {
        let mapped = (0..run).all(|k| map(&mut physical, base + k * PAGE, PAGE, slice_at(k)));
        return Some((run, mapped));
    }
    for k in 0..run {
        finish_map(&mut physical, base + k * PAGE, PAGE, slice_at(k));
    }
    Some((run, true))
}

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`victory_gpu`, queue `CopyPageMappingsBatchX`): Grand Theft Auto
/// VI maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Page by page; false if a source page is not mapped physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let (to, from, len) = (to as usize, from as usize, len as usize);
    let mut physical = physical();
    if from != 0
        && (0..len).step_by(PAGE).all(|k| {
            slice_at(&physical, from + k).is_some_and(|slice| shows(&physical, to + k, &slice))
        })
    {
        return true;
    }
    unmap(&mut physical, to, len);
    for k in (0..len).step_by(PAGE) {
        if from == 0 {
            break;
        }
        let Some(slice) = slice_at(&physical, from + k) else {
            return false;
        };
        if !map(&mut physical, to + k, PAGE, slice) {
            return false;
        }
    }
    true
}

/// The page a title view shows at `at`, if any.
fn slice_at(physical: &Physical, at: usize) -> Option<Slice> {
    let (index, s) = view_at(physical, at)?;
    Some(Slice {
        index,
        pool: s.pool.clone(),
    })
}

fn view_at(physical: &Physical, at: usize) -> Option<(usize, &Slice)> {
    physical
        .views
        .range(..=at)
        .next_back()
        .filter(|(base, (size, _))| at < *base + size)
        .map(|(base, (_, s))| (s.index + (at - base) / PAGE, s))
}

/// Whether a title view shows `slice` at `at` already. Mapping it there
/// again would change nothing, and would cost an unmap and a map, which
/// under Wine each hold the process's one virtual-memory lock across a
/// wineserver request: every page fault and protection change on other
/// threads waits for them (#50).
fn shows(physical: &Physical, at: usize, slice: &Slice) -> bool {
    view_at(physical, at)
        .is_some_and(|(index, s)| index == slice.index && Arc::ptr_eq(&s.pool, &slice.pool))
}

/// Records `slice` and the slices after it as the view at `[base, base + len)`
/// and where each page shows.
fn record(physical: &mut Physical, base: usize, len: usize, slice: Slice) {
    for k in 0..len / PAGE {
        let at = base + k * PAGE;
        match physical.shown.entry(slice.key(k)) {
            Entry::Vacant(entry) => {
                #[cfg(windows)]
                victory_gpu::title_memory_aliased(at, &[]);
                entry.insert(Addresses::One(at));
            }
            Entry::Occupied(mut entry) => {
                let shown = entry.get_mut();
                // Pages shown elsewhere already: guarded here too if they are there.
                #[cfg(windows)]
                victory_gpu::title_memory_aliased(at, shown.as_slice());
                shown.push(at);
            }
        }
    }
    physical.views.insert(base, (len, slice));
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in the title's reservation (on Windows, one placeholder).
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    if let Err(error) = host_map(base, len, &slice) {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            %error,
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    if !commit(base, len) {
        // SAFETY: the view just mapped, back to a placeholder.
        let _ = unsafe { mem::unmap_over(base as *mut u8, len) };
        return false;
    }
    finish_map(physical, base, len, slice);
    true
}

fn host_map(base: usize, len: usize, slice: &Slice) -> std::io::Result<()> {
    #[cfg(test)]
    tests::MAPS.with(|m| m.set(m.get() + 1));
    // SAFETY: the title's own reservation; the range is its to give.
    unsafe {
        slice
            .pool
            .pool
            .map_over(slice.index * PAGE, len, base, Protect::READ_WRITE)
    }
}

fn finish_map(physical: &mut Physical, base: usize, len: usize, slice: Slice) {
    // New contents, read-write: the GPU side's copies no longer guard it.
    #[cfg(windows)]
    victory_gpu::title_memory_remapped(base, len);
    if let Some(observer) = observer() {
        observer.mapped(base, len, &slice.pool.info, slice.index * PAGE);
    }
    record(physical, base, len, slice);
}

/// Commits the pool pages a view shows (once per page, whichever view).
/// Only a Windows host that was asked for reserved pages needs it.
fn commit(base: usize, len: usize) -> bool {
    #[cfg(windows)]
    {
        if on_wine() {
            return true; // sections are committed whole there
        }
        // SAFETY: `[base, base + len)` is a view of a reserved section.
        match unsafe { mem::commit(base as *mut u8, len, Protect::READ_WRITE) } {
            Ok(()) => true,
            Err(error) => {
                commit_refused(len, "physical pages", error);
                false
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (base, len);
        true
    }
}

/// Returns `[base, base + len)` to what a reservation is (a placeholder on
/// Windows); views reaching past it keep the rest of their mapping.
fn unmap(physical: &mut Physical, base: usize, len: usize) {
    // A zero-length unmap must not split the view containing `base`.
    if len == 0 {
        return;
    }
    let end = base + len;
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let mut preceding = physical
        .views
        .range(..base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    while let Some(at) = preceding
        .take()
        .or_else(|| physical.views.range(base..end).next().map(|(&at, _)| at))
    {
        let (size, slice) = physical.views.remove(&at).expect("overlapping view");
        // A Windows view goes whole and what lies outside the range is
        // mapped again; a Mach mapping is cut where the range is.
        let (from, to) = if cfg!(windows) {
            (at, at + size)
        } else {
            (at.max(base), (at + size).min(end))
        };
        for k in (from - at) / PAGE..(to - at) / PAGE {
            if let Entry::Occupied(mut entry) = physical.shown.entry(slice.key(k))
                && entry.get_mut().remove(at + k * PAGE)
            {
                entry.remove();
            }
        }
        #[cfg(windows)]
        victory_gpu::title_memory_remapped(from, to - from);
        if let Some(observer) = observer() {
            observer.unmapped(from, to - from);
        }
        // SAFETY: a view we mapped; it becomes a placeholder, or no access,
        // again.
        let _ = unsafe { mem::unmap_over(from as *mut u8, to - from) };
        if at < base {
            let left = (at, base - at, slice.clone());
            remap(physical, left);
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            remap(physical, (end, at + size - end, Slice { index, ..slice }));
        }
    }
}

/// What lies outside an unmapped range of a view, mapped again on Windows
/// and merely recorded under its own address on macOS (it never went).
fn remap(physical: &mut Physical, (at, len, slice): (usize, usize, Slice)) {
    #[cfg(windows)]
    map(physical, at, len, slice);
    #[cfg(not(windows))]
    {
        // The cut kept the pages' own entries in `shown`; only the view is
        // new.
        physical.views.insert(at, (len, slice));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Views this thread mapped for the title (`map`).
        pub(super) static MAPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// The title addresses this thread's writes into physical pages reported.
        pub(super) static REPORTED: std::cell::RefCell<Vec<usize>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// A reservation of `n` 64 KiB pages, as titles make them for mapping.
    fn reserve(n: usize) -> *mut c_void {
        #[cfg(windows)]
        {
            use super::super::xmem::{PAGES_64K, XMemVirtualAlloc};
            use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_READWRITE};
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                n * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        }
        #[cfg(not(windows))]
        {
            // Leaked; one 64 KiB page more, to start on a 64 KiB boundary
            // (macOS reservations are 16 KiB granular).
            // SAFETY: fresh address space.
            let p = unsafe { mem::reserve(None, (n + 1) * PAGE) }.expect("reserved");
            p.wrapping_add(p.align_offset(PAGE)).cast()
        }
    }

    /// Whether a page is mapped (a placeholder, or a reservation, is not).
    fn mapped(p: *mut c_void) -> bool {
        mem::query(p as usize).is_some_and(|r| r.protect == Protect::READ_WRITE)
    }

    fn at(p: *mut c_void, page: usize) -> *mut c_void {
        p.wrapping_byte_add(page * PAGE)
    }

    /// `n` pages of one new allocation.
    fn allocate(n: usize) -> Vec<usize> {
        let (mut pages, mut given) = (vec![0; n], n);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        assert_eq!(given, n);
        pages
    }

    /// The bytes of one page, through the pool's mapping.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = physical().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "synthetic release benchmark; run with --release --ignored --nocapture"]
    fn physical_mapping_timing() {
        use std::time::{Duration, Instant};
        use victory_gpu::native::memory::{PoolObserver, PoolTable};

        #[derive(Default)]
        struct Feed(PoolTable);

        impl Observer for Feed {
            fn mapped(&self, at: usize, len: usize, pool: &PoolInfo, offset: usize) {
                self.0.mapped(
                    at,
                    len,
                    &victory_gpu::native::memory::PoolInfo {
                        id: pool.id,
                        base: pool.base,
                        len: pool.len,
                        memory_entry: pool.memory_entry,
                    },
                    offset,
                );
            }

            fn unmapped(&self, at: usize, len: usize) {
                self.0.unmapped(at, len);
            }
        }

        const BACKGROUND: usize = 29_000;
        const SAMPLES: u32 = 500;
        let feed = Arc::new(Feed::default());
        let old_observer = set_observer(Some(feed.clone()));
        let base = reserve(BACKGROUND + 64);
        let pages = allocate(BACKGROUND + 64);
        // Put the measured range above a populated GPU table, as during boot.
        for (i, page) in pages[..BACKGROUND].iter().enumerate() {
            let address = at(base, i);
            assert_eq!(XMemMapPhysicalPages(address, 1, page), address);
        }
        let base_under_test = at(base, BACKGROUND);
        let contiguous = &pages[BACKGROUND..];
        let scattered: Vec<_> = contiguous.iter().step_by(2).copied().collect();
        let dense_va: Vec<_> = (0..32).map(|i| at(base_under_test, i)).collect();
        let sparse_va: Vec<_> = (0..32).map(|i| at(base_under_test, 2 * i)).collect();

        for with_gpu in [true, false] {
            if !with_gpu {
                set_observer(None);
            }
            for (label, pages, addresses) in [
                ("64 KiB contiguous", &contiguous[..1], None),
                ("64 KiB scatter", &scattered[..1], Some(&sparse_va[..1])),
                ("2 MiB contiguous", &contiguous[..32], None),
                ("2 MiB scattered physical", &scattered[..], None),
                (
                    "2 MiB scatter, dense run",
                    &contiguous[..32],
                    Some(&dense_va[..]),
                ),
                (
                    "2 MiB scatter, sparse VAs",
                    &scattered[..],
                    Some(&sparse_va[..]),
                ),
            ] {
                let change = |numbers| match addresses {
                    Some(addresses) => assert_eq!(
                        XMemMapPhysicalPagesScatter(addresses.as_ptr(), pages.len(), numbers),
                        1
                    ),
                    None => assert_eq!(
                        XMemMapPhysicalPages(base_under_test, pages.len(), numbers),
                        base_under_test
                    ),
                };
                let (mut map_time, mut unmap_time) = (Duration::ZERO, Duration::ZERO);
                for i in 0..SAMPLES + 50 {
                    let start = Instant::now();
                    change(pages.as_ptr());
                    let mapped = Instant::now();
                    change(std::ptr::null());
                    let end = Instant::now();
                    if i >= 50 {
                        map_time += mapped - start;
                        unmap_time += end - mapped;
                    }
                }
                eprintln!(
                    "GPU={with_gpu:<5} {label:<25} map {:8.2} us  unmap {:8.2} us",
                    map_time.as_secs_f64() * 1e6 / f64::from(SAMPLES),
                    unmap_time.as_secs_f64() * 1e6 / f64::from(SAMPLES),
                );
            }
        }
        set_observer(old_observer);
        assert_eq!(
            XMemMapPhysicalPages(base, BACKGROUND, std::ptr::null()),
            base
        );
        assert_eq!(XMemFreePhysicalPages(pages.len(), pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_what_shows_already_maps_nothing() {
        let pages = allocate(2);
        let (a, b) = (reserve(2), reserve(2));
        assert!(!a.is_null() && !b.is_null());
        let maps = || MAPS.with(std::cell::Cell::get);
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        let before = maps();
        // The same pages at the same address: nothing to do.
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        let scattered = [a, at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(scattered.as_ptr(), 2, pages.as_ptr()),
            1
        );
        assert_eq!(maps(), before);
        // The GPU's page-mapping copy, made twice: the second maps nothing.
        assert!(map_alias(b as u64, a as u64, 2 * PAGE as u64));
        assert_eq!(maps(), before + 2);
        assert!(map_alias(b as u64, a as u64, 2 * PAGE as u64));
        assert!(map_alias(at(b, 1) as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(maps(), before + 2);
        // Another page, or one page of the range changed, maps again.
        assert!(map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(maps(), before + 3);
        // SAFETY: both mapped read-write, showing the second page.
        unsafe {
            at(a, 1).cast::<u8>().write(6);
            assert_eq!(b.cast::<u8>().read(), 6);
        }
        // Pages swapped at `a`: both differ, so both map again.
        let swapped = [pages[1], pages[0]];
        assert_eq!(XMemMapPhysicalPages(a, 2, swapped.as_ptr()), a);
        assert_eq!(maps(), before + 5);
        // SAFETY: mapped read-write; `a` shows the second page now.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 6);
        for p in [a, b] {
            assert_eq!(XMemMapPhysicalPages(p, 2, std::ptr::null()), p);
        }
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        let (a, b) = (reserve(3), reserve(3));
        assert!(!a.is_null() && !b.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Each mapping knows the other (the GPU side's change tracking).
        let others = |p: *mut c_void| {
            let mut out = Vec::new();
            aliases(p as usize, PAGE, &mut out);
            out.into_iter().map(|(_, a)| a).collect::<Vec<_>>()
        };
        assert_eq!(others(at(a, 2)), [at(b, 1) as usize]);
        assert_eq!(others(at(b, 1)), [at(a, 2) as usize]);
        assert!(others(a).is_empty());
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert!(!mapped(at(a, 1)));
        assert!(mapped(a));
        assert!(mapped(at(a, 2)));
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);
        assert_eq!(
            others(at(a, 2)),
            [at(b, 1) as usize],
            "the kept pages still alias"
        );

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert!(!mapped(at(a, 1)));
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        let mut both = others(at(a, 2));
        both.sort_unstable();
        assert_eq!(both, [b as usize, at(b, 1) as usize]);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(
            others(at(a, 2)),
            [at(b, 1) as usize],
            "unmapped views are forgotten"
        );
        assert!(!mapped(b));
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_no_pages_changes_nothing() {
        let pages = allocate(2);
        let a = reserve(2);
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(5) };
        let second = at(a, 1);

        // A count of 0 at the start of a view, inside it and past it: with
        // pages, without (an unmap), and as the GPU's page-mapping copy.
        for p in [a, second, at(a, 2)] {
            assert_eq!(XMemMapPhysicalPages(p, 0, pages.as_ptr()), p);
            assert_eq!(XMemMapPhysicalPages(p, 0, std::ptr::null()), p);
            assert!(map_alias(p as u64, a as u64, 0));
            assert!(map_alias(p as u64, 0, 0));
        }

        // The view is still the one view it was.
        let view = physical().views.get(&(a as usize)).map(|v| v.0);
        assert_eq!(view, Some(2 * PAGE));
        assert!(mapped(a) && mapped(second));
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

        // A count of 1 still unmaps that page alone.
        assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        assert!(!mapped(second));
        assert!(mapped(a));
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn scatter_batches_fresh_runs_and_keeps_page_views() {
        let pages = allocate(6);
        let (a, b) = (reserve(6), reserve(6));
        assert_eq!(XMemMapPhysicalPages(a, 6, pages.as_ptr()), a);
        let addresses = [0, 1, 3, 4, 5].map(|i| at(b, i));
        let scattered = [pages[0], pages[1], pages[4], pages[5], pages[2]];
        let before = MAPS.with(std::cell::Cell::get);
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 5, scattered.as_ptr()),
            1
        );
        assert_eq!(MAPS.with(std::cell::Cell::get) - before, 3);
        assert!(!mapped(at(b, 2)));
        // SAFETY: `a` and every destination in `addresses` are mapped read-write.
        unsafe {
            for (i, &p) in [0, 1, 4, 5, 2].iter().zip(&addresses) {
                p.cast::<u8>().write(*i as u8 + 1);
                assert_eq!(at(a, *i).cast::<u8>().read(), *i as u8 + 1);
            }
            mem::protect(b.cast(), PAGE, Protect::READ).unwrap();
        }

        // The first entry is unchanged, the second replaces an existing mapping and
        // the third writes the second destination again. Keep order and protection.
        let addresses = [b, at(b, 1), at(b, 1)];
        let replacements = [pages[0], pages[2], pages[3]];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 3, replacements.as_ptr()),
            1
        );
        assert_eq!(mem::query(b as usize).unwrap().protect, Protect::READ);
        // SAFETY: both destinations remain mapped and readable.
        unsafe {
            at(a, 3).cast::<u8>().write(0x42);
            assert_eq!(at(b, 1).cast::<u8>().read(), 0x42);
        }
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, std::ptr::null()),
            at(b, 1)
        );
        assert_eq!(
            mem::query(at(b, 1) as usize).unwrap().protect,
            Protect::NO_ACCESS
        );
        assert_eq!(mem::query(b as usize).unwrap().protect, Protect::READ);
        assert!(mapped(at(b, 3)));
        for p in [a, b] {
            assert_eq!(XMemMapPhysicalPages(p, 6, std::ptr::null()), p);
        }
        assert_eq!(XMemFreePhysicalPages(6, pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_errors_keep_the_successful_prefix() {
        let pages = allocate(3);
        let a = reserve(3);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        assert!(XMemMapPhysicalPages(a.wrapping_byte_add(1), 1, pages.as_ptr()).is_null());
        assert_eq!(
            XMemMapPhysicalPagesScatter(std::ptr::null(), 0, std::ptr::null()),
            0
        );
        // The contiguous call clears its entire destination before mapping the prefix.
        let unknown = [pages[0], 0x10, pages[2]];
        assert!(XMemMapPhysicalPages(a, 3, unknown.as_ptr()).is_null());
        assert!(mapped(a) && !mapped(at(a, 1)) && !mapped(at(a, 2)));
        assert_eq!(
            XMemMapPhysicalPages(at(a, 2), 1, pages[2..].as_ptr()),
            at(a, 2)
        );

        // Scatter clears the failing destination, then stops without touching later entries.
        let addresses = [at(a, 1), a, at(a, 2)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 3, unknown.as_ptr()),
            0
        );
        assert!(!mapped(a) && mapped(at(a, 1)) && mapped(at(a, 2)));
        // The kernel rejects a fixed mapping outside user address space.
        let addresses = [a, (1usize << 63) as *mut c_void, at(a, 2)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 3, pages.as_ptr()),
            0
        );
        assert!(mapped(a) && mapped(at(a, 2)));
        assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn runs_group_consecutive_pages_of_one_allocation() {
        let (a, b) = (allocate(3), allocate(1));
        let lengths = |pages: &[usize], offset, len| {
            runs(pages, offset, len).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
        // Only the pages the bytes reach.
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        // Out of order, or across allocations: one run each.
        assert_eq!(
            lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
            Some(vec![1, 1, 1])
        );
        assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
        // A slice that follows on in another pool starts a run.
        assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
        // Too few pages for the bytes, or an unknown one.
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn writes_cross_runs_and_allocations() {
        let (a, b) = (allocate(2), allocate(1));
        let pages = [a[0], a[1], b[0]];
        let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
        let offset = PAGE - 32;
        // Past the third page: the second run is too short.
        assert!(!write_physical(&pages, offset, &data));
        assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
        assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
        assert!(
            !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
            "unknown page"
        );
        assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    /// A page mapped at a second address while a window writes it is
    /// reported there too when the window is done (the title maps streamed
    /// pages into its GPU heap while their reads decode); an address that
    /// stopped showing it meanwhile is not.
    #[test]
    fn page_windows_report_where_the_pages_show_when_done() {
        let pages = allocate(1);
        let (a, b) = (reserve(1), reserve(1));
        assert!(!a.is_null() && !b.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 1, pages.as_ptr()), a);
        REPORTED.with(|r| r.borrow_mut().clear());
        let mut window = page_window(&pages, 0, 16).expect("one run");
        // Mapped at `b`, unmapped from `a`, while the bytes are written.
        assert_eq!(XMemMapPhysicalPages(b, 1, pages.as_ptr()), b);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        window.bytes().fill(3);
        drop(window);
        assert_eq!(REPORTED.with(|r| r.borrow().clone()), [b as usize]);
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 3);
        // `write_physical` the same way.
        REPORTED.with(|r| r.borrow_mut().clear());
        assert!(write_physical(&pages, 0, &[4]));
        assert_eq!(REPORTED.with(|r| r.borrow().clone()), [b as usize]);
        assert_eq!(XMemMapPhysicalPages(b, 1, std::ptr::null()), b);
        assert_eq!(XMemFreePhysicalPages(1, pages.as_ptr()), 1);
    }

    #[test]
    fn page_windows_cover_one_run_in_place() {
        let (a, b) = (allocate(3), allocate(1));
        let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
        assert_eq!(window.bytes().len(), PAGE);
        window.bytes().fill(0x5a);
        drop(window);
        let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
        assert!(second[..3].iter().all(|&b| b == 0));
        assert!(second[3..].iter().all(|&b| b == 0x5a));
        assert!(third[..3].iter().all(|&b| b == 0x5a));
        assert!(third[3..].iter().all(|&b| b == 0));
        // Several runs, or an unknown page: no window.
        assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[0x10], 0, 1).is_none());
        // One page of several is still one run.
        assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn page_writes_share_one_view_of_their_pool() {
        let a = allocate(3);
        let slice = |p| physical().pages.get(&p).cloned().expect("known page");
        let pool = Arc::downgrade(&slice(a[0]).pool);
        // Every request on the pool goes through the same mapping, each
        // page at its place in it, with no view of its own to map and unmap.
        let (first, third) = (
            View::map(&slice(a[0]), 1).expect("mapped"),
            View::map(&slice(a[2]), 1).expect("mapped"),
        );
        assert_eq!(third.at() - first.at(), 2 * PAGE);
        let at = first.at();
        drop((first, third));
        assert_eq!(View::map(&slice(a[1]), 2).expect("mapped").at(), at + PAGE);
        assert!(write_physical(&a, 2 * PAGE + 3, &[4, 5]));
        assert_eq!(page_bytes(a[2])[3..5], [4, 5]);
        // The mapping lives as long as the pool: until its pages are freed.
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert!(pool.upgrade().is_none());
    }

    /// macOS: the pool's own mapping shows what the title writes at any of
    /// its addresses, whatever is mapped and unmapped (a GPU buffer over it
    /// stays valid), and a cut of a view leaves the rest in place.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_pool_base_follows_title_mappings() {
        let pages = allocate(4);
        let info = physical().pages[&pages[0]].pool.info;
        let (a, b) = (reserve(4), reserve(4));
        assert_eq!(XMemMapPhysicalPages(a, 4, pages.as_ptr()), a);
        // Pages 1 and 2, scattered at `b`.
        let to = [b, at(b, 3)];
        let which = [pages[1], pages[2]];
        assert_eq!(
            XMemMapPhysicalPagesScatter(to.as_ptr(), 2, which.as_ptr()),
            1
        );
        // SAFETY: all mapped read-write; the base is the pool's whole.
        unsafe {
            at(a, 2).cast::<u8>().write(0x42);
            assert_eq!(at(b, 3).cast::<u8>().read(), 0x42);
            assert_eq!((info.base as *const u8).add(2 * PAGE).read(), 0x42);
            (info.base as *mut u8).add(PAGE + 9).write(0x24);
            assert_eq!(b.cast::<u8>().add(9).read(), 0x24);
            assert_eq!(at(a, 1).cast::<u8>().add(9).read(), 0x24);
        }
        // Cut the middle of `a`; the ends stay, and so does the pool.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 2, std::ptr::null()),
            at(a, 1)
        );
        assert!(mapped(a) && mapped(at(a, 3)));
        assert!(!mapped(at(a, 1)) && !mapped(at(a, 2)));
        assert_eq!(pools().iter().find(|p| p.id == info.id), Some(&info));
        // Mapped again, in the other order.
        let swapped = [pages[2], pages[1]];
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 2, swapped.as_ptr()),
            at(a, 1)
        );
        // SAFETY: mapped read-write.
        unsafe {
            assert_eq!(at(a, 1).cast::<u8>().read(), 0x42);
            assert_eq!(at(a, 2).cast::<u8>().add(9).read(), 0x24);
        }
        for p in [a, b] {
            assert_eq!(XMemMapPhysicalPages(p, 4, std::ptr::null()), p);
        }
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// Records what an observer is told on this thread, per test.
    #[derive(Default)]
    struct Log;

    thread_local! {
        static EVENTS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    impl Observer for Log {
        fn pool_created(&self, pool: &PoolInfo) {
            EVENTS.with(|e| e.borrow_mut().push(format!("created {}", pool.len)));
        }
        fn pool_released(&self, pool: &PoolInfo) {
            EVENTS.with(|e| e.borrow_mut().push(format!("released {}", pool.len)));
        }
        fn mapped(&self, at: usize, len: usize, pool: &PoolInfo, offset: usize) {
            EVENTS.with(|e| {
                e.borrow_mut()
                    .push(format!("mapped {at:#x} {len:#x} {} {offset:#x}", pool.id));
            });
        }
        fn unmapped(&self, at: usize, len: usize) {
            EVENTS.with(|e| e.borrow_mut().push(format!("unmapped {at:#x} {len:#x}")));
        }
    }

    #[test]
    fn the_observer_hears_of_pools_and_mappings() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            set_observer(Some(Arc::new(Log)));
        });
        EVENTS.with(|e| e.borrow_mut().clear());
        // A size no other test uses, to find the pool.
        let pages = allocate(7);
        let info = pools()
            .into_iter()
            .find(|p| p.len == 7 * PAGE)
            .expect("the pool is listed");
        assert_ne!(info.id, 0);
        #[cfg(target_os = "macos")]
        {
            assert_ne!(info.base, 0);
            assert_eq!(info.base % 0x4000, 0);
            assert!(info.memory_entry.is_some());
        }
        let a = reserve(7);
        assert_eq!(XMemMapPhysicalPages(a, 7, pages.as_ptr()), a);
        // One run, one mapping.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        let (x, id) = (a as usize, info.id);
        let events = EVENTS.with(|e| e.borrow().clone());
        let mut want = vec![
            format!("created {}", 7 * PAGE),
            format!("mapped {x:#x} {:#x} {id} 0x0", 7 * PAGE),
        ];
        if cfg!(windows) {
            // A view goes whole, and the parts outside the range come back.
            want.push(format!("unmapped {x:#x} {:#x}", 7 * PAGE));
            want.push(format!("mapped {x:#x} {PAGE:#x} {id} 0x0"));
            want.push(format!(
                "mapped {:#x} {:#x} {id} {:#x}",
                at(a, 2) as usize,
                5 * PAGE,
                2 * PAGE
            ));
        } else {
            want.push(format!("unmapped {:#x} {PAGE:#x}", at(a, 1) as usize));
        }
        assert_eq!(events, want);
        assert_eq!(XMemMapPhysicalPages(a, 7, std::ptr::null()), a);
        EVENTS.with(|e| e.borrow_mut().clear());
        assert_eq!(XMemFreePhysicalPages(7, pages.as_ptr()), 1);
        assert_eq!(
            EVENTS.with(|e| e.borrow().clone()),
            [format!("released {}", 7 * PAGE)]
        );
        assert!(pools().iter().all(|p| p.id != info.id));
    }
}
