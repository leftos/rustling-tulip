//! A replace that cannot land leaves the target as it was and no temp file behind.

use std::collections::BTreeMap;
use std::fs;
#[cfg(windows)]
use std::fs::OpenOptions;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use claude_accounts::{
    AccountLock, AccountsRoot, ConsumeLock, CredentialBlob, Identity, PollError, Store,
    StoreErrorKind, UsageTable,
};
#[cfg(windows)]
use claude_accounts::{AccountLockGuard, Sequence};

use crate::links::{junction, snapshot, symlink_or_skip};
use crate::support::{ALICE, Scratch, grant_users_write};
#[cfg(windows)]
use crate::support::{ALICE_CRED, assert_owner_only};
#[cfg(windows)]
use crate::windows_files::pin_first_temp;
#[cfg(windows)]
use crate::windows_files::temp_files as shared_temp_files;

/// What a file outside the account store holds, which no store write may change.
#[cfg(windows)]
const VICTIM: &[u8] = b"a file outside the account store";

#[cfg(windows)]
fn temp_files(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .expect("the folder lists")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
        })
        .collect()
}

#[cfg(windows)]
#[test]
fn replace_failure_leaves_target_and_no_temp() {
    let scratch = Scratch::new("replace_failure_leaves_target_and_no_temp");
    let store = scratch.store();
    let guard = scratch.lock();
    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    let cred_before = scratch.read(ALICE_CRED);
    let sequence_before = scratch.read("accounts.json");

    {
        // The credential file can be read, but not replaced, while held; the roster is held the same way.
        let _held_cred = hold_readable(&scratch.path(ALICE_CRED));
        let _held_sequence = hold_readable(&scratch.path("accounts.json"));

        let blob = CredentialBlob::new(
            r#"{"claudeAiOauth":{"accessToken":"fake-access-next"}}"#.to_owned(),
        );
        let error = store
            .write_credentials(&guard, 1, &blob)
            .expect_err("a held target cannot be replaced");
        assert_eq!(error.kind(), StoreErrorKind::CredentialsWrite);
        let error = store
            .write_sequence(&guard, &sequence)
            .expect_err("a held roster cannot be replaced");
        assert_eq!(error.kind(), StoreErrorKind::AccountsWrite);
    }

    assert_eq!(
        scratch.read(ALICE_CRED),
        cred_before,
        "the credential file is untouched"
    );
    assert_eq!(
        scratch.read("accounts.json"),
        sequence_before,
        "the roster is untouched"
    );
    assert_eq!(
        temp_files(&scratch.path("credentials")),
        Vec::<String>::new()
    );
    assert_eq!(temp_files(&scratch.backup()), Vec::<String>::new());
}

/// `FILE_SHARE_READ`: a handle that lets others read the file, but not replace it.
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x1;

#[cfg(windows)]
/// Holds `path` open letting others read it but not replace it: a write still reads `accounts.json` before it writes
/// (an unreadable one refuses every write), and the replace onto it fails.
fn hold_readable(path: &std::path::Path) -> std::fs::File {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .expect("the file opens sharing reads only")
}

#[cfg(windows)]
/// The serial the next temp file of this process takes, read off a sequence write held at its temp file: the roster
/// held open keeps the rename failing while the temp file is pinned, so the write gives up and leaves it. The pinned
/// temp file is removed again.
fn next_temp_serial(
    scratch: &Scratch,
    store: &Store,
    guard: &AccountLockGuard,
    sequence: &Sequence,
) -> u64 {
    let held = hold_readable(&scratch.path("accounts.json"));
    let pinner = pin_first_temp(scratch.backup());
    let refused = store.write_sequence(guard, sequence);
    drop(held);
    let pinned = pinner.join().expect("the temp file was pinned");
    assert!(refused.is_err(), "a held roster cannot be replaced");
    drop(pinned);
    let left = shared_temp_files(&scratch.backup());
    assert_eq!(left.len(), 1, "the pinned temp file stays: {left:?}");
    fs::remove_file(scratch.path(&left[0])).expect("the pinned temp file can be removed");
    let serial = left[0]
        .trim_end_matches(".tmp")
        .rsplit('.')
        .next()
        .expect("the temp name has a serial");
    serial
        .parse::<u64>()
        .expect("the temp name's serial is a number")
        + 1
}

#[cfg(windows)]
/// A symbolic link planted at the names a sequence write's temp file takes next is never opened: the file it points at
/// keeps its bytes, the link stays, and the write lands through a free name.
#[test]
fn a_link_at_the_next_temp_name_is_never_followed() {
    let scratch = Scratch::new("a_link_at_the_next_temp_name_is_never_followed");
    let store = scratch.store();
    let guard = scratch.lock();
    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    let victim = scratch.dir.join("victim.txt");
    fs::write(&victim, VICTIM).expect("the victim can be written");
    // TEMP_SERIAL is process-global: a parallel test that takes these serials first makes this pass vacuously, never fail.
    let serial = next_temp_serial(&scratch, &store, &guard, &sequence);
    let links: Vec<PathBuf> = (serial..serial + 8)
        .map(|serial| scratch.path(&format!("accounts.{}.{serial}.tmp", std::process::id())))
        .collect();
    for link in &links {
        if !symlink_or_skip(link, &victim) {
            return;
        }
    }

    assert!(
        store
            .set_active(&guard, 2)
            .expect("the write takes a free name"),
        "account 2 becomes active"
    );

    assert_eq!(
        fs::read(&victim).expect("the victim stays"),
        VICTIM,
        "the linked file keeps its bytes"
    );
    for link in &links {
        let kind = fs::symlink_metadata(link)
            .expect("the link stays")
            .file_type();
        assert!(
            kind.is_symlink(),
            "a taken name is never removed or replaced"
        );
    }
    let written = fs::symlink_metadata(scratch.path("accounts.json")).expect("the roster exists");
    assert!(
        written.file_type().is_file(),
        "the roster is a plain file, not the link"
    );
    let reread = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    assert_eq!(reread.active(), Some(2), "the write landed");
}

/// Replaces the account store's `folder` with a junction to a folder outside the root that holds `file` with `bytes`,
/// and returns the outside folder.
fn plant_junction(scratch: &Scratch, folder: &str, file: &str, bytes: &[u8]) -> PathBuf {
    let outside = scratch.dir.join(format!("outside-{folder}"));
    fs::create_dir_all(&outside).expect("the outside folder can be made");
    fs::write(outside.join(file), bytes).expect("the planted file can be written");
    let inside = scratch.path(folder);
    if inside.exists() {
        fs::remove_dir_all(&inside).expect("the root's folder can be removed");
    }
    junction(&inside, &outside);
    outside
}

#[test]
fn a_junctioned_credentials_folder_is_neither_read_nor_written() {
    let scratch = Scratch::new("a_junctioned_credentials_folder_is_neither_read_nor_written");
    let planted = STANDARD.encode(r#"{"claudeAiOauth":{"accessToken":"fake-access-planted"}}"#);
    let outside = plant_junction(&scratch, "credentials", "1.cred", planted.as_bytes());
    let before = snapshot(&outside);
    let store = scratch.store();
    let guard = scratch.lock();

    let read = store
        .read_credentials(1)
        .expect_err("a junctioned folder is not read");
    let blob =
        CredentialBlob::new(r#"{"claudeAiOauth":{"accessToken":"fake-access-next"}}"#.to_owned());
    let write = store
        .write_credentials(&guard, 1, &blob)
        .expect_err("a junctioned folder is not written");

    assert_eq!(read.kind(), StoreErrorKind::CredentialsUnreadable);
    assert_eq!(write.kind(), StoreErrorKind::CredentialsWrite);
    assert_eq!(
        snapshot(&outside),
        before,
        "the folder behind the junction is unchanged"
    );
}

#[test]
fn a_junctioned_configs_folder_is_neither_read_nor_written() {
    let scratch = Scratch::new("a_junctioned_configs_folder_is_neither_read_nor_written");
    let outside = plant_junction(&scratch, "configs", "1.json", br#"{"planted": true}"#);
    let before = snapshot(&outside);
    let store = scratch.store();
    let guard = scratch.lock();

    let read = store
        .read_account_config(1)
        .expect_err("a junctioned folder is not read");
    let write = store
        .write_account_config(&guard, 1, &serde_json::json!({"written": true}))
        .expect_err("a junctioned folder is not written");

    assert_eq!(read.kind(), StoreErrorKind::ConfigUnreadable);
    assert_eq!(write.kind(), StoreErrorKind::ConfigWrite);
    assert_eq!(
        snapshot(&outside),
        before,
        "the folder behind the junction is unchanged"
    );
}

#[test]
fn a_junctioned_cache_folder_is_neither_read_nor_written() {
    let scratch = Scratch::new("a_junctioned_cache_folder_is_neither_read_nor_written");
    let outside = plant_junction(
        &scratch,
        "cache",
        "usage.json",
        br#"{"schemaVersion": 3, "accounts": {}}"#,
    );
    let before = snapshot(&outside);
    let identities = BTreeMap::from([(
        1,
        Identity::new(ALICE, "00000000-0000-4000-8000-0000000000a1"),
    )]);

    let reserved = UsageTable::reserve(&scratch.root(), &identities, jiff::Timestamp::now());

    assert!(
        matches!(reserved, Err(PollError::CacheUnreadable(_))),
        "a junctioned cache is not read: {reserved:?}"
    );
    assert_eq!(
        snapshot(&outside),
        before,
        "the folder behind the junction is unchanged"
    );
}

/// A store created from nothing makes its root and its folders owner-only, and every file it writes carries that
/// protected DACL (Windows) or mode 0600 (unix).
#[test]
fn a_store_created_from_nothing_is_owner_only() {
    let scratch = Scratch::empty("a_store_created_from_nothing_is_owner_only");
    let parent = scratch.dir.join("rustling-tulip");
    let root = AccountsRoot::new(parent.join("accounts"));
    let store = Store::open(root.clone()).expect("the store opens");
    let lock = AccountLock::acquire(&root).expect("the account lock is free");
    let sequence = serde_json::json!({"order": [1], "accounts": {"1": {"email": ALICE}}});
    std::fs::write(root.accounts_json(), sequence.to_string()).expect("the roster can be written");
    let sequence = store
        .read_sequence()
        .expect("the roster reads")
        .expect("the roster exists");
    store
        .write_sequence(&lock, &sequence)
        .expect("the roster write lands");
    store
        .write_credentials(
            &lock,
            1,
            &CredentialBlob::new(r#"{"claudeAiOauth":{}}"#.to_owned()),
        )
        .expect("the credential write lands");

    #[cfg(windows)]
    for path in [
        &parent,
        root.root(),
        &root.credentials_dir(),
        &root.accounts_json(),
        &root.credential_path(1),
    ] {
        assert_owner_only(path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for folder in [&parent, root.root(), &root.credentials_dir()] {
            assert_eq!(
                std::fs::metadata(folder)
                    .expect("the folder has metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "{}",
                folder.display()
            );
        }
        for file in [&root.accounts_json(), &root.credential_path(1)] {
            assert_eq!(
                std::fs::metadata(file)
                    .expect("the file has metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{}",
                file.display()
            );
        }
    }
}

/// A store root another principal can write is refused when the store is opened, not only read from.
#[test]
fn a_store_root_writable_by_another_principal_is_refused() {
    let scratch = Scratch::new("a_store_root_writable_by_another_principal_is_refused");
    grant_users_write(&scratch.backup());

    let error =
        Store::open(scratch.root()).expect_err("a root another principal can write is refused");

    assert_eq!(error.kind(), StoreErrorKind::RootUnsafe);
}

/// A roster another principal can write is refused on read rather than parsed.
#[test]
fn a_loose_dacl_accounts_json_is_refused_on_read() {
    let scratch = Scratch::new("a_loose_dacl_accounts_json_is_refused_on_read");
    grant_users_write(&scratch.path("accounts.json"));

    let error = scratch
        .store()
        .read_sequence()
        .expect_err("a roster another principal can write is refused");

    assert_eq!(error.kind(), StoreErrorKind::AccountsUnreadable);
}

/// The account lock refuses a store whose parent folder is a junction, and one whose `.lock` is a link.
#[test]
fn a_lock_is_refused_over_a_linked_root_or_lock_file() {
    let linked_parent = Scratch::new("a_lock_is_refused_over_a_linked_parent");
    let real = linked_parent.dir.with_file_name(format!(
        "{}-real",
        linked_parent
            .dir
            .file_name()
            .expect("the scratch folder has a name")
            .to_string_lossy()
    ));
    if real.exists() {
        fs::remove_dir_all(&real).expect("an earlier run's folder can be removed");
    }
    fs::rename(&linked_parent.dir, &real).expect("the scratch folder can be moved aside");
    junction(&linked_parent.dir, &real);
    let linked_root = AccountsRoot::new(linked_parent.dir.join("backup"));
    assert!(
        AccountLock::acquire(&linked_root).is_err(),
        "a linked parent is refused by the account lock"
    );
    assert!(
        ConsumeLock::acquire(&linked_root, 1).is_err(),
        "a linked parent is refused by the consume lock"
    );

    let scratch = Scratch::new("a_lock_is_refused_over_a_linked_lock_file");
    let root = scratch.root();
    let target = scratch.dir.join("outside-lock");
    fs::write(&target, b"").expect("the link's target can be written");
    fs::remove_file(root.lock_file()).expect("the fixture's lock file can be removed");
    if !symlink_or_skip(&root.lock_file(), &target) {
        return;
    }
    assert!(
        AccountLock::acquire(&root).is_err(),
        "a link at .lock is refused"
    );
    fs::remove_file(root.lock_file()).expect("the link can be removed");
    if !symlink_or_skip(&root.consume_lock(1), &target) {
        return;
    }
    assert!(
        ConsumeLock::acquire(&root, 1).is_err(),
        "a link at the consume lock is refused"
    );
}

/// A store whose parent folder is a junction is refused: a link above the root is never followed either.
#[test]
fn a_store_whose_parent_folder_is_a_junction_is_refused() {
    let scratch = Scratch::new("a_store_whose_parent_folder_is_a_junction_is_refused");
    let real = scratch.dir.with_file_name(format!(
        "{}-real",
        scratch
            .dir
            .file_name()
            .expect("the scratch folder has a name")
            .to_string_lossy()
    ));
    if real.exists() {
        fs::remove_dir_all(&real).expect("an earlier run's folder can be removed");
    }
    fs::rename(&scratch.dir, &real).expect("the scratch folder can be moved aside");
    junction(&scratch.dir, &real);

    let error = Store::open(AccountsRoot::new(scratch.dir.join("backup")))
        .expect_err("a linked parent is refused");

    assert_eq!(error.kind(), StoreErrorKind::RootUnsafe);
}

/// A user who links the whole account store is refused: the root itself, like the folder above it, is never followed.
#[test]
fn an_account_store_reached_through_a_junction_is_refused() {
    let scratch = Scratch::new("an_account_store_reached_through_a_junction_is_refused");
    let real = scratch.dir.join("real-backup");
    fs::rename(scratch.backup(), &real).expect("the account store can be moved aside");
    junction(&scratch.backup(), &real);

    let error = Store::open(scratch.root()).expect_err("a linked root is refused");

    assert_eq!(error.kind(), StoreErrorKind::RootUnsafe);
    assert!(
        real.join("accounts.json").is_file(),
        "nothing was written through the link"
    );
}
