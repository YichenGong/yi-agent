//! Unix implementation of handle-based side reads.
//!
//! Every open walks the relative path one component at a time with
//! `openat(dir_fd, component, O_NOFOLLOW | O_CLOEXEC)`. Intermediate components
//! must be directories; the final component is opened `O_RDONLY | O_NONBLOCK`
//! and then `fstat`ed, so a FIFO can never block the open and a device/socket is
//! refused rather than used (spec §5.4).

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

/// Open `root/relative`, refusing every symlink along the way.
pub(super) fn open_beneath(
    root: &Path,
    relative: &Path,
    want_dir: bool,
) -> io::Result<std::fs::File> {
    let mut dir = open_directory_path(root)?;
    let components: Vec<OsString> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_os_string()),
            Component::CurDir => None,
            // `..` was rejected before this point; never resolve it here.
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect();

    if components.is_empty() {
        // The root itself.
        if want_dir {
            return Ok(std::fs::File::from(dir));
        }
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "a directory is not a regular file",
        ));
    }

    for (index, component) in components.iter().enumerate() {
        let last = index + 1 == components.len();
        if !last {
            let next = openat_dir(dir.as_raw_fd(), component.as_os_str())?;
            dir = next;
            continue;
        }
        return if want_dir {
            Ok(std::fs::File::from(openat_dir(dir.as_raw_fd(), component)?))
        } else {
            openat_regular(dir.as_raw_fd(), component)
        };
    }
    unreachable!("the loop returns on the last component")
}

/// Open the directory at the exact `path`.
///
/// Only used for paths that were already trusted (a canonicalised root, or the
/// parent of an alias copy), and always with `O_NOFOLLOW` so a swapped component
/// cannot redirect it.
pub(super) fn open_directory_path(path: &Path) -> io::Result<OwnedFd> {
    // An absolute path has to be entered through root's ancestors; each of them
    // is opened component by component so no symlink is followed anywhere.
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected an absolute directory path",
        ));
    }
    let mut dir = openat_dir(libc::AT_FDCWD, OsStr::new("/"))?;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                let next = openat_dir(dir.as_raw_fd(), name)?;
                dir = next;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unexpected path component",
                ));
            }
        }
    }
    Ok(dir)
}

/// Serve a registered alias copy from its held directory handle.
pub(super) fn open_alias_copy(
    dir: &std::fs::File,
    name: &std::ffi::OsStr,
    want_dir: bool,
) -> io::Result<std::fs::File> {
    if want_dir {
        return Ok(std::fs::File::from(openat_dir(dir.as_raw_fd(), name)?));
    }
    openat_regular(dir.as_raw_fd(), name)
}

/// `openat(dir, name, O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)`.
fn openat_dir(dir_fd: libc::c_int, name: &OsStr) -> io::Result<OwnedFd> {
    let c_name = to_cstring(name)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(dir_fd, c_name.as_ptr(), flags) };
    if fd < 0 {
        return Err(classify_io_error(name));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open the final component as a regular file, without ever blocking.
fn openat_regular(dir_fd: libc::c_int, name: &OsStr) -> io::Result<std::fs::File> {
    let c_name = to_cstring(name)?;
    // `O_NONBLOCK` matters for FIFOs: without it `open` would wait for a writer
    // before we ever get to `fstat` and reject the file.
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let fd = unsafe { libc::openat(dir_fd, c_name.as_ptr(), flags) };
    if fd < 0 {
        return Err(classify_io_error(name));
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let metadata = std::fs::File::from(owned.try_clone()?).metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not a regular file",
                String::from_utf8_lossy(name.as_bytes())
            ),
        ));
    }
    // A regular file's `O_NONBLOCK` has no effect on reads, but clear it so the
    // handle the caller receives behaves like a normal file in every respect.
    let fd = owned.as_raw_fd();
    let current = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if current >= 0 {
        unsafe { libc::fcntl(fd, libc::F_SETFL, current & !libc::O_NONBLOCK) };
    }
    Ok(std::fs::File::from(owned))
}

/// Build a `CString`, rejecting interior NUL bytes.
fn to_cstring(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes().to_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path component contains NUL"))
}

/// Turn a failed `openat` into a meaningful error.
fn classify_io_error(name: &OsStr) -> io::Error {
    let errno = io::Error::last_os_error();
    let display = String::from_utf8_lossy(name.as_bytes()).into_owned();
    match errno.raw_os_error() {
        // `O_NOFOLLOW` on a symlink is exactly this errno.
        Some(libc::ELOOP) => io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{display} is a symbolic link"),
        ),
        Some(libc::ENOTDIR) => io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{display} is not a directory"),
        ),
        Some(libc::EMLINK) => io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{display} is a symbolic link"),
        ),
        _ => errno,
    }
}
