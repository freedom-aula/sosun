//! Host I/O errors as the Win32 errors Windows gives for them: the UCRT's `_dosmaperr` table
//! (Win32 error to `errno`) read backwards. Each caller picks the error for the rest.

use std::io::{self, ErrorKind};

/// The Win32 error `error` stands for, if the table names one.
pub(crate) fn code(error: &io::Error) -> Option<u32> {
    // `errno` values below 35 are the same on every Unix; std names no kind for these.
    Some(match (error.raw_os_error(), error.kind()) {
        (Some(9), _) => 6,                        // EBADF: ERROR_INVALID_HANDLE
        (Some(23 | 24), _) => 4,                  // ENFILE, EMFILE: ERROR_TOO_MANY_OPEN_FILES
        (_, ErrorKind::NotFound) => 2,            // ENOENT: ERROR_FILE_NOT_FOUND
        (_, ErrorKind::PermissionDenied) => 5,    // EACCES, EPERM: ERROR_ACCESS_DENIED
        (_, ErrorKind::OutOfMemory) => 8,         // ENOMEM: ERROR_NOT_ENOUGH_MEMORY
        (_, ErrorKind::CrossesDevices) => 17,     // EXDEV: ERROR_NOT_SAME_DEVICE
        (_, ErrorKind::AlreadyExists) => 80,      // EEXIST: ERROR_FILE_EXISTS
        (_, ErrorKind::InvalidInput) => 87,       // EINVAL: ERROR_INVALID_PARAMETER
        (_, ErrorKind::StorageFull) => 112,       // ENOSPC: ERROR_DISK_FULL
        (_, ErrorKind::DirectoryNotEmpty) => 145, // ENOTEMPTY: ERROR_DIR_NOT_EMPTY
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_errors_have_their_win32_codes() {
        let code = |errno| code(&io::Error::from_raw_os_error(errno));
        for (errno, win32) in [
            (2, 2),    // ENOENT
            (1, 5),    // EPERM
            (13, 5),   // EACCES
            (12, 8),   // ENOMEM
            (9, 6),    // EBADF
            (24, 4),   // EMFILE
            (23, 4),   // ENFILE
            (18, 17),  // EXDEV
            (17, 80),  // EEXIST
            (22, 87),  // EINVAL
            (28, 112), // ENOSPC
            (66, 145), // ENOTEMPTY (macOS)
        ] {
            assert_eq!(code(errno), Some(win32), "errno {errno}");
        }
        assert_eq!(code(5), None, "EIO: the caller's choice");
    }
}
