//! A local stand-in for the GitHub releases feed, for the live update specs:
//! a daemon given its base URL in `RUSTLING_TULIP_UPDATE_FEED` reads the
//! release list, the installer's `.sha256` and the installer from it.
//!
//! Its routes are update-core's test feed's (`crates/update-core/src/lib.rs`,
//! `release_routes`): one prerelease offering the Windows installer and its
//! `.sha256`.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::json;
use sha2::{Digest as _, Sha256};

/// The path the updater reads the release list from, query included.
const FEED_PATH: &str = "/repos/leftos/rustling-tulip/releases?per_page=100";

/// How long one connection may stall a read or a write before the server
/// gives up on it.
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// A server on `127.0.0.1` answering the releases feed for one release, on a
/// thread of its own until it is dropped.
pub struct UpdateFeed {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl UpdateFeed {
    /// Serves one prerelease, `v{version}`, offering the Windows installer
    /// `installer` and its `.sha256`.
    pub fn start(version: &str, installer: &[u8]) -> Self {
        Self::serve_release(version, installer, &sha256_hex(installer))
    }

    /// [`Self::start`], whose `.sha256` names a hash that is not the
    /// installer's, so a verifying download refuses it.
    pub fn start_with_wrong_hash(version: &str, installer: &[u8]) -> Self {
        let mut other = installer.to_vec();
        other.push(0);
        Self::serve_release(version, installer, &sha256_hex(&other))
    }

    /// Serves `v{version}` with `installer`, its `.sha256` holding `hash`.
    fn serve_release(version: &str, installer: &[u8], hash: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port can be bound");
        let addr = listener.local_addr().expect("the listener has an address");
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let routes = routes(
            &format!("http://127.0.0.1:{}", addr.port()),
            version,
            installer,
            hash,
        );
        let thread = {
            let stop = Arc::clone(&stop);
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || serve(&listener, &stop, &requests, &routes))
        };
        Self {
            addr,
            stop,
            requests,
            thread: Some(thread),
        }
    }

    /// `http://127.0.0.1:{port}`, the value for `RUSTLING_TULIP_UPDATE_FEED`.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    /// The path of every request served so far, in order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request log lock").clone()
    }
}

impl Drop for UpdateFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the blocked `accept`, which then sees the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Each path the feed answers, with its body.
type Routes = Vec<(String, Vec<u8>)>;

/// The release list, the `.sha256` (naming `hash`) and the installer of
/// `v{version}`, their download URLs under `base`.
fn routes(base: &str, version: &str, installer: &[u8], hash: &str) -> Routes {
    let name = format!("rustling-tulip-{version}-setup.exe");
    let release = json!([{
        "tag_name": format!("v{version}"),
        "draft": false,
        "prerelease": true,
        "body": "### Added\n\n- a test release",
        "assets": [
            {"name": format!("{name}.sha256"), "browser_download_url": format!("{base}/dl/{name}.sha256")},
            {"name": name, "browser_download_url": format!("{base}/dl/{name}")},
        ],
    }]);
    vec![
        (FEED_PATH.to_owned(), release.to_string().into_bytes()),
        (
            format!("/dl/{name}.sha256"),
            format!("{hash}  {name}\n").into_bytes(),
        ),
        (format!("/dl/{name}"), installer.to_vec()),
    ]
}

/// The lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Answers each connection in turn until `stop` is set.
fn serve(
    listener: &TcpListener,
    stop: &AtomicBool,
    requests: &Mutex<Vec<String>>,
    routes: &Routes,
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
        let head = read_head(&mut stream);
        let mut words = head.lines().next().unwrap_or_default().split_whitespace();
        let method = words.next().unwrap_or_default();
        let path = words.next().unwrap_or_default().to_owned();
        requests
            .lock()
            .expect("request log lock")
            .push(path.clone());
        let body = (method == "GET")
            .then(|| routes.iter().find(|(route, _)| *route == path))
            .flatten();
        let reply = match body {
            Some((_, body)) => http("200 OK", body),
            None => http("404 Not Found", b""),
        };
        let _ = stream.write_all(&reply);
        let _ = stream.flush();
    }
}

/// An HTTP/1.1 answer with `status` and `body`.
fn http(status: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// One request's head as text, read to its blank line.
fn read_head(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
