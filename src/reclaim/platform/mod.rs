//! Cross-platform filesystem operations for database reclamation.

use std::io;
use std::path::Path;

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
