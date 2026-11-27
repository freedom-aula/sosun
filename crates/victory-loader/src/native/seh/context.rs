//! The Windows x64 `CONTEXT` and `EXCEPTION_RECORD`, laid out as title code
//! (and its unwind and filter functions) read them.

/// `CONTEXT` (x64), 0x4D0 bytes. The general registers are one array in
/// the order unwind data names them: rax, rcx, rdx, rbx, rsp, rbp, rsi,
/// rdi, r8 to r15. `flt` is the `FXSAVE` image (`XMM_SAVE_AREA32`).
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct Context {
    pub p_home: [u64; 6],
    pub context_flags: u32,
    pub mx_csr: u32,
    pub seg_cs: u16,
    pub seg_ds: u16,
    pub seg_es: u16,
    pub seg_fs: u16,
    pub seg_gs: u16,
    pub seg_ss: u16,
    pub eflags: u32,
    pub dr: [u64; 6],
    pub gpr: [u64; 16],
    pub rip: u64,
    pub flt: [[u8; 16]; 32],
    pub vector_register: [[u8; 16]; 26],
    pub vector_control: u64,
    pub debug_control: u64,
    pub last_branch: [u64; 4],
}

pub const RAX: usize = 0;
pub const RCX: usize = 1;
pub const RDX: usize = 2;
pub const RBX: usize = 3;
pub const RSP: usize = 4;
pub const RBP: usize = 5;

/// `CONTEXT_CONTROL | CONTEXT_INTEGER | CONTEXT_SEGMENTS | CONTEXT_FLOATING_POINT`.
pub const CONTEXT_FULL: u32 = 0x10_000F;
pub const CONTEXT_CONTROL: u32 = 0x10_0001;
pub const CONTEXT_INTEGER: u32 = 0x10_0002;
pub const CONTEXT_FLOATING_POINT: u32 = 0x10_0008;
/// Set on a context unwound to a call site: its `rip` is a return address.
pub const CONTEXT_UNWOUND_TO_CALL: u32 = 0x2000_0000;

/// Offset in `flt` of `xmm0` (16 bytes each, 16 registers).
const XMM0_SLOT: usize = 10;

impl Context {
    pub fn zeroed() -> Context {
        // SAFETY: every field is an integer or an array of integers.
        unsafe { std::mem::zeroed() }
    }

    pub fn rsp(&self) -> u64 {
        self.gpr[RSP]
    }

    pub fn xmm(&self, i: usize) -> &[u8; 16] {
        &self.flt[XMM0_SLOT + i]
    }

    pub fn xmm_mut(&mut self, i: usize) -> &mut [u8; 16] {
        &mut self.flt[XMM0_SLOT + i]
    }

    pub fn set_mxcsr(&mut self, value: u32) {
        self.mx_csr = value;
        self.flt[1][8..12].copy_from_slice(&value.to_le_bytes());
    }

    /// Fills the integer, control and segment registers from Darwin's
    /// `x86_thread_state64`: rax, rbx, rcx, rdx, rdi, rsi, rbp, rsp, r8 to
    /// r15, rip, rflags, cs, fs, gs.
    pub fn set_from_thread_state(&mut self, t: &[u64; 21]) {
        self.gpr[..8].copy_from_slice(&[t[0], t[2], t[3], t[1], t[7], t[6], t[5], t[4]]);
        self.gpr[8..].copy_from_slice(&t[8..16]);
        self.rip = t[16];
        self.eflags = t[17] as u32;
        self.seg_cs = t[18] as u16;
        self.seg_fs = t[19] as u16;
        self.seg_gs = t[20] as u16;
        self.seg_ds = 0x2B;
        self.seg_es = 0x2B;
        self.seg_ss = 0x2B;
    }
}

const _: () = {
    assert!(size_of::<Context>() == 0x4D0);
    assert!(align_of::<Context>() == 16);
    assert!(std::mem::offset_of!(Context, context_flags) == 0x30);
    assert!(std::mem::offset_of!(Context, eflags) == 0x44);
    assert!(std::mem::offset_of!(Context, gpr) == 0x78);
    assert!(std::mem::offset_of!(Context, rip) == 0xF8);
    assert!(std::mem::offset_of!(Context, flt) == 0x100);
    assert!(std::mem::offset_of!(Context, vector_register) == 0x300);
};

/// `EXCEPTION_RECORD`, 0x98 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ExceptionRecord {
    pub code: u32,
    pub flags: u32,
    pub record: *mut ExceptionRecord,
    pub address: usize,
    pub number_parameters: u32,
    pub information: [usize; 15],
}

const _: () = assert!(size_of::<ExceptionRecord>() == 0x98);

impl ExceptionRecord {
    pub fn new(code: u32, flags: u32, address: usize) -> ExceptionRecord {
        ExceptionRecord {
            code,
            flags,
            record: std::ptr::null_mut(),
            address,
            number_parameters: 0,
            information: [0; 15],
        }
    }
}

/// `EXCEPTION_POINTERS`: what filters and vectored handlers receive.
#[repr(C)]
pub struct ExceptionPointers {
    pub record: *mut ExceptionRecord,
    pub context: *mut Context,
}

/// `DISPATCHER_CONTEXT`: what a language handler (`__C_specific_handler`)
/// is told about its frame.
#[repr(C)]
pub struct DispatcherContext {
    pub control_pc: u64,
    pub image_base: u64,
    pub function_entry: *const super::unwind::RuntimeFunction,
    pub establisher_frame: u64,
    pub target_ip: u64,
    pub context_record: *mut Context,
    pub language_handler: usize,
    pub handler_data: usize,
    pub history_table: *mut std::ffi::c_void,
    pub scope_index: u32,
    pub fill: u32,
}

// `ExceptionFlags`.
pub const EXCEPTION_NONCONTINUABLE: u32 = 0x01;
pub const EXCEPTION_UNWINDING: u32 = 0x02;
pub const EXCEPTION_EXIT_UNWIND: u32 = 0x04;
pub const EXCEPTION_TARGET_UNWIND: u32 = 0x20;
pub const EXCEPTION_COLLIDED_UNWIND: u32 = 0x40;

// Exception codes.
pub const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
pub const EXCEPTION_DATATYPE_MISALIGNMENT: u32 = 0x8000_0002;
pub const EXCEPTION_BREAKPOINT: u32 = 0x8000_0003;
pub const EXCEPTION_SINGLE_STEP: u32 = 0x8000_0004;
pub const EXCEPTION_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
pub const EXCEPTION_NONCONTINUABLE_EXCEPTION: u32 = 0xC000_0025;
pub const EXCEPTION_INT_DIVIDE_BY_ZERO: u32 = 0xC000_0094;
pub const EXCEPTION_INT_OVERFLOW: u32 = 0xC000_0095;
pub const EXCEPTION_FLT_DIVIDE_BY_ZERO: u32 = 0xC000_008E;
pub const EXCEPTION_FLT_INEXACT_RESULT: u32 = 0xC000_008F;
pub const EXCEPTION_FLT_INVALID_OPERATION: u32 = 0xC000_0090;
pub const EXCEPTION_FLT_OVERFLOW: u32 = 0xC000_0091;
pub const EXCEPTION_FLT_UNDERFLOW: u32 = 0xC000_0093;
pub const EXCEPTION_ARRAY_BOUNDS_EXCEEDED: u32 = 0xC000_008C;
/// `STATUS_UNWIND`, the code of the record `RtlUnwind` makes.
pub const STATUS_UNWIND: u32 = 0xC000_0027;
/// `STATUS_INVALID_UNWIND_TARGET`.
pub const STATUS_INVALID_UNWIND_TARGET: u32 = 0xC000_0029;

// Filter and vectored-handler results.
pub const EXCEPTION_EXECUTE_HANDLER: i32 = 1;
pub const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
pub const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;

// Language handler dispositions.
pub const EXCEPTION_CONTINUE_EXECUTION_DISPOSITION: u32 = 0;
pub const EXCEPTION_CONTINUE_SEARCH_DISPOSITION: u32 = 1;
