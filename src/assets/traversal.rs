use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
mod platform {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::process::CommandExt;

    use rustix::fd::{AsFd, OwnedFd};
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, openat, statat, unlinkat};

    use super::{Command, OsStr, OsString, Path, PathBuf, io};

    #[derive(Debug)]
    pub(crate) struct AnchoredDir {
        descriptor: OwnedFd,
        path: PathBuf,
    }

    impl AnchoredDir {
        pub(crate) fn open(path: &Path) -> io::Result<Option<Self>> {
            match openat(rustix::fs::CWD, path, directory_flags(), Mode::empty()) {
                Ok(descriptor) => Ok(Some(Self {
                    descriptor,
                    path: path.to_path_buf(),
                })),
                Err(
                    rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP,
                ) => Ok(None),
                Err(source) => Err(errno_to_io(source)),
            }
        }

        pub(crate) fn open_child(&self, name: &OsStr) -> io::Result<Option<Self>> {
            match openat(&self.descriptor, name, directory_flags(), Mode::empty()) {
                Ok(descriptor) => Ok(Some(Self {
                    descriptor,
                    path: self.path.join(name),
                })),
                Err(
                    rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP,
                ) => Ok(None),
                Err(source) => Err(errno_to_io(source)),
            }
        }

        pub(crate) fn entries(&self) -> io::Result<Vec<OsString>> {
            let mut stream = Dir::read_from(&self.descriptor).map_err(errno_to_io)?;
            let mut names = Vec::new();
            for entry in &mut stream {
                let entry = entry.map_err(errno_to_io)?;
                let name = entry.file_name().to_bytes();
                if name != b"." && name != b".." {
                    names.push(OsString::from_vec(name.to_vec()));
                }
            }
            names.sort();
            Ok(names)
        }

        pub(crate) fn regular_file_len(&self, name: &OsStr) -> io::Result<Option<u64>> {
            let stat = match statat(&self.descriptor, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(source) => return Err(errno_to_io(source)),
            };
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                return Ok(None);
            }
            u64::try_from(stat.st_size).map(Some).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "regular file has a negative size",
                )
            })
        }

        pub(crate) fn unlink_file(&self, name: &OsStr) -> io::Result<()> {
            unlinkat(&self.descriptor, name, AtFlags::empty()).map_err(errno_to_io)
        }

        pub(crate) fn remove_contents(
            &self,
            before_descend: &mut impl FnMut(&Path),
        ) -> io::Result<()> {
            for name in self.entries()? {
                before_descend(&self.path.join(&name));
                if let Some(child) = self.open_child(&name)? {
                    child.remove_contents(before_descend)?;
                    unlinkat(&self.descriptor, &name, AtFlags::REMOVEDIR).map_err(errno_to_io)?;
                } else {
                    match self.unlink_file(&name) {
                        Ok(()) => {}
                        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                        Err(source) => return Err(source),
                    }
                }
            }
            Ok(())
        }

        pub(crate) fn remove_child_directory(&self, name: &OsStr) -> io::Result<()> {
            unlinkat(&self.descriptor, name, AtFlags::REMOVEDIR).map_err(errno_to_io)
        }

        pub(crate) fn directory_bytes(&self) -> io::Result<u64> {
            let mut bytes = 0_u64;
            for name in self.entries()? {
                if let Some(child) = self.open_child(&name)? {
                    bytes = bytes.saturating_add(child.directory_bytes()?);
                } else if let Some(length) = self.regular_file_len(&name)? {
                    bytes = bytes.saturating_add(length);
                }
            }
            Ok(bytes)
        }

        #[expect(
            unsafe_code,
            reason = "pre_exec is the only portable way to anchor the child at a directory handle"
        )]
        pub(crate) fn configure_git_command(&self, command: &mut Command) {
            // Anchor the child at this exact directory handle rather than at a path, so a
            // concurrently swapped ancestor cannot redirect git outside the snapshot root.
            //
            // `/proc/self/fd/N` as a working directory only works on Linux; on macOS a
            // `/dev/fd/N` entry is the open file itself, so the child cannot chdir into it.
            // `fchdir` in the pre-exec hook is the portable equivalent and keeps the descriptor,
            // not a resolvable path, as the source of truth. The descriptor is CLOEXEC, but the
            // hook runs after fork and before exec, while it is still open.
            let descriptor = self.descriptor.try_clone();
            command.arg("--git-dir").arg(".");
            match descriptor {
                Ok(descriptor) => unsafe {
                    // SAFETY: the closure only calls `fchdir`, which is async-signal-safe
                    // and allocation-free, so it is valid between fork and exec.
                    command.pre_exec(move || {
                        rustix::process::fchdir(descriptor.as_fd())
                            .map_err(|source| io::Error::from_raw_os_error(source.raw_os_error()))
                    });
                },
                Err(_) => {
                    command.current_dir(&self.path);
                }
            }
        }

        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    fn directory_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    fn errno_to_io(source: rustix::io::Errno) -> io::Error {
        io::Error::from_raw_os_error(source.raw_os_error())
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{self, File, OpenOptions};
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    use super::{Command, OsStr, OsString, Path, PathBuf, io};

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;

    #[derive(Debug)]
    pub(crate) struct AnchoredDir {
        _handle: File,
        path: PathBuf,
    }

    impl AnchoredDir {
        pub(crate) fn open(path: &Path) -> io::Result<Option<Self>> {
            open_directory(path)
        }

        pub(crate) fn open_child(&self, name: &OsStr) -> io::Result<Option<Self>> {
            open_directory(&self.path.join(name))
        }

        pub(crate) fn entries(&self) -> io::Result<Vec<OsString>> {
            let mut names = fs::read_dir(&self.path)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<io::Result<Vec<_>>>()?;
            names.sort();
            Ok(names)
        }

        pub(crate) fn regular_file_len(&self, name: &OsStr) -> io::Result<Option<u64>> {
            let path = self.path.join(name);
            let handle = match open_handle(&path, 0) {
                Ok(handle) => handle,
                Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(source) => return Err(source),
            };
            let metadata = handle.metadata()?;
            if is_reparse_point(&metadata) || !metadata.file_type().is_file() {
                return Ok(None);
            }
            Ok(Some(metadata.len()))
        }

        pub(crate) fn unlink_file(&self, name: &OsStr) -> io::Result<()> {
            fs::remove_file(self.path.join(name))
        }

        pub(crate) fn remove_contents(
            &self,
            before_descend: &mut impl FnMut(&Path),
        ) -> io::Result<()> {
            for name in self.entries()? {
                before_descend(&self.path.join(&name));
                if let Some(child) = self.open_child(&name)? {
                    child.remove_contents(before_descend)?;
                    drop(child);
                    fs::remove_dir(self.path.join(&name))?;
                } else {
                    match self.unlink_file(&name) {
                        Ok(()) => {}
                        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                        Err(source) => return Err(source),
                    }
                }
            }
            Ok(())
        }

        pub(crate) fn remove_child_directory(&self, name: &OsStr) -> io::Result<()> {
            fs::remove_dir(self.path.join(name))
        }

        pub(crate) fn directory_bytes(&self) -> io::Result<u64> {
            let mut bytes = 0_u64;
            for name in self.entries()? {
                if let Some(child) = self.open_child(&name)? {
                    bytes = bytes.saturating_add(child.directory_bytes()?);
                } else if let Some(length) = self.regular_file_len(&name)? {
                    bytes = bytes.saturating_add(length);
                }
            }
            Ok(bytes)
        }

        pub(crate) fn configure_git_command(&self, command: &mut Command) {
            command.arg("--git-dir").arg(&self.path);
        }

        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    fn open_directory(path: &Path) -> io::Result<Option<AnchoredDir>> {
        let handle = match open_handle(path, FILE_FLAG_BACKUP_SEMANTICS) {
            Ok(handle) => handle,
            Err(source)
                if matches!(
                    source.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(source) => return Err(source),
        };
        let metadata = handle.metadata()?;
        if is_reparse_point(&metadata) || !metadata.file_type().is_dir() {
            return Ok(None);
        }
        Ok(Some(AnchoredDir {
            _handle: handle,
            path: path.to_path_buf(),
        }))
    }

    fn open_handle(path: &Path, extra_flags: u32) -> io::Result<File> {
        OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | extra_flags)
            .open(path)
    }

    fn is_reparse_point(metadata: &fs::Metadata) -> bool {
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("descriptor-anchored asset traversal requires Unix or Windows");

pub(crate) use platform::AnchoredDir;
