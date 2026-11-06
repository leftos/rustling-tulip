//! A local stand-in for the account service's profile endpoint, for the live
//! login specs: a daemon built with `e2e-loopback-peers` and given its origin
//! in `RUSTLING_TULIP_ACCOUNTS_BASE_URL` verifies a captured login here
//! instead of at the agent vendor's server.
//!
//! The identity it answers is the one the fake CLI's `auth login` writes for
//! an email (`crates/agents/claude/fake/README.md`): the email's lowercase
//! hex SHA-256 gives the access token `fake-access-<first 12 digits>`, the
//! account uuid from digits 0..32 and the organization uuid from 32..64, each
//! grouped 8-4-4-4-12.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::json;
use sha2::{Digest as _, Sha256};

/// The path the account service reads a login's profile from.
const PROFILE_PATH: &str = "/api/oauth/profile";

/// How long one connection may stall a read or a write before the server
/// gives up on it.
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// A server on `127.0.0.1` answering the profile endpoint for a fixed set of
/// emails, on a thread of its own until it is dropped.
pub struct ProfileServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl ProfileServer {
    /// Starts a server answering `GET /api/oauth/profile` for the first of
    /// `emails` whose fake access token is the request's bearer token, or for
    /// the only email when there is one; every other request is a 404.
    pub fn start(emails: &[&str]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port can be bound");
        let addr = listener.local_addr().expect("the listener has an address");
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let emails: Vec<String> = emails.iter().map(|email| (*email).to_owned()).collect();
        let thread = {
            let stop = Arc::clone(&stop);
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || serve(&listener, &stop, &requests, &emails))
        };
        Self {
            addr,
            stop,
            requests,
            thread: Some(thread),
        }
    }

    /// `http://127.0.0.1:<port>`, the value for
    /// `RUSTLING_TULIP_ACCOUNTS_BASE_URL`.
    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    /// Every request served so far, oldest first: its request line, a
    /// newline, and its `authorization` header's value (empty when it had
    /// none).
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request log lock").clone()
    }
}

impl Drop for ProfileServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the blocked `accept`, which then sees the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The account uuid and organization uuid the fake CLI's login writes for
/// `email`.
pub fn fake_login_uuids(email: &str) -> (String, String) {
    let hex = sha256_hex(email);
    (uuid_of(&hex[..32]), uuid_of(&hex[32..64]))
}

/// The access token the fake CLI's login writes for `email`.
pub fn fake_login_token(email: &str) -> String {
    format!("fake-access-{}", &sha256_hex(email)[..12])
}

/// The lowercase hex SHA-256 of `text`.
fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// 32 hex digits in uuid shape, grouped 8-4-4-4-12.
fn uuid_of(hex: &str) -> String {
    let mut parts = Vec::new();
    let mut at = 0;
    for length in [8, 4, 4, 4, 12] {
        parts.push(&hex[at..at + length]);
        at += length;
    }
    parts.join("-")
}

/// Answers each connection in turn until `stop` is set.
fn serve(
    listener: &TcpListener,
    stop: &AtomicBool,
    requests: &Mutex<Vec<String>>,
    emails: &[String],
) {
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(mut stream) = stream else { continue };
        // A client that connects and stalls ends its own connection with a
        // read error, so the thread keeps accepting and `Drop` is answered.
        let _ = stream.set_read_timeout(Some(STREAM_TIMEOUT));
        let _ = stream.set_write_timeout(Some(STREAM_TIMEOUT));
        let request = read_request(&mut stream);
        let line = request.lines().next().unwrap_or_default().to_owned();
        let authorization = request
            .lines()
            .take_while(|header| !header.is_empty())
            .find_map(|header| {
                let (name, value) = header.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().to_owned())
            })
            .unwrap_or_default();
        let reply = answer(&line, &authorization, emails);
        requests
            .lock()
            .expect("request log lock")
            .push(format!("{line}\n{authorization}"));
        let _ = stream.write_all(&reply);
        let _ = stream.flush();
    }
}

/// The bytes that answer the request whose request line is `line`.
fn answer(line: &str, authorization: &str, emails: &[String]) -> Vec<u8> {
    let mut words = line.split_whitespace();
    let is_profile = words.next() == Some("GET")
        && words
            .next()
            .is_some_and(|target| target.split('?').next() == Some(PROFILE_PATH));
    let token = authorization.strip_prefix("Bearer ").unwrap_or_default();
    let email = emails
        .iter()
        .find(|email| fake_login_token(email) == token)
        .or(match emails {
            [only] => Some(only),
            _ => None,
        });
    match (is_profile, email) {
        (true, Some(email)) => {
            let (account, organization) = fake_login_uuids(email);
            let body = json!({
                "account": { "uuid": account, "email": email },
                "organization": { "uuid": organization },
            })
            .to_string();
            http(
                "200 OK",
                &[("Content-Type", "application/json")],
                body.as_bytes(),
            )
        }
        _ => http("404 Not Found", &[], b""),
    }
}

/// An HTTP/1.1 answer with `status`, the extra `headers` and `body`.
fn http(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
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
    bytes
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
