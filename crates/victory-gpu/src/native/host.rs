//! The native host: what `host.rs` is for D3D12, over any [`GraphicsBackend`].
//!
//! One thread drives it (backends hold Objective-C objects). Queues, fences, pipelines, descriptor
//! heaps and textures are made here; title lists are [`NativeList`]s; [`NativeHost::execute`] turns
//! lists into passes and submits them without waiting. Title memory is wrapped lazily, one buffer
//! per pool (see [`super::memory`]); nothing is copied to or from it.

use super::descriptors::{Decoded, decode};
use super::list::{Cmd, ComputePso, NativeHeap, NativeList, Root, Targets};
use super::memory::PoolTable;
use std::cell::RefCell;
use std::collections::HashMap;
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
}

/// What one command resolved to: title addresses are made into buffers here, kept alive until the
/// submission has been handed over.
struct Prep<B: GraphicsBackend> {
    root: RootArgs,
    bufs: Vec<Rc<B::Buffer>>,
    vertex: Vec<(Rc<B::Buffer>, *const u8)>,
    index: Option<(Rc<B::Buffer>, *const u8)>,
    blit: Vec<(Rc<B::Buffer>, *const u8)>,
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
        NativeHost {
            backend,
            memory: Arc::new(PoolTable::default()),
            buffers: RefCell::default(),
            resources: RefCell::default(),
            heaps: RefCell::default(),
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
        let buffer = match buffers.entry(pool.id) {
            std::collections::hash_map::Entry::Occupied(e) => e.get().clone(),
            std::collections::hash_map::Entry::Vacant(e) => e
                .insert(Rc::new(
                    self.backend
                        .wrap_title_memory(pool.base as *mut u8, pool.len)?,
                ))
                .clone(),
        };
        Ok((buffer, ptr))
    }

    pub fn compute_pipeline(
        &self,
        dxil: &[u8],
        threads: [usize; 3],
    ) -> Result<Rc<ComputePso<B>>, String> {
        Ok(Rc::new(ComputePso {
            pso: self.backend.create_compute_pipeline(dxil)?,
            threads,
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
        let heap = Rc::new(NativeHeap {
            heap: self.backend.create_heap(kind, count)?,
            kind,
            start,
            count,
        });
        self.heaps.borrow_mut().push(heap.clone());
        Ok(heap)
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
            bufs: vec![],
            vertex: vec![],
            index: None,
            blit: vec![],
        };
        match cmd {
            Cmd::Dispatch { root, .. } => self.root_args(root, &mut p)?,
            Cmd::Draw {
                root,
                vertex,
                index,
                ..
            } => {
                self.root_args(root, &mut p)?;
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
                            heaps: heaps(h),
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
                                    call,
                                    ..
                                } = cmds[k]
                                else {
                                    unreachable!()
                                };
                                Draw {
                                    pipeline: &**pso,
                                    topology: *topology,
                                    root: preps[k].root.clone(),
                                    heaps: heaps(h),
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
