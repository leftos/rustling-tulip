//! The Windows calls the locks need: one byte of a file locked and unlocked, and a directory opened by handle so that
//! its identity is read and its mtime set, or its removal made, through that one handle. A switch's rollback removes a
//! live file the same way: compared and deleted through one handle.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_LOCK_VIOLATION, ERROR_NOT_SUPPORTED,
    FILETIME,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_ATTRIBUTES, FileDispositionInfo, FileDispositionInfoEx, FileIdInfo,
    GetFileInformationByHandleEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    SetFileInformationByHandle, SetFileTime, UnlockFileEx,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

use crate::store;

/// 100 ns intervals from 1601-01-01, the `FILETIME` epoch, to 1970-01-01.
const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

/// Takes an exclusive lock on byte 0 of `file` without waiting, the byte `msvcrt.locking` takes.
/// `Ok(false)` when another handle holds it.
pub(crate) fn try_lock_first_byte(file: &File) -> io::Result<bool> {
    // A zeroed OVERLAPPED is offset 0 with no event, the range the file lock covers.
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: the handle is open for the life of `file`, and `overlapped` outlives this synchronous call.
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &raw mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == i32::try_from(ERROR_LOCK_VIOLATION).ok() {
        return Ok(false);
    }
    Err(error)
}

/// Releases the lock [`try_lock_first_byte`] took.
pub(crate) fn unlock_first_byte(file: &File) -> io::Result<()> {
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: the handle is open for the life of `file`, and `overlapped` outlives this synchronous call.
    let unlocked = unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &raw mut overlapped) };
    if unlocked == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What a directory is opened to do: each opens with only the access its action needs.
#[derive(Clone, Copy, Debug)]
pub(crate) enum DirAccess {
    /// Read its identity and metadata.
    Read,
    /// Read them, and set its times.
    Touch,
    /// Read them, and mark it for deletion.
    Remove,
}

/// An open handle to a directory, so that a check of its identity and the action that follows bind to one file object
/// even if the name is given to another directory meanwhile. It shares reading, writing and deletion, so Claude Code's
/// own opens and `rmdir` proceed while it is open. It is held for one check and one action, never for a lock's life:
/// where a removal falls back to the older delete semantics (a file system without POSIX deletes), the name stays taken
/// until the last handle to the removed directory closes, and Claude Code's `mkdir` there is refused until then.
#[derive(Debug)]
pub(crate) struct DirHandle(File);

impl DirHandle {
    /// Opens the directory at `dir` for `access`.
    pub(crate) fn open(dir: &Path, access: DirAccess) -> io::Result<Self> {
        let rights = match access {
            DirAccess::Read => FILE_READ_ATTRIBUTES,
            DirAccess::Touch => FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            DirAccess::Remove => FILE_READ_ATTRIBUTES | DELETE,
        };
        OpenOptions::new()
            .access_mode(rights)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
            .map(Self)
    }

    /// The directory's identity on this machine: its volume's serial number and its 128-bit file ID (`FILE_ID_INFO`,
    /// unique on `ReFS` too, where the older 64-bit index is not). A directory removed and made again under the same
    /// name gets a new one, where its creation time may come back unchanged (NTFS name tunneling, and the clock's tick).
    pub(crate) fn identity(&self) -> io::Result<(u64, [u8; 16])> {
        let mut info = FILE_ID_INFO::default();
        let size = u32::try_from(size_of::<FILE_ID_INFO>()).map_err(io::Error::other)?;
        // SAFETY: the handle is open for the life of `self`, and `info` is a writable `FILE_ID_INFO` of `size` bytes,
        // the structure `FileIdInfo` asks for.
        let read = unsafe {
            GetFileInformationByHandleEx(
                self.0.as_raw_handle(),
                FileIdInfo,
                (&raw mut info).cast(),
                size,
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((info.VolumeSerialNumber, info.FileId.Identifier))
    }

    /// The directory's metadata, read through the handle.
    pub(crate) fn metadata(&self) -> io::Result<Metadata> {
        self.0.metadata()
    }

    /// Sets the directory's last-write time to now, as Claude Code's lock holders do to show they are alive. Needs
    /// [`DirAccess::Touch`].
    pub(crate) fn touch(&self) -> io::Result<()> {
        let now = filetime_now()?;
        // SAFETY: the handle is open for the life of `self`; null pointers leave the creation and access times alone.
        let set = unsafe {
            SetFileTime(
                self.0.as_raw_handle(),
                std::ptr::null(),
                std::ptr::null(),
                &raw const now,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Removes the directory with POSIX semantics and closes the handle: its name is free at once, even while other
    /// processes (an indexer, antivirus) still have it open, so Claude Code's next `mkdir` there succeeds. A file system
    /// without POSIX deletes (FAT32, Windows before 10 1607) gets the older semantics instead, as std's `remove_dir_all`
    /// does: the directory, and its name, go once every other handle to it has closed. Needs [`DirAccess::Remove`], and
    /// fails on a directory that is not empty.
    pub(crate) fn remove(self) -> io::Result<()> {
        mark_for_deletion(&self.0)
    }
}

/// Removes the file at `path` only while it holds exactly `written`, comparing and deleting through one handle that
/// shares reading alone: no other writer can replace, rename onto or delete the file between the compare and the
/// delete. `between` runs after the compare matched and before the delete. `Ok(true)` when the file is gone (one
/// already gone included), `Ok(false)` when it holds other bytes and was left. A symbolic link at `path` is opened as
/// itself, never followed, so it never holds `written`; a file whose length is not `written`'s is left unread. The
/// delete has POSIX semantics where the file system offers them, the older semantics otherwise, as
/// [`DirHandle::remove`].
pub(crate) fn remove_file_if_holding(
    path: &Path,
    written: &[u8],
    between: impl FnOnce(),
) -> io::Result<bool> {
    let opened = store::retry_transient(|| {
        OpenOptions::new()
            .access_mode(DELETE | FILE_GENERIC_READ)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    });
    let mut file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    if u64::try_from(written.len()).ok() != Some(file.metadata()?.len()) {
        return Ok(false);
    }
    let held = store::read_bounded(&mut file, written.len())?;
    if held.as_slice() != written {
        return Ok(false);
    }
    between();
    mark_for_deletion(&file)?;
    Ok(true)
}

/// Marks `file`, opened with `DELETE` access, for deletion with POSIX semantics: its name is free once this handle
/// closes, even while other processes still have it open. A file system without POSIX deletes (FAT32, Windows before
/// 10 1607) gets the older semantics instead, as std's `remove_dir_all` does: the file, and its name, go once every
/// other handle to it has closed.
fn mark_for_deletion(file: &File) -> io::Result<()> {
    let posix = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    match set_information(file, FileDispositionInfoEx, &posix) {
        Err(error)
            if [
                ERROR_INVALID_PARAMETER,
                ERROR_NOT_SUPPORTED,
                ERROR_INVALID_FUNCTION,
            ]
            .iter()
            .any(|&code| error.raw_os_error() == i32::try_from(code).ok()) =>
        {
            set_information(
                file,
                FileDispositionInfo,
                &FILE_DISPOSITION_INFO { DeleteFile: true },
            )
        }
        result => result,
    }
}

/// Sets the information of `class` on `file`'s handle from `info`, the structure that class asks for.
fn set_information<T>(file: &File, class: FILE_INFO_BY_HANDLE_CLASS, info: &T) -> io::Result<()> {
    let size = u32::try_from(size_of::<T>()).map_err(io::Error::other)?;
    // SAFETY: the handle is open for the life of `file`, and the pointer and `size`, `size_of::<T>()`, describe one
    // readable `T` borrowed for this synchronous call, whatever `T` is, so the call reads only memory it may. Whether
    // `T` is the structure `class` asks for decides only whether the call succeeds.
    let set = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            class,
            std::ptr::from_ref(info).cast(),
            size,
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn filetime_now() -> io::Result<FILETIME> {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let intervals = u64::try_from(since_epoch.as_nanos() / 100).map_err(io::Error::other)?;
    let [b0, b1, b2, b3, b4, b5, b6, b7] = (intervals + FILETIME_UNIX_EPOCH).to_le_bytes();
    Ok(FILETIME {
        dwLowDateTime: u32::from_le_bytes([b0, b1, b2, b3]),
        dwHighDateTime: u32::from_le_bytes([b4, b5, b6, b7]),
    })
}
