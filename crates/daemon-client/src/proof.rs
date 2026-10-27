//! The daemon proof from the client side: before a client sends the token in
//! `daemon.json` anywhere, reuses the daemon it names, or kills its pid, the
//! listener on that port proves it holds the token (`GET /proof`, answered
//! with `HMAC-SHA256(proof_key, challenge)`; `tracer_protocol::proof`).
//!
//! Also the raw `POST /shutdown` a retire sends on a connection opened before
//! the proof, so the bearer token reaches only the listener that proved itself.

use anyhow::{Context as _, anyhow, bail};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tracing::warn;

/// The most a response head may hold before the answer is refused.
const HEAD_LIMIT: usize = 8 * 1024;

/// What a listener did when asked to prove it holds a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    /// A `204` whose `x-rt-proof` answers the challenge under the token.
    Proven,
    /// An HTTP answer that is not a valid proof: another status, a missing
    /// or malformed header, a wrong HMAC, or an oversized head. Also a token
    /// that cannot be sent safely, without connecting.
    Unproven,
    /// The connection opened but no complete answer came within the budget.
    NoResponse,
    /// The connection could not be opened.
    Refused,
}

/// Asks the listener on `127.0.0.1:port` to prove it holds `token`, on a
/// connection of its own, giving the whole exchange `budget`.
pub(crate) async fn prove(port: u16, token: &str, budget: Duration) -> Proof {
    if !is_safe_token(token) {
        return Proof::Unproven;
    }
    let mut challenge = [0u8; 32];
    if let Err(err) = getrandom::fill(&mut challenge) {
        warn!(%err, port, "could not draw a proof challenge; treating the listener as unproven");
        return Proof::Unproven;
    }
    tokio::time::timeout(budget, exchange(port, token, &challenge))
        .await
        .unwrap_or(Proof::NoResponse)
}

/// Why [`connect_proven`] handed back no stream.
#[derive(Debug)]
pub enum ProvenConnectError {
    /// The connection the token would travel on could not be opened.
    Connect(anyhow::Error),
    /// The connection opened, but the listener did not prove it holds the
    /// token; nothing was sent on it.
    NotProven(Proof),
}

impl std::fmt::Display for ProvenConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(err) => write!(f, "could not open the connection: {err:#}"),
            Self::NotProven(proof) => {
                write!(
                    f,
                    "the listener did not prove it holds the token ({proof:?})"
                )
            }
        }
    }
}

impl std::error::Error for ProvenConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(err) => Some(err.as_ref()),
            Self::NotProven(_) => None,
        }
    }
}

/// Opens the loopback connection `token` will travel on, then asks the
/// listener on `127.0.0.1:port` to prove it holds `token` on a second
/// connection, and returns the first one only once the proof holds. The
/// daemon binds its port before writing `daemon.json` and holds it until it
/// stops, so a connection opened before a successful proof reached it.
///
/// Opening and proving each get `budget`.
///
/// # Errors
///
/// [`ProvenConnectError::Connect`] when the first connection cannot be
/// opened, [`ProvenConnectError::NotProven`] when the proof does not hold.
pub async fn connect_proven(
    port: u16,
    token: &str,
    budget: Duration,
) -> Result<TcpStream, ProvenConnectError> {
    let stream = connect(port, budget)
        .await
        .map_err(ProvenConnectError::Connect)?;
    match prove(port, token, budget).await {
        Proof::Proven => Ok(stream),
        proof => Err(ProvenConnectError::NotProven(proof)),
    }
}

/// Opens a connection to `127.0.0.1:port` within `budget`.
pub(crate) async fn connect(port: u16, budget: Duration) -> anyhow::Result<TcpStream> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    tokio::time::timeout(budget, TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow!("connecting to {addr} timed out"))?
        .with_context(|| format!("connecting to {addr}"))
}

/// Sends `POST /shutdown` with `token` as the bearer on `stream` and reads
/// the status line, within `budget`.
///
/// # Errors
///
/// Fails when the request cannot be sent, no status line comes back in time,
/// or the status is not a success.
pub(crate) async fn post_shutdown(
    mut stream: TcpStream,
    port: u16,
    token: &str,
    budget: Duration,
) -> anyhow::Result<()> {
    if !is_safe_token(token) {
        bail!("the daemon token holds a character that cannot go in a header");
    }
    let request = format!(
        "POST /shutdown HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n"
    );
    let exchange = async {
        stream
            .write_all(request.as_bytes())
            .await
            .context("sending the shutdown request")?;
        match read_head(&mut stream).await {
            Head::Complete(head) => Ok(head),
            Head::TooLarge => Err(anyhow!("the shutdown answer's head was too large")),
            Head::Missing => Err(anyhow!("no answer to the shutdown request")),
        }
    };
    let head = tokio::time::timeout(budget, exchange)
        .await
        .map_err(|_| anyhow!("no answer to the shutdown request in time"))??;
    let head = String::from_utf8_lossy(&head);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("the shutdown answer had no status code"))?;
    if !(200..300).contains(&status) {
        bail!("shutdown endpoint returned {status}");
    }
    Ok(())
}

/// Sends the proof request for `challenge` on a new connection and checks
/// the answer.
async fn exchange(port: u16, token: &str, challenge: &[u8; 32]) -> Proof {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return Proof::Refused;
    };
    let request = format!(
        "GET /proof HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         X-RT-Challenge: {challenge}\r\n\
         Connection: close\r\n\
         \r\n",
        challenge = tracer_protocol::to_lower_hex(challenge),
    );
    if stream.write_all(request.as_bytes()).await.is_err() {
        return Proof::NoResponse;
    }
    match read_head(&mut stream).await {
        Head::Complete(head) => verdict(&head, token, challenge),
        Head::TooLarge => Proof::Unproven,
        Head::Missing => Proof::NoResponse,
    }
}

/// Whether `head` proves `token` for `challenge`.
fn verdict(head: &[u8], token: &str, challenge: &[u8; 32]) -> Proof {
    let proof_key = tracer_protocol::daemon_proof_key(token);
    let proven = std::str::from_utf8(head)
        .ok()
        .and_then(tracer_protocol::parse_daemon_proof_head)
        .is_some_and(|proof| tracer_protocol::verify_daemon_proof(&proof_key, challenge, &proof));
    if proven {
        Proof::Proven
    } else {
        Proof::Unproven
    }
}

/// What came back as a response head.
enum Head {
    /// The head up to its blank line, without the blank line.
    Complete(Vec<u8>),
    /// The head ran past [`HEAD_LIMIT`] bytes, complete or not.
    TooLarge,
    /// A read error or a closed connection came first.
    Missing,
}

/// Reads a response head up to its `\r\n\r\n`. Bytes after it are read and
/// dropped: every request here asks for `Connection: close`.
async fn read_head(stream: &mut TcpStream) -> Head {
    let mut head: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = head.windows(4).position(|window| window == b"\r\n\r\n") {
            if end > HEAD_LIMIT {
                return Head::TooLarge;
            }
            head.truncate(end);
            return Head::Complete(head);
        }
        if head.len() > HEAD_LIMIT {
            return Head::TooLarge;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return Head::Missing,
            Ok(read) => head.extend_from_slice(&chunk[..read]),
        }
    }
}

/// Whether `token` is safe in a header value: visible ASCII, so no byte of it
/// can end this header or start another.
fn is_safe_token(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::test_fakes::{FakeListener, block_on};

    /// A hang guard only: every fake here answers at once.
    const HANG_GUARD: Duration = Duration::from_secs(120);

    #[test]
    fn connect_proven_returns_the_stream_opened_before_the_proof() {
        let fake = FakeListener::proving("token");

        let answer = block_on(async {
            let mut stream = connect_proven(fake.port, "token", HANG_GUARD)
                .await
                .expect("the fake proves the token");
            stream
                .write_all(b"GET /marker HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .await
                .expect("send on the proven stream");
            let mut answer = Vec::new();
            tokio::time::timeout(HANG_GUARD, stream.read_to_end(&mut answer))
                .await
                .expect("the fake answers the marker request")
                .expect("read the answer");
            answer
        });

        assert!(
            answer.starts_with(b"HTTP/1.1 404"),
            "{}",
            String::from_utf8_lossy(&answer)
        );
        let heads = fake.heads_by_accept();
        let accepted_as = |request: &str| {
            heads
                .iter()
                .find(|(_, head)| head.starts_with(request))
                .map(|(accepted, _)| *accepted)
        };
        assert_eq!(
            accepted_as("GET /marker "),
            Some(0),
            "the returned stream is the first connection accepted: {heads:?}"
        );
        assert_eq!(
            accepted_as("GET /proof "),
            Some(1),
            "the proof came on the second connection: {heads:?}"
        );
    }

    #[test]
    fn connect_proven_refuses_an_unproven_listener() {
        let fake = FakeListener::healthy();

        let result = block_on(connect_proven(fake.port, "token", HANG_GUARD));

        assert!(
            matches!(result, Err(ProvenConnectError::NotProven(Proof::Unproven))),
            "{result:?}"
        );
        let heads = fake.heads_when(2, HANG_GUARD);
        let first = heads
            .iter()
            .find(|(accepted, _)| *accepted == 0)
            .map(|(_, head)| head.as_str());
        assert_eq!(
            first,
            Some(""),
            "nothing was written on the first connection: {heads:?}"
        );
    }
}
