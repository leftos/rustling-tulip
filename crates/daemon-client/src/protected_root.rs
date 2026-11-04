//! The protected root: where a daemon at or above High integrity keeps its
//! config, binary cache and `daemon-id`, out of reach of the processes below
//! it.
//!
//! The root is created with [`PROTECTED_ROOT_SDDL`]: Administrators and SYSTEM
//! own it and hold full access, and everything under it inherits a High
//! mandatory label that refuses reads, writes and execution from a lower
//! integrity. A folder that already exists is never trusted blindly — an
//! ordinary process can create it first — so [`create_or_check`] reads its
//! descriptor back and refuses anything but the security this module would
//! have created. A refused root is never deleted or remade here: the refusal
//! names it for the user to delete from an elevated terminal.

use anyhow::Context as _;
#[cfg(windows)]
use anyhow::anyhow;
use std::path::Path;

/// The descriptor a protected root is created with: Administrators as the
/// explicit owner, a protected DACL granting full access to SYSTEM and
/// Administrators with inheritance to everything below them, and a High label
/// that refuses reads, writes and execution from a lower integrity. The owner
/// is named because a machine policy may otherwise make the creating user the
/// owner, which [`check_root`] refuses.
pub const PROTECTED_ROOT_SDDL: &str =
    "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)S:(ML;OICI;NRNWNX;;;HI)";

/// The security of a folder that could be the protected root, as read back
/// from its descriptor: the parts [`check_root`] compares, kept free of any
/// Windows API so the check is testable anywhere.
#[derive(Debug)]
pub struct RootSecurity {
    /// The owner's SID, `S-1-5-…`.
    pub owner: String,
    /// Whether the DACL is protected from the parent folder's inheritable
    /// entries (`SE_DACL_PROTECTED`).
    pub dacl_protected: bool,
    /// Every entry of the DACL, in order.
    pub dacl: Vec<AceEntry>,
    /// The mandatory label carried in the SACL, if there is one.
    pub label: Option<LabelEntry>,
}

/// One DACL entry: the trustee it names, what it grants or denies, and the
/// flags that carry it to the folder's children.
#[derive(Debug)]
pub struct AceEntry {
    /// Whether the entry allows access rather than denying it.
    pub allow: bool,
    /// The entry's trustee, `S-1-5-…`.
    pub sid: String,
    /// The access mask it applies.
    pub mask: u32,
    /// `ACE_HEADER::AceFlags`: the inheritance bits, and anything else the
    /// stored descriptor carries.
    pub flags: u8,
}

/// The mandatory label ACE: its integrity RID, the upward-rights mask and the
/// inheritance flags.
#[derive(Debug)]
pub struct LabelEntry {
    /// The RID of the label SID, `S-1-16-<rid>`.
    pub rid: u32,
    /// The `SYSTEM_MANDATORY_LABEL_NO_*_UP` bits it sets.
    pub mask: u32,
    /// `ACE_HEADER::AceFlags`.
    pub flags: u8,
}

/// Why a folder is refused as the protected root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootRefusal {
    /// Someone other than Administrators or SYSTEM owns it.
    Owner(String),
    /// Its DACL inherits from the parent folder, so a lower-integrity process
    /// can add an entry.
    InheritedDacl,
    /// Its DACL grants, or denies, more than Administrators and SYSTEM hold.
    Dacl,
    /// It carries no label that refuses reads and writes from below.
    Label,
}

impl std::fmt::Display for RootRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owner(owner) => {
                write!(f, "the owner is {owner}, not Administrators or SYSTEM")
            }
            Self::InheritedDacl => f.write_str("its permissions inherit from the parent folder"),
            Self::Dacl => f.write_str("its permissions allow more than Administrators and SYSTEM"),
            Self::Label => f.write_str(
                "it has no High integrity label that refuses reads and writes from below",
            ),
        }
    }
}

/// Whether `sec` is exactly the security a protected root is created with.
///
/// The check compares the parsed owner, DACL entries and label, never a
/// rendered SDDL string: a descriptor read back from disk carries ACL and ACE
/// flags (`AI`, and inheritance bits) that the creation string does not, so
/// only the principals, rights, masks and levels are compared.
///
/// # Errors
///
/// The first way `sec` differs from the protected descriptor.
pub fn check_root(sec: &RootSecurity) -> Result<(), RootRefusal> {
    /// `S-1-5-32-544`: the built-in Administrators group.
    const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
    /// `S-1-5-18`: SYSTEM.
    const SYSTEM_SID: &str = "S-1-5-18";
    /// `FILE_ALL_ACCESS`.
    const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
    /// `OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE`.
    const INHERIT_ACE_FLAGS: u8 = 0x3;
    /// `SYSTEM_MANDATORY_LABEL_NO_WRITE_UP | NO_READ_UP | NO_EXECUTE_UP`.
    const NO_UP_LABEL_MASK: u32 = 0x7;
    /// `SECURITY_MANDATORY_HIGH_RID`.
    const HIGH_RID: u32 = 0x3000;

    if sec.owner != ADMINISTRATORS_SID && sec.owner != SYSTEM_SID {
        return Err(RootRefusal::Owner(sec.owner.clone()));
    }
    if !sec.dacl_protected {
        return Err(RootRefusal::InheritedDacl);
    }
    let grants_full_access = |sid: &str| {
        sec.dacl.iter().any(|ace| {
            ace.allow
                && ace.sid == sid
                && ace.mask == FILE_ALL_ACCESS
                && ace.flags & INHERIT_ACE_FLAGS == INHERIT_ACE_FLAGS
        })
    };
    let only_administrators_and_system = sec.dacl.len() == 2
        && sec.dacl.iter().all(|ace| ace.allow)
        && grants_full_access(SYSTEM_SID)
        && grants_full_access(ADMINISTRATORS_SID);
    if !only_administrators_and_system {
        return Err(RootRefusal::Dacl);
    }
    let labelled = sec.label.as_ref().is_some_and(|label| {
        label.rid >= HIGH_RID
            && label.mask & NO_UP_LABEL_MASK == NO_UP_LABEL_MASK
            && label.flags & INHERIT_ACE_FLAGS == INHERIT_ACE_FLAGS
    });
    if !labelled {
        return Err(RootRefusal::Label);
    }
    Ok(())
}

/// Create the protected root at `path` when nothing is there, then check that
/// what is there is the protected root.
///
/// # Errors
///
/// The folder could not be created or read, or it is a reparse point, not a
/// folder, or not the security [`check_root`] accepts.
#[cfg(windows)]
pub fn create_or_check(path: &Path) -> anyhow::Result<()> {
    create_if_missing(path)?;
    check_existing(path)
}

/// Create the protected root at `path` when nothing is there, its parents
/// with the default descriptor. Whatever is already there is left as it is,
/// for [`check_handle`] to judge through a handle the caller holds.
///
/// # Errors
///
/// The folder could not be created, or `path` could not be read.
#[cfg(windows)]
pub fn create_if_missing(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            create_protected_dir(path)?;
        }
        Err(err) => {
            return Err(err).with_context(|| format!("reading {}", path.display()));
        }
    }
    Ok(())
}

/// Create `path` as a new folder carrying [`PROTECTED_ROOT_SDDL`] from its
/// first moment, so nothing below it is ever readable by a lower integrity. A
/// folder of that name created by another elevated process in the meantime is
/// no error: the caller reads it back and checks it.
#[cfg(windows)]
fn create_protected_dir(path: &Path) -> anyhow::Result<()> {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::CreateDirectoryW;
    use windows::core::HSTRING;

    let sddl = HSTRING::from(PROTECTED_ROOT_SDDL);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `sddl` is NUL-terminated and outlives the call; `descriptor`
    // receives a LocalAlloc'd descriptor, freed below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &sddl,
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    }
    .map_err(std::io::Error::other)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let name = HSTRING::from(path.as_os_str());
    // SAFETY: `name` and `attributes` (and the descriptor it points to) stay
    // alive for the call.
    let created = unsafe { CreateDirectoryW(&name, Some(&raw const attributes)) };
    // SAFETY: the descriptor the conversion allocated, freed once, after the
    // create that read it.
    let _ = unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
    match created {
        Ok(()) => Ok(()),
        Err(err) if err.code() == ERROR_ALREADY_EXISTS.to_hresult() => Ok(()),
        Err(err) => {
            Err(std::io::Error::other(err)).with_context(|| format!("creating {}", path.display()))
        }
    }
}

/// Check the folder at `path` as it is now, through a handle of its own
/// ([`open_held`], closed on return).
///
/// # Errors
///
/// The folder could not be opened, or [`check_handle`] refuses it.
#[cfg(windows)]
fn check_existing(path: &Path) -> anyhow::Result<()> {
    let handle = open_held(path)?;
    check_handle(handle.raw(), path)
}

/// Check the folder `handle` holds open, named `path` in the refusal: one
/// handle yields both the object's attributes and its security, so a reparse
/// point swapped in after an earlier look is refused here rather than
/// followed to its target, and the folder checked is the folder held.
///
/// # Errors
///
/// The folder could not be read, it is a reparse point or not a folder, or its
/// security is not what [`check_root`] accepts.
#[cfg(windows)]
pub fn check_handle(handle: windows::Win32::Foundation::HANDLE, path: &Path) -> anyhow::Result<()> {
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        GetFileInformationByHandle,
    };

    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `handle` is an open folder handle the caller holds; `info`
    // receives that object's attributes.
    unsafe { GetFileInformationByHandle(handle, &raw mut info) }
        .with_context(|| format!("reading the attributes of {}", path.display()))?;
    let reparse = (info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0) != 0;
    let directory = (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0) != 0;
    if reparse || !directory {
        anyhow::bail!(
            "protected root {} is not a plain folder; delete it from an elevated terminal and start again",
            path.display()
        );
    }
    let sec = read_security(handle)
        .with_context(|| format!("reading the security of {}", path.display()))?;
    check_root(&sec).map_err(|refusal| {
        anyhow!(
            "protected root {} is unsafe: {refusal}; delete it from an elevated terminal and start again",
            path.display()
        )
    })
}

/// `path` opened as the object it names, without following a reparse point,
/// sharing read and write but not delete: while the handle is open the folder
/// cannot be renamed or deleted, so no process can move it aside and put its
/// own folder at that path. Listing access is what makes the open take part
/// in share-mode checks (an open asking only for attribute access does not);
/// `READ_CONTROL` lets [`check_handle`] read its descriptor. Backup semantics
/// are what let a folder be opened at all.
///
/// # Errors
///
/// The folder could not be opened.
#[cfg(windows)]
pub fn open_held(path: &Path) -> anyhow::Result<OwnedHandle> {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL,
    };
    use windows::core::HSTRING;

    let name = HSTRING::from(path.as_os_str());
    // SAFETY: `name` is NUL-terminated and outlives the call; the handle
    // returned is owned by the guard below.
    let handle = unsafe {
        CreateFileW(
            &name,
            READ_CONTROL.0 | FILE_LIST_DIRECTORY.0 | FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .with_context(|| format!("opening {}", path.display()))?;
    Ok(OwnedHandle(handle))
}

/// A handle [`open_held`] opened, closed once when dropped unless the caller
/// keeps it for the life of the process ([`OwnedHandle::into_raw`]).
#[cfg(windows)]
pub struct OwnedHandle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl OwnedHandle {
    /// The handle, still owned by this guard.
    #[must_use]
    pub fn raw(&self) -> windows::Win32::Foundation::HANDLE {
        self.0
    }

    /// The handle, no longer closed by this guard: the caller keeps it open.
    #[must_use]
    pub fn into_raw(self) -> windows::Win32::Foundation::HANDLE {
        std::mem::ManuallyDrop::new(self).0
    }
}

#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;

        // SAFETY: the handle was opened by this module and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A security descriptor [`read_security`] had `GetSecurityInfo` allocate,
/// freed once when dropped.
#[cfg(windows)]
struct OwnedDescriptor(windows::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl Drop for OwnedDescriptor {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{HLOCAL, LocalFree};

        // SAFETY: the descriptor was allocated by GetSecurityInfo and is freed
        // once, here.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

/// Read the owner, DACL and mandatory label of the object `handle` names into
/// [`RootSecurity`].
#[cfg(windows)]
fn read_security(handle: windows::Win32::Foundation::HANDLE) -> anyhow::Result<RootSecurity> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID,
    };

    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `handle` is an open object handle opened with READ_CONTROL; the
    // pointers receive the owner SID, the DACL and the SACL, and the
    // descriptor they point into, which the guard frees once below.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            Some(&raw mut owner),
            None,
            Some(&raw mut dacl),
            Some(&raw mut sacl),
            Some(&raw mut descriptor),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(anyhow!(
            "the descriptor could not be read: {}",
            status.to_hresult().message()
        ));
    }
    let descriptor = OwnedDescriptor(descriptor);
    Ok(RootSecurity {
        owner: sid_to_string(owner)?,
        dacl_protected: dacl_is_protected(descriptor.0)?,
        dacl: dacl_entries(dacl)?,
        label: label_entry(sacl)?,
    })
}

/// `sid` in its `S-1-5-…` string form.
#[cfg(windows)]
fn sid_to_string(sid: windows::Win32::Security::PSID) -> anyhow::Result<String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::core::PWSTR;

    let mut text = PWSTR::null();
    // SAFETY: `sid` points into a live descriptor that outlives the call;
    // `text` receives a LocalAlloc'd string, freed below.
    unsafe { ConvertSidToStringSidW(sid, &raw mut text) }.context("converting a SID to text")?;
    // SAFETY: the call wrote a NUL-terminated string.
    let rendered = unsafe { text.to_string() };
    // SAFETY: the string the conversion allocated, freed once.
    let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    rendered.map_err(|err| anyhow!("a SID whose text is not UTF-16: {err}"))
}

/// Whether `descriptor`'s DACL is protected from the folder's parent
/// (`SE_DACL_PROTECTED`), so no inherited entry can ever reach the root.
#[cfg(windows)]
fn dacl_is_protected(
    descriptor: windows::Win32::Security::PSECURITY_DESCRIPTOR,
) -> anyhow::Result<bool> {
    use windows::Win32::Security::{
        GetSecurityDescriptorControl, SE_DACL_PROTECTED, SECURITY_DESCRIPTOR_CONTROL,
    };

    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: `descriptor` is a valid descriptor that outlives the call.
    unsafe { GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision) }
        .context("reading the descriptor's control bits")?;
    Ok(SECURITY_DESCRIPTOR_CONTROL(control).contains(SE_DACL_PROTECTED))
}

/// Every entry of the DACL `dacl`, in order. An entry of a type the check does
/// not model (an object or callback ACE) is recorded as a denial, so it can
/// only ever make [`check_root`] refuse.
#[cfg(windows)]
fn dacl_entries(dacl: *const windows::Win32::Security::ACL) -> anyhow::Result<Vec<AceEntry>> {
    use windows::Win32::Security::{ACCESS_ALLOWED_ACE, ACE_HEADER, GetAce, PSID};

    /// `ACCESS_ALLOWED_ACE_TYPE`.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    /// `ACCESS_DENIED_ACE_TYPE`.
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;

    if dacl.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: `dacl` is the DACL of a live descriptor.
    let count = unsafe { (*dacl).AceCount };
    let mut entries = Vec::with_capacity(usize::from(count));
    for index in 0..u32::from(count) {
        let mut ace = std::ptr::null_mut();
        // SAFETY: `index` is below the ACL's ACE count.
        unsafe { GetAce(dacl, index, &raw mut ace) }.context("reading a DACL entry")?;
        // SAFETY: `ace` points into the live DACL and starts with an ACE_HEADER.
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceType == ACCESS_ALLOWED_ACE_TYPE || header.AceType == ACCESS_DENIED_ACE_TYPE {
            // SAFETY: an access-allowed and an access-denied ACE share the
            // layout of ACCESS_ALLOWED_ACE: header, mask and a SID at SidStart.
            let entry = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            let sid = PSID((&raw const entry.SidStart).cast_mut().cast());
            entries.push(AceEntry {
                allow: header.AceType == ACCESS_ALLOWED_ACE_TYPE,
                sid: sid_to_string(sid)?,
                mask: entry.Mask,
                flags: header.AceFlags,
            });
        } else {
            entries.push(AceEntry {
                allow: false,
                sid: String::new(),
                mask: 0,
                flags: header.AceFlags,
            });
        }
    }
    Ok(entries)
}

/// The mandatory label the SACL `sacl` carries, if any.
#[cfg(windows)]
fn label_entry(sacl: *const windows::Win32::Security::ACL) -> anyhow::Result<Option<LabelEntry>> {
    use windows::Win32::Security::{
        ACE_HEADER, GetAce, GetSidSubAuthority, GetSidSubAuthorityCount, PSID,
        SYSTEM_MANDATORY_LABEL_ACE,
    };

    /// `SYSTEM_MANDATORY_LABEL_ACE_TYPE`.
    const LABEL_ACE_TYPE: u8 = 0x11;

    if sacl.is_null() {
        return Ok(None);
    }
    // SAFETY: `sacl` is the SACL of a live descriptor.
    let count = unsafe { (*sacl).AceCount };
    for index in 0..u32::from(count) {
        let mut ace = std::ptr::null_mut();
        // SAFETY: `index` is below the SACL's ACE count.
        unsafe { GetAce(sacl, index, &raw mut ace) }.context("reading a label entry")?;
        // SAFETY: `ace` points into the live SACL and starts with an ACE_HEADER.
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceType != LABEL_ACE_TYPE {
            continue;
        }
        // SAFETY: a mandatory-label ACE shares the layout of
        // SYSTEM_MANDATORY_LABEL_ACE: header, mask and a SID at SidStart.
        let label = unsafe { &*ace.cast::<SYSTEM_MANDATORY_LABEL_ACE>() };
        let sid = PSID((&raw const label.SidStart).cast_mut().cast());
        // SAFETY: `sid` is the label's SID, inside the ACE.
        let sub_authorities = unsafe { *GetSidSubAuthorityCount(sid) };
        let Some(last) = sub_authorities.checked_sub(1) else {
            return Ok(None);
        };
        // SAFETY: `last` is below the SID's sub-authority count.
        let rid = unsafe { *GetSidSubAuthority(sid, u32::from(last)) };
        return Ok(Some(LabelEntry {
            rid,
            mask: label.Mask,
            flags: header.AceFlags,
        }));
    }
    Ok(None)
}

/// Create the directory at `path` when nothing is there, then check it.
///
/// Other platforms have no integrity levels and no per-folder descriptor to
/// enforce, so an ordinary directory is the whole of it.
///
/// # Errors
///
/// The directory could not be created.
#[cfg(not(windows))]
pub fn create_or_check(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("creating the protected root {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{AceEntry, LabelEntry, RootSecurity, check_root};
    #[cfg(windows)]
    use super::{check_existing, create_or_check};

    const DACL_REFUSAL: &str = "its permissions allow more than Administrators and SYSTEM";
    const LABEL_REFUSAL: &str =
        "it has no High integrity label that refuses reads and writes from below";

    /// The security [`super::PROTECTED_ROOT_SDDL`] creates, as it reads back.
    fn good() -> RootSecurity {
        RootSecurity {
            owner: "S-1-5-32-544".to_owned(),
            dacl_protected: true,
            dacl: vec![
                AceEntry {
                    allow: true,
                    sid: "S-1-5-18".to_owned(),
                    mask: 0x001F_01FF,
                    flags: 0x3,
                },
                AceEntry {
                    allow: true,
                    sid: "S-1-5-32-544".to_owned(),
                    mask: 0x001F_01FF,
                    flags: 0x3,
                },
            ],
            label: Some(LabelEntry {
                rid: 0x3000,
                mask: 0x7,
                flags: 0x3,
            }),
        }
    }

    /// Why `sec` is refused, as the user reads it.
    fn refusal(sec: &RootSecurity) -> String {
        check_root(sec)
            .expect_err("the root must be refused")
            .to_string()
    }

    #[test]
    fn root_check_accepts_the_protected_descriptor() {
        assert!(
            check_root(&good()).is_ok(),
            "the creation descriptor is accepted"
        );

        let mut carried = good();
        for ace in &mut carried.dacl {
            ace.flags = 0x13;
        }
        if let Some(label) = carried.label.as_mut() {
            label.flags = 0x13;
        }
        assert!(
            check_root(&carried).is_ok(),
            "extra flag bits the creation SDDL lacks are accepted"
        );
    }

    #[test]
    fn root_check_accepts_system_as_owner() {
        let mut sec = good();
        sec.owner = "S-1-5-18".to_owned();
        assert!(check_root(&sec).is_ok());
    }

    #[test]
    fn root_check_refuses_an_owner_other_than_administrators_or_system() {
        let mut sec = good();
        sec.owner = "S-1-5-21-1-2-3-1001".to_owned();
        assert_eq!(
            refusal(&sec),
            "the owner is S-1-5-21-1-2-3-1001, not Administrators or SYSTEM"
        );
    }

    #[test]
    fn root_check_refuses_an_extra_dacl_entry_for_the_user() {
        let mut sec = good();
        sec.dacl.push(AceEntry {
            allow: true,
            sid: "S-1-5-21-1-2-3-1001".to_owned(),
            mask: 0x001F_01FF,
            flags: 0x3,
        });
        assert_eq!(refusal(&sec), DACL_REFUSAL);
    }

    #[test]
    fn root_check_refuses_a_deny_or_weaker_entry() {
        let mut denied = good();
        denied.dacl[1].allow = false;
        assert_eq!(refusal(&denied), DACL_REFUSAL);

        let mut weaker = good();
        weaker.dacl[1].mask = 0x0012_0089;
        assert_eq!(refusal(&weaker), DACL_REFUSAL);
    }

    #[test]
    fn root_check_refuses_an_unprotected_dacl() {
        let mut sec = good();
        sec.dacl_protected = false;
        assert_eq!(
            refusal(&sec),
            "its permissions inherit from the parent folder"
        );
    }

    #[test]
    fn root_check_refuses_a_missing_or_weaker_label() {
        let mut missing = good();
        missing.label = None;
        assert_eq!(refusal(&missing), LABEL_REFUSAL);

        let mut medium = good();
        medium.label = Some(LabelEntry {
            rid: 0x2000,
            mask: 0x7,
            flags: 0x3,
        });
        assert_eq!(refusal(&medium), LABEL_REFUSAL);

        let mut writable = good();
        writable.label = Some(LabelEntry {
            rid: 0x3000,
            mask: 0x1,
            flags: 0x3,
        });
        assert_eq!(refusal(&writable), LABEL_REFUSAL);
    }

    /// A fresh folder path for one test, cleared of a previous run's copy.
    #[cfg(windows)]
    fn scratch(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("rt-protected-root-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[cfg(windows)]
    #[test]
    fn a_default_directory_is_refused_as_a_root() {
        let dir = scratch("default");
        std::fs::create_dir_all(&dir).expect("create an ordinary folder");
        let err = create_or_check(&dir).expect_err("an ordinary folder is no protected root");
        let text = format!("{err:#}");
        assert!(text.contains("is unsafe"), "unexpected refusal: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_is_refused_as_a_root() {
        let target = scratch("junction-target");
        std::fs::create_dir_all(&target).expect("create the junction's target");
        let junction = scratch("junction");
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .status()
            .expect("run mklink");
        assert!(
            status.success(),
            "mklink /J {} -> {} failed",
            junction.display(),
            target.display()
        );
        let err = create_or_check(&junction).expect_err("a junction is no protected root");
        let text = format!("{err:#}");
        assert!(
            text.contains("is not a plain folder"),
            "unexpected refusal: {text}"
        );
        let _ = std::fs::remove_dir(&junction);
        let _ = std::fs::remove_dir_all(&target);
    }

    /// A junction swapped in after the caller's first look is refused by the
    /// post-create check, which opens the object itself rather than following
    /// the reparse point to its target.
    #[cfg(windows)]
    #[test]
    fn a_junction_made_after_the_stat_is_refused() {
        let target = scratch("late-junction-target");
        std::fs::create_dir_all(&target).expect("create the junction's target");
        let junction = scratch("late-junction");
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .status()
            .expect("run mklink");
        assert!(
            status.success(),
            "mklink /J {} -> {} failed",
            junction.display(),
            target.display()
        );
        let err = check_existing(&junction).expect_err("a junction is no plain folder");
        let text = format!("{err:#}");
        assert!(
            text.contains("is not a plain folder"),
            "unexpected refusal: {text}"
        );
        let _ = std::fs::remove_dir(&junction);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[cfg(windows)]
    #[expect(
        clippy::print_stderr,
        reason = "the skip line tells a builder run that this case did not run"
    )]
    #[test]
    fn creating_the_root_then_checking_it_passes() {
        let elevated = crate::profile::own_profile()
            .is_ok_and(|profile| profile == crate::profile::Profile::Elevated);
        if !elevated {
            eprintln!("skipped: needs an elevated test process");
            return;
        }
        let root = scratch("created");
        create_or_check(&root).expect("create the protected root");
        create_or_check(&root).expect("check the root just created");
        match std::fs::remove_dir_all(&root) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => eprintln!("left the protected root {} behind: {err}", root.display()),
        }
    }
}
