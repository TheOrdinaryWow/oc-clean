//! Cross-platform filesystem operations for database reclamation.

use std::io;
use std::path::Path;

use crate::error::Error;

#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
pub(super) fn rename_over(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
pub(super) fn rename_over(source: &Path, destination: &Path) -> io::Result<()> {
    windows::rename_over(source, destination)
}

#[cfg(not(windows))]
pub(super) fn rename_error(destination: &Path, source: io::Error) -> Error {
    Error::Io {
        path: destination.to_path_buf(),
        source,
    }
}

#[cfg(windows)]
pub(super) fn rename_error(destination: &Path, source: io::Error) -> Error {
    windows::rename_error(destination, source)
}
