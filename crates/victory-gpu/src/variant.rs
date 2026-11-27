//! Device method 0x1b8: a pipeline from a pipeline of 0x1c8 (shaders) and graphics state records.
//! The state is decoded by [`crate::native::console_state`].

use crate::native::console_state::{self, MAX_RECORDS, RECORD_WORDS};
use crate::object::{E_INVALIDARG, HRESULT, Iid, This};

/// `(this, base, count, records, iid, out)`.
pub(crate) extern "system" fn create_variant(
    _: This,
    base: This,
    count: u32,
    records: *const u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if records.is_null() || count as usize > MAX_RECORDS {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes `count` records of `RECORD_WORDS` dwords.
    let records = unsafe { std::slice::from_raw_parts(records, count as usize * RECORD_WORDS) };
    crate::pipeline::derive(
        base,
        |from| {
            let mut state = from.cloned().unwrap_or_default();
            for tag in console_state::apply(&mut state, records) {
                tracing::debug!(tag, "graphics state record of an unknown kind");
            }
            state
        },
        iid,
        out,
    )
}
