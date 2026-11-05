//! Owner-only folders and files, and the checks a credential or stash file passes before it is read.
//!
//! Every call names a trusted `base` folder. Each folder from `base` (itself included) down to the one asked for is
//! checked in turn, through a handle held open while the next one is opened, so no folder on the way can be a link or
//! be swapped for one between its check and its use. A file is read through the handle that was checked, and never
//! more than 64 KiB of it. A file this module creates is new (`CREATE_NEW`, never a name that exists), owned by the
//! current user, with the same protected DACL as a folder, and a file of one link.
//!
//! On Windows a folder the guard makes has an explicit, protected DACL with one ACE, full control for the current
//! user, inherited by what is made inside; its owner is the current user. A folder or file passes only when it is no
//! link or reparse point, its owner is the current user's SID, and no principal but that user, `SYSTEM` and
//! `Administrators` holds an allow ACE carrying a write, delete, `WRITE_DAC` or `WRITE_OWNER` right; a NULL DACL fails,
//! and so does an ACE too short for what it claims to hold. On a folder that includes the ACEs it hands down to what is
//! made inside (inherit-only ones too); on a file an inherit-only ACE does not apply. Deny ACEs never make a check fail.
//! Folder handles are opened with `FILE_TRAVERSE` and without delete sharing, so none can be renamed while it is held.
//! A name ending in a dot or a space, or holding a `:`, is refused before anything is opened: Windows would open it as
//! another name.
//!
//! Off Windows a folder is made with mode 0700, and a folder or file passes only when it is no symlink, belongs to
//! this process's effective uid and has no group or other permission bit at all: off Windows the sealed payload is
//! plaintext. The chain is walked with `openat` and `O_NOFOLLOW` from `base`.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};

use zeroize::Zeroizing;

use crate::seal::{MAX_SEALED_LEN, SealedBytes};

/// Which check a folder or file failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardErrorKind {
    /// It does not exist.
    Missing,
    /// Its owner is not the current user.
    NotOwned,
    /// A principal other than the current user (and, on Windows, `SYSTEM` and `Administrators`) can write, delete or
    /// re-permission it, or it has a NULL DACL or an ACE that cannot be read.
    WritableByOther,
    /// Off Windows: a group or other permission bit lets another user read or enter it.
    ReadableByOther,
    /// It is a link or another reparse point, or a link on the way led the file somewhere else.
    Link,
    /// A file that something else also names: more than one link to it.
    SharedLink,
    /// A folder was expected and this is not one.
    NotAFolder,
    /// A file was expected and this is not one.
    NotAFile,
    /// The file is over 64 KiB.
    TooLarge,
    /// It could not be checked or read, or the path is not under the base; the error's source says why.
    Io,
}

impl GuardErrorKind {
    fn message(self) -> &'static str {
        match self {
            Self::Missing => "does not exist",
            Self::NotOwned => "is not owned by the current user",
            Self::WritableByOther => "can be written by another user",
            Self::ReadableByOther => "can be read by another user",
            Self::Link => "is a link or reparse point",
            Self::SharedLink => "has more than one hard link",
            Self::NotAFolder => "is not a folder",
            Self::NotAFile => "is not a file",
            Self::TooLarge => "is over 64 KiB",
            Self::Io => "could not be checked",
        }
    }
}

/// A refused folder or file: the path and the check it failed. It never holds the file's contents.
#[derive(Debug, thiserror::Error)]
#[error("{}: {}", .path.display(), .kind.message())]
pub struct GuardError {
    kind: GuardErrorKind,
    path: PathBuf,
    #[source]
    source: Option<io::Error>,
}

impl GuardError {
    #[cfg(any(unix, windows))]
    fn new(kind: GuardErrorKind, path: &Path) -> Self {
        Self {
            kind,
            path: path.to_path_buf(),
            source: None,
        }
    }

    /// `error` met at `path`: [`GuardErrorKind::Missing`] when it is not found, [`GuardErrorKind::Io`] otherwise.
    fn io(path: &Path, error: io::Error) -> Self {
        let kind = if error.kind() == io::ErrorKind::NotFound {
            GuardErrorKind::Missing
        } else {
            GuardErrorKind::Io
        };
        Self {
            kind,
            path: path.to_path_buf(),
            source: Some(error),
        }
    }

    /// `error` met at `path`, always [`GuardErrorKind::Io`]: for a call whose "not found" does not mean the path is
    /// missing.
    #[cfg(any(windows, test))]
    fn unchecked(path: &Path, error: io::Error) -> Self {
        Self {
            kind: GuardErrorKind::Io,
            path: path.to_path_buf(),
            source: Some(error),
        }
    }

    fn not_under(path: &Path) -> Self {
        Self::io(
            path,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a plain path under the base folder",
            ),
        )
    }

    /// Which check failed.
    #[must_use]
    pub fn kind(&self) -> GuardErrorKind {
        self.kind
    }

    /// The folder or file that failed it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Makes `folder` and every missing folder between `base` and it owner-only. `base` must exist and is never made;
/// it and every folder on the way that already exists are checked as [`check_folder`] checks them, and the path is
/// validated before anything is opened or made.
///
/// # Errors
///
/// [`GuardErrorKind::Io`] when `folder` is not a plain path below `base` (`base` itself included) or a folder cannot
/// be made; the first check a folder on the way fails.
pub fn create_folder(base: &Path, folder: &Path) -> Result<(), GuardError> {
    let names = names_below(base, folder)?;
    if names.is_empty() {
        return Err(GuardError::not_under(folder));
    }
    platform::walk(base, &names, true).map(drop)
}

/// Checks `base` and every folder from it down to `folder`: each a folder, no link, owned by the current user and
/// open to no one else. When `folder` is `base`, `base` alone is checked.
///
/// # Errors
///
/// [`GuardErrorKind::Io`] when `folder` is neither `base` nor a plain path below it; the first check a folder fails.
pub fn check_folder(base: &Path, folder: &Path) -> Result<(), GuardError> {
    let names = names_below(base, folder)?;
    platform::walk(base, &names, false).map(drop)
}

/// Makes the new file at `path` after checking every folder from `base` down to its own, as [`check_folder`] does,
/// and opens it for writing. The name must not exist: an existing file, a link or another reparse point at it is an
/// error and is left untouched. The file is owned by the current user, carries a protected DACL that gives only that
/// user full control, and has one link.
///
/// # Errors
///
/// [`GuardErrorKind::Io`] when `path` is not a plain path below `base` or the name already exists, the first check a
/// folder on the way fails, or the file cannot be made; [`GuardErrorKind::Link`] when what exists at the name is a
/// link or another reparse point, and [`GuardErrorKind::NotAFile`] when it is not a regular file of one link.
pub fn create_file(base: &Path, path: &Path) -> Result<File, GuardError> {
    let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(GuardError::not_under(path));
    };
    let names = names_below(base, folder)?;
    if !plain_name(name) {
        return Err(GuardError::not_under(path));
    }
    platform::create_file(base, path, &names)
}

/// Reads the file at `path` after checking every folder from `base` down to its own, as [`check_folder`] does, and
/// the file itself the same way, and that it is at most 64 KiB.
///
/// # Errors
///
/// The first check a folder or the file fails, [`GuardErrorKind::TooLarge`] for a file over 64 KiB, or
/// [`GuardErrorKind::Io`] when the path is not under `base` or the file cannot be read.
pub fn read_file(base: &Path, path: &Path) -> Result<SealedBytes, GuardError> {
    let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(GuardError::not_under(path));
    };
    let names = names_below(base, folder)?;
    if !plain_name(name) {
        return Err(GuardError::not_under(path));
    }
    let chain = platform::walk(base, &names, false)?;
    platform::read_in(&chain, path, name).map(SealedBytes::from_file_bytes)
}

/// The names of the folders from `base` down to `folder`, each a plain name; checked before anything is opened.
fn names_below<'a>(base: &Path, folder: &'a Path) -> Result<Vec<&'a OsStr>, GuardError> {
    let relative = folder
        .strip_prefix(base)
        .map_err(|_| GuardError::not_under(folder))?;
    relative
        .components()
        .map(|component| match component {
            Component::Normal(name) if plain_name(name) => Ok(name),
            _ => Err(GuardError::not_under(folder)),
        })
        .collect()
}

/// Whether `name` opens as itself. Windows strips a trailing dot or space from the last name of a path but not from
/// an inner one, and reads `:` as a stream, so such a name could be checked as one folder and used as another.
fn plain_name(name: &OsStr) -> bool {
    if cfg!(windows) {
        let name = name.to_string_lossy();
        !(name.ends_with('.') || name.ends_with(' ') || name.contains(':'))
    } else {
        true
    }
}

/// At most 64 KiB of the checked, open `file` at `path`, whose metadata gave `len` bytes; the buffer holds the most it
/// can be, so it is never reallocated, leaving no unzeroed copy.
#[cfg(any(unix, windows))]
fn read_capped(path: &Path, file: File, len: u64) -> Result<Zeroizing<Vec<u8>>, GuardError> {
    let cap = MAX_SEALED_LEN as u64;
    if len > cap {
        return Err(GuardError::new(GuardErrorKind::TooLarge, path));
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_SEALED_LEN + 1));
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| GuardError::io(path, error))?;
    if bytes.len() > MAX_SEALED_LEN {
        return Err(GuardError::new(GuardErrorKind::TooLarge, path));
    }
    Ok(bytes)
}

/// Whether a folder or file owned by `file_uid` with permission bits `mode` passes for the effective uid
/// `current_uid`: it must be that uid's and carry no group or other bit.
#[cfg(any(unix, test))]
pub(crate) fn unix_verdict(
    file_uid: u32,
    current_uid: u32,
    mode: u32,
) -> Result<(), GuardErrorKind> {
    if file_uid != current_uid {
        Err(GuardErrorKind::NotOwned)
    } else if mode & 0o022 != 0 {
        Err(GuardErrorKind::WritableByOther)
    } else if mode & 0o077 != 0 {
        Err(GuardErrorKind::ReadableByOther)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::{CString, OsStr};
    use std::fs::{self, File, Metadata};
    use std::io;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::Path;

    use zeroize::Zeroizing;

    use super::{GuardError, GuardErrorKind, read_capped, unix_verdict};

    const FOLDER_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    /// `O_NONBLOCK` keeps a FIFO planted where the file should be from blocking the open.
    const FILE_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    /// A file this module makes: new, readable by its owner alone, and never through a link.
    const CREATE_FLAGS: libc::c_int =
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    /// The last folder of a checked chain, held open.
    pub(super) struct Chain {
        folder: File,
    }

    pub(super) fn walk(base: &Path, names: &[&OsStr], create: bool) -> Result<Chain, GuardError> {
        // Rebuilt from its components, `base/` loses the trailing slash that would make the kernel follow a symlink
        // at `base` despite O_NOFOLLOW.
        let mut path: std::path::PathBuf = base.components().collect();
        let base_name = c_string(&path, path.as_os_str())?;
        let mut folder = open_at(None, &base_name, FOLDER_FLAGS)
            .map_err(|error| refused(&path, error, GuardErrorKind::NotAFolder))?;
        checked(&path, &folder)?;
        for name in names {
            path.push(name);
            let c_name = c_string(&path, name)?;
            if create {
                make_at(&folder, &c_name).map_err(|error| GuardError::io(&path, error))?;
            }
            let next = open_at(Some(&folder), &c_name, FOLDER_FLAGS)
                .map_err(|error| refused(&path, error, GuardErrorKind::NotAFolder))?;
            checked(&path, &next)?;
            folder = next;
        }
        Ok(Chain { folder })
    }

    pub(super) fn read_in(
        chain: &Chain,
        path: &Path,
        name: &OsStr,
    ) -> Result<Zeroizing<Vec<u8>>, GuardError> {
        let c_name = c_string(path, name)?;
        let file = open_at(Some(&chain.folder), &c_name, FILE_FLAGS)
            .map_err(|error| refused(path, error, GuardErrorKind::NotAFile))?;
        let metadata = file
            .metadata()
            .map_err(|error| GuardError::io(path, error))?;
        check_file(path, &metadata)?;
        checked(path, &file)?;
        read_capped(path, file, metadata.len())
    }

    pub(super) fn create_file(
        base: &Path,
        path: &Path,
        names: &[&OsStr],
    ) -> Result<File, GuardError> {
        let chain = walk(base, names, false)?;
        let Some(name) = path.file_name() else {
            return Err(GuardError::not_under(path));
        };
        let c_name = c_string(path, name)?;
        // SAFETY: `c_name` is NUL-terminated and outlives the call, and `chain` holds the folder's open descriptor.
        let fd = unsafe {
            libc::openat(
                chain.folder.as_raw_fd(),
                c_name.as_ptr(),
                CREATE_FLAGS,
                0o600,
            )
        };
        if fd < 0 {
            return Err(refused(
                path,
                io::Error::last_os_error(),
                GuardErrorKind::NotAFile,
            ));
        }
        // SAFETY: `fd` is a fresh descriptor that nothing else owns.
        let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let metadata = file
            .metadata()
            .map_err(|error| GuardError::io(path, error))?;
        check_file(path, &metadata)?;
        checked(path, &file)?;
        Ok(file)
    }

    /// A file that is one regular file: not a folder, a FIFO or anything else, and not a file another name also
    /// reaches.
    fn check_file(path: &Path, metadata: &Metadata) -> Result<(), GuardError> {
        if !metadata.is_file() {
            return Err(GuardError::new(GuardErrorKind::NotAFile, path));
        }
        if metadata.nlink() != 1 {
            return Err(GuardError::new(GuardErrorKind::SharedLink, path));
        }
        Ok(())
    }

    fn c_string(path: &Path, name: &OsStr) -> Result<CString, GuardError> {
        CString::new(name.as_bytes()).map_err(|_| GuardError::not_under(path))
    }

    /// `name` opened with `flags` under the folder `parent`, or as a path of its own when there is none.
    fn open_at(parent: Option<&File>, name: &CString, flags: libc::c_int) -> io::Result<File> {
        // SAFETY: `name` is NUL-terminated and outlives the call, and `parent` holds an open folder descriptor.
        let fd = unsafe {
            match parent {
                Some(parent) => libc::openat(parent.as_raw_fd(), name.as_ptr(), flags),
                None => libc::open(name.as_ptr(), flags),
            }
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor that nothing else owns.
        Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Makes `name` under `parent` with mode 0700; one that already exists is left to the check that follows.
    fn make_at(parent: &File, name: &CString) -> io::Result<()> {
        // SAFETY: `name` is NUL-terminated and outlives the call, and `parent` holds an open folder descriptor.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EEXIST) {
            Ok(())
        } else {
            Err(error)
        }
    }

    /// What a failed `O_NOFOLLOW` open of `path` means: a link, not the kind asked for (`wrong_kind`), or an I/O error.
    fn refused(path: &Path, error: io::Error, wrong_kind: GuardErrorKind) -> GuardError {
        match error.raw_os_error() {
            Some(libc::ELOOP) => GuardError::new(GuardErrorKind::Link, path),
            Some(libc::ENOTDIR) => {
                let is_link = fs::symlink_metadata(path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink());
                GuardError::new(
                    if is_link {
                        GuardErrorKind::Link
                    } else {
                        wrong_kind
                    },
                    path,
                )
            }
            _ => GuardError::io(path, error),
        }
    }

    fn checked(path: &Path, file: &File) -> Result<Metadata, GuardError> {
        let metadata = file
            .metadata()
            .map_err(|error| GuardError::io(path, error))?;
        // SAFETY: geteuid takes no arguments, cannot fail and touches no memory of ours.
        let current_uid = unsafe { libc::geteuid() };
        unix_verdict(metadata.uid(), current_uid, metadata.mode())
            .map_err(|kind| GuardError::new(kind, path))?;
        Ok(metadata)
    }
}

/// Neither Unix nor Windows: nothing can be checked, so nothing passes.
#[cfg(not(any(unix, windows)))]
mod platform {
    use std::ffi::OsStr;
    use std::io;
    use std::path::Path;

    use zeroize::Zeroizing;

    use super::GuardError;

    pub(super) struct Chain;

    pub(super) fn walk(base: &Path, _names: &[&OsStr], _create: bool) -> Result<Chain, GuardError> {
        Err(GuardError::io(
            base,
            io::Error::from(io::ErrorKind::Unsupported),
        ))
    }

    pub(super) fn read_in(
        _chain: &Chain,
        path: &Path,
        _name: &OsStr,
    ) -> Result<Zeroizing<Vec<u8>>, GuardError> {
        Err(GuardError::io(
            path,
            io::Error::from(io::ErrorKind::Unsupported),
        ))
    }

    pub(super) fn create_file(
        _base: &Path,
        path: &Path,
        _names: &[&OsStr],
    ) -> Result<std::fs::File, GuardError> {
        Err(GuardError::io(
            path,
            io::Error::from(io::ErrorKind::Unsupported),
        ))
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::{OsStr, OsString};
    use std::fs::{File, Metadata, OpenOptions};
    use std::io;
    use std::ops::Range;
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        ERROR_INSUFFICIENT_BUFFER, GENERIC_ALL, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION,
        AclSizeInformation, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE,
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetLengthSid,
        GetTokenInformation, INHERIT_ONLY_ACE, InitializeAcl, InitializeSecurityDescriptor,
        IsValidSid, IsWellKnownSid, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
        TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE,
        FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
        FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, GetFileInformationByHandle,
        GetFinalPathNameByHandleW, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::core::BOOL;
    use zeroize::Zeroizing;

    use super::{GuardError, GuardErrorKind, read_capped};

    /// `MAXIMUM_ALLOWED`, which grants whatever the rest of the descriptor allows.
    const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
    /// The rights that let a principal change, delete or re-permission a file or folder.
    pub(super) const WRITE_RIGHTS: u32 = FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_DELETE_CHILD
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | WRITE_DAC
        | WRITE_OWNER
        | GENERIC_WRITE
        | GENERIC_ALL
        | MAXIMUM_ALLOWED;
    /// `SECURITY_DESCRIPTOR_REVISION`.
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
    /// Every handle the guard opens lets others read and write but not delete or rename what it holds.
    const SHARE_NO_DELETE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
    /// `FILE_NAME_NORMALIZED | VOLUME_NAME_DOS`: the final path as a drive-letter path.
    const FINAL_PATH_DOS: u32 = 0;

    /// The ACE types that grant access (winnt.h): plain and callback ACEs hold their SID right after the mask, object
    /// ACEs after a flags word and the GUIDs it says are present, and a compound ACE is never parsed.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x0;
    const ACCESS_ALLOWED_COMPOUND_ACE_TYPE: u8 = 0x4;
    const ACCESS_ALLOWED_OBJECT_ACE_TYPE: u8 = 0x5;
    const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 0x9;
    const ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE: u8 = 0xB;
    /// `ACE_OBJECT_TYPE_PRESENT` and `ACE_INHERITED_OBJECT_TYPE_PRESENT` in an object ACE's flags.
    const OBJECT_TYPE_PRESENT: u32 = 0x1;
    const INHERITED_OBJECT_TYPE_PRESENT: u32 = 0x2;
    /// A SID's revision, sub-authority count and identifier authority, before its sub-authorities.
    const SID_HEADER_LEN: usize = 8;

    /// Who an allow ACE names.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Principal {
        User,
        System,
        Administrators,
        Other,
    }

    /// An allow ACE that applies to the object: who it names and the rights it grants.
    #[derive(Clone, Copy, Debug)]
    pub(super) struct Grant {
        pub(super) principal: Principal,
        pub(super) mask: u32,
    }

    /// What an ACE that cannot be read stands for: every right, to someone unknown.
    const UNREADABLE: Grant = Grant {
        principal: Principal::Other,
        mask: u32::MAX,
    };

    /// Whether an object passes, given whether the current user owns it and its DACL's allow ACEs (`None` for a NULL
    /// DACL).
    pub(super) fn verdict(
        owner_is_user: bool,
        dacl: Option<&[Grant]>,
    ) -> Result<(), GuardErrorKind> {
        if !owner_is_user {
            return Err(GuardErrorKind::NotOwned);
        }
        let Some(grants) = dacl else {
            return Err(GuardErrorKind::WritableByOther);
        };
        if grants
            .iter()
            .any(|grant| grant.principal == Principal::Other && grant.mask & WRITE_RIGHTS != 0)
        {
            return Err(GuardErrorKind::WritableByOther);
        }
        Ok(())
    }

    /// What an ACE's bytes hold, read without trusting its size.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum AceLayout {
        /// Not an allow ACE, or one that does not apply to this object.
        Skip,
        /// Too short for what it claims to hold, or a kind this check does not read.
        Unreadable,
        /// An allow ACE granting `mask` to the SID at `sid` in the ACE's bytes.
        Allow { mask: u32, sid: Range<usize> },
    }

    fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
        let word = bytes.get(at..at.checked_add(4)?)?;
        Some(u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
    }

    /// The layout of the ACE `ace` (its `AceSize` bytes) on a folder (`is_folder`) or a file: an inherit-only ACE
    /// applies to a folder that hands it down, never to a file.
    pub(super) fn ace_layout(ace: &[u8], is_folder: bool) -> AceLayout {
        let Some(&[ace_type, flags, _, _]) = ace.first_chunk::<4>() else {
            return AceLayout::Unreadable;
        };
        let flags = u32::from(flags);
        let hands_down = flags & (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) != 0;
        if flags & INHERIT_ONLY_ACE != 0 && !(is_folder && hands_down) {
            return AceLayout::Skip;
        }
        let sid_start = match ace_type {
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => 8,
            ACCESS_ALLOWED_OBJECT_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE => {
                let Some(object_flags) = u32_at(ace, 8) else {
                    return AceLayout::Unreadable;
                };
                12 + 16 * usize::from(object_flags & OBJECT_TYPE_PRESENT != 0)
                    + 16 * usize::from(object_flags & INHERITED_OBJECT_TYPE_PRESENT != 0)
            }
            ACCESS_ALLOWED_COMPOUND_ACE_TYPE => return AceLayout::Unreadable,
            _ => return AceLayout::Skip,
        };
        let (Some(mask), Some(&count)) = (u32_at(ace, 4), ace.get(sid_start + 1)) else {
            return AceLayout::Unreadable;
        };
        let sid_end = sid_start + SID_HEADER_LEN + 4 * usize::from(count);
        if sid_end > ace.len() {
            return AceLayout::Unreadable;
        }
        AceLayout::Allow {
            mask,
            sid: sid_start..sid_end,
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }

    fn succeeded(result: BOOL) -> io::Result<()> {
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Every folder of a checked chain held open, the last one being `folder`.
    pub(super) struct Chain {
        handles: Vec<File>,
        folder: PathBuf,
    }

    pub(super) fn walk(base: &Path, names: &[&OsStr], create: bool) -> Result<Chain, GuardError> {
        let mut path = base.to_path_buf();
        let mut handles = vec![open_folder(&path)?];
        for name in names {
            path.push(name);
            if create {
                match make_folder(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(GuardError::io(&path, error)),
                }
            }
            handles.push(open_folder(&path)?);
        }
        Ok(Chain {
            handles,
            folder: path,
        })
    }

    pub(super) fn read_in(
        chain: &Chain,
        path: &Path,
        name: &OsStr,
    ) -> Result<Zeroizing<Vec<u8>>, GuardError> {
        let file =
            open(&chain.folder.join(name), FILE_GENERIC_READ).map_err(|error| GuardError {
                path: path.to_path_buf(),
                ..error
            })?;
        let metadata = file
            .metadata()
            .map_err(|error| GuardError::io(path, error))?;
        check_kind(path, &metadata, false)?;
        check_single_link(path, &file)?;
        let Some(folder) = chain.handles.last() else {
            return Err(GuardError::not_under(path));
        };
        if final_path(path, &file)?.parent() != Some(final_path(path, folder)?.as_path()) {
            return Err(GuardError::new(GuardErrorKind::Link, path));
        }
        check_security(path, &file, false)?;
        read_capped(path, file, metadata.len())
    }

    pub(super) fn create_file(
        base: &Path,
        path: &Path,
        names: &[&OsStr],
    ) -> Result<File, GuardError> {
        let _chain = walk(base, names, false)?;
        let owner = OwnerOnly::new().map_err(|error| GuardError::io(path, error))?;
        let attributes = owner
            .attributes()
            .map_err(|error| GuardError::io(path, error))?;
        let target = wide(path);
        // SAFETY: `target` is NUL-terminated and `attributes` points at a descriptor whose owner SID and ACL live in
        // `owner`; both outlive the call.
        let handle = unsafe {
            CreateFileW(
                target.as_ptr(),
                GENERIC_WRITE | READ_CONTROL | FILE_READ_ATTRIBUTES,
                SHARE_NO_DELETE,
                &raw const attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(GuardError::io(path, io::Error::last_os_error()));
        }
        // SAFETY: `handle` is a fresh, open handle that nothing else owns.
        let file = unsafe { File::from_raw_handle(handle) };
        let metadata = file
            .metadata()
            .map_err(|error| GuardError::io(path, error))?;
        check_kind(path, &metadata, false)?;
        check_single_link(path, &file)?;
        check_security(path, &file, false)?;
        Ok(file)
    }

    /// A file whose handle names one link to it: another name for the same bytes is refused.
    fn check_single_link(path: &Path, file: &File) -> Result<(), GuardError> {
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // SAFETY: `file` is open and `info` is a writable BY_HANDLE_FILE_INFORMATION.
        let filled = unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) };
        if filled == 0 {
            return Err(GuardError::io(path, io::Error::last_os_error()));
        }
        // SAFETY: a successful call filled `info`.
        let links = unsafe { info.assume_init() }.nNumberOfLinks;
        if links == 1 {
            Ok(())
        } else {
            Err(GuardError::new(GuardErrorKind::SharedLink, path))
        }
    }

    /// Makes `path` with the current user as owner and a protected DACL whose one ACE gives that user full control,
    /// inherited by files and folders made inside. An existing path is `AlreadyExists`.
    pub(super) fn make_folder(path: &Path) -> io::Result<()> {
        let owner = OwnerOnly::new()?;
        let attributes = owner.attributes()?;
        let name = wide(path);
        // SAFETY: `name` is NUL-terminated and `attributes` points at a descriptor whose owner SID and ACL live in
        // `owner`; both outlive the call.
        succeeded(unsafe { CreateDirectoryW(name.as_ptr(), &raw const attributes) })
    }

    /// A security descriptor that gives the current user alone full control, protected from inheritance, with the
    /// SID and ACL it points at kept alive alongside it.
    /// The SID and ACL the descriptor points at are kept beside it: dropping either would dangle its pointer.
    struct OwnerOnly {
        _user: TokenUserBuffer,
        _acl: Vec<u32>,
        descriptor: Box<SECURITY_DESCRIPTOR>,
    }

    impl OwnerOnly {
        fn new() -> io::Result<Self> {
            let user = current_user()?;
            let sid = user.sid();
            // SAFETY: `sid` points at the valid SID inside `user`, which outlives every use below.
            let sid_len = unsafe { GetLengthSid(sid) } as usize;
            let acl_len =
                size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + sid_len;
            let mut acl = vec![0_u32; acl_len.div_ceil(4)];
            let acl_pointer = acl.as_mut_ptr().cast::<ACL>();
            let acl_size = u32::try_from(acl.len() * 4).map_err(io::Error::other)?;
            // SAFETY: `acl_pointer` points at `acl_size` writable, 4-aligned bytes that outlive every use below.
            succeeded(unsafe { InitializeAcl(acl_pointer, acl_size, ACL_REVISION) })?;
            // SAFETY: the ACL was initialised with room for this one ACE and `sid` is valid.
            succeeded(unsafe {
                AddAccessAllowedAceEx(
                    acl_pointer,
                    ACL_REVISION,
                    OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                    FILE_ALL_ACCESS,
                    sid,
                )
            })?;
            let mut descriptor = Box::new(SECURITY_DESCRIPTOR::default());
            let pointer: PSECURITY_DESCRIPTOR = (&raw mut *descriptor).cast();
            // SAFETY: `pointer` is a writable SECURITY_DESCRIPTOR; the owner SID and the ACL it is given live in
            // `user` and `acl`, both kept in the returned value.
            unsafe {
                succeeded(InitializeSecurityDescriptor(
                    pointer,
                    SECURITY_DESCRIPTOR_REVISION,
                ))?;
                succeeded(SetSecurityDescriptorOwner(pointer, sid, 0))?;
                succeeded(SetSecurityDescriptorDacl(pointer, 1, acl_pointer, 0))?;
                succeeded(SetSecurityDescriptorControl(
                    pointer,
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                ))?;
            }
            Ok(Self {
                _user: user,
                _acl: acl,
                descriptor,
            })
        }

        fn attributes(&self) -> io::Result<SECURITY_ATTRIBUTES> {
            let length =
                u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).map_err(io::Error::other)?;
            Ok(SECURITY_ATTRIBUTES {
                nLength: length,
                lpSecurityDescriptor: (&raw const *self.descriptor).cast_mut().cast(),
                bInheritHandle: 0,
            })
        }
    }

    /// The folder at `path`, opened without delete sharing and checked on its handle.
    fn open_folder(path: &Path) -> Result<File, GuardError> {
        // FILE_TRAVERSE is an access Windows counts for sharing, so the missing FILE_SHARE_DELETE is enforced and the
        // folder cannot be renamed or deleted while this handle is held; READ_CONTROL and attributes alone are not.
        let handle = open(path, READ_CONTROL | FILE_READ_ATTRIBUTES | FILE_TRAVERSE)?;
        let metadata = handle
            .metadata()
            .map_err(|error| GuardError::io(path, error))?;
        check_kind(path, &metadata, true)?;
        check_security(path, &handle, true)?;
        Ok(handle)
    }

    /// `path` itself opened for `access`, never what a link or reparse point there leads to; a folder opens too.
    fn open(path: &Path, access: u32) -> Result<File, GuardError> {
        OpenOptions::new()
            .access_mode(access)
            .share_mode(SHARE_NO_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .map_err(|error| GuardError::io(path, error))
    }

    /// The path Windows resolves `handle` to; errors name `path`.
    fn final_path(path: &Path, handle: &File) -> Result<PathBuf, GuardError> {
        let mut buffer = vec![0_u16; 512];
        loop {
            let capacity = u32::try_from(buffer.len())
                .map_err(|error| GuardError::io(path, io::Error::other(error)))?;
            // SAFETY: `handle` is open and `buffer` holds `capacity` writable UTF-16 units.
            let written = unsafe {
                GetFinalPathNameByHandleW(
                    handle.as_raw_handle(),
                    buffer.as_mut_ptr(),
                    capacity,
                    FINAL_PATH_DOS,
                )
            } as usize;
            if written == 0 {
                // ERROR_PATH_NOT_FOUND here means a volume with no drive letter, not a missing file.
                return Err(GuardError::unchecked(path, io::Error::last_os_error()));
            }
            if written < buffer.len() {
                return Ok(PathBuf::from(OsString::from_wide(&buffer[..written])));
            }
            // Too small: `written` is the size needed, its NUL included.
            buffer.resize(written + 1, 0);
        }
    }

    fn check_kind(path: &Path, metadata: &Metadata, want_folder: bool) -> Result<(), GuardError> {
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            Err(GuardError::new(GuardErrorKind::Link, path))
        } else if want_folder && !metadata.is_dir() {
            Err(GuardError::new(GuardErrorKind::NotAFolder, path))
        } else if !want_folder && metadata.is_dir() {
            Err(GuardError::new(GuardErrorKind::NotAFile, path))
        } else {
            Ok(())
        }
    }

    fn check_security(path: &Path, handle: &File, is_folder: bool) -> Result<(), GuardError> {
        let io_error = |error| GuardError::io(path, error);
        let descriptor = Descriptor::of(handle).map_err(io_error)?;
        let user = current_user().map_err(io_error)?;
        // SAFETY: both SIDs are valid and live in buffers (`descriptor`, `user`) that outlive the call.
        let owner_is_user =
            !descriptor.owner.is_null() && unsafe { EqualSid(descriptor.owner, user.sid()) } != 0;
        let grants = descriptor.grants(user.sid(), is_folder).map_err(io_error)?;
        verdict(owner_is_user, grants.as_deref()).map_err(|kind| GuardError::new(kind, path))
    }

    /// An object's owner and DACL as `GetSecurityInfo` returned them, inside one buffer freed on drop.
    struct Descriptor {
        buffer: PSECURITY_DESCRIPTOR,
        owner: PSID,
        dacl: *mut ACL,
    }

    impl Descriptor {
        fn of(handle: &File) -> io::Result<Self> {
            let mut descriptor = Self {
                buffer: ptr::null_mut(),
                owner: ptr::null_mut(),
                dacl: ptr::null_mut(),
            };
            // SAFETY: `handle` is open with READ_CONTROL; the out pointers are writable and the group and SACL ones are
            // null, which the API allows as they are not asked for.
            let status = unsafe {
                GetSecurityInfo(
                    handle.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &raw mut descriptor.owner,
                    ptr::null_mut(),
                    &raw mut descriptor.dacl,
                    ptr::null_mut(),
                    &raw mut descriptor.buffer,
                )
            };
            if status != 0 {
                return Err(io::Error::from_raw_os_error(
                    i32::try_from(status).map_err(io::Error::other)?,
                ));
            }
            Ok(descriptor)
        }

        /// The allow ACEs that apply to the object, on a folder those it hands down too; `None` for a NULL DACL.
        fn grants(&self, user: PSID, is_folder: bool) -> io::Result<Option<Vec<Grant>>> {
            if self.dacl.is_null() {
                return Ok(None);
            }
            let mut size = ACL_SIZE_INFORMATION::default();
            let size_len =
                u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).map_err(io::Error::other)?;
            // SAFETY: `dacl` points into this descriptor's buffer and `size` is a writable ACL_SIZE_INFORMATION.
            succeeded(unsafe {
                GetAclInformation(
                    self.dacl,
                    (&raw mut size).cast(),
                    size_len,
                    AclSizeInformation,
                )
            })?;
            let mut grants = Vec::new();
            for index in 0..size.AceCount {
                let mut ace: *mut core::ffi::c_void = ptr::null_mut();
                // SAFETY: `index` is below the ACL's ACE count and `ace` is writable.
                succeeded(unsafe { GetAce(self.dacl, index, &raw mut ace) })?;
                // SAFETY: GetAce set `ace` to an ACE inside the DACL, which lives as long as this descriptor.
                if let Some(grant) = unsafe { grant_of(ace.cast::<u8>(), user, is_folder) } {
                    grants.push(grant);
                }
            }
            Ok(Some(grants))
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            if !self.buffer.is_null() {
                // SAFETY: GetSecurityInfo allocated `buffer` with LocalAlloc; it is freed here once.
                unsafe { LocalFree(self.buffer) };
            }
        }
    }

    /// The grant the ACE at `ace` makes, if it is an allow ACE that applies; [`UNREADABLE`] for an ACE whose fields
    /// do not fit its size or whose SID is not valid.
    ///
    /// # Safety
    ///
    /// `ace` points at an ACE of a DACL whose header and `AceSize` bytes are valid for reads, and `user` at a valid
    /// SID.
    pub(super) unsafe fn grant_of(ace: *const u8, user: PSID, is_folder: bool) -> Option<Grant> {
        // SAFETY: every ACE starts with an ACE_HEADER.
        let header = unsafe { ace.cast::<ACE_HEADER>().read_unaligned() };
        // SAFETY: the caller guarantees `AceSize` bytes at `ace`.
        let bytes = unsafe { std::slice::from_raw_parts(ace, usize::from(header.AceSize)) };
        let (mask, sid) = match ace_layout(bytes, is_folder) {
            AceLayout::Skip => return None,
            AceLayout::Unreadable => return Some(UNREADABLE),
            AceLayout::Allow { mask, sid } => (mask, sid),
        };
        let sid_len = sid.len();
        let sid: PSID = bytes[sid].as_ptr().cast_mut().cast();
        // SAFETY: `ace_layout` checked that the SID's header and every sub-authority its count names lie inside the
        // ACE, so IsValidSid and GetLengthSid read only those bytes.
        let valid =
            unsafe { IsValidSid(sid) } != 0 && unsafe { GetLengthSid(sid) } as usize == sid_len;
        if !valid {
            return Some(UNREADABLE);
        }
        Some(Grant {
            // SAFETY: `sid` is valid as checked above, and `user` per the caller.
            principal: unsafe { principal_of(sid, user) },
            mask,
        })
    }

    /// # Safety
    ///
    /// `sid` and `user` point at valid SIDs.
    unsafe fn principal_of(sid: PSID, user: PSID) -> Principal {
        // SAFETY: the caller passes valid SIDs.
        unsafe {
            if EqualSid(sid, user) != 0 {
                Principal::User
            } else if IsWellKnownSid(sid, WinLocalSystemSid) != 0 {
                Principal::System
            } else if IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0 {
                Principal::Administrators
            } else {
                Principal::Other
            }
        }
    }

    /// A `TOKEN_USER` read from this process's token, with the SID it points to.
    struct TokenUserBuffer(Vec<u64>);

    impl TokenUserBuffer {
        /// The user's SID, which points into this buffer.
        fn sid(&self) -> PSID {
            // SAFETY: the buffer is 8-aligned, at least `size_of::<TOKEN_USER>()` bytes, and holds the `TOKEN_USER`
            // `GetTokenInformation` wrote.
            unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
        }
    }

    /// The current process's user.
    fn current_user() -> io::Result<TokenUserBuffer> {
        let mut raw: HANDLE = ptr::null_mut();
        // SAFETY: the pseudo handle for this process needs no closing and `raw` is writable.
        succeeded(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut raw) })?;
        // SAFETY: `raw` is a fresh, open token handle that nothing else owns.
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut size: u32 = 0;
        // SAFETY: a null buffer of length 0 only asks for the size, written to `size`.
        let sized = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                ptr::null_mut(),
                0,
                &raw mut size,
            )
        };
        if sized == 0 {
            let error = io::Error::last_os_error();
            if error
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok())
                != Some(ERROR_INSUFFICIENT_BUFFER)
            {
                return Err(error);
            }
        }
        let bytes = usize::try_from(size)
            .map_err(io::Error::other)?
            .max(size_of::<TOKEN_USER>());
        let mut buffer = TokenUserBuffer(vec![0_u64; bytes.div_ceil(8)]);
        let length = u32::try_from(buffer.0.len() * 8).map_err(io::Error::other)?;
        // SAFETY: `token` is open with TOKEN_QUERY, `buffer` holds `length` writable, 8-aligned bytes and outlives the
        // call, and `size` is writable.
        succeeded(unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.0.as_mut_ptr().cast(),
                length,
                &raw mut size,
            )
        })?;
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::{GuardError, GuardErrorKind, unix_verdict};

    #[test]
    fn an_unchecked_not_found_is_io_not_missing() {
        let error = GuardError::unchecked(
            std::path::Path::new("held"),
            std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        assert_eq!(error.kind(), GuardErrorKind::Io);
        let missing = GuardError::io(
            std::path::Path::new("held"),
            std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        assert_eq!(missing.kind(), GuardErrorKind::Missing);
    }

    #[test]
    fn unix_accepts_the_current_uids_private_files_and_folders() {
        assert_eq!(unix_verdict(1000, 1000, 0o100_600), Ok(()));
        assert_eq!(unix_verdict(1000, 1000, 0o040_700), Ok(()));
        assert_eq!(unix_verdict(0, 0, 0o100_400), Ok(()));
    }

    #[test]
    fn unix_refuses_another_uid() {
        assert_eq!(
            unix_verdict(1001, 1000, 0o100_600),
            Err(GuardErrorKind::NotOwned)
        );
        assert_eq!(
            unix_verdict(0, 1000, 0o040_700),
            Err(GuardErrorKind::NotOwned)
        );
    }

    #[test]
    fn unix_refuses_any_group_or_other_write_bit() {
        for mode in [
            0o100_620, 0o100_602, 0o040_770, 0o040_707, 0o100_666, 0o041_777,
        ] {
            assert_eq!(
                unix_verdict(1000, 1000, mode),
                Err(GuardErrorKind::WritableByOther),
                "{mode:o}"
            );
        }
    }

    #[test]
    fn unix_refuses_any_group_or_other_read_or_enter_bit() {
        for mode in [
            0o100_640, 0o100_604, 0o100_644, 0o040_710, 0o040_701, 0o040_750, 0o040_755,
        ] {
            assert_eq!(
                unix_verdict(1000, 1000, mode),
                Err(GuardErrorKind::ReadableByOther),
                "{mode:o}"
            );
        }
    }

    #[cfg(windows)]
    #[expect(
        clippy::expect_used,
        reason = "a hand-built ACE that does not fit must fail the test loudly"
    )]
    mod windows {
        use windows_sys::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_GENERIC_READ,
            FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER,
        };

        use super::super::GuardErrorKind;
        use super::super::platform::{
            AceLayout, Grant, Principal, WRITE_RIGHTS, ace_layout, grant_of, make_folder, verdict,
            walk,
        };

        /// A folder in a held chain cannot be renamed: its handle carries an access Windows counts for sharing and
        /// shares no delete.
        #[test]
        fn a_held_folder_cannot_be_renamed() {
            let root =
                std::env::temp_dir().join(format!("rt-accounts-guard-held-{}", std::process::id()));
            if root.exists() {
                std::fs::remove_dir_all(&root).expect("an old run's folder can be removed");
            }
            make_folder(&root).expect("the root can be made");
            let chain = walk(&root, &[std::ffi::OsStr::new("held")], true)
                .expect("the chain is made and held");
            let renamed = std::fs::rename(root.join("held"), root.join("moved"));
            assert_eq!(
                renamed
                    .as_ref()
                    .err()
                    .and_then(std::io::Error::raw_os_error),
                Some(32),
                "{renamed:?}"
            );
            drop(chain);
            std::fs::rename(root.join("held"), root.join("moved"))
                .expect("a released folder can be renamed");
            std::fs::remove_dir_all(&root).expect("the root can be removed");
        }

        const fn grant(principal: Principal, mask: u32) -> Grant {
            Grant { principal, mask }
        }

        #[test]
        fn another_owner_is_refused_whatever_the_dacl() {
            let owner_only = [grant(Principal::User, FILE_ALL_ACCESS)];
            assert_eq!(
                verdict(false, Some(&owner_only)),
                Err(GuardErrorKind::NotOwned)
            );
            assert_eq!(verdict(false, None), Err(GuardErrorKind::NotOwned));
        }

        #[test]
        fn a_null_dacl_is_refused() {
            assert_eq!(verdict(true, None), Err(GuardErrorKind::WritableByOther));
        }

        #[test]
        fn the_user_system_and_administrators_may_write() {
            let grants = [
                grant(Principal::User, FILE_ALL_ACCESS),
                grant(Principal::System, FILE_ALL_ACCESS),
                grant(Principal::Administrators, GENERIC_ALL),
                grant(Principal::Other, FILE_GENERIC_READ),
            ];
            assert_eq!(verdict(true, Some(&grants)), Ok(()));
            assert_eq!(
                verdict(true, Some(&[])),
                Ok(()),
                "an empty DACL lets no one in"
            );
        }

        /// Each right on its own, named here rather than read from `WRITE_RIGHTS`, so dropping one from the set fails.
        #[test]
        fn every_write_right_for_another_principal_is_refused() {
            let rights = [
                FILE_WRITE_DATA,
                FILE_APPEND_DATA,
                FILE_WRITE_EA,
                FILE_DELETE_CHILD,
                FILE_WRITE_ATTRIBUTES,
                DELETE,
                WRITE_DAC,
                WRITE_OWNER,
                GENERIC_WRITE,
                GENERIC_ALL,
                0x0200_0000,
            ];
            for mask in rights {
                let grants = [
                    grant(Principal::User, FILE_ALL_ACCESS),
                    grant(Principal::Other, mask),
                ];
                assert_eq!(
                    verdict(true, Some(&grants)),
                    Err(GuardErrorKind::WritableByOther),
                    "{mask:#x}"
                );
            }
            assert_eq!(
                rights.iter().fold(0, |all, right| all | right),
                WRITE_RIGHTS
            );
        }

        /// Everyone (S-1-1-0): revision 1, one sub-authority, authority 1, sub-authority 0.
        const EVERYONE_SID: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];

        /// An ACE of `ace_type` with `flags`, `mask` and `body` after the mask, its size the bytes given.
        fn ace(ace_type: u8, flags: u8, mask: u32, body: &[u8]) -> Vec<u8> {
            let size = u16::try_from(8 + body.len()).expect("a small ACE");
            let mut bytes = vec![ace_type, flags];
            bytes.extend_from_slice(&size.to_le_bytes());
            bytes.extend_from_slice(&mask.to_le_bytes());
            bytes.extend_from_slice(body);
            bytes
        }

        #[test]
        fn a_plain_allow_ace_names_its_sid() {
            let bytes = ace(0, 0, FILE_WRITE_DATA, &EVERYONE_SID);
            assert_eq!(
                ace_layout(&bytes, false),
                AceLayout::Allow {
                    mask: FILE_WRITE_DATA,
                    sid: 8..20
                }
            );
        }

        #[test]
        fn an_object_ace_finds_its_sid_after_the_guids_it_holds() {
            let mut body = 3_u32.to_le_bytes().to_vec();
            body.extend_from_slice(&[0; 32]);
            body.extend_from_slice(&EVERYONE_SID);
            let bytes = ace(5, 0, DELETE, &body);
            assert_eq!(
                ace_layout(&bytes, false),
                AceLayout::Allow {
                    mask: DELETE,
                    sid: 44..56
                }
            );
            assert_eq!(
                ace_layout(&bytes[..50], false),
                AceLayout::Unreadable,
                "the SID runs past the ACE"
            );
            assert_eq!(
                ace_layout(&bytes[..10], false),
                AceLayout::Unreadable,
                "the object flags run past the ACE"
            );
        }

        #[test]
        fn an_ace_too_short_for_its_fields_is_unreadable() {
            let bytes = ace(0, 0, FILE_WRITE_DATA, &EVERYONE_SID);
            assert_eq!(
                ace_layout(&bytes[..3], false),
                AceLayout::Unreadable,
                "no whole header"
            );
            assert_eq!(
                ace_layout(&bytes[..6], false),
                AceLayout::Unreadable,
                "no whole mask"
            );
            assert_eq!(
                ace_layout(&bytes[..15], false),
                AceLayout::Unreadable,
                "no whole SID header"
            );
            assert_eq!(
                ace_layout(&bytes[..19], false),
                AceLayout::Unreadable,
                "a sub-authority cut short"
            );
            let mut five = EVERYONE_SID;
            five[1] = 5;
            assert_eq!(
                ace_layout(&ace(0, 0, FILE_WRITE_DATA, &five), false),
                AceLayout::Unreadable,
                "count past the end"
            );
            assert_eq!(
                ace_layout(&ace(4, 0, FILE_WRITE_DATA, &EVERYONE_SID), false),
                AceLayout::Unreadable,
                "compound"
            );
        }

        #[test]
        fn inherit_only_aces_apply_to_a_folder_that_hands_them_down_and_never_to_a_file() {
            let inherit_only = ace(0, 0x08 | 0x01, FILE_WRITE_DATA, &EVERYONE_SID);
            assert_eq!(ace_layout(&inherit_only, false), AceLayout::Skip);
            assert_eq!(
                ace_layout(&inherit_only, true),
                AceLayout::Allow {
                    mask: FILE_WRITE_DATA,
                    sid: 8..20
                }
            );
            assert_eq!(
                ace_layout(&ace(1, 0, FILE_WRITE_DATA, &EVERYONE_SID), true),
                AceLayout::Skip,
                "a deny ACE"
            );
        }

        #[test]
        fn an_invalid_sid_counts_as_anyone_with_every_right() {
            let mut sid = EVERYONE_SID;
            sid[0] = 9;
            let bytes = ace(0, 0, FILE_GENERIC_READ, &sid);
            // SAFETY: `bytes` is a whole ACE of its `AceSize`; the user SID is never read for an invalid SID.
            let made = unsafe { grant_of(bytes.as_ptr(), std::ptr::null_mut(), false) }
                .expect("an allow ACE");
            assert_eq!((made.principal, made.mask), (Principal::Other, u32::MAX));
        }
    }
}
