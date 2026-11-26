//! Wave64 programs on a 32-lane host (`Options::wave32_host`,
//! `lower/halves.rs`): synthetic GFX10 programs, written for these tests
//! (assembled with LLVM's `llvm-mc -mcpu=gfx1030 -mattr=+wavefrontsize64`),
//! lowered with and without the option. What each lowering must contain is
//! checked in the DXIL it gives: the wave size, the group barriers and the
//! group-shared exchange array; `a_wave64_program_matches_a_reference_on_metal`
//! in `victory-metal` runs the output.

use super::lower::{self, Lowered, Parts};
use crate::native::{ComputeIo, HwStage, NativeProgram, StageIo};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
/// `s_cbranch_execz` and `s_cbranch_scc1` over the next `words` words.
const fn skip_execz(words: u32) -> u32 {
    0xBF88_0000 | words
}
const fn skip_scc1(words: u32) -> u32 {
    0xBF85_0000 | words
}

/// `v_cmp_lt_u32 vcc, 5, v0; s_bcnt1_i32_b64 s2, vcc`: the population count
/// of a ballot.
const BALLOT: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE82_106A, // s_bcnt1_i32_b64 s2, vcc
    S_ENDPGM,
];

/// `v_readlane_b32 s4, v1, 40`: lane 40, a lane of the upper half.
const READLANE: &[u32] = &[
    0x3402_0082, // v_lshlrev_b32_e32 v1, 2, v0
    0xD760_0004,
    0x0001_5101, // v_readlane_b32 s4, v1, 40
    S_ENDPGM,
];

/// Each lane reads `v0` of the lane 32 above it (modulo 64).
const BPERMUTE: &[u32] = &[
    0x4A02_00A0, // v_add_nc_u32_e32 v1, 32, v0
    0x3402_0282, // v_lshlrev_b32_e32 v1, 2, v1
    0xDACC_0000,
    0x0200_0001, // ds_bpermute_b32 v2, v1, v0
    S_ENDPGM,
];

/// What stays inside a host wave: a DPP row shift, lane numbers, a lane
/// write, a swizzle.
const IN_HALF: &[u32] = &[
    0x7E02_0300, // v_mov_b32_e32 v1, v0
    0x4A02_02FA,
    0xFF09_1101, // v_add_nc_u32_dpp v1, v1, v1 row_shr:1 bound_ctrl
    0xD765_0002,
    0x0001_00C1, // v_mbcnt_lo_u32_b32 v2, -1, 0
    0xD766_0002,
    0x0002_04C1, // v_mbcnt_hi_u32_b32 v2, -1, v2
    0xD761_0001,
    0x0001_5003, // v_writelane_b32 v1, s3, 40
    0xD8D4_041F,
    0x0300_0001, // ds_swizzle_b32 v3, v1 offset:0x041F (lane ^ 1)
    S_ENDPGM,
];

/// A readlane in the lanes below 10 only.
const DIVERGENT_READLANE: &[u32] = &[
    0x7D88_008A, // v_cmp_gt_u32_e32 vcc, 10, v0
    0xBE82_246A, // s_and_saveexec_b64 s[2:3], vcc
    skip_execz(2),
    0xD760_0004,
    0x0001_5101, // v_readlane_b32 s4, v1, 40
    0x88FE_027E, // s_or_b64 exec, exec, s[2:3]
    0x7E0A_0204, // v_mov_b32_e32 v5, s4 (the result is used)
    S_ENDPGM,
];

/// A readlane behind a branch on a scalar (the same in every lane).
const UNIFORM_BRANCH_READLANE: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE82_106A, // s_bcnt1_i32_b64 s2, vcc (SCC: some lane)
    skip_scc1(3),
    0xD760_0004,
    0x0001_5101, // v_readlane_b32 s4, v1, 40
    0x7E0A_0204, // v_mov_b32_e32 v5, s4
    S_ENDPGM,
];

/// A ballot in the lanes below 10 only.
const DIVERGENT_BALLOT: &[u32] = &[
    0x7D88_008A, // v_cmp_gt_u32_e32 vcc, 10, v0
    0xBE82_246A, // s_and_saveexec_b64 s[2:3], vcc
    skip_execz(2),
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE84_106A, // s_bcnt1_i32_b64 s4, vcc
    0x88FE_027E, // s_or_b64 exec, exec, s[2:3]
    0x7E0A_0204, // v_mov_b32_e32 v5, s4 (the result is used)
    S_ENDPGM,
];

/// Lowers `words` as a wave64 compute program of `lanes` threads (512 bytes
/// of LDS if `lds`).
fn lowered(
    words: &[u32],
    lanes: u32,
    lds: bool,
    wave32_host: bool,
) -> Result<Lowered, crate::ShaderError> {
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 256,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: u32::from(lds) << 15 | 2 << 1,
            threads: [lanes, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, true, 256).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let l = lower::lower(&Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
        atomic_locks: false,
        bias_as_gradients: false,
        narrow_waves: false,
        wave32_host,
        zero_copy_memory: false,
        view_marks: false,
    })?;
    // VICTORY_DUMP_DXIL=<dir>: keep the containers, to run Apple's converter on.
    if let (Some(dir), true) = (std::env::var_os("VICTORY_DUMP_DXIL"), wave32_host) {
        let hash = words.iter().fold(0u32, |h, w| h.rotate_left(5) ^ w);
        let name = format!("{hash:08x}_{lanes}.dxil");
        std::fs::write(std::path::Path::new(&dir).join(name), &l.dxil).unwrap();
    }
    Ok(l)
}

fn host32(words: &[u32], lanes: u32) -> Lowered {
    lowered(words, lanes, false, true).expect("lowers")
}

#[test]
fn without_the_option_wave64_programs_still_ask_for_64_lanes() {
    for words in [BALLOT, READLANE, BPERMUTE] {
        let l = lowered(words, 64, false, false).unwrap();
        assert_eq!(l.wave_size, Some(64));
        assert_eq!(l.barriers, 0);
        assert!(!l.exchanges);
    }
}

#[test]
fn a_ballot_of_64_lanes_is_two_host_waves_joined() {
    let l = host32(BALLOT, 64);
    assert_eq!(l.wave_size, Some(32));
    // Write, barrier, read, barrier.
    assert_eq!(l.barriers, 2);
    assert!(l.exchanges);
}

#[test]
fn a_lane_read_across_the_halves_goes_through_group_shared_memory() {
    for words in [READLANE, BPERMUTE] {
        let l = host32(words, 64);
        assert_eq!(l.wave_size, Some(32));
        assert!(l.exchanges);
    }
    assert_eq!(host32(READLANE, 64).barriers, 2);
    // The permute also ballots the lanes that run (0 from the others).
    assert_eq!(host32(BPERMUTE, 64).barriers, 4);
}

#[test]
fn operations_inside_32_lanes_stay_native() {
    let l = host32(IN_HALF, 64);
    assert_eq!(l.wave_size, Some(32));
    assert_eq!(l.barriers, 0);
    assert!(!l.exchanges);
}

#[test]
fn a_group_of_32_lanes_or_fewer_needs_no_exchange() {
    for words in [BALLOT, READLANE, BPERMUTE, IN_HALF] {
        let l = host32(words, 32);
        assert_eq!(l.wave_size, Some(32));
        assert_eq!(l.barriers, 0);
        assert!(!l.exchanges);
    }
}

#[test]
fn exchanges_wait_for_every_thread_to_arrive() {
    // In the lanes below 10: the second host wave never gets there. The
    // value is left unknown (the program is refused where something uses
    // it), and no barrier is emitted.
    for words in [DIVERGENT_READLANE, DIVERGENT_BALLOT] {
        let l = host32(words, 64);
        assert_eq!((l.barriers, l.exchanges), (0, false), "{words:x?}");
    }
    // Behind a scalar branch every lane takes the same way, both halves.
    assert_eq!(host32(UNIFORM_BRANCH_READLANE, 64).barriers, 4);
    // Not in a group of several waves: the scalar may differ between them.
    assert_eq!(host32(UNIFORM_BRANCH_READLANE, 128).barriers, 2);
}

#[test]
fn lds_a_wave_shares_unsynced_in_divergent_code_is_refused() {
    // Write and read both under the branch: no barrier can go there.
    const WRITE_READ_DIVERGENT: &[u32] = &[
        0x3402_0082, // v_lshlrev_b32_e32 v1, 2, v0
        0xBE80_047E, // s_mov_b64 s[0:1], exec
        0x7DA4_0080, // v_cmpx_eq_u32 exec, 0, v0
        skip_execz(4),
        0xD834_0000,
        0x0000_0001, // ds_write_b32 v1, v0
        0xD8D8_0004,
        0x0200_0001, // ds_read_b32 v2, v1 offset:4
        0xBEFE_0400, // s_mov_b64 exec, s[0:1]
        S_ENDPGM,
    ];
    let r = lowered(WRITE_READ_DIVERGENT, 64, true, true);
    assert!(matches!(r, Err(crate::ShaderError::Unsupported { .. })));
    // Where a barrier fits it is added, and no 64-lane wave is asked for.
    const WRITE_THEN_READ: &[u32] = &[
        0x3402_0082, // v_lshlrev_b32_e32 v1, 2, v0
        0xD834_0000,
        0x0000_0001, // ds_write_b32 v1, v0
        0xD8D8_0004,
        0x0200_0001, // ds_read_b32 v2, v1 offset:4
        S_ENDPGM,
    ];
    let l = lowered(WRITE_THEN_READ, 64, true, true).unwrap();
    assert_eq!(l.wave_size, None);
    assert_eq!(l.barriers, 1);
}

/// A sum over the 64 lanes the way a compiler expands `WaveActiveSum`: all
/// lanes on, butterflies inside each row of 16, then the rows' sums by lane.
const REDUCE: &[u32] = &[
    0xBE82_25C1, // s_or_saveexec_b64 s[2:3], -1
    0xD501_0001,
    0x000A_0080, // v_cndmask_b32_e64 v1, 0, v0, s[2:3]
    0x4A02_02FA,
    0xFF00_B101, // v_add_nc_u32_dpp v1, v1, v1 quad_perm:[1,0,3,2]
    0x4A02_02FA,
    0xFF00_4E01, // v_add_nc_u32_dpp v1, v1, v1 quad_perm:[2,3,0,1]
    0x4A02_02FA,
    0xFF01_4101, // v_add_nc_u32_dpp v1, v1, v1 row_half_mirror
    0x4A02_02FA,
    0xFF01_4001, // v_add_nc_u32_dpp v1, v1, v1 row_mirror
    0xD778_0002,
    0x0305_8301, // v_permlanex16_b32 v2, v1, -1, -1
    0x4A02_0501, // v_add_nc_u32_e32 v1, v1, v2
    0xD760_0004,
    0x0001_0101, // v_readlane_b32 s4, v1, 0
    0xD760_0005,
    0x0001_4101, // v_readlane_b32 s5, v1, 32
    0xD525_0003,
    0x0000_0A04, // v_add_nc_u32_e64 v3, s4, s5
    0xBEFE_0402, // s_mov_b64 exec, s[2:3]
    0x7E0A_0303, // v_mov_b32_e32 v5, v3
    S_ENDPGM,
];

#[test]
fn a_whole_wave_sum_combines_the_two_halves() {
    let l = host32(REDUCE, 64);
    assert_eq!(l.wave_size, Some(32));
    // Whatever the section leaves is one reduction: write, barrier, read, barrier.
    assert_eq!(l.barriers, 2);
}
