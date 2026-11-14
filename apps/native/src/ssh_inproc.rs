//! The in-process SSH transport: an SSH session the app opens itself
//! (russh), for a platform with no `ssh` to run. It trusts a host only by
//! the fingerprint saved for it, signs in with the phone's own ed25519 key,
//! runs the bridge command on an exec channel and speaks the daemon
//! protocol over it as the spawned transport does, through a [`LineSocket`].

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use daemon_client::device_tokens::DeviceTokenStore;
use russh::ChannelStream;
use russh::client;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{
    Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};

use crate::ssh::LineSocket;
use crate::target::SshHost;

/// The device-token-store name the phone's SSH key is kept under.
pub const CLIENT_KEY_NAME: &str = "ssh-client-key.invalid";

/// The SSH port when the destination names none.
pub const DEFAULT_PORT: u16 = 22;

/// The comment the phone's key carries, which ends its `authorized_keys` line.
const KEY_COMMENT: &str = "rustling-tulip-phone";

/// How long the TCP dial may take before the host counts as unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The time a host has to finish the SSH handshake, sign-in and the bridge
/// command's start.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The reason given when the TCP dial or the session setup runs out of time.
const NO_ANSWER: &str = "no answer within 10 s";

/// How often a quiet session is probed, as `ServerAliveInterval=15`.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// How many unanswered probes end the session, as `ServerAliveCountMax=3`.
const KEEPALIVE_MAX: usize = 3;

/// Why an in-process SSH connection did not open. Its `Display` is the
/// message the phone shows.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The host string is not `user@host` or `user@host:port`.
    #[error("the phone connects to user@host or user@host:port, not {name}")]
    BadDestination {
        /// The host string as typed.
        name: String,
    },
    /// No host key is saved for the host: the user is asked to trust this one.
    #[error("{host} offered a host key this phone has not seen: {fingerprint}")]
    UnknownHostKey {
        /// The host dialled.
        host: String,
        /// The offered key's fingerprint, `SHA256:<base64>`.
        fingerprint: String,
    },
    /// The host offered a key other than the saved one.
    #[error(
        "{host}'s host key changed: it offered {presented}, but this phone trusts {saved}; not connecting"
    )]
    HostKeyChanged {
        /// The host dialled.
        host: String,
        /// The fingerprint this phone trusts for the host.
        saved: String,
        /// The fingerprint of the key the host offered.
        presented: String,
    },
    /// The host refused the phone's key.
    #[error(
        "{host} refused this phone's key; add it to ~/.ssh/authorized_keys there: {public_key}"
    )]
    KeyRefused {
        /// The host dialled.
        host: String,
        /// The phone's `authorized_keys` line.
        public_key: String,
    },
    /// The phone's key could not be read from, made or stored in the store.
    #[error("the phone's SSH key could not be read or made: {reason}")]
    ClientKey {
        /// What failed.
        reason: String,
    },
    /// The host could not be reached, or the session failed.
    #[error("could not reach {host}: {reason}")]
    Connect {
        /// The host dialled.
        host: String,
        /// What failed.
        reason: String,
    },
}

/// Where an in-process SSH session goes: the user, the host and its port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// The account signed in as.
    pub user: String,
    /// The host name or address dialled.
    pub host: String,
    /// The SSH port.
    pub port: u16,
}

/// `user@host` or `user@host:port`, as typed.
pub fn parse_destination(name: &str) -> Result<Destination, OpenError> {
    let bad = || OpenError::BadDestination {
        name: name.to_owned(),
    };
    let (user, rest) = name.split_once('@').ok_or_else(bad)?;
    if user.is_empty() || rest.contains('@') || rest.starts_with('[') {
        return Err(bad());
    }
    let (host, port) = match rest.split_once(':') {
        Some((host, port)) => (host, parse_port(port).ok_or_else(bad)?),
        None => (rest, DEFAULT_PORT),
    };
    if host.is_empty() {
        return Err(bad());
    }
    Ok(Destination {
        user: user.to_owned(),
        host: host.to_owned(),
        port,
    })
}

/// A port written as digits only, not zero.
fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|port| *port != 0)
}

/// The phone's key: read from `tokens`, or made and stored on first use.
pub fn client_key(tokens: &dyn DeviceTokenStore) -> Result<PrivateKey, OpenError> {
    let stored = tokens
        .get(CLIENT_KEY_NAME)
        .map_err(|err| key_error(format!("{err:#}")))?;
    if let Some(text) = stored {
        return PrivateKey::from_openssh(text).map_err(key_error);
    }
    let mut key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).map_err(key_error)?;
    key.set_comment(KEY_COMMENT);
    let text = key.to_openssh(LineEnding::LF).map_err(key_error)?;
    tokens
        .set(CLIENT_KEY_NAME, text.as_str())
        .map_err(|err| key_error(format!("{err:#}")))?;
    Ok(key)
}

/// The `authorized_keys` line for the phone's key (`ssh-ed25519 AAAA… rustling-tulip-phone`).
pub fn public_key_line(tokens: &dyn DeviceTokenStore) -> Result<String, OpenError> {
    key_line(&client_key(tokens)?)
}

/// `key`'s public half as an `authorized_keys` line.
fn key_line(key: &PrivateKey) -> Result<String, OpenError> {
    key.public_key().to_openssh().map_err(key_error)
}

/// `key`'s OpenSSH SHA-256 fingerprint, `SHA256:<base64>`.
#[must_use]
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Dials `host` over TCP (10 s connect timeout) and runs [`open_over`] on it.
pub async fn open(
    host: &SshHost,
    saved_host_key: Option<&str>,
    tokens: &dyn DeviceTokenStore,
) -> Result<LineSocket, OpenError> {
    let dest = parse_destination(host.name())?;
    let dial = tokio::net::TcpStream::connect((dest.host.as_str(), dest.port));
    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, dial).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => return Err(connect_error(&dest, err)),
        Err(_) => return Err(connect_error(&dest, NO_ANSWER)),
    };
    let remote_command = host
        .remote_command()
        .unwrap_or(crate::ssh::DEFAULT_REMOTE_COMMAND);
    open_over(stream, &dest, remote_command, saved_host_key, tokens).await
}

/// The SSH session over `stream`: host key check, key auth, exec, line socket.
pub async fn open_over<S>(
    stream: S,
    dest: &Destination,
    remote_command: &str,
    saved_host_key: Option<&str>,
    tokens: &dyn DeviceTokenStore,
) -> Result<LineSocket, OpenError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let setup = start_session(stream, dest, remote_command, saved_host_key, tokens);
    let (reader, writer) = tokio::time::timeout(HANDSHAKE_TIMEOUT, setup)
        .await
        .map_err(|_| connect_error(dest, NO_ANSWER))??;
    Ok(LineSocket::new(reader, writer, tokio::io::empty()))
}

/// Everything [`open_over`] does before the line socket: the handshake,
/// the host key check, sign-in and the exec, ending at the channel's halves.
async fn start_session<S>(
    stream: S,
    dest: &Destination,
    remote_command: &str,
    saved_host_key: Option<&str>,
    tokens: &dyn DeviceTokenStore,
) -> Result<(SessionReader, WriteHalf<ChannelStream<client::Msg>>), OpenError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let key = client_key(tokens)?;
    let public_key = key_line(&key)?;
    let offered = Arc::new(Mutex::new(None));
    let check = HostKeyCheck {
        saved: saved_host_key.map(str::to_owned),
        offered: Arc::clone(&offered),
    };
    let mut session = match client::connect_stream(Arc::new(client_config()), stream, check).await {
        Ok(session) => session,
        Err(err) => return Err(refused(dest, saved_host_key, &offered, &err)),
    };
    let auth = session
        .authenticate_publickey(
            dest.user.as_str(),
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        )
        .await
        .map_err(|err| connect_error(dest, err))?;
    if !auth.success() {
        return Err(OpenError::KeyRefused {
            host: dest.host.clone(),
            public_key,
        });
    }
    let channel = session
        .channel_open_session()
        .await
        .map_err(|err| connect_error(dest, err))?;
    channel
        .exec(true, remote_command)
        .await
        .map_err(|err| connect_error(dest, err))?;
    let (read, writer) = tokio::io::split(channel.into_stream());
    let reader = SessionReader {
        _session: Box::new(session),
        read,
    };
    Ok((reader, writer))
}

/// Why the handshake failed: the offered host key when it was not trusted,
/// else the failure itself.
fn refused(
    dest: &Destination,
    saved: Option<&str>,
    offered: &Mutex<Option<String>>,
    err: &russh::Error,
) -> OpenError {
    let presented = offered
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    match (presented, saved) {
        (Some(fingerprint), None) => OpenError::UnknownHostKey {
            host: dest.host.clone(),
            fingerprint,
        },
        (Some(presented), Some(saved)) if presented != saved => OpenError::HostKeyChanged {
            host: dest.host.clone(),
            saved: saved.to_owned(),
            presented,
        },
        _ => connect_error(dest, err),
    }
}

fn connect_error(dest: &Destination, reason: impl std::fmt::Display) -> OpenError {
    OpenError::Connect {
        host: dest.host.clone(),
        reason: reason.to_string(),
    }
}

fn key_error(reason: impl std::fmt::Display) -> OpenError {
    OpenError::ClientKey {
        reason: reason.to_string(),
    }
}

/// The session's settings: keepalives as the spawned `ssh` sends them, and
/// no idle cut-off.
fn client_config() -> client::Config {
    client::Config {
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        keepalive_max: KEEPALIVE_MAX,
        inactivity_timeout: None,
        ..client::Config::default()
    }
}

/// Trusts the host's key only when its fingerprint is the saved one, and
/// records the fingerprint offered so a refusal can name it.
struct HostKeyCheck {
    saved: Option<String>,
    offered: Arc<Mutex<Option<String>>>,
}

impl client::Handler for HostKeyCheck {
    type Error = russh::Error;

    fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        std::future::ready(Ok(self.trusts(server_public_key)))
    }
}

impl HostKeyCheck {
    /// Whether `offered` is a plain key whose fingerprint is the saved one,
    /// recording that fingerprint; a certificate is never trusted.
    fn trusts(&self, offered: &PublicKeyOrCertificate) -> bool {
        let PublicKeyOrCertificate::PublicKey { key, .. } = offered else {
            return false;
        };
        let presented = fingerprint(key);
        let trusted = self.saved.as_deref() == Some(presented.as_str());
        *self.offered.lock().unwrap_or_else(PoisonError::into_inner) = Some(presented);
        trusted
    }
}

/// The exec channel's read half, holding the session it runs in so the
/// session lives exactly as long as the socket reading from it.
struct SessionReader {
    _session: Box<client::Handle<HostKeyCheck>>,
    read: ReadHalf<ChannelStream<client::Msg>>,
}

impl AsyncRead for SessionReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.read).poll_read(cx, buf)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
    use std::time::Duration;

    use daemon_client::device_tokens::{DeviceTokenStore as _, MemoryTokens};
    use futures::{SinkExt as _, StreamExt as _};
    use russh::keys::{Algorithm, PrivateKey, PublicKey};
    use russh::server::{self, Auth, Msg, Session};
    use russh::{Channel, ChannelId};
    use tokio_tungstenite::tungstenite::Message;

    use super::{
        CLIENT_KEY_NAME, Destination, LineSocket, OpenError, client_key, fingerprint, open_over,
        parse_destination, public_key_line,
    };
    use crate::ssh::DEFAULT_REMOTE_COMMAND;

    /// The hang guard on a wait for the session; a passing run never comes
    /// near it.
    const HANG_GUARD: Duration = Duration::from_secs(120);

    /// The line sent and echoed back.
    const HELLO: &str = r#"{"type":"hello"}"#;

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn random_key() -> PrivateKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("an ed25519 key")
    }

    /// What the test server saw: auth attempts, exec commands and the keys
    /// offered to it.
    #[derive(Clone, Default)]
    struct Seen {
        auths: Arc<AtomicUsize>,
        execs: Arc<Mutex<Vec<String>>>,
        keys: Arc<Mutex<Vec<PublicKey>>>,
    }

    impl Seen {
        fn auths(&self) -> usize {
            self.auths.load(Ordering::SeqCst)
        }

        fn execs(&self) -> Vec<String> {
            lock(&self.execs).clone()
        }

        fn keys(&self) -> Vec<PublicKey> {
            lock(&self.keys).clone()
        }
    }

    /// An SSH server that lets in `allowed` (any key when `None`), records
    /// each exec and echoes the channel's bytes.
    struct TestServer {
        allowed: Option<PublicKey>,
        seen: Seen,
    }

    impl server::Handler for TestServer {
        type Error = russh::Error;

        fn auth_publickey(
            &mut self,
            _user: &str,
            key: server::VerifiedPublicKey<'_>,
        ) -> impl Future<Output = Result<Auth, Self::Error>> + Send {
            self.seen.auths.fetch_add(1, Ordering::SeqCst);
            lock(&self.seen.keys).push(key.public_key.clone());
            let allowed = self
                .allowed
                .as_ref()
                .is_none_or(|allowed| allowed.key_data() == key.public_key.key_data());
            std::future::ready(Ok(if allowed {
                Auth::Accept
            } else {
                Auth::reject()
            }))
        }

        async fn channel_open_session(
            &mut self,
            _channel: Channel<Msg>,
            reply: server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            Ok(())
        }

        fn exec_request(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            lock(&self.seen.execs).push(String::from_utf8_lossy(data).into_owned());
            std::future::ready(session.channel_success(channel))
        }

        fn data(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            std::future::ready(session.data(channel, data.to_vec()))
        }
    }

    /// A host to dial: its host key, the client key it lets in, and what it saw.
    struct Host {
        key: PrivateKey,
        allowed: Option<PublicKey>,
        seen: Seen,
    }

    impl Host {
        fn new(allowed: Option<PublicKey>) -> Self {
            Self {
                key: random_key(),
                allowed,
                seen: Seen::default(),
            }
        }

        fn fingerprint(&self) -> String {
            fingerprint(self.key.public_key())
        }

        /// Starts a server session over an in-memory pipe and opens the
        /// client over its other end.
        async fn open(
            &self,
            command: &str,
            saved: Option<&str>,
            tokens: &MemoryTokens,
        ) -> Result<LineSocket, OpenError> {
            let (client_end, server_end) = tokio::io::duplex(64 * 1024);
            let config = Arc::new(server::Config {
                keys: vec![self.key.clone()],
                auth_rejection_time: Duration::from_millis(1),
                auth_rejection_time_initial: Some(Duration::ZERO),
                inactivity_timeout: None,
                ..server::Config::default()
            });
            let handler = TestServer {
                allowed: self.allowed.clone(),
                seen: self.seen.clone(),
            };
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(config, server_end, handler).await {
                    _ = running.await;
                }
            });
            let dest = Destination {
                user: "alice".to_owned(),
                host: "box".to_owned(),
                port: 22,
            };
            tokio::time::timeout(
                HANG_GUARD,
                open_over(client_end, &dest, command, saved, tokens),
            )
            .await
            .expect("hung: the session never opened")
        }
    }

    /// Sends [`HELLO`] and checks it comes back.
    async fn echo(socket: &mut LineSocket) {
        socket
            .send(Message::Text(HELLO.into()))
            .await
            .expect("the line is sent");
        let echoed = tokio::time::timeout(HANG_GUARD, socket.next())
            .await
            .expect("hung: no line came back")
            .expect("the stream ended")
            .expect("the line read");
        assert_eq!(echoed, Message::Text(HELLO.into()));
    }

    #[test]
    fn parses_a_destination() {
        let cases = [("alice@box", "box", 22), ("alice@box:2222", "box", 2222)];
        for (name, host, port) in cases {
            let dest = parse_destination(name).expect("a destination");
            assert_eq!(
                dest,
                Destination {
                    user: "alice".to_owned(),
                    host: host.to_owned(),
                    port,
                },
                "{name}"
            );
        }
    }

    #[test]
    fn refuses_a_bad_destination() {
        for name in [
            "box",
            "@box",
            "alice@",
            "alice@box:0",
            "alice@box:abc",
            "alice@[::1]:22",
        ] {
            let err = parse_destination(name).expect_err("refused");
            assert!(
                matches!(&err, OpenError::BadDestination { name: refused } if refused == name),
                "{name}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn connects_runs_the_command_and_echoes_a_line() {
        let host = Host::new(None);
        let tokens = MemoryTokens::default();
        let trusted = host.fingerprint();
        let mut socket = host
            .open(DEFAULT_REMOTE_COMMAND, Some(&trusted), &tokens)
            .await
            .expect("the session opens");
        echo(&mut socket).await;
        assert_eq!(host.seen.execs(), ["rustling-tulipd bridge"]);
        drop(socket);

        let mut socket = host
            .open("rt bridge --x", Some(&trusted), &tokens)
            .await
            .expect("the session opens again");
        echo(&mut socket).await;
        assert_eq!(
            host.seen.execs(),
            ["rustling-tulipd bridge", "rt bridge --x"]
        );
    }

    #[tokio::test]
    async fn an_unknown_host_key_is_reported_for_trust() {
        let host = Host::new(None);
        let tokens = MemoryTokens::default();
        let err = host
            .open(DEFAULT_REMOTE_COMMAND, None, &tokens)
            .await
            .err()
            .expect("an unsaved host key is not trusted");
        let presented = host.fingerprint();
        assert!(
            matches!(&err, OpenError::UnknownHostKey { fingerprint, .. } if *fingerprint == presented),
            "{err:?}"
        );
        assert_eq!(host.seen.auths(), 0);
        assert_eq!(host.seen.execs(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_changed_host_key_is_refused_with_both_fingerprints() {
        let host = Host::new(None);
        let tokens = MemoryTokens::default();
        let other = fingerprint(random_key().public_key());
        let err = host
            .open(DEFAULT_REMOTE_COMMAND, Some(&other), &tokens)
            .await
            .err()
            .expect("a different host key is refused");
        let presented_now = host.fingerprint();
        assert!(
            matches!(&err, OpenError::HostKeyChanged { saved, presented, .. }
                if *saved == other && *presented == presented_now),
            "{err:?}"
        );
        let text = err.to_string();
        assert!(
            text.contains(&other) && text.contains(&presented_now),
            "{text}"
        );
        assert_eq!(host.seen.auths(), 0);
    }

    #[tokio::test]
    async fn the_key_is_made_on_first_use_and_reused() {
        let host = Host::new(None);
        let tokens = MemoryTokens::default();
        let trusted = host.fingerprint();
        let mut socket = host
            .open(DEFAULT_REMOTE_COMMAND, Some(&trusted), &tokens)
            .await
            .expect("the session opens");
        echo(&mut socket).await;
        let stored = tokens
            .get(CLIENT_KEY_NAME)
            .expect("the store reads")
            .expect("a key is stored on first use");

        let mut socket = host
            .open(DEFAULT_REMOTE_COMMAND, Some(&trusted), &tokens)
            .await
            .expect("the session opens again");
        echo(&mut socket).await;
        assert_eq!(
            tokens.get(CLIENT_KEY_NAME).expect("the store reads"),
            Some(stored)
        );

        let keys = host.seen.keys();
        let first = keys
            .first()
            .expect("the server saw a key")
            .key_data()
            .clone();
        assert!(keys.len() >= 2, "{} keys seen", keys.len());
        assert!(
            keys.iter().all(|key| *key.key_data() == first),
            "a key other than the first was offered"
        );
        let line = public_key_line(&tokens).expect("the line");
        let parsed = PublicKey::from_openssh(&line).expect("the line parses");
        assert_eq!(*parsed.key_data(), first);
        assert!(line.ends_with(" rustling-tulip-phone"), "{line}");
    }

    #[test]
    fn a_stored_key_that_does_not_parse_is_an_error() {
        let tokens = MemoryTokens::with(CLIENT_KEY_NAME, "not a key");
        let err = client_key(&tokens).expect_err("a bad stored key is refused");
        assert!(matches!(err, OpenError::ClientKey { .. }), "{err:?}");
        assert_eq!(
            tokens.get(CLIENT_KEY_NAME).expect("the store reads"),
            Some("not a key".to_owned())
        );
    }

    #[tokio::test]
    async fn a_refused_key_names_the_public_key_to_authorize() {
        let host = Host::new(Some(random_key().public_key().clone()));
        let tokens = MemoryTokens::default();
        let trusted = host.fingerprint();
        let err = host
            .open(DEFAULT_REMOTE_COMMAND, Some(&trusted), &tokens)
            .await
            .err()
            .expect("the key is refused");
        let line = public_key_line(&tokens).expect("the line");
        assert!(
            matches!(&err, OpenError::KeyRefused { public_key, .. } if *public_key == line),
            "{err:?}"
        );
        assert!(host.seen.auths() > 0);
        assert_eq!(host.seen.execs(), Vec::<String>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_host_times_out() {
        let (client, _server) = tokio::io::duplex(64 * 1024);
        let dest = Destination {
            user: "alice".to_owned(),
            host: "box".to_owned(),
            port: 22,
        };
        let err = open_over(
            client,
            &dest,
            "rustling-tulipd bridge",
            None,
            &MemoryTokens::default(),
        )
        .await
        .err()
        .expect("a host that never speaks SSH is given up on");
        assert!(
            matches!(&err, OpenError::Connect { reason, .. } if reason == "no answer within 10 s"),
            "{err:?}"
        );
    }
}
