//! A command list on the native host: the calls a title makes on its list (`SetPipelineState`,
//! `SetComputeRoot*`, `Dispatch`, `CopyBufferRegion`, `ClearRenderTargetView`, `DrawInstanced`, ...)
//! recorded as they arrive and turned into backend passes at submission ([`super::host::NativeHost::execute`]).
//!
//! Title addresses are kept as addresses: they resolve to GPU buffers when the list is submitted, by
//! which time the pool observer has told us about every mapping the title made.

use std::rc::Rc;
use victory_backend::resources::{DrawCall, GraphicsBackend, HeapKind, Topology};

/// A heap the title wrote views into: `start` is its descriptor memory (title addresses).
pub struct NativeHeap<B: GraphicsBackend> {
    pub heap: B::Heap,
    pub kind: HeapKind,
    pub start: u64,
    pub count: usize,
}

/// A compute pipeline and the group size its shader was written with (`numthreads`).
pub struct ComputePso<B: GraphicsBackend> {
    pub pso: B::Pipeline,
    pub threads: [usize; 3],
}

/// One root parameter as the title set it.
pub enum Root<B: GraphicsBackend> {
    Constants(Vec<u32>),
    /// A root SRV, UAV or CBV: a title address.
    View(u64),
    /// A descriptor table: heap and the index of its first entry.
    Table(Rc<NativeHeap<B>>, usize),
}

impl<B: GraphicsBackend> Clone for Root<B> {
    fn clone(&self) -> Self {
        match self {
            Root::Constants(c) => Root::Constants(c.clone()),
            Root::View(a) => Root::View(*a),
            Root::Table(h, i) => Root::Table(h.clone(), *i),
        }
    }
}

type Heaps<B> = (Option<Rc<NativeHeap<B>>>, Option<Rc<NativeHeap<B>>>);

pub struct Targets<B: GraphicsBackend> {
    pub color: Vec<Rc<B::Texture>>,
    pub depth: Option<Rc<B::Texture>>,
}

impl<B: GraphicsBackend> Clone for Targets<B> {
    fn clone(&self) -> Self {
        Targets {
            color: self.color.clone(),
            depth: self.depth.clone(),
        }
    }
}

impl<B: GraphicsBackend> Targets<B> {
    pub(super) fn same(&self, o: &Targets<B>) -> bool {
        self.color.len() == o.color.len()
            && self
                .color
                .iter()
                .zip(&o.color)
                .all(|(a, b)| Rc::ptr_eq(a, b))
            && match (&self.depth, &o.depth) {
                (Some(a), Some(b)) => Rc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

pub(super) enum Cmd<B: GraphicsBackend> {
    Dispatch {
        pso: Rc<ComputePso<B>>,
        root: Vec<Option<Root<B>>>,
        heaps: Heaps<B>,
        groups: [usize; 3],
    },
    Copy {
        dst: u64,
        src: u64,
        len: usize,
    },
    Fill {
        dst: u64,
        len: usize,
        value: u8,
    },
    Clear {
        color: Option<(Rc<B::Texture>, [f32; 4])>,
        depth: Option<(Rc<B::Texture>, f32)>,
    },
    Draw {
        pso: Rc<B::RenderPipeline>,
        topology: Topology,
        root: Vec<Option<Root<B>>>,
        heaps: Heaps<B>,
        targets: Targets<B>,
        vertex: Vec<u64>,
        index: Option<(u64, bool)>,
        call: DrawCall,
    },
}

/// The state a title sets between commands.
struct State<B: GraphicsBackend> {
    compute: Option<Rc<ComputePso<B>>>,
    graphics: Option<(Rc<B::RenderPipeline>, Topology)>,
    compute_root: Vec<Option<Root<B>>>,
    graphics_root: Vec<Option<Root<B>>>,
    heaps: Heaps<B>,
    targets: Targets<B>,
    vertex: Vec<u64>,
    index: Option<(u64, bool)>,
}

pub struct NativeList<B: GraphicsBackend> {
    pub(super) cmds: Vec<Cmd<B>>,
    state: State<B>,
}

fn set<B: GraphicsBackend>(root: &mut Vec<Option<Root<B>>>, slot: usize, value: Root<B>) {
    if root.len() <= slot {
        root.resize_with(slot + 1, || None);
    }
    root[slot] = Some(value);
}

impl<B: GraphicsBackend> Default for NativeList<B> {
    fn default() -> Self {
        NativeList {
            cmds: Vec::new(),
            state: State {
                compute: None,
                graphics: None,
                compute_root: Vec::new(),
                graphics_root: Vec::new(),
                heaps: (None, None),
                targets: Targets {
                    color: Vec::new(),
                    depth: None,
                },
                vertex: Vec::new(),
                index: None,
            },
        }
    }
}

impl<B: GraphicsBackend> NativeList<B> {
    /// `Reset`: forget everything recorded and set.
    pub fn reset(&mut self) {
        *self = NativeList::default();
    }

    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }

    /// `SetDescriptorHeaps`.
    pub fn set_heaps(
        &mut self,
        resource: Option<&Rc<NativeHeap<B>>>,
        sampler: Option<&Rc<NativeHeap<B>>>,
    ) {
        self.state.heaps = (resource.cloned(), sampler.cloned());
    }

    pub fn set_compute_pipeline(&mut self, pso: &Rc<ComputePso<B>>) {
        self.state.compute = Some(pso.clone());
    }

    pub fn set_graphics_pipeline(&mut self, pso: &Rc<B::RenderPipeline>, topology: Topology) {
        self.state.graphics = Some((pso.clone(), topology));
    }

    /// `SetComputeRoot32BitConstants` (the whole parameter).
    pub fn set_compute_root_constants(&mut self, slot: usize, words: &[u32]) {
        set(
            &mut self.state.compute_root,
            slot,
            Root::Constants(words.to_vec()),
        );
    }

    /// `SetComputeRootShaderResourceView` / `UnorderedAccessView` / `ConstantBufferView`.
    pub fn set_compute_root_view(&mut self, slot: usize, address: u64) {
        set(&mut self.state.compute_root, slot, Root::View(address));
    }

    /// `SetComputeRootDescriptorTable`.
    pub fn set_compute_root_table(&mut self, slot: usize, heap: &Rc<NativeHeap<B>>, index: usize) {
        set(
            &mut self.state.compute_root,
            slot,
            Root::Table(heap.clone(), index),
        );
    }

    pub fn set_graphics_root_constants(&mut self, slot: usize, words: &[u32]) {
        set(
            &mut self.state.graphics_root,
            slot,
            Root::Constants(words.to_vec()),
        );
    }

    pub fn set_graphics_root_view(&mut self, slot: usize, address: u64) {
        set(&mut self.state.graphics_root, slot, Root::View(address));
    }

    pub fn set_graphics_root_table(&mut self, slot: usize, heap: &Rc<NativeHeap<B>>, index: usize) {
        set(
            &mut self.state.graphics_root,
            slot,
            Root::Table(heap.clone(), index),
        );
    }

    /// `Dispatch`. Needs the compute pipeline set.
    pub fn dispatch(&mut self, x: u32, y: u32, z: u32) -> Result<(), String> {
        let pso = self
            .state
            .compute
            .clone()
            .ok_or("dispatch without a compute pipeline")?;
        self.cmds.push(Cmd::Dispatch {
            pso,
            root: self.state.compute_root.clone(),
            heaps: self.state.heaps.clone(),
            groups: [x as usize, y as usize, z as usize],
        });
        Ok(())
    }

    /// `CopyBufferRegion` between title addresses. Runs on the GPU, in order with the rest.
    pub fn copy_buffer_region(&mut self, dst: u64, src: u64, len: usize) {
        self.cmds.push(Cmd::Copy { dst, src, len });
    }

    /// A byte fill of title memory (the console's `FillMemoryX`).
    pub fn fill(&mut self, dst: u64, len: usize, value: u8) {
        self.cmds.push(Cmd::Fill { dst, len, value });
    }

    /// `OMSetRenderTargets`.
    pub fn set_render_targets(&mut self, color: &[Rc<B::Texture>], depth: Option<&Rc<B::Texture>>) {
        self.state.targets = Targets {
            color: color.to_vec(),
            depth: depth.cloned(),
        };
    }

    /// `ClearRenderTargetView`. shortcut: each clear is a render pass of its own (load action
    /// clear); folding it into the following draws' pass saves one pass per clear.
    pub fn clear_render_target(&mut self, target: &Rc<B::Texture>, color: [f32; 4]) {
        self.cmds.push(Cmd::Clear {
            color: Some((target.clone(), color)),
            depth: None,
        });
    }

    pub fn clear_depth(&mut self, target: &Rc<B::Texture>, depth: f32) {
        self.cmds.push(Cmd::Clear {
            color: None,
            depth: Some((target.clone(), depth)),
        });
    }

    /// `IASetVertexBuffers` for one slot: the title address of its data (the stride is the pipeline's).
    pub fn set_vertex_buffer(&mut self, slot: usize, address: u64) {
        if self.state.vertex.len() <= slot {
            self.state.vertex.resize(slot + 1, 0);
        }
        self.state.vertex[slot] = address;
    }

    pub fn set_index_buffer(&mut self, address: u64, wide: bool) {
        self.state.index = Some((address, wide));
    }

    fn draw_call(&mut self, call: DrawCall) -> Result<(), String> {
        let (pso, topology) = self
            .state
            .graphics
            .clone()
            .ok_or("draw without a graphics pipeline")?;
        self.cmds.push(Cmd::Draw {
            pso,
            topology,
            root: self.state.graphics_root.clone(),
            heaps: self.state.heaps.clone(),
            targets: self.state.targets.clone(),
            vertex: self.state.vertex.clone(),
            index: self.state.index,
            call,
        });
        Ok(())
    }

    pub fn draw_instanced(
        &mut self,
        vertices: u32,
        instances: u32,
        first_vertex: u32,
        first_instance: u32,
    ) -> Result<(), String> {
        self.draw_call(DrawCall::Arrays {
            vertex_count: vertices,
            instance_count: instances,
            first_vertex,
            first_instance,
        })
    }

    pub fn draw_indexed_instanced(
        &mut self,
        indices: u32,
        instances: u32,
        first_index: u32,
        base_vertex: i32,
        first_instance: u32,
    ) -> Result<(), String> {
        let wide = self
            .state
            .index
            .ok_or("indexed draw without an index buffer")?
            .1;
        self.draw_call(DrawCall::Indexed {
            index_count: indices,
            instance_count: instances,
            first_index,
            base_vertex,
            first_instance,
            wide,
        })
    }
}
