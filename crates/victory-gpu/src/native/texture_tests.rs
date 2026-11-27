//! Title textures on the native host: textures laid out in the console's tiled layouts in title
//! memory, detiled into host textures; textures the title places in memory itself; render targets
//! cleared through the title's views. Our own synthetic texels. Skipped without Metal.

use super::Pool;
use crate::native::glue::{self, with_list, with_title};
use crate::native::list::NativeList;
use crate::native::title::Title;
use crate::native::memory::{PoolInfo, PoolObserver};
use crate::object::{This, slot};
use victory_backend::resources::*;
use victory_formats::texture::Swizzle;
use victory_metal::mem::{Alias, page_size};

fn pool(id: usize, pages: usize) -> Option<Pool> {
    // Needs a device; the title is made on first use.
    with_title(|_| ())?;
    let mem = Alias::new(pages * page_size().max(16384)).unwrap();
    let info = PoolInfo {
        id,
        base: mem.primary as usize,
        len: mem.len,
        memory_entry: None,
    };
    let observer = glue::pool_observer();
    observer.pool_created(&info);
    observer.mapped(mem.alias as usize, mem.len, &info, 0);
    Some(Pool { mem })
}

/// What `x`, `y` hold in mip `mip` of the test texture.
fn texel(mip: u32, x: u32, y: u32) -> [u8; 4] {
    [x as u8, y as u8, mip as u8 * 40, 255]
}

/// Tiles a texture's mips into `pool` memory at `at`, texel by `texel`.
fn tile(pool: &Pool, at: usize, layout: &victory_formats::texture::Layout) {
    let mut bytes = vec![0u8; layout.size as usize];
    for mip in 0..layout.mips.len() as u32 {
        let m = &layout.mips[mip as usize];
        let linear: Vec<u8> = (0..m.height)
            .flat_map(|y| (0..m.width).flat_map(move |x| texel(mip, x, y)))
            .collect();
        layout
            .tile(&mut bytes, mip, 0, &linear, m.width as usize * 4, linear.len())
            .unwrap();
    }
    pool.write(at, &bytes);
}

fn read_mip(key: usize, mip: usize, w: usize, h: usize) -> Vec<u8> {
    with_title(|t| {
        let tex = t.texture_of(key).expect("a host texture");
        let mut out = vec![0u8; w * h * 4];
        t.host.backend.read_texture(&tex, mip, 0, &mut out, w * 4, 0);
        out
    })
    .unwrap()
}

/// A tiled texture with a mip chain is detiled mip by mip into its host texture when work is
/// submitted, again only when its memory changed.
#[test]
fn a_tiled_title_texture_is_detiled_into_the_host_texture() {
    let Some(pool) = pool(9101, 8) else { return };
    // The XOR modes (`_X`, `_T`) too: their equations come from the pipe configuration, not a
    // fixed pattern, and a texture of theirs is no longer left empty.
    for swizzle in [
        Swizzle::LINEAR,
        Swizzle::SW_256B_S,
        Swizzle::SW_4KB_S,
        Swizzle::SW_64KB_D,
        Swizzle::SW_4KB_S_X,
        Swizzle::SW_64KB_D_X,
        Swizzle::SW_64KB_S_T,
    ] {
        let at = 0;
        let res = crate::resource::test_texture_at(pool.va(at), (64, 32), 4, 28, swizzle.0, 0);
        let info = crate::resource::texture_info(res).unwrap();
        tile(&pool, at, &info.layout);
        let key = res as usize;
        let uploads = |key| with_title(|t| t.uploads_of(key)).unwrap();
        with_title(|t| t.sync_textures());
        // Nothing named the texture yet: no host texture, nothing detiled.
        assert_eq!(uploads(key), 0);
        let mips: Vec<(usize, usize)> = (0..4).map(|m| (64 >> m, 32 >> m)).collect();
        // Naming it makes it; the next sync fills it.
        with_title(|t| {
            assert!(t.realize(key));
            t.sync_textures();
        });
        for (mip, &(w, h)) in mips.iter().enumerate() {
            let out = read_mip(key, mip, w, h);
            for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1), (w / 2, h / 2)] {
                let at = (y * w + x) * 4;
                assert_eq!(
                    out[at..at + 4],
                    texel(mip as u32, x as u32, y as u32),
                    "{swizzle:?} mip {mip} at {x},{y}"
                );
            }
        }
        assert_eq!(uploads(key), 1, "uploaded once");
        // Unchanged memory is not detiled again; changed memory is.
        with_title(|t| t.sync_textures());
        assert_eq!(uploads(key), 1);
        pool.write(at, &[0xFFu8; 16]);
        with_title(|t| t.sync_textures());
        assert_eq!(uploads(key), 2);
    }
}

/// Held by the tests that count on a texture staying hot: the many submissions of
/// `a_texture_blank_for_thousands_of_submissions_is_still_probed` cool every other test's textures.
static AGING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A texture uploaded blank and written whole later (a movie's planes) is uploaded again at the
/// next submission, not a scanner round later (a round is seconds long once the title streams).
#[test]
fn a_texture_uploaded_blank_is_probed_at_each_submission() {
    let _aging = AGING.lock().unwrap_or_else(|e| e.into_inner());
    // 256 KiB: 64 pages, more than a sampling probe reads.
    let Some(pool) = pool(9105, (256 << 10) / page_size() + 1) else { return };
    let res = crate::resource::test_texture_at(pool.va(0), (256, 256), 1, 28, Swizzle::LINEAR.0, 0);
    let info = crate::resource::texture_info(res).unwrap();
    let key = res as usize;
    with_title(|t| {
        assert!(t.realize(key));
        t.sync_dirty();
        assert_eq!(t.uploads_of(key), 1, "the blank first upload");
    });
    pool.write(0, &vec![0xABu8; info.size]);
    with_title(|t| {
        t.sync_dirty();
        assert_eq!(t.uploads_of(key), 2, "uploaded again at the next submission");
    });
    // A glyph's worth of bytes in page 3 (a page a sampling probe would skip).
    pool.write(3 * 4096 + 100, &[1u8; 16]);
    with_title(|t| {
        t.sync_dirty();
        assert_eq!(t.uploads_of(key), 3, "a small write is seen at the next submission too");
    });
}

/// A texture uploaded blank stays hot however long it stays blank: the boot movie's planes are
/// made about 40 s (thousands of submissions) before the movie decodes into them, and once cold
/// their first frames waited seconds for the scanner (green).
#[test]
fn a_texture_blank_for_thousands_of_submissions_is_still_probed() {
    let _aging = AGING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pool) = pool(9123, (256 << 10) / page_size() + 1) else { return };
    let res = crate::resource::test_texture_at(pool.va(0), (256, 256), 1, 28, Swizzle::LINEAR.0, 0);
    let info = crate::resource::texture_info(res).unwrap();
    let key = res as usize;
    with_title(|t| {
        assert!(t.realize(key));
        for _ in 0..2000 {
            t.sync_dirty();
        }
        assert_eq!(t.uploads_of(key), 1, "the blank first upload");
    });
    pool.write(0, &vec![0xABu8; info.size]);
    // Within a few submissions: other tests' blank textures share the probe's time budget.
    with_title(|t| {
        let n = (1..=4).find(|_| {
            t.sync_dirty();
            t.uploads_of(key) == 2
        });
        assert!(n.is_some(), "the first frame is uploaded within a few submissions");
    });
}

/// A texture rewritten whole (a movie's chroma plane) is not read whole at every submission, yet
/// a small write its samples miss is still seen within `WHOLE_EVERY` submissions, and from then
/// on at the next one (a glyph atlas).
#[test]
fn a_texture_rewritten_whole_is_read_whole_only_now_and_then() {
    let _aging = AGING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pool) = pool(9124, (256 << 10) / page_size() + 1) else { return };
    let res = crate::resource::test_texture_at(pool.va(0), (256, 256), 1, 28, Swizzle::LINEAR.0, 0);
    let info = crate::resource::texture_info(res).unwrap();
    let key = res as usize;
    let sync = || with_title(|t| { t.sync_dirty(); t.uploads_of(key) }).unwrap();
    with_title(|t| assert!(t.realize(key)));
    assert_eq!(sync(), 1, "the blank first upload");
    for (i, n) in [(1u8, 2), (2, 3), (3, 4)] {
        pool.write(0, &vec![i; info.size]);
        assert_eq!(sync(), n, "a whole rewrite is seen by the samples at once");
        sync();
    }
    // A glyph's worth of bytes in page 3 (a page the samples skip).
    pool.write(3 * 4096 + 100, &[9u8; 16]);
    let n = (1..=crate::native::textures::WHOLE_EVERY + 1).find(|_| sync() == 5);
    assert!(n.is_some(), "a write the samples miss is seen within a whole-read period");
    pool.write(5 * 4096 + 100, &[9u8; 16]);
    assert_eq!(sync(), 6, "and the next one at the next submission");
}

/// A movie plane uploaded blank among hundreds of other hot textures (a title streaming) is still
/// probed at the next submission when the probe deadline allows it: a count limit on the probes
/// left its first frames green.
/// (Every texture is written, so which one a limited probe reaches first does not matter.)
#[test]
fn a_hot_texture_is_probed_at_each_submission_among_hundreds() {
    let _aging = AGING.lock().unwrap_or_else(|e| e.into_inner());
    const N: usize = 200;
    const STRIDE: usize = 64 << 10;
    let Some(pool) = pool(9121, N * STRIDE / page_size() + 1) else { return };
    let keys: Vec<usize> = (0..N)
        .map(|i| crate::resource::test_texture_at(pool.va(i * STRIDE), (64, 64), 1, 28, Swizzle::LINEAR.0, 0) as usize)
        .collect();
    let size = crate::resource::texture_info(keys[0] as _).unwrap().size;
    with_title(|t| {
        for &k in &keys {
            assert!(t.realize(k));
        }
        t.sync_dirty();
        assert!(keys.iter().all(|&k| t.uploads_of(k) == 1), "every blank upload");
        let bytes = vec![0xABu8; size];
        for i in 0..N {
            pool.write(i * STRIDE, &bytes);
        }
        t.sync_dirty_without_probe_deadline();
        // The scanner can trigger uploads too. The probe removes written textures from blank_keys.
        let probed = keys.iter().filter(|&k| !t.blank_keys.contains(k)).count();
        assert_eq!(probed, N, "all {N} blank textures probed in one submission");
        let seen = keys.iter().filter(|&&k| t.uploads_of(k) == 2).count();
        assert_eq!(seen, N, "all {N} hot textures written, seen at the next submission");
    });
}

/// A block-compressed texture: 16 bytes a 4x4 block, in the console's layout, detiled as blocks.
#[test]
fn a_block_compressed_title_texture_is_detiled_as_blocks() {
    let Some(pool) = pool(9102, 4) else { return };
    // BC7 (98): a 32x16 texture is 8x4 blocks.
    let res = crate::resource::test_texture_at(pool.va(0), (32, 16), 1, 98, Swizzle::SW_64KB_D.0, 0);
    let info = crate::resource::texture_info(res).unwrap();
    assert_eq!(info.layout.desc.element_bytes, 16);
    let m = &info.layout.mips[0];
    let blocks: Vec<u8> = (0..m.width * m.height * 16).map(|i| (i * 7) as u8).collect();
    let mut bytes = vec![0u8; info.layout.size as usize];
    info.layout
        .tile(&mut bytes, 0, 0, &blocks, m.width as usize * 16, blocks.len())
        .unwrap();
    pool.write(0, &bytes);
    let key = res as usize;
    with_title(|t| {
        assert!(t.realize(key));
        t.sync_textures();
        let tex = t.texture_of(key).unwrap();
        let mut out = vec![0u8; blocks.len()];
        t.host.backend.read_texture(&tex, 0, 0, &mut out, m.width as usize * 16, 0);
        assert_eq!(out, blocks);
    });
}

/// A texture a view names by memory address and description, without a resource object (the
/// console's `CreateShaderResourceViewPlaced`): the host makes it when the view is written.
#[test]
fn a_placed_texture_view_makes_and_fills_its_texture() {
    let Some(pool) = pool(9103, 4) else { return };
    let res = crate::resource::test_texture_at(pool.va(0), (32, 32), 1, 28, Swizzle::SW_64KB_S.0, 0);
    let info = crate::resource::texture_info(res).unwrap();
    tile(&pool, 0, &info.layout);
    let words = crate::resource::texture_desc(res).unwrap();
    let heap = crate::descriptor::test_heap(false, 4);
    let start = slot::<extern "system" fn(This) -> u64>(heap, 0x48)(heap);
    let mut view = [0u32; 16];
    (view[0], view[1], view[5]) = (28, 4, 1);
    (view[10], view[11]) = (pool.va(0) as u32, (pool.va(0) >> 32) as u32);
    crate::descriptor::create_placed_srv(std::ptr::null(), &words, &view, start);
    // The heap is made (and the view written into it) when a list names it.
    let key = crate::native::textures::placed_key(pool.va(0), &words);
    with_title(|t| {
        assert!(t.host.knows(key), "made when the view was written");
        t.sync_textures();
    });
    let out = read_mip(key, 0, 32, 32);
    assert_eq!(out[(5 * 32 + 9) * 4..][..4], texel(0, 9, 5));
}

/// A placed texture has no object the title releases: it goes when another description is placed
/// at its address, and when its pool is released.
#[test]
fn a_placed_texture_goes_when_replaced_and_with_its_pool() {
    let Some(pool) = pool(9107, 4) else { return };
    let heap = crate::descriptor::test_heap(false, 4);
    let start = slot::<extern "system" fn(This) -> u64>(heap, 0x48)(heap);
    let place = |size: u32| {
        let res = crate::resource::test_texture_at(pool.va(0), (size.into(), size), 1, 28, Swizzle::SW_64KB_S.0, 0);
        let words = crate::resource::texture_desc(res).unwrap();
        let mut view = [0u32; 16];
        (view[0], view[1], view[5]) = (28, 4, 1);
        (view[10], view[11]) = (pool.va(0) as u32, (pool.va(0) >> 32) as u32);
        crate::descriptor::create_placed_srv(std::ptr::null(), &words, &view, start);
        crate::native::textures::placed_key(pool.va(0), &words)
    };
    let first = place(32);
    assert!(with_title(|t| t.host.knows(first)).unwrap());
    let second = place(16);
    assert_ne!(first, second);
    with_title(|t| {
        assert!(t.host.knows(second));
        assert!(!t.host.knows(first), "replaced at its address");
    });
    let info = PoolInfo {
        id: 9107,
        base: pool.mem.primary as usize,
        len: pool.mem.len,
        memory_entry: None,
    };
    glue::pool_observer().pool_released(&info);
    with_title(|t| {
        t.sync_dirty();
        assert!(!t.host.knows(second), "released with its pool");
        assert!(t.texture_of(second).is_none());
    });
}

/// A render target the title clears through its view: the host texture is the target, never
/// filled from memory.
#[test]
fn a_render_target_is_cleared_through_the_titles_view() {
    let Some(pool) = pool(9104, 4) else { return };
    let rt = crate::resource::test_texture_at(pool.va(0), (16, 16), 1, 28, Swizzle::SW_64KB_R_X.0, 1);
    pool.write(0, &[0x7Fu8; 64]);
    // An RTV in descriptor memory the title owns.
    let heap = crate::descriptor::test_heap(false, 4);
    let handle = slot::<extern "system" fn(This) -> u64>(heap, 0x48)(heap);
    crate::descriptor::create_rtv(std::ptr::null(), rt, std::ptr::null(), handle);
    let list = crate::list::test_list(false, std::ptr::null());
    let (q, f) = (crate::queue::test_queue(false), {
        let mut f = std::ptr::null();
        crate::fence::create(std::ptr::null(), 0, 0, &crate::fence::FENCE_IIDS[0], &mut f);
        f
    });
    // OMSetRenderTargets(1, &handle, false, null), then ClearRenderTargetView(handle, color).
    slot::<extern "system" fn(This, u32, *const u64, i32, *const u64)>(list, 0x190)(
        list,
        1,
        &handle,
        0,
        std::ptr::null(),
    );
    let color = [0.0f32, 1.0, 0.0, 1.0];
    slot::<extern "system" fn(This, u64, *const [f32; 4], u32, *const u8)>(list, 0x1a0)(
        list,
        handle,
        &color,
        0,
        std::ptr::null(),
    );
    assert_eq!(slot::<extern "system" fn(This) -> i32>(list, 0x48)(list), 0);
    slot::<extern "system" fn(This, u32, *const This)>(q, 0x50)(q, 1, &list);
    slot::<extern "system" fn(This, This, u64) -> i32>(q, 0x70)(q, f, 1);
    assert_eq!(
        slot::<extern "system" fn(This, u64, *mut std::ffi::c_void) -> i32>(f, 0x48)(
            f,
            1,
            std::ptr::null_mut()
        ),
        0
    );
    let out = read_mip(rt as usize, 0, 16, 16);
    assert!(out.chunks(4).all(|p| p == [0, 255, 0, 255]), "{:?}", &out[..8]);
}

/// A render target view names the format its writes are stored in. Through a plain view of an
/// sRGB texture a value is stored as it is (a movie's picture, which is display values already);
/// through an sRGB view of a plain texture it is encoded.
#[test]
fn a_render_target_view_stores_in_the_format_it_names() {
    let Some(pool) = pool(9120, 16) else { return };
    // The texture's format, the view's (0: none described), and the byte half grey is stored as.
    for (i, (own, named, stored)) in [(29, 28, 128), (28, 29, 188), (29, 0, 188), (28, 28, 128)]
        .into_iter()
        .enumerate()
    {
        let at = i * 0x10000;
        let rt = crate::resource::test_texture_at(
            pool.va(at),
            (16, 16),
            1,
            own,
            Swizzle::SW_64KB_R_X.0,
            1,
        );
        let heap = crate::descriptor::test_heap(false, 4);
        let handle = slot::<extern "system" fn(This) -> u64>(heap, 0x48)(heap);
        // A 2D view of mip 0.
        let desc = [named, 4, 0, 0, 0, 0];
        let desc = if named == 0 {
            std::ptr::null()
        } else {
            &raw const desc
        };
        crate::descriptor::create_rtv(std::ptr::null(), rt, desc, handle);
        let list = crate::list::test_list(false, std::ptr::null());
        let (q, f) = (crate::queue::test_queue(false), {
            let mut f = std::ptr::null();
            crate::fence::create(std::ptr::null(), 0, 0, &crate::fence::FENCE_IIDS[0], &mut f);
            f
        });
        slot::<extern "system" fn(This, u32, *const u64, i32, *const u64)>(list, 0x190)(
            list,
            1,
            &handle,
            0,
            std::ptr::null(),
        );
        let color = [0.5f32, 0.5, 0.5, 1.0];
        slot::<extern "system" fn(This, u64, *const [f32; 4], u32, *const u8)>(list, 0x1a0)(
            list,
            handle,
            &color,
            0,
            std::ptr::null(),
        );
        assert_eq!(slot::<extern "system" fn(This) -> i32>(list, 0x48)(list), 0);
        slot::<extern "system" fn(This, u32, *const This)>(q, 0x50)(q, 1, &list);
        slot::<extern "system" fn(This, This, u64) -> i32>(q, 0x70)(q, f, 1);
        assert_eq!(
            slot::<extern "system" fn(This, u64, *mut std::ffi::c_void) -> i32>(f, 0x48)(
                f,
                1,
                std::ptr::null_mut()
            ),
            0
        );
        let out = read_mip(rt as usize, 0, 16, 16);
        assert!(
            out.chunks(4)
                .all(|p| p[0].abs_diff(stored) <= 1 && p[3] == 255),
            "texture {own} view {named}: {:?}",
            &out[..4]
        );
    }
}

/// A layout the host cannot address (`SW_4KB_R_X`, which has no equations) is counted and left empty, not
/// an error that stops the submission.
#[test]
fn an_undetilable_layout_is_counted_not_fatal() {
    let Some(pool) = pool(9105, 4) else { return };
    let res = crate::resource::test_texture_at(pool.va(0), (64, 64), 1, 28, Swizzle::SW_4KB_R_X.0, 0);
    let before = with_title(|t| t.stats.undetilable).unwrap();
    with_title(|t| {
        assert!(t.realize(res as usize));
        t.sync_textures();
        t.sync_textures();
    });
    assert_eq!(with_title(|t| t.stats.undetilable).unwrap(), before + 1, "once per content");
}

/// `PresentX`: the first resource of the largest plane is drawn over the app's layer (scaled,
/// whatever its channel order), after the queue's earlier work. Offscreen: the layer is not in a
/// window; its last drawable is read back.
#[test]
fn present_draws_the_back_buffer_over_the_apps_layer() {
    check_present_order(false);
    check_present_order(true);
}

fn check_present_order(held: bool) {
    let Some(pool) = pool(if held { 9116 } else { 9106 }, 8) else { return };
    let rt = crate::resource::test_texture_at(pool.va(0), (16, 16), 1, 28, Swizzle::SW_64KB_S.0, 1);
    let heap = crate::descriptor::test_heap(false, 4);
    let handle = slot::<extern "system" fn(This) -> u64>(heap, 0x48)(heap);
    crate::descriptor::create_rtv(std::ptr::null(), rt, std::ptr::null(), handle);
    let layer = glue::create_layer(32, 32).expect("a layer");
    glue::set_layer(layer.as_ptr()).expect("a surface");
    let (q, f) = (crate::queue::test_queue(false), {
        let mut f = std::ptr::null();
        crate::fence::create(std::ptr::null(), 0, 0, &crate::fence::FENCE_IIDS[0], &mut f);
        f
    });
    let mut gate = std::ptr::null();
    crate::fence::create(std::ptr::null(), 0, 0, &crate::fence::FENCE_IIDS[0], &mut gate);
    if held {
        glue::queue_wait(q, gate, 1);
    }
    let list = crate::list::test_list(false, std::ptr::null());
    slot::<extern "system" fn(This, u32, *const u64, i32, *const u64)>(list, 0x190)(
        list,
        1,
        &handle,
        0,
        std::ptr::null(),
    );
    // Red in the title's RGBA.
    let color = [1.0f32, 0.0, 0.0, 1.0];
    slot::<extern "system" fn(This, u64, *const [f32; 4], u32, *const u8)>(list, 0x1a0)(
        list,
        handle,
        &color,
        0,
        std::ptr::null(),
    );
    slot::<extern "system" fn(This) -> i32>(list, 0x48)(list);
    slot::<extern "system" fn(This, u32, *const This)>(q, 0x50)(q, 1, &list);
    // One plane: the frame token, one resource, its address.
    #[repr(C)]
    struct Plane {
        token: u64,
        count: u32,
        pad: u32,
        resources: *const This,
    }
    let plane = Plane {
        token: 1,
        count: 1,
        pad: 0,
        resources: &rt,
    };
    let before = glue::frames_presented();
    let present = slot::<extern "system" fn(This, u32, *const Plane, *const u8) -> i32>(q, 0xf0);
    let frames = if held { 6 } else { 1 };
    for _ in 0..frames {
        assert_eq!(present(q, 1, &plane, std::ptr::null()), 0);
    }
    // A signal after the present must also stay behind it.
    slot::<extern "system" fn(This, This, u64) -> i32>(q, 0x70)(q, f, 1);
    if held {
        // Present must remain behind the unsignalled wait and the red clear.
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(glue::frames_presented(), before, "present bypassed the fence wait");
        assert_eq!(slot::<extern "system" fn(This) -> u64>(f, 0x40)(f), 0);
        // Signal the backend directly so only the timer worker drains the deferred presents.
        glue::with_title(|t| {
            use victory_backend::backend::ComputeBackend;
            let f = t.fence(gate as usize, 0).unwrap();
            t.host.backend.signal(&f, 1);
        });
    }
    let start = std::time::Instant::now();
    while glue::frames_presented() < before + frames {
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "worker failed to drain presents");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(glue::frames_presented(), before + frames);
    // The frame is drawn when the fence is reached.
    assert_eq!(
        slot::<extern "system" fn(This, u64, *mut std::ffi::c_void) -> i32>(f, 0x48)(f, 1, std::ptr::null_mut()),
        0
    );
    let (w, h, bgra) = glue::last_frame().expect("a frame");
    assert_eq!((w, h), (32, 32));
    assert!(
        bgra.chunks(4).all(|p| p == [0, 0, 255, 255]),
        "red as BGRA: {:?}",
        &bgra[..8]
    );
    drop(layer);
}

/// A colour target the host cannot make must keep its slot: binding the later ones would put
/// `SV_Target1` on slot 0 and write a surface this pass does not own. Nothing is bound instead,
/// and the draws that follow are refused.
#[test]
fn a_target_the_host_cannot_make_does_not_shift_the_ones_after_it() {
    let Some(pool) = pool(9201, 4) else { return };
    // 12 (R16G16B16A16_UINT) has no host texture; 28 is R8G8B8A8_UNORM.
    let gone = crate::resource::test_texture_at(pool.va(0), (16, 16), 1, 12, Swizzle::SW_64KB_R_X.0, 1);
    let good = crate::resource::test_texture_at(pool.va(0x2000), (16, 16), 1, 28, Swizzle::SW_64KB_R_X.0, 1);
    // Two view handles, 64 bytes apart, as a title's own descriptor memory holds them.
    let mut slots = vec![0u64; 16];
    let first = slots.as_mut_ptr() as u64;
    let second = first + 64;
    crate::descriptor::create_rtv(std::ptr::null(), gone, std::ptr::null(), first);
    crate::descriptor::create_rtv(std::ptr::null(), good, std::ptr::null(), second);
    let list = crate::list::test_list(false, std::ptr::null());
    let handles = [first, second];
    // OMSetRenderTargets(2, &handles, false, null).
    slot::<extern "system" fn(This, u32, *const u64, i32, *const u64)>(list, 0x190)(
        list,
        handles.len() as u32,
        handles.as_ptr(),
        0,
        std::ptr::null(),
    );
    let bound = with_list(list, |_, l| l.targets.color.len()).unwrap();
    assert_eq!(bound, 0, "a target without a host texture must not renumber the ones after it");
    // A set the host can make is taken again.
    let only = [second];
    slot::<extern "system" fn(This, u32, *const u64, i32, *const u64)>(list, 0x190)(
        list,
        only.len() as u32,
        only.as_ptr(),
        0,
        std::ptr::null(),
    );
    let bound = with_list(list, |_, l| l.targets.color.len()).unwrap();
    assert_eq!(bound, 1, "a makeable target binds again after a refused pass");
    drop(slots);
}

/// A compute dispatch writes a tiled title texture through a UAV; once its fence value shows, the
/// title's own memory holds the texels, in the texture's layout (the ocean maps' path, #46).
#[test]
fn a_texture_compute_writes_is_copied_back_into_title_memory() {
    // Skips without the Shader Converter.
    if super::host().is_none() {
        return;
    }
    let dxil = include_bytes!("../../../victory-metal/tests/fixtures/tex_fill.dxil");
    const W: u32 = 64;
    // A title host of its own: no timer thread, and a pool no other test shares.
    let mut t = Title::new(victory_metal::metal::Metal::new().unwrap(), Default::default());
    let mem = Alias::new((64 << 10).max(page_size())).unwrap();
    let info = PoolInfo { id: 9131, base: mem.primary as usize, len: mem.len, memory_entry: None };
    t.host.memory().pool_created(&info);
    t.host.memory().mapped(mem.alias as usize, mem.len, &info, 0);
    let pool = Pool { mem };
    t.set_texture_back(true);
    for (n, swizzle) in [Swizzle::LINEAR, Swizzle::SW_4KB_S, Swizzle::SW_64KB_D_X].into_iter().enumerate() {
        // Flags 4: unordered access. DXGI 28: RGBA8.
        let res = crate::resource::test_texture_at(pool.va(0), (u64::from(W), W), 1, 28, swizzle.0, 4);
        let info = crate::resource::texture_info(res).unwrap();
        let key = res as usize;
        pool.write(0, &vec![0u8; info.size]);
        // The title's descriptor memory: one UAV (format, 2D, mip 0).
        let slots = vec![0u32; 8 * 8];
        let handle = slots.as_ptr() as u64;
        let uav = [28u32, 4, 0, 0, 0, 0, 0, 0, 0, 0];
        crate::descriptor::create_uav(std::ptr::null(), res, std::ptr::null(), &uav, handle);
        let (queue, fence) = (1 + n, 11 + n);
        t.texture_created(key, info.clone());
        assert!(t.realize(key), "{swizzle:?}");
        let heap = t.host.heap(HeapKind::Resource, handle, 8).unwrap();
        let pso = t.host.compute_pipeline(dxil, [8, 8, 1]).unwrap();
        let mut l = NativeList::default();
        l.set_heaps(Some(&heap), None);
        l.set_compute_pipeline(&pso);
        l.set_compute_root_table(0, &heap, 0);
        l.dispatch(W / 8, W / 8, 1).unwrap();
        t.ensure_queue(queue).unwrap();
        t.submit(queue, vec![(l, vec![])]);
        t.signal_queue(queue, fence, 0, 1);
        let start = std::time::Instant::now();
        while !t.try_wait(fence, 0, 1) {
            assert!(start.elapsed().as_secs() < 10, "the dispatch did not complete");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // The value shows, so the memory is there already.
        let mut out = vec![0u8; info.size];
        assert!(crate::native::safe_read(pool.va(0) as *const u8, &mut out));
        let linear: Vec<u8> = (0..W)
            .flat_map(|y| (0..W).flat_map(move |x| [x as u8, y as u8, 64, 255]))
            .collect();
        let mut want = vec![0u8; info.size];
        info.layout
            .tile(&mut want, 0, 0, &linear, W as usize * 4, linear.len())
            .unwrap();
        assert!(out == want, "{swizzle:?}: title memory is not the retiled texels");
        // Our own write is not mistaken for the title's: nothing is uploaded again.
        let uploads = t.uploads_of(key);
        t.sync_textures();
        assert_eq!(t.uploads_of(key), uploads, "{swizzle:?}");
        // The title writes the memory while the same work runs: its bytes stay.
        let mut l = NativeList::default();
        let heap = t.host.heap_at(handle).unwrap().0;
        l.set_heaps(Some(&heap), None);
        l.set_compute_pipeline(&pso);
        l.set_compute_root_table(0, &heap, 0);
        l.dispatch(W / 8, W / 8, 1).unwrap();
        t.submit(queue, vec![(l, vec![])]);
        t.signal_queue(queue, fence, 0, 2);
        pool.write(0, &vec![0x5Au8; info.size]);
        let start = std::time::Instant::now();
        while !t.try_wait(fence, 0, 2) {
            assert!(start.elapsed().as_secs() < 10, "the dispatch did not complete");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(crate::native::safe_read(pool.va(0) as *const u8, &mut out));
        assert!(out.iter().all(|&b| b == 0x5A), "{swizzle:?}: the title's own write was overwritten");
        t.host.drop_heap(handle);
        t.host.forget_resource(key);
        t.texture_released(key);
    }
}

/// Depth and depth-stencil textures are GPU targets whose CPU tile layout (e.g. a 4-byte or
/// 2-byte depth plane) does not match Metal's combined texel layout, so they must not be watched
/// or uploaded from CPU memory even when placed/typeless (not marked as a render target).
#[test]
fn depth_textures_are_not_watched_or_uploaded_from_cpu_memory() {
    let Some(pool) = pool(9202, (512 << 10) / page_size() + 1) else { return };
    // DXGI formats: 20 (D32_FLOAT_S8X24_UINT), 40 (D32_FLOAT), 55 (D16_UNORM),
    // and typeless 19 (R32G8X24_TYPELESS) with flags = 0 (not marked ALLOW_RENDER_TARGET).
    for (i, format) in [20u32, 40, 55, 19].into_iter().enumerate() {
        let at = i * 0x20000;
        let res = crate::resource::test_texture_at(
            pool.va(at),
            (64, 64),
            1,
            format,
            Swizzle::SW_64KB_R_X.0,
            0,
        );
        let key = res as usize;
        with_title(|t| {
            assert!(t.realize(key), "format {format} should realize");
            t.sync_textures();
            assert_eq!(t.uploads_of(key), 0, "depth format {format} must not upload on sync_textures");
        });
        pool.write(at, &[0xA5u8; 64]);
        with_title(|t| {
            t.sync_dirty();
            assert_eq!(t.uploads_of(key), 0, "depth format {format} must not upload on sync_dirty");
        });
    }
}

/// A described RTV binds and clears its selected subresource in the named format.
#[test]
fn a_render_target_view_selects_its_mip_and_slice() {
    use std::rc::Rc;
    use victory_backend::backend::ComputeBackend;

    let pool = pool(9138, 16).expect("Metal device and generated texture pool");
    let heap = crate::descriptor::test_heap(false, 4);
    let handle = slot::<extern "system" fn(This) -> u64>(heap, 0x48)(heap);
    let other_heap = crate::descriptor::test_heap(false, 4);
    let next_handle = slot::<extern "system" fn(This) -> u64>(other_heap, 0x48)(other_heap);
    let grey = [0.5, 0.5, 0.5, 1.0];

    for (case, own_format, layers, array_view, mip, slice) in [
        ("2D mip", 28, 1, false, 1, 0),
        ("array slice", 28, 2, true, 0, 1),
        ("array mip and slice", 28, 2, true, 1, 1),
        ("plain view of an sRGB array mip and slice", 29, 2, true, 1, 1),
        ("nonarray view of the first slice", 28, 2, false, 1, 0),
    ] {
        let rt = crate::resource::test_texture_array_at(pool.va(0), (8, 8), 2, layers, own_format, Swizzle::LINEAR.0, 1);
        let key = rt as usize;
        let subresources: Vec<_> = (0..usize::from(layers))
            .flat_map(|layer| (0..2).map(move |level| {
                (level, layer, 8 >> level, [17 + (level * 40 + layer * 80) as u8, 31, 47, 255])
            }))
            .collect();
        with_title(|title| {
            let texture = title.texture_of(key).expect("realized texture");
            for &(level, layer, n, sentinel) in &subresources {
                title.host.backend.upload_texture(&texture, level, layer, &sentinel.repeat(n * n), n * 4, 0);
            }
        }).expect("Metal title");
        let desc = if array_view {
            [28, 5, mip, slice, 1, 0]
        } else {
            [28, 4, mip, 0, 0, 0]
        };
        crate::descriptor::create_rtv(std::ptr::null(), rt, &desc, handle);
        crate::descriptor::create_rtv(std::ptr::null(), rt, std::ptr::null(), next_handle);
        let list = crate::list::test_list(false, std::ptr::null());
        glue::targets(list, &[handle], 0);
        with_list(list, |title, l| {
            let (expected, format) = title.target_of(crate::descriptor::rtv_record(handle).unwrap()).unwrap();
            let (base, base_format) = title.target_of(crate::descriptor::rtv_record(next_handle).unwrap()).unwrap();
            assert!(l.targets_ready);
            assert_eq!(l.targets.color, [Format::Rgba8Unorm]);
            assert_eq!(format, Format::Rgba8Unorm);
            let [attached] = l.list.color_targets() else { panic!("one attachment was expected") };
            assert!(Rc::ptr_eq(attached, &expected), "binding reuses the cached attachment");
            assert!(Rc::ptr_eq(&base, &title.texture_of(key).unwrap()), "default keeps base attachment");
            assert!(!Rc::ptr_eq(&base, attached));
            assert_eq!(base_format, if own_format == 29 { Format::Rgba8UnormSrgb } else { Format::Rgba8Unorm });
            if array_view {
                for (other_mip, other_slice) in [(1 - mip, slice), (mip, 1 - slice)] {
                    let other_desc = [28, 5, other_mip, other_slice, 1, 0];
                    crate::descriptor::create_rtv(std::ptr::null(), rt, &other_desc, next_handle);
                    let (other, _) = title.target_of(crate::descriptor::rtv_record(next_handle).unwrap()).unwrap();
                    assert!(!Rc::ptr_eq(attached, &other), "distinct subresources must keep distinct identities");
                }
            }
        }).expect("native command list");

        let clear = slot::<extern "system" fn(This, u64, *const [f32; 4], u32, *const u8)>(list, 0x1a0);
        clear(list, handle, &grey, 0, std::ptr::null());
        with_list(list, |_, l| {
            let [super::super::list::Cmd::Clear { color: Some((cleared, _)), .. }] = l.list.cmds.as_slice() else {
                panic!("one clear was expected");
            };
            assert!(Rc::ptr_eq(&l.list.color_targets()[0], cleared), "draw binding and clear resolve the same view");
        }).expect("native command list");
        if layers == 2 {
            for invalid in [
                [28, 5, 2, 0, 1, 0], // Out-of-range mip.
                [28, 5, 0, 2, 1, 0], // Out-of-range slice.
                [28, 5, 0, 0, 0, 0], // Empty array view.
                [28, 5, 0, 0, 2, 0], // Unsupported layered view.
                [28, 5, 0, 0, 1, 1], // Unsupported plane.
                [28, 2, 0, 0, 0, 0], // 1D view of a 2D resource.
                [28, 6, 0, 0, 0, 0], // Multisampled view of a single-sampled resource.
            ] {
                crate::descriptor::create_rtv(std::ptr::null(), rt, &invalid, next_handle);
                glue::targets(list, &[next_handle, handle], 0);
                clear(list, next_handle, &[1.0, 0.0, 0.0, 1.0], 0, std::ptr::null());
                with_list(list, |_, l| {
                    assert!(!l.targets_ready, "{case} accepted {invalid:?}");
                    assert!(l.list.color_targets().is_empty(), "valid attachment must keep its slot");
                    assert_eq!(l.list.cmds.len(), 1, "an invalid clear must record no command");
                }).expect("native command list");
            }
        }

        assert_eq!(slot::<extern "system" fn(This) -> i32>(list, 0x48)(list), 0);
        let queue = crate::queue::test_queue(false);
        let mut fence = std::ptr::null();
        assert_eq!(crate::fence::create(std::ptr::null(), 0, 0, &crate::fence::FENCE_IIDS[0], &mut fence), 0);
        slot::<extern "system" fn(This, u32, *const This)>(queue, 0x50)(queue, 1, &list);
        assert_eq!(slot::<extern "system" fn(This, This, u64) -> i32>(queue, 0x70)(queue, fence, 1), 0);
        with_title(|title| {
            let fence = title.fence(fence as usize, 0).expect("native fence");
            assert!(title.host.backend.wait(&fence, 1, 2000), "native clear did not finish");
            let texture = title.texture_of(key).expect("realized texture");
            let mut pixels = 0;
            for &(level, layer, n, sentinel) in &subresources {
                let expected = if (level, layer) == (mip as usize, slice as usize) { [128, 128, 128, 255] } else { sentinel };
                let mut data = vec![0u8; n * n * 4];
                title.host.backend.read_texture(&texture, level, layer, &mut data, n * 4, 0);
                assert!(data.as_chunks::<4>().0.iter().all(|p| *p == expected),
                        "{case} mip {level} slice {layer}: expected {expected:?}, got {:?}", &data[..4]);
                pixels += n * n;
            }
            println!("{case}: cleared mip {mip} slice {slice}, verified {pixels} texels");
        }).expect("Metal title");
    }
}
