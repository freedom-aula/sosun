//! The native host: what `host.rs` is for D3D12, over any [`GraphicsBackend`].
//!
//! One thread drives it (backends hold Objective-C objects). Queues, fences, pipelines, descriptor
//! heaps and textures are made here; title lists are [`NativeList`]s; [`NativeHost::execute`] turns
//! lists into passes and submits them without waiting. Title memory is wrapped lazily, one buffer
//! per pool (see [`super::memory`]); nothing is copied to or from it.

use super::addr::{self, Mapping, Pages, Pairs};
use super::descriptors::{Decoded, decode};
use super::list::{Binding, Cmd, ComputePso, NativeHeap, NativeList, Root, Targets};
use super::memory::PoolTable;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use victory_backend::backend::{Dispatch, Heaps, RootArgs};
use victory_backend::resources::*;

/// A title resource object the host knows by its key (the object's address on the title's side).
enum Res<B: GraphicsBackend> {
    /// A buffer: title address and size.
    Buffer(u64, u64),
    Texture(Rc<B::Texture>),
}

pub struct NativeHost<B: GraphicsBackend> {
    pub backend: B,
    memory: Arc<PoolTable>,
    /// A buffer over each pool, made the first time the GPU needs it.
    buffers: RefCell<HashMap<usize, Rc<B::Buffer>>>,
    resources: RefCell<HashMap<usize, Res<B>>>,
    heaps: RefCell<Vec<Rc<NativeHeap<B>>>>,
    contract: RefCell<Contract<B>>,
}

/// The host's side of the pool-offset contract ([`super::addr`]).
struct Contract<B: GraphicsBackend> {
    pairs: Pairs,
    /// Superseded tables, kept while submissions that read them may still run.
    // shortcut: the GPU is assumed not to be more than `RETIRED` table rebuilds behind.
    retired: VecDeque<Rc<Pages<B>>>,
    /// The memory the null view (pair 0) has, with size 0.
    null: Option<Rc<Pages<B>>>,
    /// A resource heap of the host's own, for shaders that need the pairs and the title bound none.
    heap: Option<Rc<NativeHeap<B>>>,
}

impl<B: GraphicsBackend> Default for Contract<B> {
    fn default() -> Self {
        Contract {
            pairs: Pairs::default(),
            retired: VecDeque::new(),
            null: None,
            heap: None,
        }
    }
}

const RETIRED: usize = 8;

/// What one command resolved to: title addresses are made into buffers here, kept alive until the
/// submission has been handed over.
struct Prep<B: GraphicsBackend> {
    root: RootArgs,
    /// The pixel stage's own arguments.
    ps_root: Option<RootArgs>,
    bufs: Vec<Rc<B::Buffer>>,
    vertex: Vec<(Rc<B::Buffer>, *const u8)>,
    index: Option<(Rc<B::Buffer>, *const u8)>,
    blit: Vec<(Rc<B::Buffer>, *const u8)>,
    /// The resource heap the work needs in place of the one the title bound (none bound).
    heap: Option<Rc<NativeHeap<B>>>,
}

enum Group {
    Compute(usize, usize),
    Blit(usize, usize),
    Render(usize, usize),
    Clear(usize),
}

type HeapPair<B> = (Option<Rc<NativeHeap<B>>>, Option<Rc<NativeHeap<B>>>);

fn heaps<B: GraphicsBackend>(h: &HeapPair<B>) -> Heaps<'_, B> {
    Heaps {
        resource: h.0.as_ref().map(|h| &h.heap),
        sampler: h.1.as_ref().map(|h| &h.heap),
    }
}

impl<B: GraphicsBackend> NativeHost<B> {
    pub fn new(backend: B) -> Self {
        Self::with_memory(backend, Arc::new(PoolTable::default()))
    }

    /// A host over a pool table the pool observer already feeds.
    pub fn with_memory(backend: B, memory: Arc<PoolTable>) -> Self {
        NativeHost {
            backend,
            memory,
            buffers: RefCell::default(),
            resources: RefCell::default(),
            heaps: RefCell::default(),
            contract: RefCell::default(),
        }
    }

    /// The table the pool observer keeps current; register it as (or call it from) the observer.
    pub fn memory(&self) -> Arc<PoolTable> {
        self.memory.clone()
    }

    /// The buffer and pool-mapping address of `[addr, addr + len)` of title memory.
    pub fn resolve(&self, addr: u64, len: u64) -> Result<(Rc<B::Buffer>, *const u8), String> {
        let mut buffers = self.buffers.borrow_mut();
        for gone in self.memory.take_released() {
            buffers.remove(&gone);
        }
        let (pool, ptr) = self
            .memory
            .resolve(addr, len)
            .ok_or_else(|| format!("title address {addr:#x}+{len:#x} is not in a mapped pool"))?;
        drop(buffers);
        Ok((self.pool_buffer(pool.id, pool.base, pool.len)?, ptr))
    }

    /// The buffer over pool `id`, made the first time the GPU needs it.
    fn pool_buffer(&self, id: usize, base: usize, len: usize) -> Result<Rc<B::Buffer>, String> {
        let mut buffers = self.buffers.borrow_mut();
        Ok(match buffers.entry(id) {
            std::collections::hash_map::Entry::Occupied(e) => e.get().clone(),
            std::collections::hash_map::Entry::Vacant(e) => e
                .insert(Rc::new(self.backend.wrap_title_memory(base as *mut u8, len)?))
                .clone(),
        })
    }

    pub fn compute_pipeline(
        &self,
        dxil: &[u8],
        threads: [usize; 3],
    ) -> Result<Rc<ComputePso<B>>, String> {
        Ok(Rc::new(ComputePso {
            pso: self.backend.create_compute_pipeline(dxil)?,
            threads,
            binding: None,
        }))
    }

    pub fn render_pipeline(
        &self,
        desc: &RenderPipelineDesc<'_>,
    ) -> Result<Rc<B::RenderPipeline>, String> {
        self.backend.create_render_pipeline(desc).map(Rc::new)
    }

    pub fn texture(&self, desc: &TextureDesc) -> Result<Rc<B::Texture>, String> {
        self.backend.create_texture(desc).map(Rc::new)
    }

    /// Names a buffer resource object `key` (what `CreateShaderResourceView` is passed) at title memory.
    pub fn register_buffer(&self, key: usize, addr: u64, len: u64) {
        self.resources
            .borrow_mut()
            .insert(key, Res::Buffer(addr, len));
    }

    pub fn register_texture(&self, key: usize, texture: &Rc<B::Texture>) {
        self.resources
            .borrow_mut()
            .insert(key, Res::Texture(texture.clone()));
    }

    /// A shader-visible heap over the title's descriptor memory at `start` (zeroed memory the title
    /// writes views into; 32 bytes an entry, 16 for samplers).
    pub fn heap(
        &self,
        kind: HeapKind,
        start: u64,
        count: usize,
    ) -> Result<Rc<NativeHeap<B>>, String> {
        // A resource heap is longer than the title made it by the entries that hold the pool-offset
        // contract's view pairs.
        let entries = if kind == HeapKind::Resource {
            if !addr::fits(count) {
                tracing::warn!(
                    count,
                    "a resource heap too long for title memory views: shaders that find memory by address will not run with it"
                );
            }
            count + addr::RESERVED
        } else {
            count
        };
        let heap = Rc::new(NativeHeap {
            heap: self.backend.create_heap(kind, entries)?,
            kind,
            start,
            count,
            pairs: Default::default(),
            table: Default::default(),
        });
        self.heaps.borrow_mut().push(heap.clone());
        Ok(heap)
    }

    /// The heap `handle` (a descriptor's address, a root table's) lies in, and the entry's index.
    pub fn heap_at(&self, handle: u64) -> Option<(Rc<NativeHeap<B>>, usize)> {
        self.heaps.borrow().iter().find_map(|h| {
            let index = (handle.checked_sub(h.start)? / Self::step(h.kind)) as usize;
            (index < h.count).then(|| (h.clone(), index))
        })
    }

    /// The title released the heap whose descriptors start at `start`.
    pub fn drop_heap(&self, start: u64) {
        self.heaps.borrow_mut().retain(|h| h.start != start);
    }

    /// The title released resource object `key`: later views naming it decode to nothing.
    pub fn forget_resource(&self, key: usize) {
        self.resources.borrow_mut().remove(&key);
    }

    /// Whether resource object `key` is registered (as a buffer or a texture).
    pub fn knows(&self, key: usize) -> bool {
        self.resources.borrow().contains_key(&key)
    }

    fn step(kind: HeapKind) -> u64 {
        u64::from(crate::descriptor::increment(u32::from(kind == HeapKind::Sampler)).unwrap_or(32))
    }

    /// The title wrote a view at descriptor `handle` (the call `descriptor::mirror` makes on
    /// D3D12): decode its record and write the backend's entry. Stale or unsupported views leave
    /// the entry as it was; returns whether an entry was written.
    pub fn mirror(&self, handle: u64) -> Result<bool, String> {
        let found = self.heaps.borrow().iter().find_map(|h| {
            let at = handle.checked_sub(h.start)?;
            let index = (at / Self::step(h.kind)) as usize;
            (index < h.count).then(|| (h.clone(), index))
        });
        let Some((heap, index)) = found else {
            return Ok(false);
        };
        let Some(view) = crate::descriptor::view(handle) else {
            return Ok(false);
        };
        let resources = self.resources.borrow();
        let buffer_of = |r: usize| match resources.get(&r) {
            Some(Res::Buffer(a, n)) => Some((*a, *n)),
            _ => None,
        };
        let Some(decoded) = decode(&view, &buffer_of) else {
            return Ok(false);
        };
        match decoded {
            Decoded::Buffer { addr, size } => {
                let (buf, ptr) = self.resolve(addr, u64::from(size))?;
                self.backend
                    .write_buffer(&heap.heap, index, &buf, ptr, size);
            }
            Decoded::Image {
                resource,
                mips,
                format,
            } => {
                let Some(Res::Texture(tex)) = resources.get(&resource) else {
                    return Ok(false);
                };
                match (mips, format) {
                    (Some((first, count)), Some(format)) if count > 0 => {
                        let v = self.backend.create_texture_view(
                            tex,
                            &ViewDesc {
                                format,
                                mips: (first as usize, count as usize),
                                slices: (0, 1),
                            },
                        )?;
                        self.backend.write_texture(&heap.heap, index, &v, 0.0);
                    }
                    _ => self.backend.write_texture(&heap.heap, index, tex, 0.0),
                }
            }
            Decoded::Sampler { desc, lod_bias } => {
                let s = self.backend.create_sampler(&desc)?;
                self.backend.write_sampler(&heap.heap, index, &s, lod_bias);
            }
        }
        Ok(true)
    }

    /// The arguments of a translated title shader: its user-data block, then its root views.
    fn translated_args(
        &self,
        root: &[Option<Root<B>>],
        b: &Binding,
        heap: Option<&Rc<NativeHeap<B>>>,
        p: &mut Prep<B>,
    ) -> Result<(), String> {
        use victory_shader::emit::{ROOT_SRV_VIEW, ROOT_UAV_VIEW, ROOT_VIEW_OFFSET};
        use victory_shader::RootType;
        let set = |slot: u8| {
            root.get(usize::from(slot))
                .and_then(|r| r.as_ref())
                .ok_or_else(|| format!("root parameter {slot} was never set"))
        };
        let mut args = std::mem::take(&mut p.root);
        if b.dwords > 0 {
            let mut values = vec![0u32; b.dwords];
            for u in &b.user_data {
                values[u.index as usize] = match (u.root, set(u.slot)?) {
                    (RootType::Constants, Root::Constants(c)) => {
                        c.get(u.dword as usize).copied().unwrap_or(0)
                    }
                    // The byte offset of the table's first entry in its heap.
                    (RootType::Table, Root::Table(h, at)) => (*at as u64 * Self::step(h.kind)) as u32,
                    (RootType::Descriptor, Root::View(a)) if u.dword < 2 => {
                        (a >> (32 * u.dword)) as u32
                    }
                    // A view of the pool holding the root view, and the address's offset in it.
                    (RootType::Descriptor, Root::View(a))
                        if matches!(u.dword, ROOT_SRV_VIEW | ROOT_UAV_VIEW | ROOT_VIEW_OFFSET) =>
                    {
                        let heap = match &p.heap {
                            Some(h) => h.clone(),
                            None => match heap {
                                Some(h) => h.clone(),
                                None => self.host_heap()?,
                            },
                        };
                        p.heap = Some(heap.clone());
                        let (view, offset) = self.root_view(&heap, *a);
                        match u.dword {
                            ROOT_SRV_VIEW => view,
                            ROOT_UAV_VIEW => view + 1,
                            _ => offset,
                        }
                    }
                    (kind, _) => {
                        return Err(format!(
                            "root parameter {} is not what the shader reads it as ({kind:?}, dword {})",
                            u.slot, u.dword
                        ));
                    }
                };
            }
            args = args.constants(&values);
        }
        for &(slot, _) in &b.views {
            let Root::View(a) = set(slot)? else {
                return Err(format!("root parameter {slot} is not a root view"));
            };
            let (buf, ptr) = self.resolve(*a, 1)?;
            let gpu = self.backend.gpu_address(&buf, ptr);
            p.bufs.push(buf);
            args = args.address(gpu);
        }
        p.root = args;
        Ok(())
    }

    /// The page table of the pool-offset contract for `heap` and the pool table as it is now:
    /// rebuilt, into a buffer of its own, when a pool or a mapping changed since it was made.
    fn memory_table(&self, heap: &NativeHeap<B>) -> Result<Rc<Pages<B>>, String> {
        if !addr::fits(heap.count) {
            return Err(format!(
                "a resource heap of {} entries has no room for title memory views",
                heap.count
            ));
        }
        let (generation, mappings) = self.memory.mappings();
        if let Some((g, t)) = &*heap.table.borrow()
            && *g == generation
        {
            return Ok(t.clone());
        }
        let maps: Vec<Mapping> = mappings
            .iter()
            .map(|(at, len, pool, offset)| Mapping {
                at: *at as u64,
                len: *len as u64,
                pool: pool.id,
                base: pool.base,
                pool_len: pool.len as u64,
                offset: *offset as u64,
            })
            .collect();
        let mut c = self.contract.borrow_mut();
        let words = addr::build(&maps, &mut c.pairs, heap.count as u32);
        let table = Rc::new(Pages::new(&self.backend, words.len() * 4)?);
        table.write(&words);
        if let Some((_, old)) = heap.table.borrow_mut().replace((generation, table.clone())) {
            c.retired.push_back(old);
            while c.retired.len() > RETIRED {
                c.retired.pop_front();
            }
        }
        Ok(table)
    }

    /// Writes the view pairs the table names into `heap`'s reserved entries (those it lacks).
    fn write_pairs(&self, heap: &NativeHeap<B>) -> Result<(), String> {
        let mut c = self.contract.borrow_mut();
        let have = heap.pairs.get();
        let want = c.pairs.keys.len() + 1;
        for n in have..want {
            let index = addr::pair_index(heap.count as u32, n as u32) as usize;
            let (buffer, ptr, size) = if n == 0 {
                if c.null.is_none() {
                    c.null = Some(Rc::new(Pages::new(&self.backend, 1)?));
                }
                let null = c.null.as_ref().expect("made above");
                (null.buf.clone(), null.ptr().cast_const(), 0)
            } else {
                let key = c.pairs.keys[n - 1];
                let buffer = self.pool_buffer(key.pool, key.base, key.len as usize)?;
                let (from, size) = Pairs::span(&key);
                (buffer, (key.base + from as usize) as *const u8, size)
            };
            // Both of the pair name the same bytes (the converter's descriptors are the same).
            for i in [index, index + 1] {
                self.backend.write_buffer(&heap.heap, i, &buffer, ptr, size);
            }
        }
        heap.pairs.set(want);
        Ok(())
    }

    /// The pair view and the offset in it of root view address `a` (see [`addr::root_view`]).
    fn root_view(&self, heap: &NativeHeap<B>, a: u64) -> (u32, u32) {
        let maps = self.mapping_list();
        addr::root_view(&maps, &mut self.contract.borrow_mut().pairs, heap.count as u32, a)
    }

    /// The pool table as the page table builder reads it.
    fn mapping_list(&self) -> Vec<Mapping> {
        self.memory
            .mappings()
            .1
            .iter()
            .map(|(at, len, pool, offset)| Mapping {
                at: *at as u64,
                len: *len as u64,
                pool: pool.id,
                base: pool.base,
                pool_len: pool.len as u64,
                offset: *offset as u64,
            })
            .collect()
    }

    /// What a shader that finds title memory through the page table is given besides its user data:
    /// the table (the last root parameter) and, in the heap it reads, the view pairs.
    fn memory_args(
        &self,
        heap: Option<&Rc<NativeHeap<B>>>,
        p: &mut Prep<B>,
    ) -> Result<(), String> {
        let heap = match heap {
            Some(h) => h.clone(),
            None => self.host_heap()?,
        };
        let table = self.memory_table(&heap)?;
        self.write_pairs(&heap)?;
        let gpu = self.backend.gpu_address(&table.buf, table.ptr());
        p.bufs.push(table.buf.clone());
        p.root = std::mem::take(&mut p.root).address(gpu);
        p.heap = Some(heap);
        Ok(())
    }

    /// A resource heap of the host's own, for work that needs the view pairs and has no heap.
    fn host_heap(&self) -> Result<Rc<NativeHeap<B>>, String> {
        if let Some(h) = &self.contract.borrow().heap {
            return Ok(h.clone());
        }
        let heap = Rc::new(NativeHeap {
            heap: self.backend.create_heap(HeapKind::Resource, addr::RESERVED)?,
            kind: HeapKind::Resource,
            start: 0,
            count: 0,
            pairs: Default::default(),
            table: Default::default(),
        });
        self.contract.borrow_mut().heap = Some(heap.clone());
        Ok(heap)
    }

    fn root_args(&self, root: &[Option<Root<B>>], p: &mut Prep<B>) -> Result<(), String> {
        for (i, r) in root.iter().enumerate() {
            let args = std::mem::take(&mut p.root);
            p.root = match r
                .as_ref()
                .ok_or_else(|| format!("root parameter {i} was never set"))?
            {
                Root::Constants(w) => args.constants(w),
                Root::View(a) => {
                    let (buf, ptr) = self.resolve(*a, 1)?;
                    let gpu = self.backend.gpu_address(&buf, ptr);
                    p.bufs.push(buf);
                    args.address(gpu)
                }
                Root::Table(h, at) => args.address(self.backend.heap_address(&h.heap, *at)),
            };
        }
        Ok(())
    }

    fn prep(&self, cmd: &Cmd<B>) -> Result<Prep<B>, String> {
        let mut p = Prep {
            root: RootArgs::default(),
            ps_root: None,
            bufs: vec![],
            vertex: vec![],
            index: None,
            blit: vec![],
            heap: None,
        };
        match cmd {
            Cmd::Dispatch {
                root, pso, heaps, ..
            } => match &pso.binding {
                Some(b) => {
                    self.translated_args(root, b, heaps.0.as_ref(), &mut p)?;
                    if b.memory {
                        self.memory_args(heaps.0.as_ref(), &mut p)?;
                    } else if let Some(h) = p.heap.clone() {
                        self.write_pairs(&h)?;
                    }
                }
                None => self.root_args(root, &mut p)?,
            },
            Cmd::Draw {
                root,
                binding,
                heaps,
                vertex,
                index,
                ..
            } => {
                match binding {
                    Some(b) => {
                        self.translated_args(root, b, heaps.0.as_ref(), &mut p)?;
                        if let Some(ps) = &b.ps {
                            let vs_args = std::mem::take(&mut p.root);
                            self.translated_args(root, ps, heaps.0.as_ref(), &mut p)?;
                            p.ps_root = Some(std::mem::replace(&mut p.root, vs_args));
                        }
                        if let Some(h) = p.heap.clone() {
                            self.write_pairs(&h)?;
                        }
                    }
                    None => self.root_args(root, &mut p)?,
                }
                for a in vertex {
                    p.vertex.push(self.resolve(*a, 1)?);
                }
                if let Some((a, _)) = index {
                    p.index = Some(self.resolve(*a, 1)?);
                }
            }
            Cmd::Copy { dst, src, len } => {
                p.blit.push(self.resolve(*src, *len as u64)?);
                p.blit.push(self.resolve(*dst, *len as u64)?);
            }
            Cmd::Fill { dst, len, .. } => p.blit.push(self.resolve(*dst, *len as u64)?),
            Cmd::Clear { .. } => {}
        }
        Ok(p)
    }

    /// `ExecuteCommandLists` plus the queue's waits and signals: resolve every title address, group
    /// the commands into compute, render and blit passes in order, and queue them. Never waits.
    pub fn execute(
        &self,
        queue: &B::Queue,
        lists: &[&NativeList<B>],
        waits: &[(&B::Fence, u64)],
        signals: &[(&B::Fence, u64)],
    ) -> Result<(), String> {
        let cmds: Vec<&Cmd<B>> = lists.iter().flat_map(|l| l.cmds.iter()).collect();
        let preps = cmds
            .iter()
            .map(|c| self.prep(c))
            .collect::<Result<Vec<_>, _>>()?;

        let mut groups = vec![];
        let mut i = 0;
        while i < cmds.len() {
            let mut j = i + 1;
            let same = |a: &Cmd<B>, b: &Cmd<B>| match (a, b) {
                (Cmd::Dispatch { .. }, Cmd::Dispatch { .. }) => true,
                (Cmd::Copy { .. } | Cmd::Fill { .. }, Cmd::Copy { .. } | Cmd::Fill { .. }) => true,
                (Cmd::Draw { targets: x, .. }, Cmd::Draw { targets: y, .. }) => x.same(y),
                _ => false,
            };
            while j < cmds.len() && same(cmds[i], cmds[j]) {
                j += 1;
            }
            groups.push(match cmds[i] {
                Cmd::Dispatch { .. } => Group::Compute(i, j),
                Cmd::Copy { .. } | Cmd::Fill { .. } => Group::Blit(i, j),
                Cmd::Draw { .. } => Group::Render(i, j),
                Cmd::Clear { .. } => {
                    j = i + 1;
                    Group::Clear(i)
                }
            });
            i = j;
        }

        // Every pool buffer the work reaches is resident for the queue's work from now on (once).
        let mut buffers: Vec<&B::Buffer> = Vec::new();
        for p in &preps {
            buffers.extend(p.bufs.iter().map(|b| &**b));
            buffers.extend(p.vertex.iter().map(|b| &*b.0));
            buffers.extend(p.index.iter().map(|b| &*b.0));
            buffers.extend(p.blit.iter().map(|b| &*b.0));
        }
        if let Err(e) = self.backend.make_resident(queue, &buffers) {
            tracing::warn!(error = %e, "pool buffers were not made resident");
        }

        let compute: Vec<Vec<Dispatch<'_, B>>> = groups
            .iter()
            .map(|g| match g {
                Group::Compute(a, b) => (*a..*b)
                    .map(|k| {
                        let Cmd::Dispatch {
                            pso,
                            heaps: h,
                            groups,
                            ..
                        } = cmds[k]
                        else {
                            unreachable!()
                        };
                        Dispatch {
                            pipeline: &pso.pso,
                            root: preps[k].root.clone(),
                            resources: preps[k].bufs.iter().map(|b| &**b).collect(),
                            heaps: match &preps[k].heap {
                                Some(m) => Heaps {
                                    resource: Some(&m.heap),
                                    sampler: h.1.as_ref().map(|s| &s.heap),
                                },
                                None => heaps(h),
                            },
                            groups: *groups,
                            threads_per_group: pso.threads,
                        }
                    })
                    .collect(),
                _ => vec![],
            })
            .collect();

        let mut passes: Vec<Pass<'_, B>> = vec![];
        for (n, g) in groups.iter().enumerate() {
            passes.push(match g {
                Group::Compute(..) => Pass::Compute(&compute[n]),
                Group::Blit(a, b) => Pass::Blit(
                    (*a..*b)
                        .map(|k| match cmds[k] {
                            Cmd::Copy { len, .. } => {
                                let p = &preps[k].blit;
                                BlitOp::Copy {
                                    src: (&*p[0].0, p[0].1),
                                    dst: (&*p[1].0, p[1].1),
                                    len: *len,
                                }
                            }
                            Cmd::Fill { len, value, .. } => {
                                let p = &preps[k].blit[0];
                                BlitOp::Fill {
                                    dst: (&*p.0, p.1),
                                    len: *len,
                                    value: *value,
                                }
                            }
                            _ => unreachable!(),
                        })
                        .collect(),
                ),
                Group::Clear(k) => {
                    let Cmd::Clear { color, depth } = cmds[*k] else {
                        unreachable!()
                    };
                    Pass::Render(RenderPass {
                        color: color
                            .iter()
                            .map(|(t, c)| ColorTarget {
                                texture: &**t,
                                clear: Some(*c),
                            })
                            .collect(),
                        depth: depth.as_ref().map(|(t, d)| DepthTarget {
                            texture: &**t,
                            clear: Some(*d),
                        }),
                        draws: vec![],
                    })
                }
                Group::Render(a, b) => {
                    let Cmd::Draw {
                        targets: Targets { color, depth },
                        ..
                    } = cmds[*a]
                    else {
                        unreachable!()
                    };
                    Pass::Render(RenderPass {
                        color: color
                            .iter()
                            .map(|t| ColorTarget {
                                texture: &**t,
                                clear: None,
                            })
                            .collect(),
                        depth: depth.as_ref().map(|t| DepthTarget {
                            texture: &**t,
                            clear: None,
                        }),
                        draws: (*a..*b)
                            .map(|k| {
                                let Cmd::Draw {
                                    pso,
                                    topology,
                                    heaps: h,
                                    state,
                                    call,
                                    ..
                                } = cmds[k]
                                else {
                                    unreachable!()
                                };
                                Draw {
                                    pipeline: &**pso,
                                    topology: *topology,
                                    state: *state,
                                    root: preps[k].root.clone(),
                                    fragment_root: preps[k].ps_root.clone(),
                                    heaps: match &preps[k].heap {
                                        Some(m) => Heaps {
                                            resource: Some(&m.heap),
                                            sampler: h.1.as_ref().map(|s| &s.heap),
                                        },
                                        None => heaps(h),
                                    },
                                    resources: preps[k].bufs.iter().map(|b| &**b).collect(),
                                    vertex_buffers: preps[k]
                                        .vertex
                                        .iter()
                                        .map(|(b, p)| (&**b, *p))
                                        .collect(),
                                    index_buffer: preps[k].index.as_ref().map(|(b, p)| (&**b, *p)),
                                    call: *call,
                                }
                            })
                            .collect(),
                    })
                }
            });
        }
        self.backend.submit_passes(
            queue,
            &PassSubmission {
                waits,
                passes: &passes,
                signals,
            },
        )
    }
}
