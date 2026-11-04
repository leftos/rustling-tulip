//! Claude Code's own files: the atomic live credential write and the key-scoped `~/.claude.json` update, each under the
//! lock Claude Code guards the file with.

use std::fs;

use claude_accounts::{
    ClaudeCodeLocks, ClaudeCredentialsGuard, ClaudeFileError, ClaudePaths, CredentialBlob,
    DirLockGuard, clear_primary_api_key, update_global_config, write_live_credentials,
};
use serde_json::{Value, json};

use crate::support::Scratch;
use crate::windows_files::{hold_open, pin_first_temp, temp_files};

fn paths(scratch: &Scratch) -> ClaudePaths {
    ClaudePaths::new(
        scratch.dir.join(".claude"),
        scratch.dir.join(".claude.json"),
    )
}

/// Claude Code's two credential locks for `paths`, held.
fn credentials_guard(paths: &ClaudePaths) -> ClaudeCredentialsGuard {
    ClaudeCodeLocks::credentials(paths).expect("Claude Code's credential locks are free")
}

/// Claude Code's config lock for `paths`, held.
fn config_guard(paths: &ClaudePaths) -> DirLockGuard {
    ClaudeCodeLocks::config(paths).expect("Claude Code's config lock is free")
}

fn blob(access: &str) -> CredentialBlob {
    CredentialBlob::new(
        json!({ "claudeAiOauth": { "accessToken": access, "refreshToken": "fake-refresh" } })
            .to_string(),
    )
}

/// A config as Claude Code writes it, with keys out of alphabetical order, nested values and a managed API key.
const CONFIG_WITH_KEY: &str = r#"{
  "numStartups": 3,
  "zeta": {
    "b": 1,
    "a": [
      true,
      null,
      2.5
    ]
  },
  "primaryApiKey": "sk-ant-fake",
  "customApiKeyResponses": {
    "approved": [
      "fake"
    ]
  },
  "alpha": [],
  "projects": {}
}"#;

/// [`CONFIG_WITH_KEY`] without its `primaryApiKey` line.
const CONFIG_WITHOUT_KEY: &str = r#"{
  "numStartups": 3,
  "zeta": {
    "b": 1,
    "a": [
      true,
      null,
      2.5
    ]
  },
  "customApiKeyResponses": {
    "approved": [
      "fake"
    ]
  },
  "alpha": [],
  "projects": {}
}"#;

/// A config with keys out of alphabetical order, nested values and a managed API key.
fn config_with_key() -> Value {
    json!({
        "numStartups": 3,
        "zeta": { "b": 1, "a": [true, null, 2.5] },
        "primaryApiKey": "sk-ant-fake",
        "customApiKeyResponses": { "approved": ["fake"] },
        "oauthAccount": { "emailAddress": "one@example.test", "organizationUuid": "org" },
        "alpha": [],
        "projects": {},
    })
}

#[test]
fn live_credentials_write_replaces_the_file_and_leaves_no_temp() {
    let scratch = Scratch::empty("claude_files_live_write");
    let paths = paths(&scratch);
    let guard = credentials_guard(&paths);

    write_live_credentials(&paths, &guard, &blob("fake-access-old"))
        .expect("a first write makes the folder and the file");
    write_live_credentials(&paths, &guard, &blob("fake-access-new"))
        .expect("a second write replaces it");

    let written = fs::read_to_string(paths.credentials_file()).expect("the live file can be read");
    assert_eq!(written, blob("fake-access-new").expose());
    assert!(
        temp_files(&scratch.dir.join(".claude")).is_empty(),
        "no temp file is left behind"
    );
}

#[test]
fn live_credentials_write_leaves_the_old_file_intact_when_the_rename_fails() {
    let scratch = Scratch::empty("claude_files_live_rename_fails");
    let paths = paths(&scratch);
    let guard = credentials_guard(&paths);
    write_live_credentials(&paths, &guard, &blob("fake-access-old"))
        .expect("the old file is written");
    let held = hold_open(&paths.credentials_file());

    let result = write_live_credentials(&paths, &guard, &blob("fake-access-new"));
    drop(held);

    assert!(
        matches!(result, Err(ClaudeFileError::Write(_))),
        "the rename fails: {result:?}"
    );
    let kept = fs::read_to_string(paths.credentials_file()).expect("the live file can be read");
    assert_eq!(
        kept,
        blob("fake-access-old").expose(),
        "the old file is intact"
    );
    assert!(
        temp_files(&scratch.dir.join(".claude")).is_empty(),
        "the temp file is removed"
    );
}

#[test]
fn live_credentials_write_whose_temp_cannot_be_removed_reports_it() {
    let scratch = Scratch::empty("claude_files_live_temp_left");
    let paths = paths(&scratch);
    let guard = credentials_guard(&paths);
    write_live_credentials(&paths, &guard, &blob("fake-access-old"))
        .expect("the old file is written");
    let held = hold_open(&paths.credentials_file());
    let pinner = pin_first_temp(scratch.dir.join(".claude"));

    let result = write_live_credentials(&paths, &guard, &blob("fake-access-new"));
    drop(held);
    let pinned = pinner.join().expect("the temp file was pinned");

    assert!(
        matches!(result, Err(ClaudeFileError::WriteLeftTemp(_))),
        "got {result:?}"
    );
    assert_eq!(
        temp_files(&scratch.dir.join(".claude")).len(),
        1,
        "the temp file is still there"
    );
    let kept = fs::read_to_string(paths.credentials_file()).expect("the live file can be read");
    assert_eq!(
        kept,
        blob("fake-access-old").expose(),
        "the old file is intact"
    );
    drop(pinned);
}

#[test]
fn global_config_update_keeps_every_other_key_and_its_order() {
    let scratch = Scratch::empty("claude_files_config_order");
    let paths = paths(&scratch);
    fs::write(paths.global_config(), CONFIG_WITH_KEY).expect("the config is written");

    let guard = config_guard(&paths);
    assert!(
        clear_primary_api_key(&paths, &guard).expect("the key is cleared"),
        "a key was removed"
    );

    assert_eq!(
        fs::read_to_string(paths.global_config()).expect("the config can be read"),
        CONFIG_WITHOUT_KEY,
        "only primaryApiKey changed"
    );
}

#[test]
fn global_config_update_sets_a_key_in_place() {
    let scratch = Scratch::empty("claude_files_config_set");
    let paths = paths(&scratch);
    fs::write(
        paths.global_config(),
        serde_json::to_string_pretty(&config_with_key()).expect("the config serializes"),
    )
    .expect("the config is written");

    let guard = config_guard(&paths);
    let written = update_global_config(&paths, &guard, |config| {
        config.insert(
            "oauthAccount".to_owned(),
            json!({ "emailAddress": "two@example.test" }),
        );
        true
    })
    .expect("the update is written");

    assert!(written);
    let mut expected = config_with_key();
    expected["oauthAccount"] = json!({ "emailAddress": "two@example.test" });
    let expected = serde_json::to_string_pretty(&expected).expect("the config serializes");
    assert_eq!(
        fs::read_to_string(paths.global_config()).expect("the config can be read"),
        expected,
        "the key keeps its place"
    );
}

#[test]
fn global_config_update_round_trips_floats_byte_for_byte() {
    let scratch = Scratch::empty("claude_files_config_floats");
    let paths = paths(&scratch);
    let original = "{\n  \"a\": 11.486232000000001,\n  \"b\": 0.23547200000000001\n}";
    fs::write(paths.global_config(), original).expect("the config is written");

    let guard = config_guard(&paths);
    assert!(update_global_config(&paths, &guard, |_| true).expect("the update is written"));

    assert_eq!(
        fs::read_to_string(paths.global_config()).expect("the config can be read"),
        original,
        "every float is written back as it was read"
    );
}

#[test]
fn torn_global_config_is_refused_unchanged() {
    let scratch = Scratch::empty("claude_files_config_torn");
    let paths = paths(&scratch);
    let torn = r#"{"oauthAccount": {"emailAddress": "one@exa"#;
    fs::write(paths.global_config(), torn).expect("the torn config is written");

    let guard = config_guard(&paths);
    let cleared = clear_primary_api_key(&paths, &guard);
    let updated = update_global_config(&paths, &guard, |_| true);

    assert!(
        matches!(cleared, Err(ClaudeFileError::ConfigUnreadable)),
        "got {cleared:?}"
    );
    assert!(
        matches!(updated, Err(ClaudeFileError::ConfigUnreadable)),
        "got {updated:?}"
    );
    assert_eq!(
        fs::read_to_string(paths.global_config()).expect("the config can be read"),
        torn,
        "the torn file is left as it was"
    );
}

#[test]
fn absent_primary_api_key_is_a_no_op() {
    let scratch = Scratch::empty("claude_files_config_no_key");
    let paths = paths(&scratch);
    let compact = r#"{"oauthAccount":{"emailAddress":"one@example.test"},"numStartups":3}"#;
    fs::write(paths.global_config(), compact).expect("the config is written");
    let guard = config_guard(&paths);

    assert!(!clear_primary_api_key(&paths, &guard).expect("nothing to clear is not an error"));
    assert_eq!(
        fs::read_to_string(paths.global_config()).expect("the config can be read"),
        compact,
        "the file is not rewritten"
    );

    fs::remove_file(paths.global_config()).expect("the config can be removed");
    assert!(!clear_primary_api_key(&paths, &guard).expect("an absent config has nothing to clear"));
    assert!(
        !paths.global_config().exists(),
        "an absent config is not created"
    );
}

/// Another taker's stale takeover of the lock at `dir`: it removes the directory and makes its own in its place.
fn take_over(dir: &std::path::Path) {
    fs::remove_dir(dir).expect("the other taker removes the lock directory");
    fs::create_dir(dir).expect("the other taker makes its own lock directory");
}

#[test]
fn live_credentials_write_refuses_a_lost_lock_and_writes_nothing() {
    let scratch = Scratch::empty("claude_files_live_lock_lost");
    let paths = paths(&scratch);
    let guard = credentials_guard(&paths);
    write_live_credentials(&paths, &guard, &blob("fake-access-old"))
        .expect("the old file is written");
    take_over(&paths.credentials_lock());

    let result = write_live_credentials(&paths, &guard, &blob("fake-access-new"));

    assert!(
        matches!(result, Err(ClaudeFileError::LockLost)),
        "got {result:?}"
    );
    let kept = fs::read_to_string(paths.credentials_file()).expect("the live file can be read");
    assert_eq!(
        kept,
        blob("fake-access-old").expose(),
        "the old file is intact"
    );
    assert!(
        temp_files(&scratch.dir.join(".claude")).is_empty(),
        "no temp file was made"
    );
}

#[test]
fn global_config_update_refuses_a_lost_lock() {
    let scratch = Scratch::empty("claude_files_config_lock_lost");
    let paths = paths(&scratch);
    fs::write(paths.global_config(), CONFIG_WITH_KEY).expect("the config is written");
    let guard = config_guard(&paths);
    take_over(&paths.config_lock());

    let updated = update_global_config(&paths, &guard, |_| true);
    let cleared = clear_primary_api_key(&paths, &guard);

    assert!(
        matches!(updated, Err(ClaudeFileError::LockLost)),
        "got {updated:?}"
    );
    assert!(
        matches!(cleared, Err(ClaudeFileError::LockLost)),
        "got {cleared:?}"
    );
    assert_eq!(
        fs::read_to_string(paths.global_config()).expect("the config can be read"),
        CONFIG_WITH_KEY,
        "the config is unchanged"
    );
}

#[test]
fn writer_refuses_a_guard_for_other_paths() {
    let scratch = Scratch::empty("claude_files_wrong_guard");
    let mine = paths(&scratch);
    let other = ClaudePaths::new(
        scratch.dir.join("other").join(".claude"),
        scratch.dir.join("other").join(".claude.json"),
    );
    let original = serde_json::to_string_pretty(&config_with_key()).expect("the config serializes");
    fs::write(mine.global_config(), &original).expect("the config is written");
    let credentials = credentials_guard(&other);
    let config = config_guard(&other);

    let written = write_live_credentials(&mine, &credentials, &blob("fake-access-new"));
    let updated = update_global_config(&mine, &config, |_| true);
    let cleared = clear_primary_api_key(&mine, &config);

    assert!(
        matches!(written, Err(ClaudeFileError::WrongLock)),
        "got {written:?}"
    );
    assert!(
        matches!(updated, Err(ClaudeFileError::WrongLock)),
        "got {updated:?}"
    );
    assert!(
        matches!(cleared, Err(ClaudeFileError::WrongLock)),
        "got {cleared:?}"
    );
    assert!(
        !mine.credentials_file().exists(),
        "no live file was written"
    );
    assert_eq!(
        fs::read_to_string(mine.global_config()).expect("the config can be read"),
        original,
        "the config is unchanged"
    );
}
