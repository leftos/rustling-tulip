//! The plugin's account service over a store on disk, and its three HTTP
//! transports against a local server on 127.0.0.1 reached through the
//! base-URL override.

#![expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[cfg(all(windows, target_pointer_width = "64"))]
use agent_api::accounts::EngineTick;
use agent_api::{
    AccountKindRow, AccountsConfig, AccountsFailure, AccountsFailureKind, AccountsService, Agent,
    AutoSwitchRules, FleetWindowRow, PollScope, UsageStatus,
};
use agent_claude::accounts::{BODY_CAP, ClaudeAccounts, ProfileClient, RefreshClient, UsageClient};
use claude_accounts::{
    AccessToken, AccountLock, AccountsRoot, ClaudePaths, CredentialBlob, ProfileTransport,
    RefreshError, Store, TransportError, UsageTransport, refresh_grant,
};
use protocol::AutoSwitchStrategy;
use serde_json::json;

/// What the local server does with each request it reads.
#[derive(Clone)]
enum Reply {
    /// Writes these bytes and closes the connection.
    Bytes(Vec<u8>),
    /// Never answers.
    Hang,
}

/// An HTTP/1.1 answer with `status`, the extra `headers` and `body`.
fn http(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    let mut text = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        for part in [*name, ": ", *value, "\r\n"] {
            text.push_str(part);
        }
    }
    text.push_str("\r\n");
    let mut bytes = text.into_bytes();
    bytes.extend_from_slice(body);
    Reply::Bytes(bytes)
}

/// A server on 127.0.0.1 answering every request with `reply`: its origin,
/// and each request it read (head and body, as text), sent before it answers.
fn serve(reply: Reply) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port can be bound");
    let origin = format!(
        "http://{}",
        listener.local_addr().expect("the listener has an address")
    );
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let request = read_request(&mut stream);
            if tx.send(request).is_err() {
                return;
            }
            match &reply {
                Reply::Bytes(bytes) => {
                    let _ = stream.write_all(bytes);
                    let _ = stream.flush();
                }
                Reply::Hang => std::thread::sleep(Duration::from_secs(30)),
            }
        }
    });
    (origin, rx)
}

/// One request's head and body as text, the body read to its
/// `Content-Length`.
fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let head_end = bytes.windows(4).position(|w| w == b"\r\n\r\n");
        if let Some(end) = head_end {
            let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                break;
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn token() -> AccessToken {
    AccessToken::new("tok-1".to_owned())
}

/// The one request the server read.
fn the_request(rx: &mpsc::Receiver<String>) -> String {
    rx.recv_timeout(Duration::from_secs(5))
        .expect("the server read a request")
}

#[test]
fn usage_transport_reads_body_and_status() {
    let (origin, rx) = serve(http("200 OK", &[], br#"{"five_hour":{"utilization":12}}"#));

    let usage = UsageClient::new(Some(&origin))
        .get_usage(&token())
        .expect("a 200 is an answer");

    assert_eq!(usage.status, 200);
    assert_eq!(usage.body, json!({"five_hour": {"utilization": 12}}));
    assert_eq!(usage.retry_after, None);
    let request = the_request(&rx);
    let lower = request.to_ascii_lowercase();
    assert!(request.starts_with("GET /api/oauth/usage "), "{request}");
    assert!(lower.contains("authorization: bearer tok-1"), "{request}");
    assert!(
        lower.contains("anthropic-beta: oauth-2025-04-20"),
        "{request}"
    );
}

#[test]
fn usage_transport_returns_ok_on_429_with_retry_after() {
    let (origin, _rx) = serve(http("429 Too Many Requests", &[("Retry-After", "30")], b""));

    let usage = UsageClient::new(Some(&origin))
        .get_usage(&token())
        .expect("a 429 is an answer, not an error");

    assert_eq!(usage.status, 429);
    assert_eq!(usage.body, serde_json::Value::Null);
    assert_eq!(usage.retry_after, Some(30.0));
}

#[test]
fn refresh_transport_returns_ok_on_400_invalid_grant() {
    let (origin, rx) = serve(http(
        "400 Bad Request",
        &[("Content-Type", "application/json")],
        br#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#,
    ));
    let blob = CredentialBlob::new(
        json!({"claudeAiOauth": {"accessToken": "a-1", "refreshToken": "r-1", "expiresAt": 1}})
            .to_string(),
    );

    let verdict = refresh_grant(
        &RefreshClient::new(Some(&origin)),
        &blob,
        jiff::Timestamp::now(),
    );

    // A transport error would be `Transient`: `InvalidGrant` means the 400's
    // body reached the classifier.
    assert!(
        matches!(verdict, Err(RefreshError::InvalidGrant)),
        "{verdict:?}"
    );
    let request = the_request(&rx);
    let lower = request.to_ascii_lowercase();
    assert!(request.starts_with("POST /v1/oauth/token "), "{request}");
    assert!(
        lower.contains("content-type: application/json"),
        "{request}"
    );
    assert!(request.contains(r#""refresh_token":"r-1""#), "{request}");
}

#[test]
fn profile_transport_sends_user_agent() {
    let (origin, rx) = serve(http(
        "200 OK",
        &[],
        br#"{"account":{"uuid":"u-1","email":"a@example.com"}}"#,
    ));

    let profile = ProfileClient::new(Some(&origin))
        .get_profile(&token())
        .expect("a 200 is an answer");

    assert_eq!(profile.status, 200);
    assert_eq!(profile.body["account"]["uuid"], "u-1");
    let request = the_request(&rx);
    let lower = request.to_ascii_lowercase();
    assert!(request.starts_with("GET /api/oauth/profile "), "{request}");
    assert!(
        lower.contains("user-agent: rustling-tulip/1.0"),
        "{request}"
    );
    assert!(
        lower.contains("content-type: application/json"),
        "{request}"
    );
    assert!(lower.contains("authorization: bearer tok-1"), "{request}");
}

#[test]
fn transport_caps_body_at_64_kib() {
    let string_body = |len: usize| format!("\"{}\"", "x".repeat(len - 2)).into_bytes();
    let (under, _rx) = serve(http("200 OK", &[], &string_body(BODY_CAP)));
    let (over, _rx_over) = serve(http("200 OK", &[], &string_body(BODY_CAP + 1)));

    let read = UsageClient::new(Some(&under))
        .get_usage(&token())
        .expect("a body at the cap is read");
    let cut = UsageClient::new(Some(&over)).get_usage(&token());

    assert_eq!(read.body.as_str().map(str::len), Some(BODY_CAP - 2));
    assert_eq!(cut, Err(TransportError::MalformedBody));
}

#[test]
fn transport_does_not_follow_redirects() {
    let (origin, rx) = serve(http(
        "302 Found",
        &[("Location", "/api/oauth/elsewhere")],
        b"",
    ));

    let usage = UsageClient::new(Some(&origin))
        .get_usage(&token())
        .expect("a 302 is an answer");

    assert_eq!(usage.status, 302);
    let first = the_request(&rx);
    assert!(first.starts_with("GET /api/oauth/usage "), "{first}");
    assert!(rx.try_recv().is_err(), "the redirect was followed");
}

#[test]
fn transport_times_out() {
    let (origin, _rx) = serve(Reply::Hang);
    let started = Instant::now();

    let usage = UsageClient::new(Some(&origin)).get_usage(&token());

    assert_eq!(usage, Err(TransportError::Timeout));
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

/// A private scratch folder for `test`, emptied first.
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("agent-claude-accounts")
        .join(test);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("the test's old folder can be removed");
    }
    std::fs::create_dir_all(&dir).expect("the test's folder can be made");
    make_private(&dir);
    // A Unix temp folder can sit under a link (macOS's `/var` is `/private/var`), which the store refuses as a root's
    // parent: the scratch folder is named by its real path.
    if cfg!(unix) {
        std::fs::canonicalize(&dir).expect("the test's folder has a real path")
    } else {
        dir
    }
}

/// Off Windows makes the store at `root` owner-only, as the store makes its own folders and files: the folders a test
/// writes there are 0755 and its files 0644 under the usual umask, which the store refuses. On Windows the scratch
/// folder's DACL already covers them.
#[cfg(unix)]
fn restrict(root: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(metadata) = std::fs::symlink_metadata(root) else {
        return;
    };
    if metadata.is_dir() {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
            .expect("a store folder can be made private");
        for entry in std::fs::read_dir(root).expect("a store folder lists") {
            restrict(&entry.expect("a store entry can be read").path());
        }
    } else if metadata.is_file() {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o600))
            .expect("a store file can be made private");
    }
}

#[cfg(not(unix))]
fn restrict(_root: &Path) {}

#[cfg(windows)]
fn make_private(dir: &Path) {
    let whoami = std::process::Command::new("whoami")
        .output()
        .expect("whoami can be started");
    let user = String::from_utf8(whoami.stdout).expect("whoami prints text");
    let output = std::process::Command::new("icacls")
        .arg(dir)
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
fn make_private(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .expect("the test's folder can be made private");
}

#[cfg(not(any(unix, windows)))]
fn make_private(_dir: &Path) {}

fn service(root: PathBuf) -> Arc<dyn AccountsService> {
    let home = root.with_file_name("claude");
    service_at(root, home, None)
}

/// The plugin's service over the store at `root`, with Claude Code's home at
/// `home` and its requests sent to `origin` when one is given.
fn service_at(root: PathBuf, home: PathBuf, origin: Option<&str>) -> Arc<dyn AccountsService> {
    restrict(&root);
    let mut cfg = AccountsConfig::new(root);
    cfg.agent_home = Some(home);
    cfg.base_url = origin.map(str::to_owned);
    Agent::accounts(&agent_claude::CLAUDE, &cfg).expect("the plugin keeps accounts")
}

/// A store at `dir/accounts` holding account 1, active, with a credential
/// that has not expired and no usage read yet.
fn store_with_one_account(dir: &Path) -> PathBuf {
    let root = write_store(dir, &[1], &[1], 1, &json!({}));
    let store = Store::open(AccountsRoot::new(root.clone())).expect("the scratch store opens");
    let lock =
        AccountLock::acquire(&AccountsRoot::new(root.clone())).expect("the account lock is free");
    let credential = json!({"claudeAiOauth": {
        "accessToken": "fake-access-1",
        "refreshToken": "fake-refresh-1",
        "expiresAt": 4_102_444_800_000_u64,
        "scopes": ["user:inference", "user:profile"],
        "subscriptionType": "max"
    }});
    store
        .write_credentials(&lock, 1, &CredentialBlob::new(credential.to_string()))
        .expect("the credential is written");
    root
}

/// `at` as an HTTP-date (IMF-fixdate).
fn http_date(at: jiff::Timestamp) -> String {
    at.strftime("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

#[test]
fn usage_transport_honours_an_http_date_retry_after() {
    let date = http_date(jiff::Timestamp::now() + jiff::SignedDuration::from_secs(120));
    let (origin, _rx) = serve(http(
        "429 Too Many Requests",
        &[("Retry-After", &date)],
        b"",
    ));

    let usage = UsageClient::new(Some(&origin))
        .get_usage(&token())
        .expect("a 429 is an answer, not an error");

    assert_eq!(usage.status, 429);
    let secs = usage.retry_after.expect("the date is read as a wait");
    assert!(secs > 0.0 && secs <= 120.0, "{secs}");
}

/// A store at `dir/accounts` with `accounts` as its `accounts.json`, `rows`
/// as its usage table's rows and `settings` as its `settings.json`.
fn write_raw_store(
    dir: &Path,
    accounts: &serde_json::Value,
    rows: &serde_json::Value,
    settings: &serde_json::Value,
) -> PathBuf {
    let root = dir.join("accounts");
    std::fs::create_dir_all(root.join("cache")).expect("the store folders can be made");
    std::fs::write(root.join("accounts.json"), accounts.to_string())
        .expect("accounts.json can be written");
    let usage = json!({"schemaVersion": 3, "accounts": rows});
    std::fs::write(root.join("cache").join("usage.json"), usage.to_string())
        .expect("usage.json can be written");
    std::fs::write(root.join("settings.json"), settings.to_string())
        .expect("settings.json can be written");
    root
}

#[test]
fn poll_with_no_planned_poll_keeps_the_floor() {
    let dir = scratch("poll_with_no_planned_poll_keeps_the_floor");
    let accounts = json!({
        "schemaVersion": 1,
        "order": [1],
        "active": 1,
        "accounts": {
            "1": {
                "email": "user1@example.com",
                "uuid": "00000000-0000-4000-8000-000000000001",
                "added": "2026-09-01T10:00:00Z",
                "kind": "api_key"
            }
        }
    });
    let settings = json!({"autoswitch": {"intervalSeconds": 15}});
    let root = write_raw_store(&dir, &accounts, &json!({}), &settings);
    let (origin, _rx) = serve(http("200 OK", &[], br#"{"five_hour":{"utilization":12}}"#));

    let report = service_at(root, dir.join("claude"), Some(&origin))
        .poll(PollScope::OnDemand)
        .expect("the poll runs");

    assert_eq!(report.snapshot.accounts.len(), 1, "{report:?}");
    let next = report
        .next_poll_in
        .expect("a store with an account polls again");
    assert!(next >= Duration::from_secs(60), "{next:?}");
}

#[test]
fn a_disabled_accounts_old_plan_does_not_set_the_next_poll() {
    let dir = scratch("a_disabled_accounts_old_plan_does_not_set_the_next_poll");
    let accounts = json!({
        "schemaVersion": 1,
        "order": [1],
        "active": 1,
        "accounts": {
            "1": {
                "email": "user1@example.com",
                "uuid": "00000000-0000-4000-8000-000000000001",
                "added": "2026-09-01T10:00:00Z",
                "disabled": true
            }
        }
    });
    let rows = json!({
        "1": {
            "email": "user1@example.com",
            "organizationUuid": "",
            "lastGood": {"five_hour": {"pct": 30.0}},
            "fetchedAt": 1_000_000_000,
            "nextPollAt": 1_000_000_060
        }
    });
    let settings = json!({"autoswitch": {"intervalSeconds": 600}});
    let root = write_raw_store(&dir, &accounts, &rows, &settings);
    let (origin, _rx) = serve(http("200 OK", &[], br#"{"five_hour":{"utilization":12}}"#));

    let report = service_at(root, dir.join("claude"), Some(&origin))
        .poll(PollScope::OnDemand)
        .expect("the poll runs");

    assert_eq!(report.snapshot.accounts.len(), 1, "{report:?}");
    assert_eq!(report.next_poll_in, Some(Duration::from_secs(600)));
}

#[test]
fn accounts_service_poll_of_empty_store_parks() {
    let dir = scratch("accounts_service_poll_of_empty_store_parks");

    let report = service_at(dir.join("accounts"), dir.join("claude"), None)
        .poll(PollScope::OnDemand)
        .expect("a missing store polls as empty");

    assert!(report.snapshot.accounts.is_empty(), "{report:?}");
    assert_eq!(report.next_poll_in, None);
}

#[test]
fn accounts_service_poll_fetches_usage() {
    let dir = scratch("accounts_service_poll_fetches_usage");
    let root = store_with_one_account(&dir);
    let (origin, rx) = serve(http("200 OK", &[], br#"{"five_hour":{"utilization":12}}"#));

    let report = service_at(root, dir.join("claude"), Some(&origin))
        .poll(PollScope::OnDemand)
        .expect("the poll runs");

    let request = the_request(&rx);
    assert!(request.starts_with("GET /api/oauth/usage "), "{request}");
    let numbers: Vec<u32> = report
        .snapshot
        .accounts
        .iter()
        .map(|row| row.number)
        .collect();
    assert_eq!(numbers, [1]);
    let five_hour = report.snapshot.accounts[0]
        .usage
        .five_hour
        .as_ref()
        .expect("the 5-hour window was read");
    assert!((five_hour.pct - 12.0).abs() < 1e-9, "{five_hour:?}");
    let next = report
        .next_poll_in
        .expect("a store with an account polls again");
    assert!(next >= Duration::from_secs(60), "{next:?}");
}

#[tokio::test]
async fn dropping_a_used_service_on_a_runtime_thread_does_not_panic() {
    let dir = scratch("dropping_a_used_service_on_a_runtime_thread_does_not_panic");
    let root = store_with_one_account(&dir);
    let (origin, _rx) = serve(http("200 OK", &[], br#"{"five_hour":{"utilization":12}}"#));
    let service = service_at(root, dir.join("claude"), Some(&origin));

    let polling = Arc::clone(&service);
    let polled = tokio::task::spawn_blocking(move || polling.poll(PollScope::OnDemand))
        .await
        .expect("the poll did not panic");
    assert!(polled.is_ok(), "{polled:?}");

    // The last handle, and with it the client the poll built, drops here, on
    // a runtime thread.
    drop(service);
}

#[test]
fn begin_shutdown_with_nothing_in_flight_is_idle() {
    let dir = scratch("begin_shutdown_with_nothing_in_flight_is_idle");
    let service = service_at(dir.join("accounts"), dir.join("claude"), None);

    assert!(service.begin_shutdown(Duration::ZERO));
}

#[test]
fn accounts_service_snapshot_of_empty_store_is_empty() {
    let dir = scratch("accounts_service_snapshot_of_empty_store_is_empty");

    let snapshot = service(dir.join("accounts"))
        .snapshot()
        .expect("a missing store is an empty snapshot");

    assert!(snapshot.accounts.is_empty(), "{:?}", snapshot.accounts);
    assert!(
        !snapshot.auto.as_ref().is_some_and(|auto| auto.enabled),
        "auto-switch is not on: {:?}",
        snapshot.auto
    );
}

#[test]
fn accounts_service_snapshot_lists_accounts_in_order() {
    let dir = scratch("accounts_service_snapshot_lists_accounts_in_order");
    let root = dir.join("accounts");
    std::fs::create_dir_all(root.join("cache")).expect("the store folders can be made");
    let accounts = json!({
        "schemaVersion": 1,
        "nextNumber": 3,
        "order": [2, 1],
        "active": 1,
        "accounts": {
            "1": {
                "email": "alice@example.com",
                "uuid": "00000000-0000-4000-8000-000000000001",
                "orgUuid": "00000000-0000-4000-8000-0000000000a1",
                "orgName": "Example Org",
                "added": "2026-09-01T10:00:00Z",
                "alias": "work"
            },
            "2": {
                "email": "bob@example.com",
                "uuid": "00000000-0000-4000-8000-000000000002",
                "added": "2026-09-02T11:00:00Z",
                "disabled": true,
                "kind": "api_key"
            }
        }
    });
    std::fs::write(root.join("accounts.json"), accounts.to_string())
        .expect("accounts.json can be written");
    let usage = json!({
        "schemaVersion": 3,
        "accounts": {
            "1": {
                "email": "alice@example.com",
                "organizationUuid": "00000000-0000-4000-8000-0000000000a1",
                "lastGood": {
                    "five_hour": {"pct": 42.5, "resets_at": "2026-11-04T12:00:00Z"},
                    "seven_day": {"pct": 10.0}
                },
                "fetchedAt": 1_793_000_000,
                "lastError": "http-429"
            }
        }
    });
    std::fs::write(root.join("cache").join("usage.json"), usage.to_string())
        .expect("usage.json can be written");

    let snapshot = service(root).snapshot().expect("the store reads");

    let numbers: Vec<u32> = snapshot.accounts.iter().map(|row| row.number).collect();
    assert_eq!(numbers, [2, 1]);
    let bob = &snapshot.accounts[0];
    assert_eq!(bob.email, "bob@example.com");
    assert_eq!(bob.kind, AccountKindRow::ApiKey);
    assert!(bob.disabled && !bob.active);
    assert_eq!(bob.org_name, None);
    assert_eq!(bob.usage.status, UsageStatus::ApiKey);
    let alice = &snapshot.accounts[1];
    assert_eq!(alice.alias.as_deref(), Some("work"));
    assert_eq!(alice.org_name.as_deref(), Some("Example Org"));
    assert_eq!(alice.kind, AccountKindRow::Oauth);
    assert!(alice.active && !alice.disabled);
    assert_eq!(alice.usage.status, UsageStatus::Ok);
    let five_hour = alice.usage.five_hour.as_ref().expect("the 5-hour window");
    assert!((five_hour.pct - 42.5).abs() < f64::EPSILON);
    assert_eq!(
        five_hour.resets_at.map(|at| at.to_rfc3339()),
        Some("2026-11-04T12:00:00+00:00".to_owned())
    );
    assert_eq!(
        alice.usage.fetched_at.map(|at| at.timestamp()),
        Some(1_793_000_000)
    );
    assert_eq!(alice.usage.error.as_deref(), Some("http-429"));
}

/// A store at `dir/accounts` holding OAuth accounts `numbers` (account `n`
/// is `user<n>@example.com`), in rotation `order`, with `active` in use and
/// `rows` as its usage table's rows.
fn write_store(
    dir: &Path,
    numbers: &[u32],
    order: &[u32],
    active: u32,
    rows: &serde_json::Value,
) -> PathBuf {
    let root = dir.join("accounts");
    std::fs::create_dir_all(root.join("cache")).expect("the store folders can be made");
    let records: serde_json::Map<String, serde_json::Value> = numbers
        .iter()
        .map(|number| {
            (
                number.to_string(),
                json!({
                    "email": format!("user{number}@example.com"),
                    "uuid": format!("00000000-0000-4000-8000-00000000000{number}"),
                    "added": "2026-09-01T10:00:00Z"
                }),
            )
        })
        .collect();
    let accounts = json!({
        "schemaVersion": 1,
        "order": order,
        "active": active,
        "accounts": records
    });
    std::fs::write(root.join("accounts.json"), accounts.to_string())
        .expect("accounts.json can be written");
    let usage = json!({"schemaVersion": 3, "accounts": rows});
    std::fs::write(root.join("cache").join("usage.json"), usage.to_string())
        .expect("usage.json can be written");
    restrict(&root);
    root
}

#[test]
fn snapshot_lists_records_missing_from_the_order() {
    let dir = scratch("snapshot_lists_records_missing_from_the_order");
    let root = write_store(&dir, &[1, 2, 3, 4], &[3, 1], 4, &json!({}));

    let snapshot = service(root).snapshot().expect("the store reads");

    let numbers: Vec<u32> = snapshot.accounts.iter().map(|row| row.number).collect();
    assert_eq!(numbers, [3, 1, 2, 4]);
    let active: Vec<u32> = snapshot
        .accounts
        .iter()
        .filter(|row| row.active)
        .map(|row| row.number)
        .collect();
    assert_eq!(active, [4]);
}

#[test]
fn snapshot_status_follows_the_latest_failed_poll() {
    let dir = scratch("snapshot_status_follows_the_latest_failed_poll");
    let row = |number: u32, failures: u32, error: Option<&str>| {
        json!({
            "email": format!("user{number}@example.com"),
            "organizationUuid": "",
            "lastGood": {"five_hour": {"pct": 30.0}},
            "fetchedAt": 1_793_000_000,
            "consecutiveFailures": failures,
            "lastError": error
        })
    };
    let rows = json!({
        "1": row(1, 2, Some("http-401")),
        "2": row(2, 1, Some("invalid_grant")),
        "3": row(3, 3, Some("timeout")),
        "4": row(4, 0, None),
        "5": row(5, 1, Some("refresh-failed")),
        "6": row(6, 1, Some("no_refresh_token"))
    });
    let root = write_store(&dir, &[1, 2, 3, 4, 5, 6], &[1, 2, 3, 4, 5, 6], 4, &rows);

    let snapshot = service(root).snapshot().expect("the store reads");

    let statuses: Vec<(u32, UsageStatus)> = snapshot
        .accounts
        .iter()
        .map(|row| (row.number, row.usage.status))
        .collect();
    assert_eq!(
        statuses,
        [
            (1, UsageStatus::TokenExpired),
            (2, UsageStatus::ReloginRequired),
            (3, UsageStatus::Unavailable),
            (4, UsageStatus::Ok),
            (5, UsageStatus::TokenExpired),
            (6, UsageStatus::ReloginRequired),
        ]
    );
    assert!(
        snapshot.accounts[0].usage.five_hour.is_some(),
        "the saved reading is still shown"
    );
}

/// The kind of the failure `result` must be.
fn failure_kind<T: std::fmt::Debug>(result: Result<T, AccountsFailure>) -> AccountsFailureKind {
    result.expect_err("the write is refused").kind
}

/// The organization every test login below is in.
const TEST_ORG: &str = "00000000-0000-4000-8000-0000000000a1";

/// Claude Code at `home` logged in as `user<number>@example.com` in
/// [`TEST_ORG`], with `fake-access-<number>` as its live credential when
/// `with_credential`.
fn log_in(home: &Path, number: u32, with_credential: bool) {
    std::fs::create_dir_all(home).expect("Claude Code's home can be made");
    let global = home.join(".claude.json");
    let oauth = json!({
        "emailAddress": format!("user{number}@example.com"),
        "organizationUuid": TEST_ORG,
        "accountUuid": format!("00000000-0000-4000-8000-00000000001{number}"),
        "organizationName": "Org"
    });
    std::fs::write(&global, json!({"oauthAccount": oauth}).to_string())
        .expect("the global config can be written");
    if with_credential {
        let credential = json!({"claudeAiOauth": {
            "accessToken": format!("fake-access-{number}"),
            "refreshToken": format!("fake-refresh-{number}"),
            "expiresAt": 4_102_444_800_000_u64,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max"
        }});
        let paths = ClaudePaths::new(home.to_path_buf(), global);
        std::fs::write(paths.credentials_file(), credential.to_string())
            .expect("the live credential can be written");
    }
}

/// A profile endpoint naming `user<number>@example.com` in [`TEST_ORG`], as
/// [`log_in`] logs in: its origin, and the requests it reads, which it
/// answers only while the receiver is kept.
fn profile_of(number: u32) -> (String, mpsc::Receiver<String>) {
    let body = json!({
        "account": {
            "uuid": format!("00000000-0000-4000-8000-00000000001{number}"),
            "email": format!("user{number}@example.com")
        },
        "organization": {"uuid": TEST_ORG}
    })
    .to_string();
    serve(http("200 OK", &[], body.as_bytes()))
}

/// A store holding accounts 1 and 2 in that order, 1 active, and the service
/// over it with Claude Code's home at `dir/claude` and its requests sent to a
/// local server.
fn two_accounts(dir: &Path) -> Arc<dyn AccountsService> {
    let root = write_store(dir, &[1, 2], &[1, 2], 1, &json!({}));
    service_at(root, dir.join("claude"), Some(&profile_of(1).0))
}

#[cfg(any(all(windows, target_pointer_width = "64"), target_os = "macos"))]
#[test]
fn set_alias_renames_and_reports_a_change() {
    let dir = scratch("set_alias_renames_and_reports_a_change");
    let service = two_accounts(&dir);

    let changed = service
        .set_alias(2, Some("work".to_owned()))
        .expect("the alias is set");

    assert!(changed);
    let snapshot = service.snapshot().expect("the store reads");
    let aliases: Vec<Option<&str>> = snapshot
        .accounts
        .iter()
        .map(|row| row.alias.as_deref())
        .collect();
    assert_eq!(aliases, [None, Some("work")]);
}

#[cfg(any(all(windows, target_pointer_width = "64"), target_os = "macos"))]
#[test]
fn set_alias_unchanged_reports_no_change() {
    let dir = scratch("set_alias_unchanged_reports_no_change");
    let service = two_accounts(&dir);
    service
        .set_alias(1, Some("home".to_owned()))
        .expect("the alias is set");

    let changed = service
        .set_alias(1, Some("home".to_owned()))
        .expect("the same alias is set again");

    assert!(!changed);
}

#[cfg(any(all(windows, target_pointer_width = "64"), target_os = "macos"))]
#[test]
fn set_order_with_a_wrong_permutation_is_refused() {
    let dir = scratch("set_order_with_a_wrong_permutation_is_refused");
    let service = two_accounts(&dir);

    let failure = service
        .set_order(vec![1, 3])
        .expect_err("the order names an account the store does not hold");

    assert_eq!(failure.kind, AccountsFailureKind::Refused, "{failure:?}");
    assert_eq!(failure.code, "order_mismatch");
    let snapshot = service.snapshot().expect("the store reads");
    let numbers: Vec<u32> = snapshot.accounts.iter().map(|row| row.number).collect();
    assert_eq!(numbers, [1, 2]);
}

#[cfg(any(all(windows, target_pointer_width = "64"), target_os = "macos"))]
#[test]
fn remove_unknown_account_is_unknown() {
    let dir = scratch("remove_unknown_account_is_unknown");
    let service = two_accounts(&dir);

    let failure = service
        .remove_account(7)
        .expect_err("no account has number 7");

    assert_eq!(
        failure.kind,
        AccountsFailureKind::UnknownAccount,
        "{failure:?}"
    );
    assert_eq!(failure.code, "unknown_account");
}

#[cfg(any(all(windows, target_pointer_width = "64"), target_os = "macos"))]
#[test]
fn remove_the_active_account_is_refused() {
    let dir = scratch("remove_the_active_account_is_refused");
    let service = two_accounts(&dir);

    let failure = service
        .remove_account(1)
        .expect_err("the active account is not removed");

    assert_eq!(failure.kind, AccountsFailureKind::Refused, "{failure:?}");
    assert_eq!(failure.code, "active_account");
    assert!(
        failure.message.contains("switch to another account first"),
        "{failure:?}"
    );
    let snapshot = service.snapshot().expect("the store reads");
    assert_eq!(snapshot.accounts.len(), 2);
}

#[cfg(any(all(windows, target_pointer_width = "64"), target_os = "macos"))]
#[test]
fn writes_after_begin_shutdown_are_draining() {
    let dir = scratch("writes_after_begin_shutdown_are_draining");
    let service = two_accounts(&dir);
    assert!(service.begin_shutdown(Duration::ZERO));

    assert_eq!(
        failure_kind(service.set_alias(2, Some("work".to_owned()))),
        AccountsFailureKind::Draining
    );
    assert_eq!(
        failure_kind(service.set_order(vec![2, 1])),
        AccountsFailureKind::Draining
    );
    assert_eq!(
        failure_kind(service.remove_account(2)),
        AccountsFailureKind::Draining
    );
    let snapshot = service.snapshot().expect("the store reads");
    let rows: Vec<(u32, Option<&str>)> = snapshot
        .accounts
        .iter()
        .map(|row| (row.number, row.alias.as_deref()))
        .collect();
    assert_eq!(rows, [(1, None), (2, None)]);
}

/// A store holding accounts 1 and 2, 1 active, and Claude Code logged in as
/// account 1 with no live credential, so a switch asks no oracle.
fn logged_in_as_one(dir: &Path) -> Arc<dyn AccountsService> {
    let root = write_roster_of_two(dir);
    log_in(&dir.join("claude"), 1, false);
    service_at(root, dir.join("claude"), Some(&profile_of(1).0))
}

/// A store at `dir/accounts` holding accounts 1 and 2 of [`TEST_ORG`], 1
/// active, with no stored credential.
fn write_roster_of_two(dir: &Path) -> PathBuf {
    let root = dir.join("accounts");
    std::fs::create_dir_all(root.join("cache")).expect("the store folders can be made");
    let record = |number: u32| {
        json!({
            "email": format!("user{number}@example.com"),
            "uuid": format!("00000000-0000-4000-8000-00000000001{number}"),
            "orgUuid": TEST_ORG,
            "orgName": "Org",
            "added": "2026-09-01T10:00:00Z"
        })
    };
    let accounts = json!({
        "schemaVersion": 1,
        "nextNumber": 3,
        "order": [1, 2],
        "active": 1,
        "accounts": {"1": record(1), "2": record(2)}
    });
    std::fs::write(root.join("accounts.json"), accounts.to_string())
        .expect("accounts.json can be written");
    root
}

#[test]
fn switch_to_the_active_account_is_a_no_op() {
    let dir = scratch("switch_to_the_active_account_is_a_no_op");
    let service = logged_in_as_one(&dir);
    let global = dir.join("claude").join(".claude.json");
    let before = std::fs::read(&global).expect("the global config reads");

    let report = service
        .switch_to(1)
        .expect("a switch to the live login is a no-op");

    assert!(!report.switched, "{report:?}");
    assert_eq!(report.to.number, Some(1));
    assert_eq!(report.to.email, "user1@example.com");
    assert_eq!(
        std::fs::read(&global).expect("the global config reads"),
        before
    );
}

#[test]
fn switch_to_an_unknown_number_is_unknown_account() {
    let dir = scratch("switch_to_an_unknown_number_is_unknown_account");
    let service = logged_in_as_one(&dir);

    let failure = service.switch_to(5).expect_err("no account has number 5");

    assert_eq!(
        failure.kind,
        AccountsFailureKind::UnknownAccount,
        "{failure:?}"
    );
    assert_eq!(failure.code, "unknown_account");
}

#[test]
fn an_unknown_switch_sends_no_request() {
    let dir = scratch("an_unknown_switch_sends_no_request");
    let root = write_roster_of_two(&dir);
    // Logged in as account 1 with a live credential its (missing) backup does
    // not hold: a probe of the outgoing login would ask the profile endpoint.
    log_in(&dir.join("claude"), 1, true);
    let (origin, requests) = profile_of(1);
    let service = service_at(root, dir.join("claude"), Some(&origin));

    let failure = service.switch_to(5).expect_err("no account has number 5");

    assert_eq!(
        failure.kind,
        AccountsFailureKind::UnknownAccount,
        "{failure:?}"
    );
    let sent = requests.try_recv();
    assert!(sent.is_err(), "the switch sent a request: {sent:?}");
}

/// While the test holds the account lock, an alias write waits on it; a
/// shutdown begun meanwhile must not report idle while that write can still
/// land. Either the write landed before the shutdown returned, or it was
/// refused as draining and never lands.
#[cfg(any(all(windows, target_pointer_width = "64"), target_os = "macos"))]
#[test]
fn begin_shutdown_waits_for_an_alias_write_in_flight() {
    let dir = scratch("begin_shutdown_waits_for_an_alias_write_in_flight");
    let root = write_store(&dir, &[1, 2], &[1, 2], 1, &json!({}));
    let lock =
        AccountLock::acquire(&AccountsRoot::new(root.clone())).expect("the account lock is free");
    let service = service_at(root, dir.join("claude"), Some(&profile_of(1).0));
    let alias_of_two = |service: &dyn AccountsService| {
        service
            .snapshot()
            .expect("the store reads")
            .accounts
            .into_iter()
            .find(|row| row.number == 2)
            .and_then(|row| row.alias)
    };

    let writer = Arc::clone(&service);
    let (written_tx, written_rx) = mpsc::channel();
    let write = std::thread::spawn(move || {
        let _ = written_tx.send(writer.set_alias(2, Some("work".to_owned())));
    });
    // Lets the writer reach its lock wait; the assertions below hold whatever
    // the interleaving.
    std::thread::sleep(Duration::from_millis(300));
    let stopper = Arc::clone(&service);
    let (stopped_tx, stopped_rx) = mpsc::channel();
    let stop = std::thread::spawn(move || {
        let idle = stopper.begin_shutdown(Duration::from_secs(30));
        let _ = stopped_tx.send((idle, alias_of_two(stopper.as_ref())));
    });
    let (idle, alias_at_stop) = stopped_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the shutdown returns");
    drop(lock);
    let written = written_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the write returns");
    write.join().expect("the writer did not panic");
    stop.join().expect("the shutdown did not panic");

    assert!(idle, "the shutdown did not report idle");
    match written {
        Ok(changed) => {
            assert!(changed);
            assert_eq!(
                alias_at_stop.as_deref(),
                Some("work"),
                "the shutdown reported idle before the write landed"
            );
        }
        Err(failure) => {
            assert_eq!(failure.kind, AccountsFailureKind::Draining, "{failure:?}");
            assert_eq!(alias_of_two(service.as_ref()), None);
        }
    }
}

#[cfg(any(all(windows, target_pointer_width = "64"), target_os = "macos"))]
#[test]
fn add_current_login_adds_the_live_login() {
    let dir = scratch("add_current_login_adds_the_live_login");
    let home = dir.join("claude");
    log_in(&home, 1, true);
    let (origin, _requests) = profile_of(1);
    let service = service_at(dir.join("accounts"), home, Some(&origin));

    let number = service
        .add_current_login()
        .expect("the live login is added");

    let snapshot = service.snapshot().expect("the store reads");
    let rows: Vec<(u32, &str, bool)> = snapshot
        .accounts
        .iter()
        .map(|row| (row.number, row.email.as_str(), row.active))
        .collect();
    assert_eq!(rows, [(number, "user1@example.com", true)]);
}

#[cfg(any(all(windows, target_pointer_width = "64"), target_os = "macos"))]
#[test]
fn switch_to_another_account_switches() {
    let dir = scratch("switch_to_another_account_switches");
    let root = dir.join("accounts");
    let home = dir.join("claude");
    log_in(&home, 1, true);
    let (first_origin, _first_requests) = profile_of(1);
    let first = service_at(root.clone(), home.clone(), Some(&first_origin))
        .add_current_login()
        .expect("login 1 is added");
    log_in(&home, 2, true);
    let (origin, _requests) = profile_of(2);
    let service = service_at(root, home.clone(), Some(&origin));
    let second = service.add_current_login().expect("login 2 is added");

    let report = service.switch_to(first).expect("the switch runs");

    assert!(report.switched, "{report:?}");
    let from = report.from.as_ref().expect("the outgoing login is named");
    assert_eq!(from.number, Some(second));
    assert_eq!(from.email, "user2@example.com");
    assert_eq!(report.to.number, Some(first));
    assert_eq!(report.to.email, "user1@example.com");
    let global =
        std::fs::read_to_string(home.join(".claude.json")).expect("the global config reads");
    assert!(global.contains("user1@example.com"), "{global}");
    let snapshot = service.snapshot().expect("the store reads");
    let active: Vec<u32> = snapshot
        .accounts
        .iter()
        .filter(|row| row.active)
        .map(|row| row.number)
        .collect();
    assert_eq!(active, [first]);
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn set_auto_switch_writes_the_settings_file() {
    let dir = scratch("set_auto_switch_writes_the_settings_file");
    let service = two_accounts(&dir);
    let file = dir.join("accounts").join("settings.json");

    let changed = service
        .set_auto_switch(Some(true), Some(85.0))
        .expect("auto-switch is set");

    assert!(changed);
    let written = claude_accounts::Settings::read(&file).expect("the settings file reads");
    assert!(written.autoswitch.enabled);
    assert!(
        (written.autoswitch.threshold - 85.0).abs() < 1e-9,
        "{written:?}"
    );
    let auto = service
        .snapshot()
        .expect("the store reads")
        .auto
        .expect("the snapshot carries the auto-switch state");
    assert!(auto.enabled);
    assert!((auto.threshold_pct - 85.0).abs() < 1e-9, "{auto:?}");

    let changed = service
        .set_auto_switch(None, Some(90.0))
        .expect("the threshold is set");
    assert!(changed);
    let written = claude_accounts::Settings::read(&file).expect("the settings file reads");
    assert!(
        written.autoswitch.enabled,
        "a threshold edit keeps the switch"
    );
    assert!(
        (written.autoswitch.threshold - 90.0).abs() < 1e-9,
        "{written:?}"
    );

    let unchanged = service
        .set_auto_switch(Some(true), None)
        .expect("the same switch is set again");
    assert!(!unchanged);

    assert_eq!(
        failure_kind(service.set_auto_switch(None, Some(10.0))),
        AccountsFailureKind::Refused
    );
    let written = claude_accounts::Settings::read(&file).expect("the settings file reads");
    assert!(
        (written.autoswitch.threshold - 90.0).abs() < 1e-9,
        "a refused threshold writes nothing: {written:?}"
    );
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn set_auto_switch_rules_writes_the_rules() {
    let dir = scratch("set_auto_switch_rules_writes_the_rules");
    let service = two_accounts(&dir);
    let file = dir.join("accounts").join("settings.json");

    let changed = service
        .set_auto_switch_rules(AutoSwitchRules {
            hysteresis_pct: Some(7.5),
            cooldown_secs: Some(600),
            strategy: Some(AutoSwitchStrategy::ConsumeFirst),
            rate_window_minutes: Some(120),
        })
        .expect("the rules are set");

    assert!(changed);
    let written = claude_accounts::Settings::read(&file).expect("the settings file reads");
    assert!(
        (written.autoswitch.hysteresis_pct - 7.5).abs() < 1e-9,
        "{written:?}"
    );
    assert!(
        (written.autoswitch.cooldown_seconds - 600.0).abs() < 1e-9,
        "{written:?}"
    );
    assert_eq!(
        written.autoswitch.strategy,
        claude_accounts::StrategyName::ConsumeFirst
    );
    assert_eq!(
        written.fleet.rate_window,
        claude_accounts::RateWindow::TwoHours
    );

    let auto = service
        .snapshot()
        .expect("the store reads")
        .auto
        .expect("the snapshot carries the auto-switch state");
    assert!((auto.hysteresis_pct - 7.5).abs() < 1e-9, "{auto:?}");
    assert_eq!(auto.cooldown_secs, 600);
    assert_eq!(auto.strategy, AutoSwitchStrategy::ConsumeFirst);
    assert_eq!(auto.rate_window_minutes, Some(120));
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn set_auto_switch_rules_refuses_an_off_list_forecast_window() {
    let dir = scratch("set_auto_switch_rules_refuses_an_off_list_forecast_window");
    let service = two_accounts(&dir);
    let file = dir.join("accounts").join("settings.json");

    assert_eq!(
        failure_kind(service.set_auto_switch_rules(AutoSwitchRules {
            rate_window_minutes: Some(45),
            ..AutoSwitchRules::default()
        })),
        AccountsFailureKind::Refused
    );
    let written = claude_accounts::Settings::read(&file).expect("the settings file reads");
    assert_eq!(
        written.fleet.rate_window,
        claude_accounts::RateWindow::default(),
        "a refused window writes nothing"
    );
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn set_auto_switch_rules_refuses_an_out_of_range_hysteresis() {
    let dir = scratch("set_auto_switch_rules_refuses_an_out_of_range_hysteresis");
    let service = two_accounts(&dir);
    let file = dir.join("accounts").join("settings.json");
    let default = claude_accounts::AutoSwitchSection::default().hysteresis_pct;

    assert_eq!(
        failure_kind(service.set_auto_switch_rules(AutoSwitchRules {
            hysteresis_pct: Some(60.0),
            ..AutoSwitchRules::default()
        })),
        AccountsFailureKind::Refused
    );
    let written = claude_accounts::Settings::read(&file).expect("the settings file reads");
    assert!(
        (written.autoswitch.hysteresis_pct - default).abs() < 1e-9,
        "a refused hysteresis writes nothing: {written:?}"
    );
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn set_auto_switch_rules_with_the_same_values_changes_nothing() {
    let dir = scratch("set_auto_switch_rules_with_the_same_values_changes_nothing");
    let service = two_accounts(&dir);
    let rules = AutoSwitchRules {
        hysteresis_pct: Some(7.5),
        cooldown_secs: Some(600),
        strategy: Some(AutoSwitchStrategy::ConsumeFirst),
        rate_window_minutes: Some(120),
    };

    assert!(
        service
            .set_auto_switch_rules(rules)
            .expect("the rules are set")
    );
    assert!(
        !service
            .set_auto_switch_rules(rules)
            .expect("the same rules are set again"),
        "a write with nothing to change reports no change"
    );
}

/// The two auto-switch writes hold one guard across their read-modify-write,
/// so a switch and a rules change started together both land; without it the
/// later write puts back the other's old field, and `enabled` can come back
/// true after the engine was retired.
#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn concurrent_auto_switch_and_rules_writes_keep_both_changes() {
    use std::sync::Barrier;

    for round in 0..20 {
        let dir = scratch(&format!("concurrent-auto-switch-and-rules-{round}"));
        let service = two_accounts(&dir);
        assert!(
            service
                .set_auto_switch(Some(true), None)
                .expect("auto-switch is turned on"),
            "round {round}: the switch starts on"
        );
        let barrier = Arc::new(Barrier::new(2));

        let switching = {
            let service = Arc::clone(&service);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                service.set_auto_switch(Some(false), None).map(|_| ())
            })
        };
        let ruling = {
            let service = Arc::clone(&service);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                service
                    .set_auto_switch_rules(AutoSwitchRules {
                        cooldown_secs: Some(600),
                        ..AutoSwitchRules::default()
                    })
                    .map(|_| ())
            })
        };
        switching
            .join()
            .expect("the switch thread did not panic")
            .expect("the switch writes");
        ruling
            .join()
            .expect("the rules thread did not panic")
            .expect("the rules write");

        let file = dir.join("accounts").join("settings.json");
        let written = claude_accounts::Settings::read(&file).expect("the settings file reads");
        assert!(
            !written.autoswitch.enabled,
            "round {round}: the switch was lost: {written:?}"
        );
        assert!(
            (written.autoswitch.cooldown_seconds - 600.0).abs() < 1e-9,
            "round {round}: the rules were lost: {written:?}"
        );
    }
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn the_snapshot_carries_auto_switch_off_by_default() {
    let dir = scratch("the_snapshot_carries_auto_switch_off_by_default");
    let defaults = claude_accounts::Settings::default().autoswitch;

    for snapshot in [
        service(dir.join("missing")).snapshot(),
        two_accounts(&dir).snapshot(),
    ] {
        let auto = snapshot
            .expect("the store reads")
            .auto
            .expect("the snapshot carries the auto-switch state");
        assert!(!auto.enabled, "{auto:?}");
        assert!(
            (auto.threshold_pct - defaults.threshold).abs() < 1e-9,
            "{auto:?}"
        );
        assert!(
            (auto.hysteresis_pct - defaults.hysteresis_pct).abs() < 1e-9,
            "{auto:?}"
        );
        assert_eq!(auto.paused_reason, None);
        assert!(!auto.warm_up_done);
        assert_eq!(auto.last_decision, None);
    }
}

/// The engine lease file of the store at `dir/accounts`.
#[cfg(all(windows, target_pointer_width = "64"))]
fn engine_lease(dir: &Path) -> PathBuf {
    AccountsRoot::new(dir.join("accounts")).engine_lease()
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn engine_tick_is_off_by_default_and_claims_no_lease() {
    let dir = scratch("engine_tick_is_off_by_default_and_claims_no_lease");
    let service = two_accounts(&dir);

    let ticked = service.engine_tick().expect("the tick answers");

    assert_eq!(ticked, EngineTick::Off);
    assert!(
        !engine_lease(&dir).exists(),
        "an engine that is off claims no lease"
    );
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn engine_tick_runs_when_on_and_releases_its_lease_when_turned_off() {
    let dir = scratch("engine_tick_runs_when_on_and_releases_its_lease_when_turned_off");
    let service = two_accounts(&dir);
    service
        .set_auto_switch(Some(true), None)
        .expect("auto-switch is turned on");

    let ticked = service.engine_tick().expect("the tick answers");

    assert!(matches!(ticked, EngineTick::Ran { .. }), "{ticked:?}");
    assert!(engine_lease(&dir).exists(), "the tick claimed the lease");
    let auto = service
        .snapshot()
        .expect("the store reads")
        .auto
        .expect("the snapshot carries the auto-switch state");
    assert!(auto.enabled, "{auto:?}");

    service
        .set_auto_switch(Some(false), None)
        .expect("auto-switch is turned off");
    let ticked = service.engine_tick().expect("the tick answers");

    assert_eq!(ticked, EngineTick::Off);
    assert!(
        !engine_lease(&dir).exists(),
        "turning auto-switch off released the lease"
    );
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn an_unreadable_settings_file_fails_the_tick_and_keeps_the_engine() {
    let dir = scratch("an_unreadable_settings_file_fails_the_tick_and_keeps_the_engine");
    let service = two_accounts(&dir);
    service
        .set_auto_switch(Some(true), None)
        .expect("auto-switch is turned on");
    let ticked = service.engine_tick().expect("the tick answers");
    assert!(matches!(ticked, EngineTick::Ran { .. }), "{ticked:?}");
    let settings = dir.join("accounts").join("settings.json");
    std::fs::write(
        &settings,
        json!({"autoswitch": {"enabled": true, "threshold": 10}}).to_string(),
    )
    .expect("settings.json can be written");

    let failed = service
        .engine_tick()
        .expect_err("an unreadable settings file fails the tick");

    assert!(
        failed
            .message
            .starts_with("the account settings could not be read"),
        "{failed}"
    );
    assert!(engine_lease(&dir).exists(), "the engine keeps its lease");
    std::fs::write(
        &settings,
        json!({"autoswitch": {"enabled": true}}).to_string(),
    )
    .expect("settings.json can be written");
    let ticked = service.engine_tick().expect("the tick answers");
    assert!(
        matches!(ticked, EngineTick::Ran { halted: None, .. }),
        "{ticked:?}"
    );
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn engine_tick_off_does_not_open_the_store() {
    let dir = scratch("engine_tick_off_does_not_open_the_store");
    let root = dir.join("accounts");
    std::fs::write(&root, "not a folder").expect("the file can be written");
    let service = service(root);
    assert!(service.snapshot().is_err(), "the store cannot be opened");

    let ticked = service.engine_tick().expect("an off tick opens no store");

    assert_eq!(ticked, EngineTick::Off);
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn begin_shutdown_releases_the_engine_lease() {
    let dir = scratch("begin_shutdown_releases_the_engine_lease");
    let service = two_accounts(&dir);
    service
        .set_auto_switch(Some(true), None)
        .expect("auto-switch is turned on");
    let ticked = service.engine_tick().expect("the tick answers");
    assert!(matches!(ticked, EngineTick::Ran { .. }), "{ticked:?}");
    assert!(engine_lease(&dir).exists(), "the tick claimed the lease");

    assert!(service.begin_shutdown(Duration::from_secs(30)), "idle");

    assert!(
        !engine_lease(&dir).exists(),
        "the shutdown released the lease"
    );
    assert_eq!(
        service.engine_tick().expect("the tick answers"),
        EngineTick::Off,
        "no tick runs once the drain has begun"
    );
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn an_unreadable_settings_file_pauses_auto_switch_with_its_reason() {
    let dir = scratch("an_unreadable_settings_file_pauses_auto_switch_with_its_reason");
    let service = two_accounts(&dir);
    std::fs::write(
        dir.join("accounts").join("settings.json"),
        json!({"autoswitch": {"enabled": true, "threshold": 10}}).to_string(),
    )
    .expect("settings.json can be written");

    let snapshot = service
        .snapshot()
        .expect("a bad settings file does not fail the snapshot");

    assert_eq!(snapshot.accounts.len(), 2, "{snapshot:?}");
    let auto = snapshot
        .auto
        .expect("the snapshot carries the auto-switch state");
    assert!(!auto.enabled, "the defaults stand in: {auto:?}");
    let reason = auto.paused_reason.expect("the state is paused");
    assert!(
        reason.starts_with(
            "the auto-switch settings could not be read: settings.json is not valid: autoswitch.threshold"
        ),
        "{reason}"
    );
}

/// Account `number`'s usage row, its 5-hour window at `pct` resetting at
/// `resets_at`, fetched at `fetched` (seconds since the epoch).
fn usage_row(number: u32, pct: f64, resets_at: &str, fetched: i64) -> serde_json::Value {
    json!({
        "email": format!("user{number}@example.com"),
        "organizationUuid": "",
        "lastGood": {"five_hour": {"pct": pct, "resets_at": resets_at}},
        "fetchedAt": fetched,
    })
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn the_snapshot_carries_a_fleet_verdict_with_accounts() {
    let dir = scratch("the_snapshot_carries_a_fleet_verdict_with_accounts");
    let now = jiff::Timestamp::now();
    let resets_at = now
        .checked_add(jiff::SignedDuration::from_hours(4))
        .expect("the test's time is in range")
        .to_string();
    let fetched = now.as_second() - 300;
    let rows = json!({
        "1": usage_row(1, 30.0, &resets_at, fetched),
        "2": usage_row(2, 20.0, &resets_at, fetched),
    });
    let root = write_store(&dir, &[1, 2], &[1, 2], 1, &rows);

    let snapshot = service(root).snapshot().expect("the store reads");
    let fleet = snapshot
        .auto
        .expect("the snapshot carries the auto-switch state")
        .fleet
        .expect("the snapshot carries the fleet verdict");

    assert_eq!(fleet.members, 2, "both accounts are in the pool: {fleet:?}");
    assert_eq!(
        fleet.accounts, 2,
        "both accounts are on the timeline: {fleet:?}"
    );
    assert_eq!(fleet.rate_window, FleetWindowRow::FiveHour, "{fleet:?}");
    assert!((fleet.remaining - 150.0).abs() < 1e-6, "{fleet:?}");
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn a_second_snapshot_without_a_new_fetch_adds_no_sample() {
    let dir = scratch("a_second_snapshot_without_a_new_fetch_adds_no_sample");
    let now = jiff::Timestamp::now();
    let resets_at = now
        .checked_add(jiff::SignedDuration::from_hours(4))
        .expect("the test's time is in range")
        .to_string();
    let rows = json!({"1": usage_row(1, 30.0, &resets_at, now.as_second() - 300)});
    let root = write_store(&dir, &[1], &[1], 1, &rows);
    let mut cfg = AccountsConfig::new(root);
    cfg.agent_home = Some(dir.join("claude"));
    let service = ClaudeAccounts::new(cfg);

    service.snapshot().expect("the store reads");
    let recorded = service.history_samples();
    assert_eq!(recorded, 1, "one counted account leaves one sample");
    service.snapshot().expect("the store reads");

    assert_eq!(
        service.history_samples(),
        recorded,
        "a repeated snapshot of the same fetch adds no sample"
    );
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn a_failed_poll_keeps_the_account_in_the_forecast_and_its_history() {
    let dir = scratch("a_failed_poll_keeps_the_account_in_the_forecast_and_its_history");
    let now = jiff::Timestamp::now();
    let resets_at = now
        .checked_add(jiff::SignedDuration::from_hours(4))
        .expect("the test's time is in range")
        .to_string();
    let fetched = now.as_second() - 300;
    let rows = json!({
        "1": usage_row(1, 30.0, &resets_at, fetched),
        "2": usage_row(2, 20.0, &resets_at, fetched),
    });
    let root = write_store(&dir, &[1, 2], &[1, 2], 1, &rows);
    let mut cfg = AccountsConfig::new(root.clone());
    cfg.agent_home = Some(dir.join("claude"));
    let service = ClaudeAccounts::new(cfg);

    let before = service
        .snapshot()
        .expect("the store reads")
        .auto
        .expect("the snapshot carries the auto-switch state")
        .fleet
        .expect("the snapshot carries the fleet verdict");
    assert_eq!(before.accounts, 2, "{before:?}");
    assert_eq!(service.history_samples(), 2);

    let mut failed = usage_row(2, 20.0, &resets_at, fetched);
    failed["consecutiveFailures"] = json!(1);
    failed["lastError"] = json!("http-429");
    std::fs::write(
        root.join("cache").join("usage.json"),
        json!({
            "schemaVersion": 3,
            "accounts": {
                "1": usage_row(1, 30.0, &resets_at, fetched),
                "2": failed,
            },
        })
        .to_string(),
    )
    .expect("usage.json can be written");
    let after = service
        .snapshot()
        .expect("the store reads")
        .auto
        .expect("the snapshot carries the auto-switch state")
        .fleet
        .expect("the snapshot carries the fleet verdict");

    assert_eq!(
        after.accounts, before.accounts,
        "a failed poll keeps the account on the timeline: {after:?}"
    );
    assert_eq!(
        service.history_samples(),
        2,
        "a failed poll keeps the account's samples"
    );
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[test]
fn the_fleet_rate_window_follows_the_setting() {
    let dir = scratch("the_fleet_rate_window_follows_the_setting");
    let now = jiff::Timestamp::now();
    let resets_at = now
        .checked_add(jiff::SignedDuration::from_hours(4))
        .expect("the test's time is in range")
        .to_string();
    let older = now.as_second() - 6_000;
    let newer = now.as_second() - 300;
    let root = write_store(
        &dir,
        &[1],
        &[1],
        1,
        &json!({"1": usage_row(1, 40.0, &resets_at, older)}),
    );
    let service = service(root.clone());
    service.snapshot().expect("the store reads");
    let rows = json!({"1": usage_row(1, 50.0, &resets_at, newer)});
    std::fs::write(
        root.join("cache").join("usage.json"),
        json!({"schemaVersion": 3, "accounts": rows}).to_string(),
    )
    .expect("usage.json can be written");
    let sixty = service
        .snapshot()
        .expect("the store reads")
        .auto
        .expect("the snapshot carries the auto-switch state")
        .fleet
        .expect("the snapshot carries the fleet verdict");

    let mut settings = claude_accounts::Settings::default();
    settings.fleet.rate_window = claude_accounts::RateWindow::TwoHours;
    claude_accounts::Settings::write(&AccountsRoot::new(root), &settings)
        .expect("the settings are written");
    let one_twenty = service
        .snapshot()
        .expect("the store reads")
        .auto
        .expect("the snapshot carries the auto-switch state")
        .fleet
        .expect("the snapshot carries the fleet verdict");

    assert!(
        (sixty.rate - 54.5).abs() < 1.0,
        "the 60-minute span keeps one 5-minute-old sample and reads the window's average: {sixty:?}"
    );
    assert!(
        (one_twenty.rate - 6.0).abs() < 1.0,
        "the 120-minute span reads the rise across both samples: {one_twenty:?}"
    );
}
