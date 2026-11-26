//! A console wave of 64 lanes split into two 32-lane host waves (`lower/halves.rs` in
//! `victory_shader`), run on the GPU: synthetic GFX10 programs of our own (assembled with
//! `llvm-mc -arch=amdgcn -mcpu=gfx1030 -mattr=+wavefrontsize64`) whose lanes exchange values across
//! the halves after part of the wave has left. `s[2:3]` is `dst`, `s4` a flag. Skipped without the
//! Metal Shader Converter.

use super::addr_tests::{CANARY, Fixture, VA_A, dispatch, halves, title, translate_lds};
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
    let Some(mut t) = title() else { return Vec::new() };
    let f = Fixture::new(&t);
    let cs = translate_lds(words, 5, lds, shaders::options());
    let [c, d] = halves(VA_A + 0x4_0000);
    dispatch(&mut t, &cs, &[0, 0, c, d, flag], false);
    assert_eq!(t.shaders.refusals(), 0, "{:?}", t.shaders.refused);
    f.get(DST, 64)
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
