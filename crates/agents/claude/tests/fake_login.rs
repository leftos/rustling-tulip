//! The fake Claude CLI's `auth login` mode: the login it writes for the
//! account store's capture, the identity that follows the email, and the ways
//! it refuses.

// Windows only: the fake runs under `node`, which the Mac builder's non-login
// PATH lacks, and the live tier that uses it is Windows-only.
#![cfg(windows)]
#![expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The repository root, three folders above this crate. Not canonicalized:
/// Rust's [`std::fs::canonicalize`] returns a verbatim `\\?\` path on Windows,
/// which node cannot resolve as its entry script.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// The fake CLI's entry script.
fn fake_cli() -> PathBuf {
    repo_root().join("crates/agents/claude/fake/index.mjs")
}

/// `node <the fake> <args>`, with `dir` as the config dir and the login's own
/// knobs cleared, so the run reads only what a test sets.
fn fake(args: &[&str], dir: &Path) -> Command {
    let mut command = Command::new("node");
    command
        .arg(fake_cli())
        .args(args)
        .env("CLAUDE_CONFIG_DIR", dir)
        .env_remove("FAKE_CLAUDE_LOGIN")
        .env_remove("FAKE_CLAUDE_LOGIN_EMAIL");
    command
}

/// `node <the fake> auth login --claudeai` in `dir`.
fn login(dir: &Path) -> Command {
    fake(&["auth", "login", "--claudeai"], dir)
}

/// Runs `command` to its end; a missing `node` fails the test by name.
fn run(mut command: Command) -> Output {
    command
        .output()
        .expect("node runs the fake CLI (node must be on PATH)")
}

/// The run's stderr, for a failure message.
fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The run's stdout, for the session path's banner.
fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A private scratch folder for `test`, emptied first.
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("agent-claude-fake-login")
        .join(test);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
    }
    std::fs::create_dir_all(&dir).expect("the test's folder can be made");
    dir
}

/// The JSON in `path`.
fn json(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).expect("the login's file can be read");
    serde_json::from_str(&text).expect("the login's file is JSON")
}

/// Runs a successful login for `email` into `root/<name>`, and returns that
/// folder.
fn login_as(root: &Path, name: &str, email: &str) -> PathBuf {
    let dir = root.join(name);
    let mut command = login(&dir);
    command.env("FAKE_CLAUDE_LOGIN_EMAIL", email);
    let out = run(command);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    dir
}

/// The account uuid a login in `dir` names.
fn account_uuid(dir: &Path) -> String {
    json(&dir.join(".claude.json"))["oauthAccount"]["accountUuid"]
        .as_str()
        .expect("the account names an account uuid")
        .to_owned()
}

#[test]
fn fake_login_writes_a_capturable_login() {
    let dir = scratch("writes-a-capturable-login");

    let out = run(login(&dir));

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let credentials = json(&dir.join(".credentials.json"));
    let refresh = credentials["claudeAiOauth"]["refreshToken"]
        .as_str()
        .expect("the credential holds a refresh token");
    assert!(!refresh.is_empty(), "the refresh token is empty");
    let account = json(&dir.join(".claude.json"));
    assert_eq!(
        account["oauthAccount"]["emailAddress"].as_str(),
        Some("login@example.com")
    );
}

#[test]
fn fake_login_identity_follows_the_email() {
    let root = scratch("identity-follows-the-email");

    let first = account_uuid(&login_as(&root, "a1", "a@example.com"));
    let other = account_uuid(&login_as(&root, "b1", "b@example.com"));
    let again = account_uuid(&login_as(&root, "a2", "a@example.com"));

    assert_ne!(first, other, "two emails share an account uuid");
    assert_eq!(first, again, "the same email changed its account uuid");
}

#[test]
fn fake_login_fail_writes_nothing() {
    let dir = scratch("fail-writes-nothing");
    let mut command = login(&dir);
    command.env("FAKE_CLAUDE_LOGIN", "fail");

    let out = run(command);

    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        !dir.join(".credentials.json").exists(),
        "the failed login wrote a credential"
    );
    assert!(
        !dir.join(".claude.json").exists(),
        "the failed login wrote an account"
    );
}

#[test]
fn fake_login_rejects_an_unknown_mode() {
    let dir = scratch("rejects-an-unknown-mode");
    let mut command = login(&dir);
    command.env("FAKE_CLAUDE_LOGIN", "bogus");

    let out = run(command);

    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        !dir.join(".credentials.json").exists(),
        "the rejected login wrote a credential"
    );
    assert!(
        !dir.join(".claude.json").exists(),
        "the rejected login wrote an account"
    );
}

#[test]
fn fake_login_keeps_the_other_keys_of_claude_json() {
    let dir = scratch("keeps-the-other-keys-of-claude-json");
    std::fs::write(
        dir.join(".claude.json"),
        "{\"hasCompletedOnboarding\": true}\n",
    )
    .expect("the seeded account file can be written");

    let out = run(login(&dir));

    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let account = json(&dir.join(".claude.json"));
    assert_eq!(account["hasCompletedOnboarding"], serde_json::json!(true));
    assert_eq!(
        account["oauthAccount"]["emailAddress"].as_str(),
        Some("login@example.com")
    );
}

#[test]
fn only_auth_login_enters_the_login_mode() {
    let dir = scratch("only-auth-login-enters-the-login-mode");

    // The session path prints its ready banner and exits on the closed stdin.
    for args in [
        &["auth"][..],
        &["auth", "status"][..],
        &["--resume", "x", "auth", "login"][..],
    ] {
        let out = run(fake(args, &dir));
        assert!(out.status.success(), "{args:?} stderr: {}", stderr(&out));
        assert!(
            stdout(&out).contains("[fake-claude] ready"),
            "{args:?} left the session path: {}",
            stdout(&out)
        );
    }
    for args in [&["auth", "login"][..], &["auth", "login", "--claudeai"][..]] {
        let out = run(fake(args, &dir));
        assert!(out.status.success(), "{args:?} stderr: {}", stderr(&out));
        assert!(
            !stdout(&out).contains("[fake-claude] ready"),
            "{args:?} took the session path"
        );
    }
}

#[test]
fn fake_login_needs_a_config_dir() {
    let mut command = Command::new("node");
    command
        .arg(fake_cli())
        .args(["auth", "login"])
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("FAKE_CLAUDE_LOGIN");

    let out = run(command);

    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
}
