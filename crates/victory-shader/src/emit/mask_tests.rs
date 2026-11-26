//! Lane masks read as numbers in compute programs: synthetic wave64 GFX10
//! sequences (assembled with LLVM's `llvm-mc -mcpu=gfx1030
//! -mattr=+wavefrontsize64`), each followed by stores of the VGPRs it
//! writes so that a value the translation cannot make is refused, lowered
//! for the console's wave size and for the 32-lane host
//! (`Options::wave32_host`). A mask is a ballot (or, held where its lanes
//! are not those that run, the scalar operation's own result on the
//! console's lanes); SCC of an operation on masks read as data is
//! "some lane in the result".

use super::lower::{self, Lowered, Parts};
use crate::native::{ComputeIo, HwStage, NativeProgram, StageIo};
use crate::{io, ir, resource, structure};

/// `v_mov_b32 v1, 0; v_mov_b32 v2, 0; v_mov_b32 v5, 0; v_mov_b32 v8, 0;
/// v_mov_b32 v9, 0`: the registers a case writes and stores start defined.
const PRELUDE: &[u32] = &[
    0x7E02_0280,
    0x7E04_0280,
    0x7E0A_0280,
    0x7E10_0280,
    0x7E12_0280,
];

/// `global_store_dword v[8:9], v1, off` (and v2, v5), `s_endpgm`.
const EPILOGUE: &[u32] = &[
    0xDC70_8000,
    0x007D_0108,
    0xDC70_8000,
    0x007D_0208,
    0xDC70_8000,
    0x007D_0508,
    0xBF81_0000,
];

/// Lowers PRELUDE, `body` and EPILOGUE as a wave64 compute program of
/// `lanes` threads.
fn lowered(body: &[u32], lanes: u32, wave32_host: bool) -> Result<Lowered, crate::ShaderError> {
    let words: Vec<u32> = [PRELUDE, body, EPILOGUE].concat();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 256,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1,
            threads: [lanes, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, true, 256).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    lower::lower(&Parts {
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
        rect_list: false,
        copy_params: [0; 32],
        view_marks: false,
    })
}

const BCNT: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE82_106A, // s_bcnt1_i32_b64 s2, vcc
    0x7E0A_0202, // v_mov_b32_e32 v5, s2
];
const SCC_AFTER_AND: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8782_7E6A, // s_and_b64 s[2:3], vcc, exec
    0x8505_8987, // s_cselect_b32 s5, 7, 9
    0x7E02_0205, // v_mov_b32_e32 v1, s5
];
const MASK_TO_VGPR: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8782_7E6A, // s_and_b64 s[2:3], vcc, exec
    0x7E02_0202, // v_mov_b32_e32 v1, s2
    0x7E04_0203, // v_mov_b32_e32 v2, s3
];
const CMP_MASK_CSELECT: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBF12_806A, // s_cmp_eq_u64 vcc, 0
    0x8505_8987, // s_cselect_b32 s5, 7, 9
    0x7E02_0205, // v_mov_b32_e32 v1, s5
];
const LSHL_EXEC: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE82_047E, // s_mov_b64 s[2:3], exec
    0x8FFE_816A, // s_lshl_b64 exec, vcc, 1
    0x7E02_0283, // v_mov_b32_e32 v1, 3
    0xBEFE_0402, // s_mov_b64 exec, s[2:3]
];
const FF1_READLANE: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE84_146A, // s_ff1_i32_b64 s4, vcc
    0xD760_0005, // v_readlane_b32 s5, v1, s4
    0x0000_0901,
    0x7E04_0205, // v_mov_b32_e32 v2, s5
];
const AND_ARITH: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8704_FF6A, // s_and_b32 s4, vcc_lo, 0xff
    0x0000_00FF,
    0x8005_8104, // s_add_u32 s5, s4, 1
    0x7E02_0205, // v_mov_b32_e32 v1, s5
];
const ARITH_THEN_SELECT: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE82_046A, // s_mov_b64 s[2:3], vcc
    0x8002_8102, // s_add_u32 s2, s2, 1
    0xD501_0001, // v_cndmask_b32_e64 v1, 0, 1, s[2:3]
    0x0009_0280,
];
const LANE_SELECT_MASK: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE84_146A, // s_ff1_i32_b64 s4, vcc
    0x8F82_0481, // s_lshl_b64 s[2:3], 1, s4
    0xD501_0001, // v_cndmask_b32_e64 v1, 0, 1, s[2:3]
    0x0009_0280,
];
const WRITELANE_MASK: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8782_7E6A, // s_and_b64 s[2:3], vcc, exec
    0xD761_0001, // v_writelane_b32 v1, s2, 3
    0x0001_0602,
];
const BITSET: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE82_046A, // s_mov_b64 s[2:3], vcc
    0xBE82_1D85, // s_bitset1_b32 s2, 5
    0xD501_0001, // v_cndmask_b32_e64 v1, 0, 1, s[2:3]
    0x0009_0280,
];
const CMP_LO_BRANCH: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBF07_806A, // s_cmp_lg_u32 vcc_lo, 0
    0xBF85_0001, // s_cbranch_scc1 1
    0x7E02_0283, // v_mov_b32_e32 v1, 3
];
const LSHR_DATA: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x9084_886A, // s_lshr_b64 s[4:5], vcc, 8
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const SAVEEXEC_SCC_DATA: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE82_246A, // s_and_saveexec_b64 s[2:3], vcc
    0x8505_8987, // s_cselect_b32 s5, 7, 9
    0xBEFE_0402, // s_mov_b64 exec, s[2:3]
    0x7E02_0205, // v_mov_b32_e32 v1, s5
];
const ADDC_SCC: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8782_7E6A, // s_and_b64 s[2:3], vcc, exec
    0x8205_8005, // s_addc_u32 s5, s5, 0
    0x7E02_0205, // v_mov_b32_e32 v1, s5
];
const NOT_MASK_DATA: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE82_086A, // s_not_b64 s[2:3], vcc
    0x7E02_0202, // v_mov_b32_e32 v1, s2
];
const BFE_MASK: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x9384_FF6A, // s_bfe_u32 s4, vcc_lo, 0x80008
    0x0008_0008,
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const S_MUL_EXEC: &[u32] = &[
    0xBE82_047E, // s_mov_b64 s[2:3], exec
    0x9304_8302, // s_mul_i32 s4, s2, 3
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const POPCOUNT_EXEC_BRANCH: &[u32] = &[
    0xBE84_107E, // s_bcnt1_i32_b64 s4, exec
    0xBF06_C004, // s_cmp_eq_u32 s4, 64
    0x8505_8081, // s_cselect_b32 s5, 1, 0
    0x7E02_0205, // v_mov_b32_e32 v1, s5
];
const MASK_XOR_MASK_DATA: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xD4C4_0006, // v_cmp_gt_u32_e64 s[6:7], 9, v0
    0x0002_0089,
    0x8982_066A, // s_xor_b64 s[2:3], vcc, s[6:7]
    0xBF13_8002, // s_cmp_lg_u64 s[2:3], 0
    0x8505_8987, // s_cselect_b32 s5, 7, 9
    0x7E02_0205, // v_mov_b32_e32 v1, s5
];
const MASK_MIN_MAX: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8782_7E6A, // s_and_b64 s[2:3], vcc, exec
    0x8384_8502, // s_min_u32 s4, s2, 5
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const MASK_TO_VGPR_READFIRST: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xD501_0001, // v_cndmask_b32_e64 v1, 0, -1, vcc
    0x01A9_8280,
    0x7E08_0501, // v_readfirstlane_b32 s4, v1
];
const BCNT0: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE84_0E6A, // s_bcnt0_i32_b64 s4, vcc
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const FLBIT: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE84_166A, // s_flbit_i32_b64 s4, vcc
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const FF0: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE84_126A, // s_ff0_i32_b64 s4, vcc
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const BITCMP: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBF0D_876A, // s_bitcmp1_b32 vcc_lo, 7
    0x8504_8281, // s_cselect_b32 s4, 1, 2
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const BITCMP64: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBF0F_A86A, // s_bitcmp1_b64 vcc, 40
    0x8504_8281, // s_cselect_b32 s4, 1, 2
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const ASHR: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x9184_886A, // s_ashr_i64 s[4:5], vcc, 8
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const BFE64: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x9484_FF6A, // s_bfe_u64 s[4:5], vcc, 0x80008
    0x0008_0008,
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const BREV: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE84_0C6A, // s_brev_b64 s[4:5], vcc
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const MASK_EQ_EXEC_SCC: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBF12_7E6A, // s_cmp_eq_u64 vcc, exec
    0x8504_8281, // s_cselect_b32 s4, 1, 2
    0x7E02_0204, // v_mov_b32_e32 v1, s4
];
const MASK_COPY_DATA: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8784_7E6A, // s_and_b64 s[4:5], vcc, exec
    0xBE86_0304, // s_mov_b32 s6, s4
    0xBE87_0305, // s_mov_b32 s7, s5
    0x7E02_0206, // v_mov_b32_e32 v1, s6
    0x7E04_0207, // v_mov_b32_e32 v2, s7
];
const CSELECT_MASK_DATA: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8784_7E6A, // s_and_b64 s[4:5], vcc, exec
    0x8586_8004, // s_cselect_b64 s[6:7], s[4:5], 0
    0x7E02_0206, // v_mov_b32_e32 v1, s6
];
const SUB_MASK: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8784_7E6A, // s_and_b64 s[4:5], vcc, exec
    0x8086_8104, // s_sub_u32 s6, s4, 1
    0x8287_8005, // s_subb_u32 s7, s5, 0
    0x8786_0406, // s_and_b64 s[6:7], s[6:7], s[4:5]
    0x7E02_0206, // v_mov_b32_e32 v1, s6
    0x7E04_0207, // v_mov_b32_e32 v2, s7
];
const DIV_BCNT: &[u32] = &[
    0x7D88_008A, // v_cmp_gt_u32_e32 vcc, 10, v0
    0xBE82_246A, // s_and_saveexec_b64 s[2:3], vcc
    0xBF88_0004, // s_cbranch_execz 4
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0xBE84_106A, // s_bcnt1_i32_b64 s4, vcc
    0x7E02_0204, // v_mov_b32_e32 v1, s4
    0x88FE_027E, // s_or_b64 exec, exec, s[2:3]
];
const DIV_SCC: &[u32] = &[
    0x7D88_008A, // v_cmp_gt_u32_e32 vcc, 10, v0
    0xBE82_246A, // s_and_saveexec_b64 s[2:3], vcc
    0xBF88_0003, // s_cbranch_execz 3
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8786_7E6A, // s_and_b64 s[6:7], vcc, exec
    0x8504_8281, // s_cselect_b32 s4, 1, 2
    0x7E02_0204, // v_mov_b32_e32 v1, s4
    0x88FE_027E, // s_or_b64 exec, exec, s[2:3]
];
const VCC_TO_VGPR: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x7E02_026A, // v_mov_b32_e32 v1, vcc_lo
    0x7E04_026B, // v_mov_b32_e32 v2, vcc_hi
];
const ANDN2_EXEC_SCC: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8A84_6A7E, // s_andn2_b64 s[4:5], exec, vcc
    0x8506_8281, // s_cselect_b32 s6, 1, 2
    0x7E02_0206, // v_mov_b32_e32 v1, s6
];
const XOR_CNDMASK_SCALAR: &[u32] = &[
    0x7D82_0085, // v_cmp_lt_u32_e32 vcc, 5, v0
    0x8984_7E6A, // s_xor_b64 s[4:5], vcc, exec
    0xBE86_1404, // s_ff1_i32_b64 s6, s[4:5]
    0x8F88_0681, // s_lshl_b64 s[8:9], 1, s6
    0x8A88_086A, // s_andn2_b64 s[8:9], vcc, s[8:9]
    0xD501_0001, // v_cndmask_b32_e64 v1, 0, 1, s[8:9]
    0x0021_0280,
];

/// Every consumer of a mask as a number lowers: on the console's wave, and
/// on the 32-lane host with one or two host waves for the console's wave.
#[test]
fn masks_read_as_data_lower() {
    for (name, body) in [
        ("bcnt", BCNT),
        ("scc_after_and", SCC_AFTER_AND),
        ("mask_to_vgpr", MASK_TO_VGPR),
        ("cmp_mask_cselect", CMP_MASK_CSELECT),
        ("lshl_exec", LSHL_EXEC),
        ("ff1_readlane", FF1_READLANE),
        ("and_arith", AND_ARITH),
        ("arith_then_select", ARITH_THEN_SELECT),
        ("lane_select_mask", LANE_SELECT_MASK),
        ("writelane_mask", WRITELANE_MASK),
        ("bitset", BITSET),
        ("cmp_lo_branch", CMP_LO_BRANCH),
        ("lshr_data", LSHR_DATA),
        ("saveexec_scc_data", SAVEEXEC_SCC_DATA),
        ("addc_scc", ADDC_SCC),
        ("not_mask_data", NOT_MASK_DATA),
        ("bfe_mask", BFE_MASK),
        ("s_mul_exec", S_MUL_EXEC),
        ("popcount_exec_branch", POPCOUNT_EXEC_BRANCH),
        ("mask_xor_mask_data", MASK_XOR_MASK_DATA),
        ("mask_min_max", MASK_MIN_MAX),
        ("mask_to_vgpr_readfirst", MASK_TO_VGPR_READFIRST),
        ("bcnt0", BCNT0),
        ("flbit", FLBIT),
        ("ff0", FF0),
        ("bitcmp", BITCMP),
        ("bitcmp64", BITCMP64),
        ("ashr", ASHR),
        ("brev", BREV),
        ("mask_eq_exec_scc", MASK_EQ_EXEC_SCC),
        ("mask_copy_data", MASK_COPY_DATA),
        ("cselect_mask_data", CSELECT_MASK_DATA),
        ("sub_mask", SUB_MASK),
        ("div_scc", DIV_SCC),
        ("vcc_to_vgpr", VCC_TO_VGPR),
        ("andn2_exec_scc", ANDN2_EXEC_SCC),
        ("xor_cndmask_scalar", XOR_CNDMASK_SCALAR),
    ] {
        for (lanes, host32) in [(64, false), (64, true), (32, true)] {
            if let Err(e) = lowered(body, lanes, host32) {
                panic!("{name} ({lanes} lanes, wave32 host {host32}): {e:?}");
            }
        }
    }
}

/// SCC of `s_and_b64 s[2:3], vcc, exec` read as data (`s_cselect`,
/// `s_addc`, after `s_and_saveexec`) is a ballot of the result's lanes.
#[test]
fn scc_of_a_mask_operation_is_a_ballot() {
    for body in [SCC_AFTER_AND, ADDC_SCC, SAVEEXEC_SCC_DATA, DIV_SCC] {
        for host32 in [false, true] {
            lowered(body, 64, host32).unwrap_or_else(|e| panic!("{body:x?}: {e:?}"));
        }
    }
}

/// A mask shifted into EXEC or built from scalars holds lanes the
/// structurisation cannot see into: the console's lanes are asked for.
#[test]
fn a_shifted_or_built_mask_needs_the_consoles_lanes() {
    for body in [LSHL_EXEC, BITSET, LANE_SELECT_MASK] {
        let l = lowered(body, 64, true).unwrap();
        assert_eq!(l.wave_size, Some(32));
        let l = lowered(body, 64, false).unwrap();
        assert_eq!(l.wave_size, Some(64));
    }
}

/// What is not translated stays refused, saying what.
#[test]
fn what_has_no_form_is_refused_clearly() {
    // A 64-bit bit field of a mask.
    let r = lowered(BFE64, 64, false).err();
    assert!(
        matches!(
            r,
            Some(crate::ShaderError::Unsupported {
                what: "s_bfe_u64",
                ..
            })
        ),
        "{r:?}"
    );
    // A ballot in lanes below 10: the second host wave never gets there.
    let r = lowered(DIV_BCNT, 64, true).err();
    assert!(
        matches!(r, Some(crate::ShaderError::Unsupported { .. })),
        "{r:?}"
    );
}
