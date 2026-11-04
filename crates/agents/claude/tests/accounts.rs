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
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent_api::{AccountKindRow, AccountsConfig, Agent, UsageStatus};
use agent_claude::accounts::{BODY_CAP, ProfileClient, RefreshClient, UsageClient};
use claude_accounts::{
    AccessToken, CredentialBlob, ProfileTransport, RefreshError, TransportError, UsageTransport,
    refresh_grant,
};
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
    dir
}

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

fn service(root: PathBuf) -> Box<dyn agent_api::AccountsService> {
    Agent::accounts(&agent_claude::CLAUDE, &AccountsConfig::new(root))
        .expect("the plugin keeps accounts")
}

#[test]
fn accounts_service_snapshot_of_empty_store_is_empty() {
    let dir = scratch("accounts_service_snapshot_of_empty_store_is_empty");

    let snapshot = service(dir.join("accounts"))
        .snapshot()
        .expect("a missing store is an empty snapshot");

    assert!(snapshot.accounts.is_empty(), "{:?}", snapshot.accounts);
    assert_eq!(snapshot.auto, None);
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
