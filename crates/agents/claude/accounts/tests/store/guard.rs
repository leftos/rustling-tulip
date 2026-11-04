//! The file guard: folders it creates are this user's alone, and every folder from the base down, and the file, is
//! refused when another user owns it, another non-admin principal can write it (off Windows: reach it at all), it is a
//! link, or (a file) it is over 64 KiB. Owner-is-another-SID and another uid cannot be staged without privileges or
//! `chown`; the pure decisions are unit-tested in `src/guard.rs`.

use std::fs;
use std::path::{Path, PathBuf};

use claude_accounts::guard::{self, GuardErrorKind};

use crate::support::Scratch;

/// A scratch folder for `test` and in it a base only the current user can reach: on Windows a DACL with no inherited
/// ACEs and one for this user, off Windows mode 0700.
pub fn private_base(test: &str) -> (Scratch, PathBuf) {
    let scratch = Scratch::empty(test);
    let base = scratch.dir.join("base");
    fs::create_dir(&base).expect("the base can be made");
    make_private(&base);
    (scratch, base)
}

#[cfg(windows)]
fn make_private(base: &Path) {
    let whoami = std::process::Command::new("whoami")
        .output()
        .expect("whoami can be started");
    let user = String::from_utf8(whoami.stdout).expect("whoami prints text");
    let output = std::process::Command::new("icacls")
        .arg(base)
        .args([
            "/inheritance:r",
            "/grant:r",
            &format!("{}:(OI)(CI)F", user.trim()),
        ])
        .output()
        .expect("icacls can be started");
    assert!(
        output.status.success(),
        "icacls failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[cfg(unix)]
fn make_private(base: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(base, fs::Permissions::from_mode(0o700))
        .expect("the base can be made private");
}

/// A private base for `test` and, under it, `accounts/credentials` made by the guard.
fn made(test: &str) -> (Scratch, PathBuf, PathBuf) {
    let (scratch, base) = private_base(&format!("guard-{test}"));
    let folder = base.join("accounts").join("credentials");
    guard::create_folder(&base, &folder).expect("the guard makes the folder");
    (scratch, base, folder)
}

fn kind_of_read(base: &Path, path: &Path) -> GuardErrorKind {
    guard::read_file(base, path)
        .expect_err("the read must be refused")
        .kind()
}

fn kind_of_check(base: &Path, folder: &Path) -> GuardErrorKind {
    guard::check_folder(base, folder)
        .expect_err("the folder must be refused")
        .kind()
}

#[test]
fn a_file_in_a_made_folder_reads_back() {
    let (_scratch, base, folder) = made("reads-back");
    guard::check_folder(&base, &folder).expect("the made chain passes");
    let file = folder.join("1.cred");
    fs::write(&file, b"sealed bytes").expect("the file can be written");
    assert_eq!(
        guard::read_file(&base, &file)
            .expect("the file reads")
            .as_bytes(),
        b"sealed bytes"
    );
    fs::write(base.join("top.cred"), b"top").expect("the file can be written");
    assert_eq!(
        guard::read_file(&base, &base.join("top.cred"))
            .expect("a file in the base reads")
            .as_bytes(),
        b"top"
    );
}

#[test]
fn making_a_folder_that_exists_checks_it() {
    let (_scratch, base, folder) = made("exists");
    guard::create_folder(&base, &folder).expect("an existing owner-only folder passes");
}

#[test]
fn a_path_outside_the_base_or_climbing_out_of_it_makes_nothing() {
    let (scratch, base) = private_base("guard-outside");
    let outside = scratch.dir.join("outside");
    assert_eq!(
        guard::create_folder(&base, &outside)
            .expect_err("outside the base")
            .kind(),
        GuardErrorKind::Io
    );
    assert!(!outside.exists(), "nothing is made outside the base");
    let climbing = base.join("made").join("..").join("..").join("climbed");
    assert_eq!(
        guard::create_folder(&base, &climbing)
            .expect_err("a `..` is refused")
            .kind(),
        GuardErrorKind::Io
    );
    assert!(
        !base.join("made").exists(),
        "nothing is made before the path is validated"
    );
    assert!(!scratch.dir.join("climbed").exists());
    assert_eq!(
        kind_of_read(&base, &outside.join("1.cred")),
        GuardErrorKind::Io
    );
}

#[test]
fn a_file_of_64_kib_reads_and_one_byte_more_is_refused() {
    let (_scratch, base, folder) = made("size");
    let file = folder.join("1.cred");
    fs::write(&file, vec![0x5a; 65_536]).expect("the file can be written");
    assert_eq!(
        guard::read_file(&base, &file)
            .expect("64 KiB reads")
            .as_bytes()
            .len(),
        65_536
    );
    fs::write(&file, vec![0x5a; 65_537]).expect("the file can be written");
    assert_eq!(kind_of_read(&base, &file), GuardErrorKind::TooLarge);
}

#[test]
fn a_refusal_names_the_path_and_the_check_and_no_content() {
    let (_scratch, base, folder) = made("message");
    let file = folder.join("1.cred");
    let mut contents = b"guard-secret-text".repeat(4_000);
    contents.truncate(65_537);
    fs::write(&file, &contents).expect("the file can be written");
    let error = guard::read_file(&base, &file).expect_err("the file is too large");
    assert_eq!(error.path(), file.as_path());
    assert_eq!(
        error.to_string(),
        format!("{}: is over 64 KiB", file.display())
    );
    assert!(
        !format!("{error:?}").contains("guard-secret-text"),
        "{error:?}"
    );
}

#[test]
fn a_missing_file_or_folder_is_missing() {
    let (_scratch, base, folder) = made("missing");
    assert_eq!(
        kind_of_read(&base, &folder.join("2.cred")),
        GuardErrorKind::Missing
    );
    let absent = folder.join("absent");
    assert_eq!(kind_of_check(&base, &absent), GuardErrorKind::Missing);
    assert_eq!(
        kind_of_read(&base, &absent.join("1.cred")),
        GuardErrorKind::Missing
    );
}

#[test]
fn a_file_for_a_folder_and_a_folder_for_a_file_are_refused() {
    let (_scratch, base, folder) = made("wrong-type");
    let file = folder.join("1.cred");
    fs::write(&file, b"x").expect("the file can be written");
    assert_eq!(kind_of_check(&base, &file), GuardErrorKind::NotAFolder);
    let inner = folder.join("inner");
    guard::create_folder(&base, &inner).expect("the guard makes the folder");
    assert_eq!(kind_of_read(&base, &inner), GuardErrorKind::NotAFile);
}

#[test]
fn creating_the_base_itself_is_refused_and_checking_it_checks_it_alone() {
    let (_scratch, base, _folder) = made("base-itself");
    let error = guard::create_folder(&base, &base).expect_err("the base is never made");
    assert_eq!(error.kind(), GuardErrorKind::Io);
    assert_eq!(error.path(), base.as_path());
    guard::check_folder(&base, &base).expect("checking the base checks it alone");
}

#[cfg(unix)]
#[test]
fn a_symlinked_base_named_with_a_trailing_slash_is_refused() {
    let (scratch, base, _folder) = made("base-slash");
    let link = scratch.dir.join("linked-base");
    std::os::unix::fs::symlink(&base, &link).expect("the symlink can be made");
    let slashed = PathBuf::from(format!("{}/", link.display()));
    assert_eq!(
        kind_of_check(&slashed, &slashed.join("accounts")),
        GuardErrorKind::Link
    );
    assert_eq!(
        kind_of_check(&link, &link.join("accounts")),
        GuardErrorKind::Link
    );
}

#[cfg(unix)]
mod unix {
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::path::Path;

    use claude_accounts::guard::{self, GuardErrorKind};

    use super::{kind_of_check, kind_of_read, made};

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("the mode can be set");
    }

    #[test]
    fn a_made_folder_is_mode_0700() {
        let (_scratch, base, folder) = made("mode");
        for path in [base.join("accounts"), folder] {
            let mode = fs::metadata(&path)
                .expect("the folder has metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{}", path.display());
        }
    }

    #[test]
    fn a_symlinked_folder_is_refused_in_and_above_the_chain() {
        let (_scratch, base, folder) = made("symlink-folder");
        fs::write(folder.join("1.cred"), b"x").expect("the file can be written");
        chmod(&folder.join("1.cred"), 0o600);
        let link = base.join("linked");
        symlink(base.join("accounts"), &link).expect("the symlink can be made");
        assert_eq!(kind_of_check(&base, &link), GuardErrorKind::Link);
        assert_eq!(
            kind_of_read(&base, &link.join("credentials").join("1.cred")),
            GuardErrorKind::Link
        );
        let error = guard::create_folder(&base, &link.join("credentials").join("new"))
            .expect_err("a symlink on the way");
        assert_eq!(error.kind(), GuardErrorKind::Link);
        assert!(
            !folder.join("new").exists(),
            "nothing is made through the symlink"
        );
    }

    #[test]
    fn a_symlinked_file_is_refused() {
        let (_scratch, base, folder) = made("symlink-file");
        let target = folder.join("1.cred");
        fs::write(&target, b"x").expect("the file can be written");
        chmod(&target, 0o600);
        let link = folder.join("2.cred");
        symlink(&target, &link).expect("the symlink can be made");
        assert_eq!(kind_of_read(&base, &link), GuardErrorKind::Link);
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let (_scratch, base, folder) = made("fifo");
        let fifo = folder.join("1.cred");
        let name = CString::new(fifo.as_os_str().as_bytes()).expect("no NUL in the path");
        // SAFETY: `name` is NUL-terminated and outlives the call.
        assert_eq!(
            unsafe { libc::mkfifo(name.as_ptr(), 0o600) },
            0,
            "the FIFO can be made"
        );
        assert_eq!(kind_of_read(&base, &fifo), GuardErrorKind::NotAFile);
    }

    #[test]
    fn group_or_other_bits_on_a_file_or_folder_are_refused() {
        let (_scratch, base, folder) = made("modes");
        let file = folder.join("1.cred");
        fs::write(&file, b"x").expect("the file can be written");
        chmod(&file, 0o600);
        assert_eq!(
            guard::read_file(&base, &file)
                .expect("0600 reads")
                .as_bytes(),
            b"x"
        );
        for (mode, kind) in [
            (0o620, GuardErrorKind::WritableByOther),
            (0o640, GuardErrorKind::ReadableByOther),
            (0o770, GuardErrorKind::WritableByOther),
        ] {
            chmod(&file, mode);
            assert_eq!(kind_of_read(&base, &file), kind, "{mode:o}");
        }
        chmod(&file, 0o600);
        chmod(&folder, 0o770);
        assert_eq!(
            kind_of_check(&base, &folder),
            GuardErrorKind::WritableByOther
        );
        assert_eq!(kind_of_read(&base, &file), GuardErrorKind::WritableByOther);
        chmod(&folder, 0o700);
        chmod(&base, 0o755);
        assert_eq!(
            kind_of_read(&base, &file),
            GuardErrorKind::ReadableByOther,
            "the base is checked too"
        );
    }
}

#[cfg(windows)]
mod windows {
    use std::fs;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::Path;
    use std::process::Command;
    use std::ptr;

    use claude_accounts::guard::{self, GuardErrorKind};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };

    use super::{kind_of_check, kind_of_read, made};

    /// SIDs as `icacls` takes them, so the grants do not depend on the system's language.
    const EVERYONE: &str = "*S-1-1-0";
    const USERS: &str = "*S-1-5-32-545";
    const SYSTEM: &str = "*S-1-5-18";
    const ADMINISTRATORS: &str = "*S-1-5-32-544";

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }

    /// The owner and DACL of `path` in SDDL.
    fn sddl(path: &Path) -> String {
        let name = wide(path);
        let info = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `name` is a NUL-terminated path that outlives the call; the owner, group, DACL and SACL out pointers
        // are null, which the API allows when the descriptor is asked for; `descriptor` is writable.
        let status = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                info,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &raw mut descriptor,
            )
        };
        assert_eq!(status, 0, "the security of {} can be read", path.display());
        let mut text: *mut u16 = ptr::null_mut();
        let mut length: u32 = 0;
        // SAFETY: `descriptor` is the descriptor GetNamedSecurityInfoW returned; `text` and `length` are writable.
        let converted = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                info,
                &raw mut text,
                &raw mut length,
            )
        };
        assert_ne!(converted, 0, "the descriptor converts to SDDL");
        // SAFETY: the conversion succeeded, so `text` points at `length` UTF-16 units it allocated.
        let sddl =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length as usize) });
        // SAFETY: both buffers were allocated by the calls above with LocalAlloc and are freed once here.
        unsafe {
            LocalFree(text.cast());
            LocalFree(descriptor);
        }
        sddl.trim_end_matches('\0').to_owned()
    }

    /// Adds an allow ACE for `grant` (`<sid>:<flags and rights>`) to `path`.
    fn icacls_grant(path: &Path, grant: &str) {
        let output = Command::new("icacls")
            .arg(path)
            .args(["/grant", grant])
            .output()
            .expect("icacls can be started");
        assert!(
            output.status.success(),
            "icacls /grant {grant} failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// Gives `path` a NULL DACL, which lets everyone do everything.
    fn set_null_dacl(path: &Path) {
        let name = wide(path);
        // SAFETY: `name` is a NUL-terminated path that outlives the call; a null DACL with DACL_SECURITY_INFORMATION
        // sets a NULL DACL, and the owner, group and SACL pointers are null because they are not being set.
        let status = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
            )
        };
        assert_eq!(status, 0, "the NULL DACL can be set");
        assert!(sddl(path).contains("NO_ACCESS_CONTROL"), "{}", sddl(path));
    }

    /// `O:<sid>D:P(A;OICI;FA;;;<sid>)`: owned by one user, whose one ACE is full control, inherited by files and
    /// folders made inside, and nothing inherited from above.
    fn assert_owner_only(path: &Path) {
        let sddl = sddl(path);
        let owner = sddl
            .strip_prefix("O:")
            .and_then(|rest| rest.split_once("D:"))
            .map(|(owner, _)| owner.to_owned())
            .expect("the SDDL names an owner");
        assert!(
            owner.starts_with("S-1-5-21-"),
            "the owner is a user account: {sddl}"
        );
        assert_eq!(
            sddl,
            format!("O:{owner}D:P(A;OICI;FA;;;{owner})"),
            "{}",
            path.display()
        );
    }

    #[test]
    fn a_made_folder_and_its_made_parent_are_owner_only_and_protected() {
        let (_scratch, base, folder) = made("owner-only");
        assert_owner_only(&base.join("accounts"));
        assert_owner_only(&folder);
    }

    #[test]
    fn write_for_everyone_or_users_on_a_file_is_refused() {
        for principal in [EVERYONE, USERS] {
            let (_scratch, base, folder) = made(&format!("file-write-{}", &principal[1..]));
            let file = folder.join("1.cred");
            fs::write(&file, b"x").expect("the file can be written");
            guard::read_file(&base, &file).expect("an owner-only file reads");
            icacls_grant(&file, &format!("{principal}:(W)"));
            assert_eq!(
                kind_of_read(&base, &file),
                GuardErrorKind::WritableByOther,
                "{principal}"
            );
        }
    }

    #[test]
    fn delete_or_permission_rights_for_users_on_a_file_are_refused() {
        for rights in ["(D)", "(WDAC)", "(WO)"] {
            let (_scratch, base, folder) =
                made(&format!("file-rights-{}", rights.trim_matches(['(', ')'])));
            let file = folder.join("1.cred");
            fs::write(&file, b"x").expect("the file can be written");
            icacls_grant(&file, &format!("{USERS}:{rights}"));
            assert_eq!(
                kind_of_read(&base, &file),
                GuardErrorKind::WritableByOther,
                "{rights}"
            );
        }
    }

    #[test]
    fn read_for_everyone_is_accepted() {
        let (_scratch, base, folder) = made("file-read-everyone");
        let file = folder.join("1.cred");
        fs::write(&file, b"x").expect("the file can be written");
        icacls_grant(&file, &format!("{EVERYONE}:(R)"));
        assert_eq!(
            guard::read_file(&base, &file)
                .expect("read access for others is no write")
                .as_bytes(),
            b"x"
        );
    }

    #[test]
    fn write_for_users_on_the_folder_is_refused() {
        let (_scratch, base, folder) = made("folder-write-users");
        let file = folder.join("1.cred");
        fs::write(&file, b"x").expect("the file can be written");
        icacls_grant(&folder, &format!("{USERS}:(W)"));
        assert_eq!(
            kind_of_check(&base, &folder),
            GuardErrorKind::WritableByOther
        );
        assert_eq!(kind_of_read(&base, &file), GuardErrorKind::WritableByOther);
    }

    #[test]
    fn delete_child_for_users_on_the_folder_is_refused() {
        let (_scratch, base, folder) = made("folder-delete-child");
        icacls_grant(&folder, &format!("{USERS}:(DC)"));
        assert_eq!(
            kind_of_check(&base, &folder),
            GuardErrorKind::WritableByOther
        );
    }

    #[test]
    fn an_inherit_only_write_for_users_on_the_folder_is_refused() {
        let (_scratch, base, folder) = made("folder-inherit-only");
        icacls_grant(&folder, &format!("{USERS}:(OI)(IO)(W)"));
        assert_eq!(
            kind_of_check(&base, &folder),
            GuardErrorKind::WritableByOther
        );
    }

    #[test]
    fn write_for_users_on_the_base_is_refused() {
        let (_scratch, base, folder) = made("base-write-users");
        let file = folder.join("1.cred");
        fs::write(&file, b"x").expect("the file can be written");
        icacls_grant(&base, &format!("{USERS}:(W)"));
        assert_eq!(
            kind_of_read(&base, &file),
            GuardErrorKind::WritableByOther,
            "the base is checked too"
        );
        let error =
            guard::create_folder(&base, &base.join("other")).expect_err("the base is refused");
        assert_eq!(error.kind(), GuardErrorKind::WritableByOther);
        assert!(
            !base.join("other").exists(),
            "nothing is made under a refused base"
        );
    }

    #[test]
    fn write_for_system_or_administrators_is_accepted() {
        let (_scratch, base, folder) = made("system-admins");
        let file = folder.join("1.cred");
        fs::write(&file, b"x").expect("the file can be written");
        for principal in [SYSTEM, ADMINISTRATORS] {
            icacls_grant(&folder, &format!("{principal}:(OI)(CI)(F)"));
            icacls_grant(&file, &format!("{principal}:(F)"));
        }
        guard::check_folder(&base, &folder)
            .expect("SYSTEM and Administrators may write the folder");
        assert_eq!(
            guard::read_file(&base, &file)
                .expect("SYSTEM and Administrators may write the file")
                .as_bytes(),
            b"x"
        );
    }

    #[test]
    fn a_null_dacl_is_refused() {
        let (_scratch, base, folder) = made("null-dacl");
        let file = folder.join("1.cred");
        fs::write(&file, b"x").expect("the file can be written");
        set_null_dacl(&file);
        assert_eq!(kind_of_read(&base, &file), GuardErrorKind::WritableByOther);
        set_null_dacl(&folder);
        assert_eq!(
            kind_of_check(&base, &folder),
            GuardErrorKind::WritableByOther
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_junction_as_the_folder_or_as_the_file_is_refused() {
        use crate::links::junction;

        let (_scratch, base, folder) = made("junction");
        fs::write(folder.join("1.cred"), b"x").expect("the file can be written");
        let link = base.join("accounts").join("linked");
        junction(&link, &folder);
        assert_eq!(kind_of_check(&base, &link), GuardErrorKind::Link);
        assert_eq!(
            kind_of_read(&base, &link.join("1.cred")),
            GuardErrorKind::Link
        );
        assert_eq!(
            kind_of_read(&base, &link),
            GuardErrorKind::Link,
            "the junction read as the file"
        );
        let error = guard::create_folder(&base, &link.join("inner"))
            .expect_err("a junction on the way is refused");
        assert_eq!(error.kind(), GuardErrorKind::Link);
        assert!(
            !folder.join("inner").exists(),
            "nothing is made through the junction"
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_junction_above_the_files_folder_is_refused() {
        use crate::links::junction;

        let (_scratch, base, folder) = made("junction-above");
        fs::write(folder.join("1.cred"), b"x").expect("the file can be written");
        let link = base.join("linked");
        junction(&link, &base.join("accounts"));
        assert_eq!(
            kind_of_read(&base, &link.join("credentials").join("1.cred")),
            GuardErrorKind::Link
        );
        let error = guard::create_folder(&base, &link.join("credentials").join("new"))
            .expect_err("a junction above is refused");
        assert_eq!(error.kind(), GuardErrorKind::Link);
        assert!(
            !folder.join("new").exists(),
            "nothing is made through the junction"
        );
    }

    /// Windows opens a last name with a trailing dot or space as the name without it, but an inner one as written, so
    /// each is refused, and so is a `:`, before anything is opened or made.
    #[test]
    fn a_name_ending_in_a_dot_or_space_or_holding_a_colon_is_refused() {
        let (_scratch, base, folder) = made("trailing");
        fs::write(folder.join("1.cred"), b"x").expect("the file can be written");
        for odd in ["credentials ", "credentials.", "credentials:stream"] {
            let accounts = base.join("accounts");
            let inner = accounts.join(odd);
            assert_eq!(
                kind_of_check(&base, &inner),
                GuardErrorKind::Io,
                "{odd:?} last"
            );
            assert_eq!(
                kind_of_read(&base, &inner.join("1.cred")),
                GuardErrorKind::Io,
                "{odd:?} inner"
            );
            let error = guard::create_folder(&base, &inner.join("new"))
                .expect_err("refused before making anything");
            assert_eq!(error.kind(), GuardErrorKind::Io, "{odd:?}");
            assert!(!folder.join("new").exists(), "{odd:?} made nothing");
        }
        assert_eq!(
            kind_of_read(&base, &folder.join("1.cred.")),
            GuardErrorKind::Io,
            "a file name ending in a dot"
        );
    }

    /// Needs Developer Mode or elevation to make the symlink. Without them it returns early and passes, and the line
    /// saying so is printed to stdout, which nextest shows only with `--no-capture`.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_symlinked_file_is_refused() {
        use crate::links::symlink_or_skip;

        let (_scratch, base, folder) = made("symlink");
        let target = folder.join("1.cred");
        fs::write(&target, b"x").expect("the file can be written");
        let link = folder.join("2.cred");
        if !symlink_or_skip(&link, &target) {
            return;
        }
        assert_eq!(kind_of_read(&base, &link), GuardErrorKind::Link);
    }
}
