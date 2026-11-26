//! Command signatures: the record layout of indirect execution. Kept for
//! translating it later.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{
    self, COMMAND_SIGNATURE, E_INVALIDARG, HRESULT, Iid, Object, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

const SIGNATURE_IIDS: [Iid; 4] = [
    guid(
        0xc36a797c,
        0xec80,
        0x4f0a,
        [0x89, 0x85, 0xa7, 0xb2, 0x47, 0x50, 0x82, 0xd1],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
pub(crate) struct Desc {
    stride: u32,
    count: u32,
    /// Each a type and up to three 32-bit fields.
    arguments: *const [u32; 4],
    nodes: u32,
}

#[repr(C)]
struct Signature {
    object: Object,
    stride: u32,
    arguments: Vec<[u32; 4]>,
}

/// Device method.
pub(crate) extern "system" fn create(
    _: This,
    desc: *const Desc,
    _root: This,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if desc.arguments.is_null() && desc.count > 0 {
        return E_INVALIDARG;
    }
    let arguments = if desc.count == 0 {
        Vec::new()
    } else {
        // SAFETY: the title passes `count` argument descriptions.
        unsafe { std::slice::from_raw_parts(desc.arguments, desc.count as usize) }.to_vec()
    };
    tracing::debug!(stride = desc.stride, ?arguments, "command signature");
    let signature = Box::new(Signature {
        object: Object::new(&FUNCTIONS),
        stride: desc.stride,
        arguments,
    });
    object::hand_out(COMMAND_SIGNATURE, signature, iid, &SIGNATURE_IIDS, out)
}

/// A signature's record stride and arguments (type, then up to three
/// fields each).
pub(crate) fn layout<'a>(this: This) -> Option<(u32, &'a [[u32; 4]])> {
    if this.is_null() {
        return None;
    }
    // SAFETY: the title passes one of our signatures.
    let s = unsafe { &*this.cast::<Signature>() };
    Some((s.stride, &s.arguments))
}

/// What one `ExecuteIndirect` record does, for the signatures the native host runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) enum Command {
    /// `D3D12_DRAW_ARGUMENTS`, 16 bytes.
    Draw,
    /// `D3D12_DRAW_INDEXED_ARGUMENTS`, 20 bytes.
    DrawIndexed,
    /// `D3D12_DISPATCH_ARGUMENTS`, 12 bytes.
    Dispatch,
    /// `D3D12_DISPATCH_MESH_ARGUMENTS`, 12 bytes.
    DispatchMesh,
}

impl Command {
    /// The bytes of its record.
    pub(crate) fn size(self) -> u32 {
        match self {
            Command::Draw => 16,
            Command::DrawIndexed => 20,
            Command::Dispatch | Command::DispatchMesh => 12,
        }
    }
}

/// The command of a signature whose only argument is a draw or dispatch (types 0, 1, 2 and 10 of
/// `D3D12_INDIRECT_ARGUMENT_TYPE`, and the console's 105, a dispatch whose record is the three
/// group counts: a guess, see `docs/native-host.md`), and whose stride holds the record. `None`
/// for any other signature: one that also changes root arguments, vertex or index buffers.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn command(stride: u32, arguments: &[[u32; 4]]) -> Option<Command> {
    let command = match arguments {
        [[0, ..]] => Command::Draw,
        [[1, ..]] => Command::DrawIndexed,
        [[2 | 105, ..]] => Command::Dispatch,
        [[10, ..]] => Command::DispatchMesh,
        _ => return None,
    };
    (stride >= command.size()).then_some(command)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &SIGNATURE_IIDS).then_some(this);
    object::answer(COMMAND_SIGNATURE, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Signature>(this)
}

/// IUnknown 3, object 4, device child 1.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<COMMAND_SIGNATURE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table::new(f)
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_titles_five_signatures_decode() {
        // The five signatures of the prologue world load (2026-11-27), fields as logged: the
        // words after a dispatch's type are not part of its description.
        assert_eq!(command(12, &[[2, 1, 0, 0]]), Some(Command::Dispatch));
        assert_eq!(command(12, &[[10, 0, 2149590408, 1]]), Some(Command::DispatchMesh));
        assert_eq!(command(12, &[[105, 0, 60000, 0]]), Some(Command::Dispatch));
        assert_eq!(command(20, &[[1, 0, 1106733220, 1]]), Some(Command::DrawIndexed));
        assert_eq!(command(16, &[[0, 1, 8, 0]]), Some(Command::Draw));
    }

    #[test]
    fn signatures_the_host_cannot_run_do_not_decode() {
        // A root constant before the draw, a vertex buffer view, nothing, a stride too short.
        assert_eq!(command(24, &[[5, 0, 0, 2], [0, 0, 0, 0]]), None);
        assert_eq!(command(16, &[[3, 0, 0, 0]]), None);
        assert_eq!(command(16, &[]), None);
        assert_eq!(command(16, &[[1, 0, 0, 0]]), None);
        // A longer stride than the record (padding between records) is fine.
        assert_eq!(command(32, &[[0, 0, 0, 0]]).map(Command::size), Some(16));
    }
    use crate::object::{E_NOINTERFACE, E_POINTER, S_FALSE, S_OK, slot};

    #[test]
    fn null_or_argument_free_descriptions_are_rejected() {
        let null = std::ptr::null();
        let mut signature = null;
        // No description at all.
        assert_eq!(
            create(
                null,
                std::ptr::null(),
                null,
                &SIGNATURE_IIDS[0],
                &mut signature
            ),
            E_INVALIDARG
        );
        assert!(signature.is_null());
        // A count of arguments with nothing behind the pointer.
        let argumentless = Desc {
            stride: 16,
            count: 3,
            arguments: std::ptr::null(),
            nodes: 0,
        };
        assert_eq!(
            create(
                null,
                &argumentless,
                null,
                &SIGNATURE_IIDS[0],
                &mut signature
            ),
            E_INVALIDARG
        );
        assert!(signature.is_null());
    }

    #[test]
    fn signatures_without_arguments_stay_empty() {
        let null = std::ptr::null();
        // Zero arguments and no pointer is a valid, if useless, signature.
        let empty = Desc {
            stride: 16,
            count: 0,
            arguments: std::ptr::null(),
            nodes: 0,
        };
        let mut signature = null;
        assert_eq!(
            create(null, &empty, null, &SIGNATURE_IIDS[0], &mut signature),
            S_OK
        );
        let (stride, arguments) = layout(signature).expect("one of our signatures");
        assert_eq!(stride, 16);
        assert!(arguments.is_empty());
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(signature, 0x10)(signature),
            0
        );
        // The argument pointer is not read when the count is zero.
        let none: [[u32; 4]; 0] = [];
        let unread = Desc {
            stride: 16,
            count: 0,
            arguments: none.as_ptr(),
            nodes: 0,
        };
        let mut signature = null;
        assert_eq!(
            create(null, &unread, null, &SIGNATURE_IIDS[1], &mut signature),
            S_OK
        );
        let (_, arguments) = layout(signature).expect("one of our signatures");
        assert!(arguments.is_empty());
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(signature, 0x10)(signature),
            0
        );
    }

    #[test]
    fn arguments_are_copied_out_of_title_memory() {
        // A title reuses the memory its description lived in, while
        // ExecuteIndirect reads the layout long after creation.
        assert!(
            layout(std::ptr::null()).is_none(),
            "no signature, no layout"
        );
        let draw = [0u32, 3, 1, 0]; // three vertices, one instance
        let dispatch = [2u32, 4, 1, 1]; // dispatch 4x1x1 groups
        let expected = [draw, dispatch];
        let mut title_memory = expected;
        let desc = Desc {
            stride: 20,
            count: 2,
            arguments: title_memory.as_ptr(),
            nodes: 0,
        };
        let null = std::ptr::null();
        let mut signature = null;
        assert_eq!(
            create(null, &desc, null, &SIGNATURE_IIDS[0], &mut signature),
            S_OK
        );
        title_memory.fill([9, 9, 9, 9]);
        let (stride, arguments) = layout(signature).expect("one of our signatures");
        assert_eq!(stride, 20);
        assert_eq!(arguments, &expected[..]);
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(signature, 0x10)(signature),
            0
        );
    }

    #[test]
    fn signatures_answer_their_interfaces_and_count_references() {
        let arguments = [[2u32, 4, 1, 1]]; // dispatch 4x1x1 groups
        let desc = Desc {
            stride: 12,
            count: 1,
            arguments: arguments.as_ptr(),
            nodes: 0,
        };
        let null = std::ptr::null();
        let mut signature = null;
        assert_eq!(
            create(null, &desc, null, &SIGNATURE_IIDS[0], &mut signature),
            S_OK
        );
        assert!(!signature.is_null());
        let query =
            slot::<extern "system" fn(This, *const Iid, *mut This) -> HRESULT>(signature, 0x0);
        // Every interface the signature answers to hands out the same object.
        for iid in &SIGNATURE_IIDS {
            let mut out = null;
            assert_eq!(query(signature, iid, &mut out), S_OK);
            assert_eq!(out as usize, signature as usize);
        }
        // An unknown interface writes null behind and reports it missing.
        let unknown = [0u8; 16];
        let mut out = signature;
        assert_eq!(query(signature, &unknown, &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        // A null interface id is tolerated rather than matched.
        let mut out = signature;
        assert_eq!(query(signature, std::ptr::null(), &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        // No room for the answer is a pointer error.
        assert_eq!(query(signature, &unknown, std::ptr::null_mut()), E_POINTER);
        // References: one from creation, four from the queries, one added;
        // each release hands back what is left, the last frees the signature.
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(signature, 0x8)(signature),
            6
        );
        let release = slot::<extern "system" fn(This) -> u32>(signature, 0x10);
        for left in [5, 4, 3, 2, 1, 0] {
            assert_eq!(release(signature), left);
        }
    }

    #[test]
    fn creation_without_an_out_pointer_only_probes() {
        let arguments = [[2u32, 4, 1, 1]]; // dispatch 4x1x1 groups
        let desc = Desc {
            stride: 12,
            count: 1,
            arguments: arguments.as_ptr(),
            nodes: 0,
        };
        // A known interface with no out pointer asks whether creation would
        // succeed; the signature is dropped instead of handed out.
        assert_eq!(
            create(
                std::ptr::null(),
                &desc,
                std::ptr::null(),
                &SIGNATURE_IIDS[0],
                std::ptr::null_mut()
            ),
            S_FALSE
        );
        // A null interface id is tolerated: no interface, no signature.
        let mut signature = std::ptr::null();
        assert_eq!(
            create(
                std::ptr::null(),
                &desc,
                std::ptr::null(),
                std::ptr::null(),
                &mut signature
            ),
            E_NOINTERFACE
        );
        assert!(signature.is_null());
    }
}
