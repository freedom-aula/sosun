//! Title memory for the native host: which physical-page pool a title address shows.
//!
//! On the native host a GPU buffer wraps a pool's own mapping (one `MTLBuffer` per pool, zero copy),
//! so title writes and GPU writes are the same bytes. There is no shadow copy, no write guard, no
//! copy-back: `shadow.rs`, `watch.rs` and the page bitmaps of `victory_shader::emit` are Windows
//! only and never reached from here. What is left is a table from title pages to
//! `(pool, offset)`, which the pool observer of `victory_hle::modules::xmem_physical` (branch
//! `native/host-mem`, merged by `native/xmem-rest`) keeps current. The types below have the
//! observer's shape, so the host implements `PoolObserver` for [`PoolTable`] with a one-line
//! forward per method; this crate does not depend on `victory-hle`.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// `xmem_physical::PoolInfo`.
#[derive(Clone, Debug)]
pub struct PoolInfo {
    pub id: usize,
    /// The pool's own read-write mapping, 16 KiB aligned, never moved until `pool_released`.
    pub base: usize,
    pub len: usize,
    pub memory_entry: Option<u32>,
}

/// `xmem_physical::Observer`.
pub trait PoolObserver: Send + Sync {
    fn pool_created(&self, pool: &PoolInfo);
    fn pool_released(&self, pool: &PoolInfo);
    fn mapped(&self, at: usize, len: usize, pool: &PoolInfo, offset: usize);
    fn unmapped(&self, at: usize, len: usize);
}

#[derive(Clone, Copy)]
struct Map {
    len: usize,
    pool: usize,
    offset: usize,
}

#[derive(Default)]
struct Inner {
    pools: BTreeMap<usize, PoolInfo>,
    /// Title address of a mapping to what it shows; mappings never overlap.
    maps: BTreeMap<usize, Map>,
    /// Pools released since the last [`PoolTable::take_released`]: their GPU buffers must go.
    released: Vec<usize>,
    /// Counts every change to pools and mappings ([`PoolTable::generation`]).
    generation: u64,
}

impl Inner {
    /// Removes `[at, at + len)` from the mappings, keeping what is left of any it cuts.
    fn cut(&mut self, at: usize, len: usize) {
        self.generation += 1;
        let end = at + len;
        let hit: Vec<usize> = self
            .maps
            .range(..end)
            .filter(|&(&a, m)| a + m.len > at)
            .map(|(&a, _)| a)
            .collect();
        for a in hit {
            let m = self.maps.remove(&a).expect("listed above");
            if a < at {
                self.maps.insert(a, Map { len: at - a, ..m });
            }
            if a + m.len > end {
                let cut = end - a;
                self.maps.insert(
                    end,
                    Map {
                        len: m.len - cut,
                        offset: m.offset + cut,
                        ..m
                    },
                );
            }
        }
    }
}

/// The title page to pool table. Thread safe: the observer is called from title threads.
#[derive(Default)]
pub struct PoolTable {
    inner: Mutex<Inner>,
}

impl PoolTable {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The pool `[addr, addr + len)` lies in, and the pool-mapping address of `addr`. The range must
    /// lie in one mapping (shortcut: a range over two adjacent mappings of one pool is refused; merge
    /// them here if a title builds a buffer view across a map boundary).
    pub fn resolve(&self, addr: u64, len: u64) -> Option<(PoolInfo, *const u8)> {
        let (addr, len) = (
            usize::try_from(addr).ok()?,
            usize::try_from(len.max(1)).ok()?,
        );
        let g = self.lock();
        let (&at, m) = g.maps.range(..=addr).next_back()?;
        if addr + len > at + m.len {
            return None;
        }
        let pool = g.pools.get(&m.pool)?;
        Some((
            pool.clone(),
            (pool.base + m.offset + (addr - at)) as *const u8,
        ))
    }

    /// A number that changes whenever a pool or a mapping does: what a table built from
    /// [`PoolTable::mappings`] is current for.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Every mapping, by title address: `(at, len, pool, offset in the pool)`.
    pub fn mappings(&self) -> (u64, Vec<(usize, usize, PoolInfo, usize)>) {
        let g = self.lock();
        let maps = g
            .maps
            .iter()
            .filter_map(|(&at, m)| Some((at, m.len, g.pools.get(&m.pool)?.clone(), m.offset)))
            .collect();
        (g.generation, maps)
    }

    /// Pools released since the last call; the host drops their buffers.
    pub fn take_released(&self) -> Vec<usize> {
        std::mem::take(&mut self.lock().released)
    }
}

impl PoolObserver for PoolTable {
    fn pool_created(&self, pool: &PoolInfo) {
        let mut g = self.lock();
        g.generation += 1;
        g.pools.insert(pool.id, pool.clone());
    }

    fn pool_released(&self, pool: &PoolInfo) {
        let mut g = self.lock();
        g.generation += 1;
        g.pools.remove(&pool.id);
        g.maps.retain(|_, m| m.pool != pool.id);
        g.released.push(pool.id);
    }

    fn mapped(&self, at: usize, len: usize, pool: &PoolInfo, offset: usize) {
        let mut g = self.lock();
        g.pools.entry(pool.id).or_insert_with(|| pool.clone());
        g.cut(at, len);
        g.maps.insert(
            at,
            Map {
                len,
                pool: pool.id,
                offset,
            },
        );
    }

    fn unmapped(&self, at: usize, len: usize) {
        self.lock().cut(at, len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(id: usize) -> PoolInfo {
        PoolInfo {
            id,
            base: 0x1000_0000 * id,
            len: 0x100_0000,
            memory_entry: None,
        }
    }

    #[test]
    fn maps_cut_and_resolve() {
        let t = PoolTable::default();
        let p = pool(1);
        t.pool_created(&p);
        t.mapped(0x7000_0000, 0x40_0000, &p, 0x1_0000);
        let (info, ptr) = t.resolve(0x7000_0100, 16).unwrap();
        assert_eq!((info.id, ptr as usize), (1, p.base + 0x1_0100));
        // Unmapping the middle leaves both ends, still showing the right offsets.
        t.unmapped(0x7010_0000, 0x10_0000);
        assert!(t.resolve(0x7010_0000, 1).is_none());
        let (_, ptr) = t.resolve(0x7020_0000, 4).unwrap();
        assert_eq!(ptr as usize, p.base + 0x1_0000 + 0x20_0000);
        // A range across the hole is refused; a remap over it replaces what it showed.
        assert!(t.resolve(0x700F_FFF0, 0x20).is_none());
        let q = pool(2);
        t.mapped(0x7000_0000, 0x1000, &q, 0);
        assert_eq!(t.resolve(0x7000_0010, 1).unwrap().0.id, 2);
        assert_eq!(t.resolve(0x7000_1000, 1).unwrap().0.id, 1);
        t.pool_released(&p);
        assert!(t.resolve(0x7000_1000, 1).is_none());
        assert_eq!(t.take_released(), vec![1]);
    }
}
