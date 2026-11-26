//! A console wave of 64 lanes split into two 32-lane host waves (`lower/halves.rs` in
//! `victory_shader`), run on the GPU: synthetic GFX10 programs of our own (assembled with
//! `llvm-mc -arch=amdgcn -mcpu=gfx1030 -mattr=+wavefrontsize64`) whose lanes exchange values across
//! the halves after part of the wave has left. `s[2:3]` is `dst`, `s4` a flag. Skipped without the
//! Metal Shader Converter.

use super::addr_tests::{CANARY, Fixture, VA_A, dispatch, halves, title, translate_io, translate_wave};
use crate::native::list::NativeList;
use crate::native::shaders;
use crate::native::title::Pipe;

const S_ENDPGM: u32 = 0xBF81_0000;
/// `v_cmp_gt_u32 vcc, 40, v0; s_and_saveexec_b64 s[4:5], vcc`: lanes 40 and up leave.
const LANES_BELOW_40: [u32; 2] = [0x7D88_00A8, 0xBE84_246A];
const fn skip_execz(words: u32) -> u32 {
    0xBF88_0000 | words
}
const V_LSHLREV_V1_2_V0: u32 = 0x3402_0082;
const GLOBAL_STORE_V2_AT_V1: [u32; 2] = [0xDC70_8000, 0x0002_0201];
const DS_WRITE_V0_AT_V1: [u32; 2] = [0xD834_0000, 0x0000_0001];
const DS_READ_V2_AT_V3: [u32; 2] = [0xD8D8_0000, 0x0200_0003];
const S_WAITCNT_LGKM: u32 = 0xBF8C_C07F;

/// Pool offset of `dst`, at `VA_A + 0x4_0000`.
const DST: usize = 0x6_0000;

fn run(words: &[u32], lds: u32, flag: u32) -> Vec<u32> {
    run_io(words, lds << 15, 64, flag)
}

/// [`run`] of `threads` in a group, with `rsrc2` bits of system SGPRs and LDS.
fn run_io(words: &[u32], rsrc2: u32, threads: u32, flag: u32) -> Vec<u32> {
    let Some(mut t) = title() else { return Vec::new() };
    let f = Fixture::new(&t);
    let cs = translate_io(words, 5, rsrc2, threads, shaders::options());
    let [c, d] = halves(VA_A + 0x4_0000);
    dispatch(&mut t, &cs, &[0, 0, c, d, flag], false);
    assert_eq!(t.shaders.refusals(), 0, "{:?}", t.shaders.refused);
    f.get(DST, threads as usize)
}

#[test]
fn a_ballot_across_the_halves_after_lanes_left_for_the_end() {
    // Lanes 0..39 stay; each stores the count of lanes above 5 among them (34: 26 in the
    // lower half, 8 in the upper). The lanes that left store nothing and count as off.
    let mut w = LANES_BELOW_40.to_vec();
    w.extend([
        skip_execz(6),
        0x7D82_0085, // v_cmp_lt_u32 vcc, 5, v0
        0xBE86_106A, // s_bcnt1_i32_b64 s6, vcc
        V_LSHLREV_V1_2_V0,
        0x7E04_0206, // v_mov_b32 v2, s6
    ]);
    w.extend(GLOBAL_STORE_V2_AT_V1);
    w.push(S_ENDPGM);
    let got = run(&w, 0, 0);
    if got.is_empty() {
        return;
    }
    assert_eq!(got[..40], [34; 40]);
    assert_eq!(got[40..], [CANARY; 24]);
}

#[test]
fn lds_shared_across_the_halves_after_lanes_left_for_the_end() {
    // Lanes 0..39 write their index to LDS and read lane 39 - i's, with no barrier (the console
    // runs the wave in lockstep).
    let mut w = LANES_BELOW_40.to_vec();
    w.extend([skip_execz(11), V_LSHLREV_V1_2_V0]);
    w.extend(DS_WRITE_V0_AT_V1);
    w.extend([
        0x4C06_00A7, // v_sub_nc_u32 v3, 39, v0
        0x3406_0682, // v_lshlrev_b32 v3, 2, v3
        S_WAITCNT_LGKM,
    ]);
    w.extend(DS_READ_V2_AT_V3);
    w.push(S_WAITCNT_LGKM);
    w.extend(GLOBAL_STORE_V2_AT_V1);
    w.push(S_ENDPGM);
    let got = run(&w, 1, 0);
    if got.is_empty() {
        return;
    }
    assert_eq!(got[..40], (0..40).rev().collect::<Vec<u32>>());
    assert_eq!(got[40..], [CANARY; 24]);
}

#[test]
fn lds_shared_across_the_halves_behind_a_uniform_branch() {
    // Every lane writes its index to LDS; when `s4` is not 0, each reads lane 63 - i's, with no
    // barrier. The branch is on SCC, so both host waves take it or skip it together.
    let mut w = vec![V_LSHLREV_V1_2_V0];
    w.extend(DS_WRITE_V0_AT_V1);
    w.extend([
        0x4C06_00BF, // v_sub_nc_u32 v3, 63, v0
        0x3406_0682, // v_lshlrev_b32 v3, 2, v3
        0xBF07_8004, // s_cmp_lg_u32 s4, 0
        0xBF84_0006, // s_cbranch_scc0 6
        S_WAITCNT_LGKM,
    ]);
    w.extend(DS_READ_V2_AT_V3);
    w.push(S_WAITCNT_LGKM);
    w.extend(GLOBAL_STORE_V2_AT_V1);
    w.push(S_ENDPGM);
    let got = run(&w, 1, 1);
    if got.is_empty() {
        return;
    }
    assert_eq!(got, (0..64).rev().collect::<Vec<u32>>());
    assert_eq!(run(&w, 1, 0), [CANARY; 64]);
}

#[test]
fn the_thread_group_info_sgpr_numbers_the_console_waves() {
    // Two console waves of 64, four host waves. `s5`, the info SGPR (bit 10), holds the wave's
    // index from bit 6 and the group's wave count; index * 64 plus the lane's count of lanes below
    // it is the thread's index, so each slot is written once.
    let w = [
        0x8706_FF05,
        0x0000_0FC0, // s_and_b32 s6, s5, 0xfc0
        0xD765_0001,
        0x0200_0CC1, // v_mbcnt_lo_u32_b32 v1, -1, s6
        0xD766_0001,
        0x0202_02C1, // v_mbcnt_hi_u32_b32 v1, -1, v1
        0x3404_0282, // v_lshlrev_b32 v2, 2, v1
        0x7E06_0205, // v_mov_b32 v3, s5
        0xDC70_8000,
        0x0002_0302, // global_store_dword v2, v3, s[2:3]
        S_ENDPGM,
    ];
    let got = run_io(&w, 1 << 10, 128, 0);
    if got.is_empty() {
        return;
    }
    assert_eq!(got, (0..128).map(|i| i / 64 << 6 | 2).collect::<Vec<u32>>());
}

/// `v_ldexp_f32` (a pixel shader of GTA VI's world was refused for it): `s0` scaled by two to
/// the power `s1`, a signed integer, in range or past it.
#[test]
fn ldexp_scales_by_an_integer_power_of_two() {
    let cases: &[(f32, i32, f32)] = &[
        (1.5, 3, 12.0),
        (3.0, -2, 0.75),
        (1.0, 127, 2f32.powi(127)),
        (2f32.powi(-126), 200, 2f32.powi(74)),
        (2f32.powi(100), -200, 2f32.powi(-100)),
        (1.0, 1000, f32::INFINITY),
        (-1.0, 1000, f32::NEG_INFINITY),
        (1.0, -1000, 0.0),
    ];
    for &(a, e, want) in cases {
        let Some(mut t) = title() else { return };
        let f = Fixture::new(&t);
        let mut w = vec![
            0x7E04_0201, // v_mov_b32 v2, s1
            0xD762_0002, // v_ldexp_f32 v2, s0, v2
            0x0002_0400,
            V_LSHLREV_V1_2_V0,
        ];
        w.extend(GLOBAL_STORE_V2_AT_V1);
        w.push(S_ENDPGM);
        let cs = translate_io(&w, 5, 0, 64, shaders::options());
        let [c, d] = halves(VA_A + 0x4_0000);
        dispatch(&mut t, &cs, &[a.to_bits(), e as u32, c, d, 0], false);
        assert_eq!(t.shaders.refusals(), 0, "{:?}", t.shaders.refused);
        let got = f.get(DST, 1)[0];
        assert_eq!(got, want.to_bits(), "ldexp({a}, {e}) = {}, want {want}", f32::from_bits(got));
    }
}

/// `v_mov_b32 v1, -1; v_readlane_b32 s7, v1, 0`: a lane read, which fills every thread's exchange
/// slot with -1.
const FILL_XCHG: [u32; 3] = [0x7E02_02C1, 0xD760_0007, 0x0201_0101];

#[test]
fn a_group_of_96_lanes_exchanges_nothing_with_the_lanes_it_lacks() {
    // Two console waves, the second of 32 lanes: one host wave, no upper half to exchange with.
    // Each lane stores the count of lanes above 5 in its wave's ballot, plus the -1 of the fill:
    // the second wave's high dword is no lane's, 0, not the slot the fill left.
    let mut w = FILL_XCHG.to_vec();
    w.extend([
        0x7D82_0085, // v_cmp_lt_u32 vcc, 5, v0
        0xBE86_106A, // s_bcnt1_i32_b64 s6, vcc
        0x8006_0706, // s_add_u32 s6, s6, s7
        V_LSHLREV_V1_2_V0,
        0x7E04_0206, // v_mov_b32 v2, s6
    ]);
    w.extend(GLOBAL_STORE_V2_AT_V1);
    w.push(S_ENDPGM);
    let got = run_io(&w, 0, 96, 0);
    if got.is_empty() {
        return;
    }
    assert_eq!(got[..64], [57; 64]);
    assert_eq!(got[64..], [31; 32]);

    // A whole-wave sum of the lanes' indices (the lanes the wave lacks add the 0 of the
    // v_cndmask), plus the fill's -1: the missing upper half adds nothing.
    let mut w = FILL_XCHG.to_vec();
    w.extend([
        0xBE88_25C1, // s_or_saveexec_b64 s[8:9], -1
        0xD501_0001, 0x0022_0080, // v_cndmask_b32_e64 v1, 0, v0, s[8:9]
        0x4A02_02FA, 0xFF00_B101, // v_add_nc_u32_dpp v1, v1, v1 quad_perm:[1,0,3,2]
        0x4A02_02FA, 0xFF00_4E01, // v_add_nc_u32_dpp v1, v1, v1 quad_perm:[2,3,0,1]
        0x4A02_02FA, 0xFF01_4101, // v_add_nc_u32_dpp v1, v1, v1 row_half_mirror
        0x4A02_02FA, 0xFF01_4001, // v_add_nc_u32_dpp v1, v1, v1 row_mirror
        0xD778_0002, 0x0305_8301, // v_permlanex16_b32 v2, v1, -1, -1
        0x4A02_0501, // v_add_nc_u32_e32 v1, v1, v2
        0xD760_0004, 0x0201_0101, // v_readlane_b32 s4, v1, 0
        0xD760_0005, 0x0201_4101, // v_readlane_b32 s5, v1, 32
        0xD525_0003, 0x0200_0A04, // v_add_nc_u32_e64 v3, s4, s5
        0xBEFE_0408, // s_mov_b64 exec, s[8:9]
        0x4A06_0607, // v_add_nc_u32_e32 v3, s7, v3
        V_LSHLREV_V1_2_V0,
        0xDC70_8000, 0x0002_0301, // global_store_dword v1, v3, s[2:3]
        S_ENDPGM,
    ]);
    let got = run_io(&w, 0, 96, 0);
    assert_eq!(got[..64], [(0..64).sum::<u32>() - 1; 64]);
    assert_eq!(got[64..], [(64..96).sum::<u32>() - 1; 32]);
}

/// A whole-wave product (`WaveActiveProduct`): lanes 0..34 hold 2.0, the rest 1.0, so both
/// halves contribute and every lane must see 2^35.
#[test]
fn a_whole_wave_product_multiplies_both_halves() {
    let mut w = vec![
        0x7D88_00A3, // v_cmp_gt_u32 vcc, 35, v0
        0xD501_0004,
        0x01A9_E8F2, // v_cndmask_b32_e64 v4, 1.0, 2.0, vcc
        0xBE88_25C1, // s_or_saveexec_b64 s[8:9], -1
        0xD501_0001,
        0x0022_08F2, // v_cndmask_b32_e64 v1, 1.0, v4, s[8:9]
        0x1002_02FA,
        0xFF00_B101, // v_mul_f32_dpp v1, v1, v1 quad_perm:[1,0,3,2]
        0x1002_02FA,
        0xFF00_4E01, // v_mul_f32_dpp v1, v1, v1 quad_perm:[2,3,0,1]
        0x1002_02FA,
        0xFF01_4101, // v_mul_f32_dpp v1, v1, v1 row_half_mirror
        0x1002_02FA,
        0xFF01_4001, // v_mul_f32_dpp v1, v1, v1 row_mirror
        0xD778_0002,
        0x0305_8301, // v_permlanex16_b32 v2, v1, -1, -1
        0x1002_0501, // v_mul_f32_e32 v1, v1, v2
        0xD760_0004,
        0x0201_0101, // v_readlane_b32 s4, v1, 0
        0xD760_0005,
        0x0201_4101, // v_readlane_b32 s5, v1, 32
        0xD508_0003,
        0x0200_0A04, // v_mul_f32_e64 v3, s4, s5
        0xBEFE_0408, // s_mov_b64 exec, s[8:9]
        V_LSHLREV_V1_2_V0,
        0xDC70_8000,
        0x0002_0301, // global_store_dword v1, v3, s[2:3]
    ];
    w.push(S_ENDPGM);
    let got = run(&w, 0, 0);
    if got.is_empty() {
        return;
    }
    assert_eq!(got, [2f32.powi(35).to_bits(); 64]);
}

/// A dispatch of thread counts (`DispatchX` with thread dimensions, `Options::thread_counts`) of
/// `count` threads in x, in groups of `per` lanes of 64-lane waves (`wave64`) or 32: the first
/// `slots` dwords of `dst`. `s5` is the group id.
fn run_threads(words: &[u32], wave64: bool, per: u32, count: u32, slots: usize) -> Vec<u32> {
    let Some(mut t) = title() else { return Vec::new() };
    let f = Fixture::new(&t);
    let options = victory_shader::emit::Options { thread_counts: true, ..shaders::options() };
    let cs = translate_wave(words, 5, 1 << 7, per, wave64, options);
    let Pipe::Compute(pso) = t.compute_pipe(&cs, "threads") else {
        panic!("not made: {:?}", t.shaders)
    };
    let mut l = NativeList::default();
    l.set_compute_pipeline(&pso);
    let [c, d] = halves(VA_A + 0x4_0000);
    l.set_compute_root_constants(0, &[0, 0, c, d, 0]);
    l.dispatch_threads([count, 1, 1]).unwrap();
    super::run(&t.host, &[&l]);
    assert_eq!(t.shaders.refusals(), 0, "{:?}", t.shaders.refused);
    f.get(DST, slots)
}

/// Each lane EXEC has on stores the count of EXEC's lanes at `dst[group * lanes + lane]`:
/// `s_bcnt1_i32_b64 s6, exec` (`_b32`, `exec_lo` in 32-lane waves), `s_lshl_b32 s7, s5, 6` (5).
fn count_of_exec(wave64: bool) -> Vec<u32> {
    let (count, shift) = if wave64 { (0xBE86_107E, 0x8F07_8605) } else { (0xBE86_0F7E, 0x8F07_8505) };
    let mut w = vec![
        count,
        0x7E04_0206, // v_mov_b32 v2, s6
        shift,
        0x4A02_0007, // v_add_nc_u32 v1, s7, v0
        0x3402_0282, // v_lshlrev_b32 v1, 2, v1
    ];
    w.extend(GLOBAL_STORE_V2_AT_V1);
    w.push(S_ENDPGM);
    w
}

#[test]
fn a_dispatch_of_thread_counts_starts_the_lanes_past_them_off() {
    // 100 threads in groups of 64: the second group's lanes from 36 on are not launched. They
    // store nothing and the group's count of EXEC is 36 (the ray-tracing node writers of Grand
    // Theft Auto VI count their nodes so; run as 100 groups they wrote 6,400).
    let w = count_of_exec(true);
    let got = run_threads(&w, true, 64, 100, 192);
    if got.is_empty() {
        return;
    }
    assert_eq!(got[..64], [64; 64]);
    assert_eq!(got[64..100], [36; 36]);
    assert_eq!(got[100..], [CANARY; 92]);
    // One host wave's worth: the upper half of the console wave has no lane on.
    let got = run_threads(&w, true, 64, 21, 64);
    assert_eq!(got[..21], [21; 21]);
    assert_eq!(got[21..], [CANARY; 43]);
    // A count that fills its groups leaves every lane on.
    let got = run_threads(&w, true, 64, 128, 192);
    assert_eq!(got[..128], [64; 128]);
    assert_eq!(got[128..], [CANARY; 64]);
}

#[test]
fn a_dispatch_of_thread_counts_in_32_lane_waves() {
    // The title's third such shader runs 32-lane waves, one thread of one group.
    let w = count_of_exec(false);
    let got = run_threads(&w, false, 32, 1, 64);
    if got.is_empty() {
        return;
    }
    assert_eq!(got[0], 1);
    assert_eq!(got[1..], [CANARY; 63]);
    let got = run_threads(&w, false, 32, 40, 64);
    assert_eq!(got[..32], [32; 32]);
    assert_eq!(got[32..40], [8; 8]);
    assert_eq!(got[40..], [CANARY; 24]);
}

#[test]
fn a_whole_wave_sum_under_thread_counts_leaves_out_the_lanes_past_them() {
    // The sum of the lanes' indices with every lane forced on, the lanes EXEC had off adding the
    // 0 of the v_cndmask: the lanes past the count are such lanes, as on the console, where they
    // run the section too.
    let w = [
        0xBE88_25C1, // s_or_saveexec_b64 s[8:9], -1
        0xD501_0001, 0x0022_0080, // v_cndmask_b32_e64 v1, 0, v0, s[8:9]
        0x4A02_02FA, 0xFF00_B101, // v_add_nc_u32_dpp v1, v1, v1 quad_perm:[1,0,3,2]
        0x4A02_02FA, 0xFF00_4E01, // v_add_nc_u32_dpp v1, v1, v1 quad_perm:[2,3,0,1]
        0x4A02_02FA, 0xFF01_4101, // v_add_nc_u32_dpp v1, v1, v1 row_half_mirror
        0x4A02_02FA, 0xFF01_4001, // v_add_nc_u32_dpp v1, v1, v1 row_mirror
        0xD778_0002, 0x0305_8301, // v_permlanex16_b32 v2, v1, -1, -1
        0x4A02_0501, // v_add_nc_u32_e32 v1, v1, v2
        0xD760_0004, 0x0201_0101, // v_readlane_b32 s4, v1, 0
        0xD760_0005, 0x0201_4101, // v_readlane_b32 s5, v1, 32
        0xD525_0003, 0x0200_0A04, // v_add_nc_u32_e64 v3, s4, s5
        0xBEFE_0408, // s_mov_b64 exec, s[8:9]
        V_LSHLREV_V1_2_V0,
        0xDC70_8000, 0x0002_0301, // global_store_dword v1, v3, s[2:3]
        S_ENDPGM,
    ];
    let got = run_threads(&w, true, 64, 45, 64);
    if got.is_empty() {
        return;
    }
    assert_eq!(got[..45], [(0..45).sum::<u32>(); 45]);
    assert_eq!(got[45..], [CANARY; 19]);
}

#[test]
fn a_mask_moved_to_every_lane_before_a_branch_to_a_join_holds_their_lanes() {
    // A join of a lane mask where lanes arrive from two places (per lane, as the translation
    // follows the console's EXEC regions): lanes 0..39 set `s[8:9]` to -1 and branch to it, lanes
    // 40..63 skip that region and compare themselves against 50. Each reads its own path's bit:
    // 1 below 50, 0 from 50 (was refused: "data, at a join of several edges").
    let w = [
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE84_246A, // s_and_saveexec_b64 s[4:5], vcc
        0x8A86_7E04, // s_andn2_b64 s[6:7], s[4:5], exec
        skip_execz(2),
        0xBE88_04C1, // s_mov_b64 s[8:9], -1
        0xBF82_0003, // s_branch to the join
        0xBEFE_0406, // s_mov_b64 exec, s[6:7]
        0xD4C4_0008, 0x0202_00B2, // v_cmp_gt_u32 s[8:9], 50, v0
        0xBEFE_0404, // join: s_mov_b64 exec, s[4:5]
        0xD501_0002, 0x0021_0280, // v_cndmask_b32 v2, 0, 1, s[8:9]
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0xDC70_8000, 0x0002_0201, // global_store_dword v1, v2, s[2:3]
        S_ENDPGM,
    ];
    let got = run(&w, 0, 0);
    if got.is_empty() {
        return;
    }
    assert_eq!(got[..50], [1; 50]);
    assert_eq!(got[50..], [0; 14]);
}

#[test]
fn a_mask_anded_with_an_exec_its_skipping_lanes_are_not_in_is_theirs_at_zero() {
    // `s[8:9]` holds an address (data) until lanes 0..39 make it the lanes below 10 or outside
    // EXEC (`s_orn2`, which sets lanes 40..63); lanes 40..63 skip that and join holding the
    // address, but the join's only read is an AND with an EXEC they are not in, so each reads a 0
    // there (was refused: "data, ...").
    let w = [
        0xBE88_0402, // s_mov_b64 s[8:9], s[2:3]
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE84_246A, // s_and_saveexec_b64 s[4:5], vcc
        skip_execz(2),
        0x7D88_008A, // v_cmp_gt_u32 vcc, 10, v0
        0x8B88_7E6A, // s_orn2_b64 s[8:9], vcc, exec: lanes 40..63 set too
        0x8788_7E08, // join: s_and_b64 s[8:9], s[8:9], exec
        0xBEFE_0404, // s_mov_b64 exec, s[4:5]
        0xD501_0002, 0x0021_0280, // v_cndmask_b32 v2, 0, 1, s[8:9]
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0xDC70_8000, 0x0002_0201, // global_store_dword v1, v2, s[2:3]
        S_ENDPGM,
    ];
    let got = run(&w, 0, 0);
    if got.is_empty() {
        return;
    }
    assert_eq!(got[..10], [1; 10]);
    assert_eq!(got[10..], [0; 54]);
}

/// [`run_io`] of a wave64 group of 64, `dwords` read back from `dst`.
fn run_lockstep(words: &[u32], lds: u32, dwords: usize) -> Vec<u32> {
    let Some(mut t) = title() else { return Vec::new() };
    let f = Fixture::new(&t);
    let cs = translate_io(words, 5, lds << 15, 64, shaders::options());
    let [c, d] = halves(VA_A + 0x4_0000);
    dispatch(&mut t, &cs, &[0, 0, c, d, 0], false);
    assert_eq!(t.shaders.refusals(), 0, "{:?}", t.shaders.refused);
    f.get(DST, dwords)
}

#[test]
fn a_lane_read_behind_an_exec_branch_runs_in_lockstep() {
    // Lanes 0..39 read lane 35's `v3` (1035) and `exec_hi` as data (lanes 32..39: 0xff) behind
    // `s_cbranch_execz`: refused on the fast path (the halves split), run in lockstep.
    let w = [
        V_LSHLREV_V1_2_V0,
        0x4A06_00FF,
        0x0000_03E8, // v_add_nc_u32 v3, 1000, v0
        LANES_BELOW_40[0],
        LANES_BELOW_40[1],
        skip_execz(8),
        0xD760_0006,
        0x0001_4703, // v_readlane_b32 s6, v3, 35
        0x7E04_0206, // v_mov_b32 v2, s6
        GLOBAL_STORE_V2_AT_V1[0],
        GLOBAL_STORE_V2_AT_V1[1],
        0x7E04_027F, // v_mov_b32 v2, exec_hi
        0xDC70_8100,
        0x0002_0201, // global_store_dword v1, v2, s[2:3] offset:256
        0x88FE_047E, // s_or_b64 exec, exec, s[4:5]
        S_ENDPGM,
    ];
    let got = run_lockstep(&w, 0, 128);
    if got.is_empty() {
        return;
    }
    assert_eq!(got[..40], [1035; 40]);
    assert_eq!(got[40..64], [CANARY; 24]);
    assert_eq!(got[64..104], [0xff; 40]);
    assert_eq!(got[104..], [CANARY; 24]);
}

#[test]
fn a_first_lane_read_in_lockstep_is_the_console_waves_first_lane() {
    // Lanes 10..63 read the first lane EXEC leaves on (10: `v3` 1010), not each host wave's own
    // (32 in the upper one); with the lane read behind the branch the program runs in lockstep.
    let w = [
        V_LSHLREV_V1_2_V0,
        0x4A06_00FF,
        0x0000_03E8, // v_add_nc_u32 v3, 1000, v0
        0x7D82_0089, // v_cmp_lt_u32 vcc, 9, v0
        LANES_BELOW_40[1],
        skip_execz(9),
        0x7E0C_0503, // v_readfirstlane_b32 s6, v3
        0xD760_0007,
        0x0001_4703, // v_readlane_b32 s7, v3, 35
        0x7E04_0206, // v_mov_b32 v2, s6
        GLOBAL_STORE_V2_AT_V1[0],
        GLOBAL_STORE_V2_AT_V1[1],
        0x7E04_0207, // v_mov_b32 v2, s7
        0xDC70_8100,
        0x0002_0201, // global_store_dword v1, v2, s[2:3] offset:256
        0x88FE_047E, // s_or_b64 exec, exec, s[4:5]
        S_ENDPGM,
    ];
    let got = run_lockstep(&w, 0, 128);
    if got.is_empty() {
        return;
    }
    assert_eq!(got[..10], [CANARY; 10]);
    assert_eq!(got[10..64], [1010; 54]);
    assert_eq!(got[74..], [1035; 54]);
}
