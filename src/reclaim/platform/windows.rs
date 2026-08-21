//! Windows filesystem operations for database reclamation.

#![allow(unsafe_code)]

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use crate::error::Error;

const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

unsafe extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
}

pub(super) fn rename_over(source: &Path, destination: &Path) -> io::Result<()> {
    let destination_path = destination;
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both pointers reference live, NUL-terminated UTF-16 buffers for the duration of
    // this synchronous call, and the declaration matches the documented MoveFileExW ABI.
    let succeeded = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        let failure = io::Error::last_os_error();
        return Err(io::Error::new(
            failure.kind(),
            format!("{failure} [{}]", describe_rename_target(destination_path)),
        ));
    }
    Ok(())
}

/// Describes the swap destination so a denied rename identifies what still guards the file.
///
/// Windows reports `ERROR_ACCESS_DENIED` for several unrelated conditions, so the raw code alone
/// does not say whether the file is read-only, still open, or something else entirely.
fn describe_rename_target(destination: &Path) -> String {
    use std::fs;

    let Ok(metadata) = fs::metadata(destination) else {
        return "destination metadata unavailable".to_owned();
    };
    let read_only = metadata.permissions().readonly();
    let openable_for_write = fs::OpenOptions::new()
        .write(true)
        .open(destination)
        .err()
        .map_or_else(|| "writable".to_owned(), |error| error.to_string());
    format!(
        "destination read_only={read_only} len={} exclusive_write={openable_for_write}",
        metadata.len()
    )
}

pub(super) fn rename_error(destination: &Path, source: io::Error) -> Error {
    if matches!(
        source.raw_os_error(),
        Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    ) {
        Error::DatabaseBusy {
            holders: Vec::new(),
        }
    } else {
        Error::Io {
            path: destination.to_path_buf(),
            source,
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;

    use super::*;
    use crate::error::Error;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;

    #[test]
    fn held_destination_returns_database_busy_without_replacing_original() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let source = directory.path().join("replacement.db");
        let destination = directory.path().join("opencode.db");
        let original = b"original database bytes";
        fs::write(&source, b"replacement database bytes")
            .expect("replacement database should be written");
        fs::write(&destination, original).expect("original database should be written");
        let original_size = fs::metadata(&destination)
            .expect("original database metadata should be readable")
            .len();
        let _held = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&destination)
            .expect("original database should be held without delete sharing");

        let source_error = rename_over(&source, &destination)
            .expect_err("held destination should prevent replacement");
        let error = rename_error(&destination, source_error);

        assert!(matches!(&error, Error::DatabaseBusy { .. }));
        assert_eq!(error.exit_code(), 5);
        assert_eq!(
            fs::read(&destination).expect("original database should remain readable"),
            original
        );
        assert_eq!(
            fs::metadata(&destination)
                .expect("original database metadata should remain readable")
                .len(),
            original_size
        );
    }

    #[test]
    fn other_rename_errors_remain_io_errors() {
        let path = Path::new("opencode.db");

        let error = rename_error(path, io::Error::from_raw_os_error(5));

        assert!(matches!(error, Error::Io { source, .. } if source.raw_os_error() == Some(5)));
    }
}
