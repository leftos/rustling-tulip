//! The macOS Keychain smoke run: two logins captured from a real Keychain item through the real `/usr/bin/security`,
//! then a switch to the first and back to the second. The item lives in a throwaway keychain file the test creates and
//! deletes, under a scratch service name, so nothing touches the real `Claude Code-credentials` item or the login
//! keychain. Ignored by default: `rt.ps1 mac test --package claude-accounts --test smoke_macos -- --ignored`.

#![cfg(target_os = "macos")]
#![expect(
    clippy::expect_used,
    reason = "a smoke run that cannot set up its keychain or files must fail loudly"
)]

use std::fs;
use std::hash::{BuildHasher as _, RandomState};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use claude_accounts::{
    AccountsRoot, CaptureKind, ClaudePaths, Drain, LockTiming, OutgoingProbe, ProbeVerdicts,
    RefreshContext, Store, capture_live_login, switch,
};
use serde_json::{Value, json};

#[path = "refresh/support.rs"]
#[expect(
    dead_code,
    unused_imports,
    reason = "the refresh tests' fakes, of which the smoke run uses the profile endpoint and the credentials"
)]
mod support;

use support::{FakeEndpoint, FakeProfile, credential, ms_after, now, profile};

/// Apple's `security` tool.
const SECURITY: &str = "/usr/bin/security";
/// The account the scratch item is kept under.
const ACCOUNT: &str = "rt-smoke";
/// `errSecItemNotFound`: `security`'s exit code for no such item.
const NOT_FOUND: i32 = 44;

const A: &str = "smoke-a@example.test";
const A_ORG: &str = "00000000-0000-4000-8000-0000000000a5";
const A_UUID: &str = "00000000-0000-4000-8000-0000000000a6";
const B: &str = "smoke-b@example.test";
const B_ORG: &str = "00000000-0000-4000-8000-0000000000b5";
const B_UUID: &str = "00000000-0000-4000-8000-0000000000b6";

fn oauth_account(email: &str, org: &str, uuid: &str) -> Value {
    json!({"emailAddress": email, "organizationUuid": org, "accountUuid": uuid, "organizationName": "Org", "displayName": "Smoke"})
}

/// Runs `program` with `args` to the end, its output captured.
fn run(program: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .expect("`security` can be started")
}

/// Runs `program` with `args` and asserts it succeeded, naming `what` and `security`'s stderr when it did not.
fn run_ok(program: &Path, args: &[&str], what: &str) -> Output {
    let output = run(program, args);
    assert!(
        output.status.success(),
        "{what} failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// A throwaway keychain file and a `security` wrapper that names it in every call. Dropping it checks that no item of
/// the scratch service landed outside the throwaway keychain, then deletes the keychain by its path.
struct Keychain {
    path: PathBuf,
    wrapper: PathBuf,
    service: String,
}

impl Keychain {
    /// Creates, unlocks and keeps unlocked `smoke.keychain-db` in `dir`, and writes the `security-smoke` wrapper beside
    /// it: every call gets the keychain's path as its last argument, and each command line `security -i` reads on its
    /// stdin gets it the same way.
    fn create(dir: &Path, service: String) -> Self {
        let path = dir.join("smoke.keychain-db");
        let text = path
            .to_str()
            .expect("the keychain's path is text")
            .to_owned();
        let text = text.as_str();
        assert!(
            !text.contains(['\'', '"', '\\']),
            "the keychain's path holds no quote or backslash, so the wrapper can quote it"
        );
        run_ok(
            Path::new(SECURITY),
            &["create-keychain", "-p", "smoke", text],
            "create-keychain",
        );
        let keychain = Self {
            wrapper: dir.join("security-smoke"),
            path,
            service,
        };
        run_ok(
            Path::new(SECURITY),
            &["unlock-keychain", "-p", "smoke", text],
            "unlock-keychain",
        );
        run_ok(
            Path::new(SECURITY),
            &["set-keychain-settings", text],
            "set-keychain-settings",
        );
        fs::write(
            &keychain.wrapper,
            format!(
                "#!/bin/sh\n\
                 kc='{text}'\n\
                 if [ \"$1\" = \"-i\" ]; then\n\
                 \x20 while IFS= read -r line || [ -n \"$line\" ]; do\n\
                 \x20   if [ -n \"$line\" ]; then printf '%s \"%s\"\\n' \"$line\" \"$kc\"; fi\n\
                 \x20 done | {SECURITY} -i\n\
                 \x20 exit $?\n\
                 fi\n\
                 exec {SECURITY} \"$@\" \"$kc\"\n"
            ),
        )
        .expect("the wrapper can be written");
        fs::set_permissions(&keychain.wrapper, fs::Permissions::from_mode(0o700))
            .expect("the wrapper can be made runnable");
        keychain
    }

    /// Puts `value` in the scratch item through the wrapper, as Claude Code's login would.
    fn put(&self, value: &str) {
        run_ok(
            &self.wrapper,
            &[
                "add-generic-password",
                "-U",
                "-a",
                ACCOUNT,
                "-s",
                &self.service,
                "-w",
                value,
            ],
            "add-generic-password",
        );
    }

    /// The scratch item's value, read through the wrapper, without the newline `security` prints after it.
    fn item(&self) -> String {
        let output = run_ok(
            &self.wrapper,
            &[
                "find-generic-password",
                "-a",
                ACCOUNT,
                "-w",
                "-s",
                &self.service,
            ],
            "find-generic-password",
        );
        let mut value = String::from_utf8(output.stdout).expect("the item is text");
        if value.ends_with('\n') {
            value.pop();
        }
        value
    }
}

impl Drop for Keychain {
    fn drop(&mut self) {
        // The scratch item goes from the throwaway keychain first, so the search below can only find one elsewhere.
        let _removed = run(
            &self.wrapper,
            &[
                "delete-generic-password",
                "-a",
                ACCOUNT,
                "-s",
                &self.service,
            ],
        );
        // Straight to `security`, over the default search list: an item of the scratch service anywhere else leaked.
        let found = run(
            Path::new(SECURITY),
            &["find-generic-password", "-s", &self.service],
        );
        let leaked = found.status.code() != Some(NOT_FOUND);
        if found.status.success() {
            let _deleted = run(
                Path::new(SECURITY),
                &["delete-generic-password", "-s", &self.service],
            );
        }
        let deleted = self
            .path
            .to_str()
            .map(|path| run(Path::new(SECURITY), &["delete-keychain", path]));
        assert!(
            std::thread::panicking() || !leaked,
            "an item of the scratch service was found outside the throwaway keychain (find exited {}); it was deleted",
            found.status
        );
        assert!(
            std::thread::panicking() || deleted.is_some_and(|output| output.status.success()),
            "the throwaway keychain could not be deleted"
        );
    }
}

/// A fresh owner-only folder in the temp dir, named by its real path (macOS's `/var` is `/private/var`, which the
/// store refuses as a root's parent).
fn scratch_dir(tag: &str) -> PathBuf {
    // SAFETY: umask only swaps the process's file-mode creation mask; it takes no pointer and cannot fail.
    let _previous = unsafe { libc::umask(0o077) };
    let dir = std::env::temp_dir().join(format!("claude-accounts-smoke-{tag}"));
    fs::create_dir_all(&dir).expect("the smoke run's folder can be made");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .expect("the smoke run's folder can be made private");
    fs::canonicalize(&dir).expect("the smoke run's folder has a real path")
}

fn write_global(claude: &ClaudePaths, email: &str, org: &str, uuid: &str) {
    fs::write(
        claude.global_config(),
        json!({"numStartups": 1, "oauthAccount": oauth_account(email, org, uuid)}).to_string(),
    )
    .expect("the global config can be written");
}

#[test]
#[ignore = "the macOS Keychain smoke run: & .\\rt.ps1 mac test -- --package claude-accounts --test smoke_macos -- --ignored"]
fn keychain_switch_smoke() {
    let tag = format!(
        "{:08x}",
        RandomState::new().hash_one(std::process::id()) & 0xffff_ffff
    );
    let dir = scratch_dir(&tag);
    let keychain = Keychain::create(&dir, format!("Claude Code-credentials-smoke-{tag}"));
    let config_home = dir.join("claude");
    fs::create_dir_all(&config_home).expect("Claude Code's folder can be made");
    let claude = ClaudePaths::new(config_home, dir.join(".claude.json")).with_keychain(
        keychain.service.clone(),
        ACCOUNT,
        keychain.wrapper.clone(),
    );
    let store = Store::open(AccountsRoot::new(dir.join("accounts"))).expect("the store opens");
    let endpoint = FakeEndpoint::answering(Vec::new());
    let profile_endpoint =
        FakeProfile::answering(vec![profile(A_UUID, A, A_ORG), profile(B_UUID, B, B_ORG)]);
    let drain = Arc::new(Drain::new());
    let verdicts = ProbeVerdicts::new();
    let context = RefreshContext {
        store: &store,
        claude: &claude,
        transport: &endpoint,
        drain: &drain,
        securestorage_dir_set: false,
        clock: &now,
        profile: &profile_endpoint,
        verdicts: &verdicts,
        claude_locks: LockTiming::CLAUDE,
    };
    let login_a = credential(
        "fake-access-smoke-a",
        Some("fake-refresh-smoke-a"),
        ms_after(3600),
    );
    let login_b = credential(
        "fake-access-smoke-b",
        Some("fake-refresh-smoke-b"),
        ms_after(3600),
    );

    keychain.put(&login_a);
    write_global(&claude, A, A_ORG, A_UUID);
    let captured_a = capture_live_login(&context).expect("login A is captured");
    keychain.put(&login_b);
    write_global(&claude, B, B_ORG, B_UUID);
    let captured_b = capture_live_login(&context).expect("login B is captured");

    assert_eq!(
        (captured_a.kind, captured_a.number),
        (CaptureKind::Added, 1)
    );
    assert_eq!(
        (captured_b.kind, captured_b.number),
        (CaptureKind::Added, 2)
    );

    let to_a = switch(&context, 1, &OutgoingProbe::unresolved(), None).expect("the switch to A");
    assert!(to_a.switched, "the live login moved to A");
    assert_eq!(keychain.item(), login_a, "the item holds A's bytes");

    let to_b = switch(&context, 2, &OutgoingProbe::unresolved(), None).expect("the switch to B");
    assert!(to_b.switched, "the live login moved back to B");
    assert_eq!(keychain.item(), login_b, "the item holds B's bytes");

    let sequence = store
        .read_sequence()
        .expect("the store reads")
        .expect("the store has accounts");
    assert_eq!(sequence.numbers(), [1, 2], "the store lists two accounts");
    assert!(
        !claude.credentials_file().exists(),
        "no live credential file was created"
    );
}
