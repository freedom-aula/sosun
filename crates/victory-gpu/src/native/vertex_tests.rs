//! Capture a native host submission at the backend boundary without a GPU or shader converter.

use super::host::NativeHost;
use super::list::NativeList;
use super::memory::{PoolInfo, PoolObserver};
use std::cell::RefCell;
use std::rc::Rc;
use victory_backend::backend::{ComputeBackend, RootLayout, Submission};
use victory_backend::resources::*;

#[test]
fn a_draw_with_only_vertex_slot_two_submits_its_buffer() {
    let host = NativeHost::new(Capture::default());
    let pool = PoolInfo {
        id: 1,
        base: 0x40000,
        len: 0x4000,
        memory_entry: None,
    };
    host.memory().mapped(0x80000, pool.len, &pool, 0);
    let mut list = NativeList::default();
    list.set_graphics_pipeline(&Rc::new(()), Topology::Triangles);
    list.set_render_targets(&[Rc::new(())], None);
    list.set_vertex_buffer(2, 0x80100);
    list.draw_instanced(3, 1, 0, 0).unwrap();
    host.execute(&(), &[&list], &[], &[]).unwrap();

    assert_eq!(host.unmapped(), 0, "empty vertex slots dropped the draw");
    assert_eq!(*host.backend.draws.borrow(), [vec![(2, 0x40000, 0x40100)]]);
}

#[derive(Default)]
struct Capture {
    // For each draw, the (slot, pool base, resolved address) bindings received.
    draws: RefCell<Vec<Vec<(usize, usize, usize)>>>,
}

impl GraphicsBackend for Capture {
    type RenderPipeline = ();

    fn submit_passes(&self, _: &(), work: &PassSubmission<'_, Self>) -> Result<(), String> {
        for pass in work.passes {
            let Pass::Render(render) = pass else {
                panic!("expected a render pass")
            };
            for draw in &render.draws {
                self.draws.borrow_mut().push(
                    draw.vertex_buffers
                        .iter()
                        .map(|&(slot, base, address)| (slot, *base, address as usize))
                        .collect(),
                );
            }
        }
        Ok(())
    }

    fn create_render_pipeline(&self, _: &RenderPipelineDesc<'_>) -> Result<(), String> {
        unreachable!()
    }
}

impl ComputeBackend for Capture {
    type Queue = ();
    type Fence = ();
    type Buffer = usize;
    type Pipeline = ();
    type Heap = ();

    fn wrap_title_memory(&self, base: *mut u8, _: usize) -> Result<usize, String> {
        Ok(base as usize)
    }

    fn create_queue(&self) -> Result<(), String> {
        unreachable!()
    }
    fn create_fence(&self, _: u64) -> Result<(), String> {
        unreachable!()
    }
    fn host_buffer(&self, _: usize) -> Result<(usize, *mut u8), String> {
        unreachable!()
    }
    fn gpu_address(&self, _: &usize, _: *const u8) -> u64 {
        unreachable!()
    }
    fn create_compute_pipeline(&self, _: &[u8]) -> Result<(), String> {
        unreachable!()
    }
    fn create_compute_pipeline_with(&self, _: &[u8], _: &RootLayout) -> Result<(), String> {
        unreachable!()
    }
    fn submit(&self, _: &(), _: &Submission<'_, Self>) -> Result<(), String> {
        unreachable!()
    }
    fn signal(&self, _: &(), _: u64) {
        unreachable!()
    }
    fn completed(&self, _: &()) -> u64 {
        unreachable!()
    }
    fn wait(&self, _: &(), _: u64, _: u64) -> bool {
        unreachable!()
    }
}

impl ResourceBackend for Capture {
    type Texture = ();
    type Sampler = ();

    fn create_heap(&self, _: HeapKind, _: usize) -> Result<(), String> {
        unreachable!()
    }
    fn heap_address(&self, _: &(), _: usize) -> u64 {
        unreachable!()
    }
    fn write_buffer(&self, _: &(), _: usize, _: &usize, _: *const u8, _: u32) {
        unreachable!()
    }
    fn write_texture(&self, _: &(), _: usize, _: &(), _: f32) {
        unreachable!()
    }
    fn write_sampler(&self, _: &(), _: usize, _: &(), _: f32) {
        unreachable!()
    }
    fn clear_entry(&self, _: &(), _: usize) {
        unreachable!()
    }
    fn create_texture(&self, _: &TextureDesc) -> Result<(), String> {
        unreachable!()
    }
    fn create_texture_view(&self, _: &(), _: &ViewDesc) -> Result<(), String> {
        unreachable!()
    }
    fn create_texture_over_memory(
        &self,
        _: &usize,
        _: *const u8,
        _: &TextureDesc,
        _: usize,
    ) -> Result<(), String> {
        unreachable!()
    }
    fn linear_alignment(&self, _: Format) -> usize {
        unreachable!()
    }
    fn upload_texture(&self, _: &(), _: usize, _: usize, _: &[u8], _: usize, _: usize) {
        unreachable!()
    }
    fn read_texture(&self, _: &(), _: usize, _: usize, _: &mut [u8], _: usize, _: usize) {
        unreachable!()
    }
    fn create_sampler(&self, _: &SamplerDesc) -> Result<(), String> {
        unreachable!()
    }
}
