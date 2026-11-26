//! A console wave of 64 lanes split into two 32-lane host waves (`lower/halves.rs` in
//! `victory_shader`), run on the GPU: synthetic GFX10 programs of our own (assembled with
//! `llvm-mc -arch=amdgcn -mcpu=gfx1030 -mattr=+wavefrontsize64`) whose lanes exchange values across
//! the halves after part of the wave has left. `s[2:3]` is `dst`, `s4` a flag. Skipped without the
//! Metal Shader Converter.

use super::addr_tests::{CANARY, Fixture, VA_A, dispatch, halves, title, translate_io};
use crate::native::shaders;

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
