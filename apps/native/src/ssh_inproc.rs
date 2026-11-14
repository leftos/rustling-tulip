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
use russh::client::{self, AuthResult};
use russh::keys::ssh_key::LineEnding;
use russh::keys::{
    Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate,
};
use russh::{Channel, ChannelMsg, ChannelStream};
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

/// The reason given when the host took the key but wants a further step.
const PARTIAL_SIGN_IN: &str = "the host accepted this phone's key but asks for another sign-in \
                               step, which the phone cannot do";

/// The reason given when the session ends while the sign-in waits.
const CLOSED_DURING_SIGN_IN: &str = "the connection closed during sign-in";

/// The reason given when the exec channel ends before the host answers it.
const CLOSED_BEFORE_BRIDGE: &str = "the host closed the channel before running the bridge command";

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
    /// The host's host-key algorithms share none with this phone's, so it
    /// offers no key the phone can check against a saved fingerprint (for
    /// example, it holds only key types the phone does not support).
    #[error(
        "{host} offers no host key this phone can check; give its sshd a plain ed25519 host key"
    )]
    NoUsableHostKey {
        /// The host dialled.
        host: String,
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

/// Held across the read-or-make of the phone's key, so two first uses make
/// one key: process-wide, since every store instance reaches the same item.
static CLIENT_KEY_LOCK: Mutex<()> = Mutex::new(());

/// The phone's key: read from `tokens`, or made and stored on first use.
pub fn client_key(tokens: &dyn DeviceTokenStore) -> Result<PrivateKey, OpenError> {
    client_key_in(tokens, &CLIENT_KEY_LOCK)
}

/// [`client_key`] under `lock`, held from the read to the write, never
/// across an await (this is sync).
fn client_key_in(tokens: &dyn DeviceTokenStore, lock: &Mutex<()>) -> Result<PrivateKey, OpenError> {
    let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
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
    sign_in(&mut session, dest, key, public_key).await?;
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|err| connect_error(dest, err))?;
    run_bridge(&mut channel, dest, remote_command).await?;
    let (read, writer) = tokio::io::split(channel.into_stream());
    let reader = SessionReader {
        _session: Box::new(session),
        read,
    };
    Ok((reader, writer))
}

/// Signs in as `dest.user` with `key`. A refusal is [`OpenError::KeyRefused`]
/// only while the session still stands and the host asked for nothing more.
async fn sign_in(
    session: &mut client::Handle<HostKeyCheck>,
    dest: &Destination,
    key: PrivateKey,
    public_key: String,
) -> Result<(), OpenError> {
    let auth = session
        .authenticate_publickey(
            dest.user.as_str(),
            PrivateKeyWithHashAlg::new(Arc::new(key), None),
        )
        .await
        .map_err(|err| connect_error(dest, err))?;
    match auth {
        AuthResult::Success => Ok(()),
        AuthResult::Failure {
            partial_success: true,
            ..
        } => Err(connect_error(dest, PARTIAL_SIGN_IN)),
        AuthResult::Failure { .. } if session.is_closed() => {
            Err(connect_error(dest, CLOSED_DURING_SIGN_IN))
        }
        AuthResult::Failure { .. } => Err(OpenError::KeyRefused {
            host: dest.host.clone(),
            public_key,
        }),
    }
}

/// Asks the host to run `remote_command` on `channel` and waits for its
/// answer, which the channel's stream would otherwise drop unread.
async fn run_bridge(
    channel: &mut Channel<client::Msg>,
    dest: &Destination,
    remote_command: &str,
) -> Result<(), OpenError> {
    channel
        .exec(true, remote_command)
        .await
        .map_err(|err| connect_error(dest, err))?;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure) => {
                return Err(connect_error(
                    dest,
                    format!("the host refused to run the bridge command `{remote_command}`"),
                ));
            }
            None
            | Some(
                ChannelMsg::Eof
                | ChannelMsg::Close
                | ChannelMsg::ExitStatus { .. }
                | ChannelMsg::Data { .. },
            ) => return Err(connect_error(dest, CLOSED_BEFORE_BRIDGE)),
            Some(_) => {}
        }
    }
}

/// Why the handshake failed: the offered host key when it was not trusted,
/// no usable host key when the host offered none the phone negotiates, else
/// the failure itself.
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
        (None, _)
            if matches!(
                err,
                russh::Error::NoCommonAlgo {
                    kind: russh::AlgorithmKind::Key,
                    ..
                }
            ) =>
        {
            OpenError::NoUsableHostKey {
                host: dest.host.clone(),
            }
        }
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
    use std::sync::mpsc as std_mpsc;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
    use std::time::Duration;

    use daemon_client::device_tokens::{DeviceTokenStore, MemoryTokens};
    use futures::{SinkExt as _, StreamExt as _};
    use russh::keys::{Algorithm, PrivateKey, PublicKey};
    use russh::server::{self, Auth, Msg, Session};
    use russh::{Channel, ChannelId};
    use tokio_tungstenite::tungstenite::Message;

    use super::{
        CLIENT_KEY_NAME, Destination, LineSocket, OpenError, client_key, client_key_in,
        fingerprint, open_over, parse_destination, public_key_line,
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

    /// What a test server answers a key sign-in with, in place of its
    /// `allowed` check: an `Auth`, or an `Err` that ends the session.
    type SignIn = fn() -> Result<Auth, russh::Error>;

    /// An SSH server that lets in `allowed` (any key when `None`), records
    /// each exec and echoes the channel's bytes. `sign_in`, when set, is the
    /// answer to every key sign-in; `refuse_exec` refuses every exec.
    struct TestServer {
        allowed: Option<PublicKey>,
        sign_in: Option<SignIn>,
        refuse_exec: bool,
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
            if let Some(sign_in) = self.sign_in {
                return std::future::ready(sign_in());
            }
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
            std::future::ready(if self.refuse_exec {
                session.channel_failure(channel)
            } else {
                session.channel_success(channel)
            })
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
        sign_in: Option<SignIn>,
        refuse_exec: bool,
        without_host_key: bool,
        seen: Seen,
    }

    impl Host {
        fn new(allowed: Option<PublicKey>) -> Self {
            Self {
                key: random_key(),
                allowed,
                sign_in: None,
                refuse_exec: false,
                without_host_key: false,
                seen: Seen::default(),
            }
        }

        /// This host, holding no host key at all, so it offers no host-key
        /// algorithm in its key exchange.
        fn without_host_key(self) -> Self {
            Self {
                without_host_key: true,
                ..self
            }
        }

        /// This host, answering every key sign-in with `sign_in`.
        fn signing_in_with(self, sign_in: SignIn) -> Self {
            Self {
                sign_in: Some(sign_in),
                ..self
            }
        }

        /// This host, refusing every exec.
        fn refusing_exec(self) -> Self {
            Self {
                refuse_exec: true,
                ..self
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
            let keys = if self.without_host_key {
                Vec::new()
            } else {
                vec![self.key.clone()]
            };
            let config = Arc::new(server::Config {
                keys,
                auth_rejection_time: Duration::from_millis(1),
                auth_rejection_time_initial: Some(Duration::ZERO),
                inactivity_timeout: None,
                ..server::Config::default()
            });
            let handler = TestServer {
                allowed: self.allowed.clone(),
                sign_in: self.sign_in,
                refuse_exec: self.refuse_exec,
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

    /// A store over [`MemoryTokens`] that counts writes and parks its first
    /// read, after reading, until released; every later read releases it.
    struct ParkingTokens {
        inner: MemoryTokens,
        reads: AtomicUsize,
        sets: AtomicUsize,
        parked: Mutex<std_mpsc::Sender<()>>,
        release_tx: Mutex<std_mpsc::Sender<()>>,
        release_rx: Mutex<std_mpsc::Receiver<()>>,
    }

    impl ParkingTokens {
        /// The store, and the receiver its first read signals on once parked.
        fn new() -> (Self, std_mpsc::Receiver<()>) {
            let (parked, parked_rx) = std_mpsc::channel();
            let (release_tx, release_rx) = std_mpsc::channel();
            let store = Self {
                inner: MemoryTokens::default(),
                reads: AtomicUsize::new(0),
                sets: AtomicUsize::new(0),
                parked: Mutex::new(parked),
                release_tx: Mutex::new(release_tx),
                release_rx: Mutex::new(release_rx),
            };
            (store, parked_rx)
        }

        fn release(&self) {
            // The parked read may be gone already; a spare release is harmless.
            _ = lock(&self.release_tx).send(());
        }
    }

    impl DeviceTokenStore for ParkingTokens {
        fn get(&self, host: &str) -> anyhow::Result<Option<String>> {
            let value = self.inner.get(host);
            if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
                lock(&self.parked)
                    .send(())
                    .expect("the test waits for the park");
                lock(&self.release_rx)
                    .recv_timeout(HANG_GUARD)
                    .expect("hung: the parked read was never released");
            } else {
                self.release();
            }
            value
        }

        fn set(&self, host: &str, token: &str) -> anyhow::Result<()> {
            self.sets.fetch_add(1, Ordering::SeqCst);
            self.inner.set(host, token)
        }

        fn delete(&self, host: &str) -> anyhow::Result<()> {
            self.inner.delete(host)
        }
    }

    /// `key`'s public half as an `authorized_keys` line.
    fn public_line(key: &PrivateKey) -> String {
        key.public_key()
            .to_openssh()
            .expect("the public key encodes")
    }

    #[test]
    fn two_first_uses_make_one_key() {
        let (store, parked) = ParkingTokens::new();
        let store = Arc::new(store);
        let key_lock = Arc::new(Mutex::new(()));
        let (done_tx, done) = std_mpsc::channel();
        let first_use = || {
            let (store, key_lock, done_tx) =
                (Arc::clone(&store), Arc::clone(&key_lock), done_tx.clone());
            std::thread::spawn(move || {
                _ = done_tx.send(client_key_in(store.as_ref(), &key_lock));
            });
        };

        first_use();
        parked
            .recv_timeout(HANG_GUARD)
            .expect("hung: the first use never read the store");
        first_use();
        if matches!(key_lock.try_lock(), Err(TryLockError::WouldBlock)) {
            store.release();
        }

        let mut lines = Vec::new();
        for _ in 0..2 {
            let key = done
                .recv_timeout(HANG_GUARD)
                .expect("hung: a first use never returned")
                .expect("a key");
            lines.push(public_line(&key));
        }
        let stored = store
            .inner
            .get(CLIENT_KEY_NAME)
            .expect("the store reads")
            .expect("a key is stored");
        let stored = public_line(&PrivateKey::from_openssh(stored).expect("the stored key parses"));
        assert_eq!(lines[0], lines[1], "two first uses made two keys");
        assert_eq!(
            lines[0], stored,
            "a first use returned a key the store does not hold"
        );
        assert_eq!(
            store.sets.load(Ordering::SeqCst),
            1,
            "the key was stored more than once"
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

    /// Opens `host` with its key trusted and returns the error it fails with.
    async fn open_error(host: &Host) -> OpenError {
        let trusted = host.fingerprint();
        host.open(
            DEFAULT_REMOTE_COMMAND,
            Some(&trusted),
            &MemoryTokens::default(),
        )
        .await
        .err()
        .expect("the session does not open")
    }

    /// Asserts `err` is a `Connect` error for `box` with exactly `want`.
    fn assert_connect(err: &OpenError, want: &str) {
        assert!(
            matches!(err, OpenError::Connect { host, reason } if host == "box" && reason == want),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_partial_sign_in_is_not_a_refused_key() {
        let host = Host::new(None).signing_in_with(|| {
            Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: true,
            })
        });
        let err = open_error(&host).await;
        assert_connect(
            &err,
            "the host accepted this phone's key but asks for another sign-in step, \
             which the phone cannot do",
        );
        assert_eq!(host.seen.execs(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_session_dropped_during_sign_in_is_a_connect_error() {
        let host = Host::new(None).signing_in_with(|| Err(russh::Error::Disconnect));
        let err = open_error(&host).await;
        assert_connect(&err, "the connection closed during sign-in");
        assert!(host.seen.auths() > 0);
    }

    #[tokio::test]
    async fn a_refused_bridge_command_is_reported() {
        let host = Host::new(None).refusing_exec();
        let err = open_error(&host).await;
        assert_connect(
            &err,
            "the host refused to run the bridge command `rustling-tulipd bridge`",
        );
        assert_eq!(host.seen.execs(), ["rustling-tulipd bridge"]);
    }

    #[tokio::test]
    async fn a_host_offering_no_host_key_is_reported() {
        let host = Host::new(None).without_host_key();
        let err = host
            .open(DEFAULT_REMOTE_COMMAND, None, &MemoryTokens::default())
            .await
            .err()
            .expect("a host offering no host key is not trusted");
        assert!(
            matches!(&err, OpenError::NoUsableHostKey { host } if host == "box"),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "box offers no host key this phone can check; give its sshd a plain ed25519 host key"
        );
        assert_eq!(host.seen.auths(), 0);
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
