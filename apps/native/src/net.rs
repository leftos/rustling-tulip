//! Daemon connection loop. On a dedicated thread with a current-thread tokio
//! runtime it ensures a daemon is running, opens the WebSocket, sends `Hello`
//! and pumps messages both ways until the socket closes, then reconnects after
//! the delay [`Connection`] sets. GPUI runs its own executor, so the view and
//! this thread talk over unbounded futures channels.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use daemon_client::ClientIdentity;
use daemon_client::device_tokens::{CredentialManagerTokens, DeviceTokenStore, MemoryTokens};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::{SinkExt as _, StreamExt as _};
use protocol::{
    ClientMessage, DaemonHandshake, DaemonMessage, DevicePairOutcome, InboundDaemonMessage,
};
use rustls::pki_types::CertificateDer;
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};

use crate::connection::{Connection, PROBE_TIMEOUT, RestartAction, State, TICK, Watchdog};
use crate::target::{TailnetHost, Target};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Frame = Option<Result<Message, tungstenite::Error>>;
/// A pending `ensure_running`, boxed so tests can substitute one.
pub type EnsureFuture = Pin<Box<dyn Future<Output = Result<DaemonHandshake>>>>;
/// The protocol versions the native client speaks, the protocol crate's
/// generated `protocol-version.json` constants. A daemon supporting none of
/// them is retired and replaced.
pub const NATIVE_PROTOCOL_VERSIONS: &[u32] = protocol::SUPPORTED_PROTOCOL_VERSIONS;

/// A pending force-stop of the daemon.
pub type StopFuture = Pin<Box<dyn Future<Output = Result<()>>>>;

/// How the network thread reaches its daemon.
pub struct NetDeps {
    /// Starts an `ensure_running`: the daemon step of each connection
    /// attempt.
    pub ensure: Box<dyn Fn() -> EnsureFuture + Send>,
    /// The identity sent in `Hello`; `None` connects without one.
    pub identity: Option<ClientIdentity>,
    /// Force-stops the daemon, for [`NetCommand::Stop`].
    pub stop: Box<dyn Fn() -> StopFuture + Send>,
    /// The device tokens this client holds per tailnet host. The host's token
    /// goes in its `Hello`; with none held (or the store unreadable) the
    /// `Hello` carries an empty token, and the host answers "not approved".
    pub tokens: Arc<dyn DeviceTokenStore>,
    /// The TLS client config a tailnet dial uses. `None`, the production
    /// value, builds one from the OS certificate store on first use; only
    /// tests set it, to trust a test CA.
    pub tls: Option<Arc<ClientConfig>>,
}

/// A client config trusting the OS certificate store's roots, on the `ring`
/// provider. An empty store fails: there is no fallback.
fn os_tls_config() -> Result<Arc<ClientConfig>> {
    let loaded = rustls_native_certs::load_native_certs();
    if !loaded.errors.is_empty() {
        warn!(errors = ?loaded.errors, "some OS root certificates could not be loaded");
    }
    tls_config_trusting(loaded.certs)
}

/// A client config whose only roots are the parsable ones among `certs`.
fn tls_config_trusting(certs: Vec<CertificateDer<'static>>) -> Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    let (added, ignored) = roots.add_parsable_certificates(certs);
    debug!(added, ignored, "loaded the OS root certificates");
    if roots.is_empty() {
        anyhow::bail!("no usable root certificates in the OS certificate store");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("choosing the TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

impl NetDeps {
    /// The daemon under the config dir, reused when compatible, with this
    /// client's identity.
    #[must_use]
    pub fn production() -> Self {
        let identity = daemon_client::client_identity(CLIENT_ID_FILE)
            .inspect_err(|err| {
                warn!("loading the client identity, connecting without one: {err:#}");
            })
            .ok();
        Self {
            ensure: Box::new(ensure_daemon),
            identity,
            stop: Box::new(|| Box::pin(daemon_client::stop())),
            tokens: device_token_store(),
            tls: None,
        }
    }
}

/// Windows Credential Manager's device tokens; an empty in-memory store when
/// it cannot be opened, so every tailnet host is dialled without a token.
fn device_token_store() -> Arc<dyn DeviceTokenStore> {
    match CredentialManagerTokens::open() {
        Ok(store) => Arc::new(store),
        Err(err) => {
            warn!("opening the device token store, holding no device tokens: {err:#}");
            Arc::new(MemoryTokens::default())
        }
    }
}

/// The file under the config dir that holds this client's identity.
const CLIENT_ID_FILE: &str = "client-id-native";
/// How long after `Hello` the daemon has to answer with `Welcome` or
/// `AuthFailed` before the socket is dropped and the reconnect runs.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);

/// A request from the view to the network thread.
pub enum NetCommand {
    /// Forward a message to the daemon; dropped while there is no socket.
    Send(Box<ClientMessage>),
    /// Restart the daemon, or connect from scratch when there is no live
    /// connection to restart it over.
    Restart,
    /// Force-stop the daemon and stop reconnecting.
    Stop,
    /// The app is quitting: send `before` in order, then ask the daemon to
    /// shut down (`drain` stops its sessions first), and never reconnect or
    /// respawn it after the connection closes. All or nothing: with no live
    /// socket nothing is sent and [`NetEvent::ShutdownFailed`] follows.
    Shutdown {
        before: Vec<ClientMessage>,
        drain: bool,
    },
    /// Connect to this daemon instead, now: drop the live socket and start
    /// the connection over on it.
    SwitchTarget(Target),
    /// Ask the tailnet host to pair this device: `DevicePairRequest` on the
    /// pairing socket, or, when the host closed it, a redial with an empty
    /// token that sends the request once the host answers "not approved".
    /// Acts only while the connection is [`State::NotApproved`].
    PairRequest,
    /// Redeem the code the owner read on the host for the pending request.
    /// With no pairing socket the request is gone: nothing is sent and
    /// [`NetEvent::PairingLost`] follows. Acts only while
    /// [`State::NotApproved`]; `code` never prints in `{:?}`.
    PairRedeem { code: String },
    /// Withdraw the pairing request: `DevicePairCancel` on the pairing
    /// socket, and no request on the next redial.
    PairCancel,
}

impl fmt::Debug for NetCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Send(msg) => f.debug_tuple("Send").field(msg).finish(),
            Self::Restart => f.write_str("Restart"),
            Self::Stop => f.write_str("Stop"),
            Self::Shutdown { before, drain } => f
                .debug_struct("Shutdown")
                .field("before", before)
                .field("drain", drain)
                .finish(),
            Self::SwitchTarget(target) => f.debug_tuple("SwitchTarget").field(target).finish(),
            Self::PairRequest => f.write_str("PairRequest"),
            Self::PairRedeem { .. } => f
                .debug_struct("PairRedeem")
                .field("code", &format_args!("<redacted>"))
                .finish(),
            Self::PairCancel => f.write_str("PairCancel"),
        }
    }
}

/// The running daemon's handshake without its auth token.
#[derive(Debug, Clone, Copy)]
pub struct HandshakeInfo {
    /// The daemon's loopback port.
    pub port: u16,
    /// The daemon's process id.
    pub pid: u32,
    /// The protocol version the daemon wrote into `daemon.json`.
    pub protocol_version: u32,
}

impl From<&DaemonHandshake> for HandshakeInfo {
    fn from(handshake: &DaemonHandshake) -> Self {
        Self {
            port: handshake.port,
            pid: handshake.pid,
            protocol_version: handshake.protocol_version,
        }
    }
}

/// What the network thread tells the view.
pub enum NetEvent {
    /// The connection state machine changed; this is its new state.
    State(Connection),
    /// `ensure_running` returned this daemon.
    Handshake(HandshakeInfo),
    /// A message from the daemon.
    Message(Box<DaemonMessage>),
    /// A [`NetCommand::Shutdown`] went out in full; the close that follows
    /// is the daemon exiting.
    ShutdownSent,
    /// A [`NetCommand::Shutdown`] arrived with no live socket, so nothing
    /// of it was sent.
    ShutdownFailed,
    /// The tailnet host issued this device a token and it is stored; the
    /// host is being redialled with it.
    Paired,
    /// The pairing request is gone with its socket (the socket closed while
    /// it was pending, or a code came with no socket): it must be asked
    /// again.
    PairingLost,
    /// The tailnet host issued this device a token but storing it failed, so
    /// it is lost (the host shows it once): pair again. No redial follows.
    TokenSaveFailed,
}

/// Spawns the network thread on the production [`NetDeps`], connecting to
/// `target` first. It runs until the view drops its command sender.
pub fn spawn(
    target: Target,
    commands: UnboundedReceiver<NetCommand>,
    events: UnboundedSender<NetEvent>,
) {
    spawn_with(NetDeps::production(), target, commands, events);
}

/// Spawns the network thread on `deps`, connecting to `target` first: a
/// tailnet target never starts the local daemon. It runs until the view
/// drops its command sender.
pub fn spawn_with(
    deps: NetDeps,
    target: Target,
    commands: UnboundedReceiver<NetCommand>,
    events: UnboundedSender<NetEvent>,
) {
    let spawned = std::thread::Builder::new()
        .name("net".to_owned())
        .spawn(move || run_thread(commands, events, deps, target, WELCOME_TIMEOUT));
    if let Err(err) = spawned {
        error!("spawning the network thread: {err}");
    }
}

fn ensure_daemon() -> EnsureFuture {
    Box::pin(daemon_client::ensure_running(NATIVE_PROTOCOL_VERSIONS))
}

/// The network thread's body: build the runtime and run the loop on it until
/// the command channel closes, connecting to `target` first.
fn run_thread(
    commands: UnboundedReceiver<NetCommand>,
    events: UnboundedSender<NetEvent>,
    deps: NetDeps,
    target: Target,
    welcome_timeout: Duration,
) {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        // `Net::new` builds a tokio interval, so it runs inside the runtime.
        Ok(rt) => rt.block_on(async move {
            Net::new(commands, events, deps, target, welcome_timeout)
                .run()
                .await;
        }),
        Err(err) => {
            error!("building the network runtime: {err}");
            let mut conn = Connection::for_target(target);
            let _ = conn.on_error(format!("building the network runtime: {err}"));
            // The view may already be gone; nothing else to tell.
            let _ = events.unbounded_send(NetEvent::State(conn));
        }
    }
}

/// Where the loop goes next.
enum Next {
    /// Connect now.
    Connect,
    /// Wait this long before connecting, or with `None` until a command.
    Wait(Option<Duration>),
    /// The view is gone; end the thread.
    Quit,
}

/// What one pumped event asks of the open socket.
enum Step {
    Continue,
    /// Drop the socket and treat it as closed for this reason.
    Close(String),
    /// Drop the socket and go on to `Next` without a close.
    Leave(Next),
}

struct Net {
    commands: UnboundedReceiver<NetCommand>,
    events: UnboundedSender<NetEvent>,
    conn: Connection,
    deps: NetDeps,
    /// How long after `Hello` the daemon has to answer.
    welcome_timeout: Duration,
    ticker: Interval,
    watchdog: Watchdog,
    /// Whether the tailnet `Hello` on the current socket carried a stored
    /// device token.
    had_token: bool,
    /// A tailnet `Welcome` held back until the next frame says whether the
    /// connection is served or a pairing one.
    held_welcome: Option<Box<DaemonMessage>>,
    /// A [`NetCommand::PairRequest`] that found no pairing socket: the
    /// redial sends `DevicePairRequest` once the host answers "not
    /// approved". A target switch, a restart or a cancel clears it.
    pair_on_connect: bool,
    /// Whether a pairing request sent on the current socket is still
    /// waiting on it: its close then loses the request.
    request_live: bool,
    /// The id of the pairing request the current socket's host reported
    /// pending, which a redeem names.
    request_id: Option<String>,
}

impl Net {
    /// Must run inside the tokio runtime: the watchdog's interval needs its
    /// timer.
    fn new(
        commands: UnboundedReceiver<NetCommand>,
        events: UnboundedSender<NetEvent>,
        deps: NetDeps,
        target: Target,
        welcome_timeout: Duration,
    ) -> Self {
        let mut ticker = tokio::time::interval_at(Instant::now() + TICK, TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            commands,
            events,
            conn: Connection::for_target(target),
            deps,
            welcome_timeout,
            ticker,
            watchdog: Watchdog::new(SystemTime::now()),
            had_token: false,
            held_welcome: None,
            pair_on_connect: false,
            request_live: false,
            request_id: None,
        }
    }

    async fn run(mut self) {
        self.emit_state();
        let mut next = Next::Connect;
        loop {
            next = match next {
                Next::Connect => self.attempt().await,
                Next::Wait(delay) => self.wait(delay).await,
                Next::Quit => return,
            };
        }
    }

    fn emit(&self, event: NetEvent) {
        // A closed channel means the view is gone; the command channel closes
        // with it and ends the loop.
        let _ = self.events.unbounded_send(event);
    }

    fn emit_state(&self) {
        self.emit(NetEvent::State(self.conn.clone()));
    }

    fn closed(&mut self, reason: String) -> Next {
        info!(%reason, "daemon connection closed");
        let delay = self.conn.on_closed(reason);
        self.emit_state();
        self.request_id = None;
        if std::mem::take(&mut self.request_live) {
            info!("the pairing request was lost with its socket");
            self.emit(NetEvent::PairingLost);
        }
        Next::Wait(delay)
    }

    /// Whether the connection is a pairing one: the only state the pairing
    /// commands act in.
    fn not_approved(&self) -> bool {
        matches!(self.conn.state(), State::NotApproved { .. })
    }

    /// Forget every pairing step in flight, as a target switch or a restart
    /// does.
    fn forget_pairing(&mut self) {
        self.pair_on_connect = false;
        self.request_live = false;
        self.request_id = None;
    }

    /// One connection attempt on the current target. A command that arrives
    /// while the daemon or the socket is still coming up is handled at once
    /// and may cancel the attempt.
    async fn attempt(&mut self) -> Next {
        match self.conn.target().clone() {
            Target::Local => self.attempt_local().await,
            Target::Tailnet(host) => self.attempt_tailnet(&host).await,
        }
    }

    /// The local daemon: ensure it runs, open the loopback socket, pump it
    /// with the `daemon.json` token.
    async fn attempt_local(&mut self) -> Next {
        let ensured = match self.or_command((self.deps.ensure)()).await {
            Ok(ensured) => ensured,
            Err(next) => return next,
        };
        let handshake = match ensured {
            Ok(handshake) => handshake,
            Err(err) => {
                let reason = format!("{err:#}");
                warn!(%reason, "ensuring the daemon is running failed");
                let delay = self.conn.on_error(reason);
                self.emit_state();
                return Next::Wait(delay);
            }
        };
        self.emit(NetEvent::Handshake(HandshakeInfo::from(&handshake)));
        self.conn.on_connecting();
        self.emit_state();
        let url = format!("ws://127.0.0.1:{}/ws", handshake.port);
        let connect = Box::pin(tokio_tungstenite::connect_async(&url));
        let ws = match self.or_command(connect).await {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(err)) => return self.closed(format!("connecting to {url}: {err}")),
            Err(next) => return next,
        };
        self.conn.on_socket_open(handshake.port);
        self.pump(ws, handshake.auth_token).await
    }

    /// A tailnet host: never a local daemon step. Dial its listener over TLS
    /// and pump it with the device token held for the host, or an empty one
    /// when none is held (the `daemon.json` token never leaves this machine):
    /// the host then answers "not approved" and the socket stays open as a
    /// pairing connection.
    async fn attempt_tailnet(&mut self, host: &TailnetHost) -> Next {
        let token = self.device_token(host);
        self.had_token = token.is_some();
        let config = match self.tls_config() {
            Ok(config) => config,
            Err(err) => {
                let reason = format!("{err:#}");
                warn!(%reason, "no TLS configuration for the tailnet host");
                return self.failed(reason);
            }
        };
        self.conn.on_connecting();
        self.emit_state();
        let url = host.url();
        let connect = Box::pin(tokio_tungstenite::connect_async_tls_with_config(
            url.as_str(),
            None,
            false,
            Some(Connector::Rustls(config)),
        ));
        let ws = match self.or_command(connect).await {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(err)) => return self.closed(format!("connecting to {url}: {err}")),
            Err(next) => return next,
        };
        self.conn.on_socket_open(host.port());
        // An empty token is how an unpaired device asks the host to pair.
        self.pump(ws, token.unwrap_or_default()).await
    }

    /// The device token held for `host`; `None` when none is held or the
    /// store cannot be read.
    fn device_token(&self, host: &TailnetHost) -> Option<String> {
        match self.deps.tokens.get(host.name()) {
            Ok(token) => token,
            Err(err) => {
                warn!(
                    host = host.name(),
                    "reading the device token failed; dialling without one: {err:#}"
                );
                None
            }
        }
    }

    /// The attempt failed before a socket opened: show `reason` and wait the
    /// backoff.
    fn failed(&mut self, reason: String) -> Next {
        let delay = self.conn.on_error(reason);
        self.emit_state();
        Next::Wait(delay)
    }

    /// The client config a tailnet dial uses: the one the deps carry, else
    /// the OS store's, built on first use and kept for this thread.
    fn tls_config(&mut self) -> Result<Arc<ClientConfig>> {
        if let Some(config) = &self.deps.tls {
            return Ok(Arc::clone(config));
        }
        let config = os_tls_config()?;
        self.deps.tls = Some(Arc::clone(&config));
        Ok(config)
    }

    /// Drive `work` to completion unless a command cuts it short. Commands
    /// are checked first, so a closed channel ends the loop before `work` is
    /// ever polled. `Err` carries where the loop goes instead; dropping `work`
    /// then cancels it (the spawn lock `ensure_running` holds is released on
    /// drop).
    async fn or_command<F: Future + Unpin>(&mut self, mut work: F) -> Result<F::Output, Next> {
        loop {
            tokio::select! {
                biased;
                command = self.commands.next() => {
                    if let Some(next) = self.on_offline_command(command).await {
                        return Err(next);
                    }
                }
                output = &mut work => return Ok(output),
            }
        }
    }

    fn hello(&self, auth_token: String) -> ClientMessage {
        ClientMessage::Hello {
            protocol_version: protocol::PROTOCOL_VERSION,
            protocol_versions: NATIVE_PROTOCOL_VERSIONS.to_vec(),
            auth_token,
            client_id: self.deps.identity.as_ref().map(|id| id.client_id.clone()),
            client_name: self
                .deps
                .identity
                .as_ref()
                .and_then(|id| id.client_name.clone()),
        }
    }

    /// Pump the open socket both ways until it closes or a command leaves it.
    async fn pump(&mut self, mut ws: Socket, auth_token: String) -> Next {
        self.held_welcome = None;
        self.request_live = false;
        self.request_id = None;
        if let Err(err) = send(&mut ws, &self.hello(auth_token)).await {
            return self.closed(format!("sending Hello: {err:#}"));
        }
        let mut probe_deadline: Option<Instant> = None;
        let mut welcome_deadline = Some(Instant::now() + self.welcome_timeout);
        loop {
            let step = tokio::select! {
                frame = ws.next() => self.on_frame(frame, &mut probe_deadline),
                command = self.commands.next() => self.on_live_command(command, &mut ws).await,
                _ = self.ticker.tick() => self.on_live_tick(&mut ws, &mut probe_deadline).await,
                () = sleep_until(probe_deadline) => {
                    warn!("no reply to the resume probe; dropping the socket");
                    Step::Close(format!("no reply to the resume probe within {PROBE_TIMEOUT:?}"))
                }
                () = sleep_until(welcome_deadline) => {
                    warn!(timeout = ?self.welcome_timeout, "no welcome from daemon; dropping the socket");
                    Step::Close("no welcome from daemon".to_owned())
                }
            };
            // Welcome (on a tailnet host, the frame after it), AuthFailed or
            // not approved moves the machine out of Connecting.
            if !matches!(self.conn.state(), State::Connecting) {
                welcome_deadline = None;
            }
            let step = self.request_on_connect(&mut ws, step).await;
            match step {
                Step::Continue => {}
                Step::Close(reason) => return self.closed(reason),
                Step::Leave(next) => return next,
            }
        }
    }

    /// Once a redial asked for by [`NetCommand::PairRequest`] reaches "not
    /// approved", send the request it was made for.
    async fn request_on_connect(&mut self, ws: &mut Socket, step: Step) -> Step {
        if !matches!(step, Step::Continue) || !self.pair_on_connect || !self.not_approved() {
            return step;
        }
        self.pair_on_connect = false;
        self.send_pair_request(ws).await
    }

    async fn send_pair_request(&mut self, ws: &mut Socket) -> Step {
        info!(daemon = ?self.conn.target(), "asking the tailnet host to pair this device");
        // Live before the send, so a failed send's close reports it lost; the
        // old request's id names nothing once this one replaces it.
        self.request_live = true;
        self.request_id = None;
        match send(ws, &ClientMessage::DevicePairRequest).await {
            Ok(()) => Step::Continue,
            Err(err) => Step::Close(format!("sending the pairing request: {err:#}")),
        }
    }

    fn on_frame(&mut self, frame: Frame, probe_deadline: &mut Option<Instant>) -> Step {
        let text = match frame {
            None => return Step::Close("daemon closed the connection".to_owned()),
            Some(Err(err)) => return Step::Close(format!("reading websocket frame: {err}")),
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(_)) => {
                *probe_deadline = None;
                return Step::Continue;
            }
        };
        *probe_deadline = None;
        match InboundDaemonMessage::from_json_str(text.as_str()) {
            Ok(InboundDaemonMessage::Known(msg)) => return self.on_daemon_message(msg),
            Ok(InboundDaemonMessage::Unknown { type_tag, .. }) => {
                warn!("unknown or undecodable daemon message type: {type_tag}");
                self.release_welcome();
            }
            Err(err) => error!("undecodable daemon message: {err}"),
        }
        Step::Continue
    }

    /// On a tailnet host, `Welcome` is held until the next frame: a
    /// `DevicePairFailed` "device not approved" makes the connection a
    /// pairing one (the `Welcome` never reaches the view, so it never shows
    /// as open), and any other frame releases the `Welcome` first. A
    /// `DeviceToken` is stored here and never reaches the view.
    fn on_daemon_message(&mut self, msg: Box<DaemonMessage>) -> Step {
        if let DaemonMessage::DeviceToken { token, .. } = &*msg {
            return self.land_token(&token.0);
        }
        self.note_pairing_step(&msg);
        if self.held_welcome.is_some() {
            if is_not_approved(&msg) {
                self.held_welcome = None;
                self.on_not_approved();
                self.emit(NetEvent::Message(msg));
                return Step::Continue;
            }
            self.release_welcome();
        } else if self.holds_welcome(&msg) {
            self.held_welcome = Some(msg);
            return Step::Continue;
        }
        self.apply(msg);
        Step::Continue
    }

    /// Track the pairing request through the host's answers: its id once
    /// pending, and its end on any outcome but approved (the host closes the
    /// socket next, and the view already has the outcome).
    fn note_pairing_step(&mut self, msg: &DaemonMessage) {
        match msg {
            DaemonMessage::DevicePairPending { request_id, .. } => {
                self.request_id = Some(request_id.clone());
            }
            DaemonMessage::DevicePairDecided { outcome, .. }
                if *outcome != DevicePairOutcome::Approved =>
            {
                self.request_live = false;
            }
            _ => {}
        }
    }

    /// Store the device token the host issued and redial it at once with the
    /// token. A failed write loses the token (the host shows it once): it is
    /// reported and nothing redials.
    fn land_token(&mut self, token: &str) -> Step {
        self.request_live = false;
        self.request_id = None;
        let Target::Tailnet(host) = self.conn.target().clone() else {
            warn!("the local daemon sent a device token; ignored");
            return Step::Continue;
        };
        match self.deps.tokens.set(host.name(), token) {
            Ok(()) => {
                info!(
                    host = host.name(),
                    "paired with the tailnet host; reconnecting with the device token"
                );
                self.emit(NetEvent::Paired);
                Step::Leave(Next::Connect)
            }
            Err(err) => {
                warn!(
                    host = host.name(),
                    "saving the device token failed; the token is lost, pair again: {err:#}"
                );
                self.emit(NetEvent::TokenSaveFailed);
                Step::Continue
            }
        }
    }

    /// Whether `msg` is a `Welcome` to hold back: a tailnet host's first.
    fn holds_welcome(&self, msg: &DaemonMessage) -> bool {
        matches!(msg, DaemonMessage::Welcome { .. })
            && !self.conn.target().is_local()
            && matches!(self.conn.state(), State::Connecting)
    }

    /// Handle the held `Welcome`, if any, as if it had just arrived.
    fn release_welcome(&mut self) {
        if let Some(welcome) = self.held_welcome.take() {
            self.apply(welcome);
        }
    }

    /// The tailnet host has not approved this device: report it, keep the
    /// socket for the pairing steps, and never reconnect on its close.
    fn on_not_approved(&mut self) {
        info!(
            daemon = ?self.conn.target(),
            had_token = self.had_token,
            "the tailnet host has not approved this device; keeping the pairing connection"
        );
        self.conn
            .on_not_approved(protocol::DEVICE_NOT_APPROVED.to_owned(), self.had_token);
        self.emit_state();
    }

    fn apply(&mut self, msg: Box<DaemonMessage>) {
        match &*msg {
            DaemonMessage::Welcome {
                protocol_version,
                features,
                ..
            } => {
                info!(protocol_version, "connected to daemon");
                self.conn.on_welcome(*protocol_version, features.clone());
                self.emit_state();
            }
            DaemonMessage::AuthFailed { reason } => {
                warn!(%reason, "daemon rejected the auth token");
                self.conn.on_auth_failed(reason.clone());
                self.emit_state();
            }
            _ => {}
        }
        self.emit(NetEvent::Message(msg));
    }

    async fn on_live_command(&mut self, command: Option<NetCommand>, ws: &mut Socket) -> Step {
        match command {
            None => Step::Leave(Next::Quit),
            Some(NetCommand::Send(msg)) => match send(ws, &msg).await {
                Ok(()) => Step::Continue,
                Err(err) => Step::Close(format!("{err:#}")),
            },
            Some(NetCommand::Restart) => match self.restart() {
                RestartAction::SendShutdown => {
                    info!("restart: asking the daemon to shut down");
                    match send(ws, &ClientMessage::Shutdown { drain: false }).await {
                        Ok(()) => Step::Continue,
                        Err(err) => Step::Close(format!("{err:#}")),
                    }
                }
                RestartAction::FreshConnect => {
                    info!("restart: connecting from scratch");
                    self.emit_state();
                    Step::Leave(Next::Connect)
                }
            },
            Some(NetCommand::Stop) => {
                self.stop().await;
                Step::Continue
            }
            Some(NetCommand::Shutdown { before, drain }) => self.shutdown(ws, &before, drain).await,
            Some(NetCommand::SwitchTarget(target)) => {
                self.switch_target(target);
                Step::Leave(Next::Connect)
            }
            Some(
                command @ (NetCommand::PairRequest
                | NetCommand::PairRedeem { .. }
                | NetCommand::PairCancel),
            ) => self.on_live_pair_command(command, ws).await,
        }
    }

    /// A pairing command while a socket is open.
    async fn on_live_pair_command(&mut self, command: NetCommand, ws: &mut Socket) -> Step {
        // Before the state check: a redial's socket can be open and not yet
        // answered when the cancel comes, and the request must not follow.
        if matches!(command, NetCommand::PairCancel) {
            return self.cancel_on_socket(&command, ws).await;
        }
        if !self.not_approved() {
            drop_pair_command(&command, self.conn.state());
            return Step::Continue;
        }
        match command {
            NetCommand::PairRequest => self.send_pair_request(ws).await,
            NetCommand::PairRedeem { code } => self.redeem_on_socket(code, ws).await,
            _ => Step::Continue,
        }
    }

    /// Withdraw the pairing request: no request on the redial, and
    /// `DevicePairCancel` when one is waiting on this socket.
    async fn cancel_on_socket(&mut self, command: &NetCommand, ws: &mut Socket) -> Step {
        let live = self.request_live;
        if !live && !self.pair_on_connect {
            drop_pair_command(command, self.conn.state());
            return Step::Continue;
        }
        info!(on_the_socket = live, "cancelling the pairing request");
        self.forget_pairing();
        if !live {
            return Step::Continue;
        }
        match send(ws, &ClientMessage::DevicePairCancel).await {
            Ok(()) => Step::Continue,
            Err(err) => Step::Close(format!("sending the pairing cancel: {err:#}")),
        }
    }

    /// Redeem `code` for the request pending on this socket; with none
    /// pending nothing is sent.
    async fn redeem_on_socket(&mut self, code: String, ws: &mut Socket) -> Step {
        let Some(request_id) = self.request_id.clone() else {
            debug!("no pending pairing request on this socket; dropping PairRedeem");
            return Step::Continue;
        };
        info!(%request_id, "redeeming the pairing code");
        let msg = ClientMessage::DevicePairRedeem {
            request_id,
            code: protocol::Secret(code),
        };
        match send(ws, &msg).await {
            Ok(()) => Step::Continue,
            Err(err) => Step::Close(format!("sending the pairing code: {err:#}")),
        }
    }

    /// A pairing command while no socket is open.
    fn on_offline_pair_command(&mut self, command: &NetCommand) -> Option<Next> {
        if matches!(command, NetCommand::PairCancel) && self.pair_on_connect {
            info!("cancelling the pairing request before the redial asked");
            self.pair_on_connect = false;
            return None;
        }
        if !self.not_approved() {
            drop_pair_command(command, self.conn.state());
            return None;
        }
        match command {
            NetCommand::PairRequest => {
                info!(daemon = ?self.conn.target(), "the pairing socket is gone; redialling to ask");
                self.pair_on_connect = true;
                Some(Next::Connect)
            }
            NetCommand::PairRedeem { .. } => {
                debug!(
                    ?command,
                    "no pairing socket, so its request is gone; dropped"
                );
                self.emit(NetEvent::PairingLost);
                None
            }
            _ => {
                debug!(?command, "no pairing socket; nothing to cancel");
                None
            }
        }
    }

    /// [`Connection::restart`], forgetting the pairing steps in flight.
    fn restart(&mut self) -> RestartAction {
        self.forget_pairing();
        self.conn.restart()
    }

    fn switch_target(&mut self, target: Target) {
        info!(daemon = ?target, "switching the daemon target; connecting from scratch");
        self.forget_pairing();
        self.conn.switch_target(target);
        self.emit_state();
    }

    /// Sends `before`, then the shutdown, and reports it sent. A failed send
    /// closes the socket; the view's wait then offers Force quit.
    async fn shutdown(&mut self, ws: &mut Socket, before: &[ClientMessage], drain: bool) -> Step {
        if !self.conn.target().is_local() {
            warn!(
                drain,
                before = before.len(),
                "quit on a tailnet host: a remote daemon is never shut down from here; nothing sent"
            );
            self.emit(NetEvent::ShutdownFailed);
            return Step::Continue;
        }
        info!(
            drain,
            before = before.len(),
            "quitting: sending the stops, then asking the daemon to shut down; no reconnect after"
        );
        for msg in before {
            if let Err(err) = send(ws, msg).await {
                return Step::Close(format!("{err:#}"));
            }
        }
        // Before the send, so the close that follows never reconnects.
        self.conn.stop();
        match send(ws, &ClientMessage::Shutdown { drain }).await {
            Ok(()) => {
                self.emit(NetEvent::ShutdownSent);
                Step::Continue
            }
            Err(err) => Step::Close(format!("{err:#}")),
        }
    }

    async fn on_live_tick(
        &mut self,
        ws: &mut Socket,
        probe_deadline: &mut Option<Instant>,
    ) -> Step {
        if !self.resumed() || !self.conn.is_open() {
            return Step::Continue;
        }
        match send(ws, &ClientMessage::ListRepos).await {
            Ok(()) => {
                *probe_deadline = Some(Instant::now() + PROBE_TIMEOUT);
                Step::Continue
            }
            Err(err) => Step::Close(format!("sending the resume probe: {err:#}")),
        }
    }

    /// Feed the watchdog; true when the machine just resumed from sleep.
    fn resumed(&mut self) -> bool {
        let resumed = self.watchdog.tick(SystemTime::now());
        if resumed {
            info!(state = ?self.conn.state(), "system resume detected");
        }
        resumed
    }

    /// Force-stop the local daemon; true when it was stopped. A tailnet
    /// host's daemon is never stopped from here.
    async fn stop(&mut self) -> bool {
        if !self.conn.can_stop() {
            warn!(daemon = ?self.conn.target(), "stop: not offered on a tailnet host; ignored");
            return false;
        }
        info!("stopping the daemon");
        self.conn.stop();
        self.emit_state();
        if let Err(err) = (self.deps.stop)().await {
            warn!("stopping the daemon: {err:#}");
        }
        true
    }

    /// Wait for the reconnect delay (or, with `None`, only for a command).
    async fn wait(&mut self, delay: Option<Duration>) -> Next {
        let deadline = delay.map(|delay| Instant::now() + delay);
        loop {
            tokio::select! {
                biased;
                command = self.commands.next() => {
                    if let Some(next) = self.on_offline_command(command).await {
                        return next;
                    }
                }
                () = sleep_until(deadline) => return Next::Connect,
                _ = self.ticker.tick() => {
                    self.resumed();
                }
            }
        }
    }

    /// Handle a command while no socket is open (waiting, or mid-attempt).
    /// `Some` is where the loop goes next; `None` carries on.
    async fn on_offline_command(&mut self, command: Option<NetCommand>) -> Option<Next> {
        match command {
            None => Some(Next::Quit),
            Some(NetCommand::Send(_)) => {
                debug!("not connected; dropping an outbound message");
                None
            }
            Some(NetCommand::Restart) => {
                // With no live socket every restart is a fresh connect.
                let _ = self.restart();
                info!("restart: connecting from scratch");
                self.emit_state();
                Some(Next::Connect)
            }
            Some(NetCommand::Stop) => self.stop().await.then_some(Next::Wait(None)),
            Some(NetCommand::SwitchTarget(target)) => {
                self.switch_target(target);
                Some(Next::Connect)
            }
            Some(NetCommand::Shutdown { before, drain }) => {
                warn!(
                    drain,
                    before = before.len(),
                    "quit while disconnected: nothing sent; the exit dialog stays open"
                );
                self.emit(NetEvent::ShutdownFailed);
                None
            }
            Some(
                command @ (NetCommand::PairRequest
                | NetCommand::PairRedeem { .. }
                | NetCommand::PairCancel),
            ) => self.on_offline_pair_command(&command),
        }
    }
}

/// A pairing command outside a pairing connection does nothing.
fn drop_pair_command(command: &NetCommand, state: &State) {
    debug!(
        ?command,
        ?state,
        "not a pairing connection; dropping the pairing command"
    );
}

/// Whether `msg` is the daemon saying this device is not approved.
fn is_not_approved(msg: &DaemonMessage) -> bool {
    matches!(
        msg,
        DaemonMessage::DevicePairFailed { reason } if reason == protocol::DEVICE_NOT_APPROVED
    )
}

async fn send(ws: &mut Socket, msg: &ClientMessage) -> Result<()> {
    let text = serde_json::to_string(msg).context("encoding a client message")?;
    ws.send(Message::Text(text.into()))
        .await
        .context("writing to the daemon socket")
}

/// Sleep until `deadline`, or forever without one.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        EnsureFuture, Net, NetCommand, NetDeps, NetEvent, WELCOME_TIMEOUT, run_thread,
        tls_config_trusting,
    };
    use crate::connection::{Connection, State};
    use crate::target::Target;
    use daemon_client::device_tokens::{DeviceTokenStore, MemoryTokens};
    use futures::StreamExt as _;
    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
    use protocol::ClientMessage;
    use protocol::DEVICE_NOT_APPROVED;
    use protocol::DaemonHandshake;
    use protocol::DaemonMessage;
    use protocol::DevicePairOutcome;
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::cell::Cell;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};
    use tokio_tungstenite::tungstenite;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    /// How long the TLS tests wait for an event before calling it missing: a
    /// backstop only, never the expected wait.
    const BACKSTOP: Duration = Duration::from_secs(30);

    /// Deps that ensure with `ensure`, send no identity and never stop.
    fn deps(ensure: fn() -> EnsureFuture) -> NetDeps {
        NetDeps {
            ensure: Box::new(ensure),
            identity: None,
            stop: Box::new(|| Box::pin(async { Ok(()) })),
            tokens: Arc::new(MemoryTokens::default()),
            tls: None,
        }
    }

    /// The port of the silent test server `ensure_silent_server` hands out.
    static SILENT_PORT: AtomicU16 = AtomicU16::new(0);
    /// WebSocket upgrades the silent test server has completed.
    static SILENT_ACCEPTS: AtomicUsize = AtomicUsize::new(0);

    /// Stand-in for `ensure_running` that names the silent test server.
    fn ensure_silent_server() -> EnsureFuture {
        Box::pin(async {
            Ok(DaemonHandshake {
                protocol_version: 23,
                port: SILENT_PORT.load(Ordering::SeqCst),
                auth_token: "test-token".to_owned(),
                pid: 0,
                supported_versions: vec![23],
            })
        })
    }

    /// A loopback WebSocket server that completes `accepts` upgrades and never
    /// answers `Hello`; it returns the sockets so they stay open.
    fn silent_server(
        accepts: usize,
    ) -> std::thread::JoinHandle<Vec<tungstenite::WebSocket<std::net::TcpStream>>> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().expect("listener address").port();
        SILENT_PORT.store(port, Ordering::SeqCst);
        std::thread::spawn(move || {
            (0..accepts)
                .map(|_| {
                    let (stream, _) = listener.accept().expect("accept a connection");
                    let socket = tungstenite::accept(stream).expect("websocket upgrade");
                    SILENT_ACCEPTS.fetch_add(1, Ordering::SeqCst);
                    socket
                })
                .collect()
        })
    }

    /// Wait up to 5 s for a state event whose state is `Closed { reason }`.
    fn saw_closed(events: &mut UnboundedReceiver<NetEvent>, reason: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match events.try_recv() {
                Ok(NetEvent::State(conn)) if conn.reason() == Some(reason) => {
                    if matches!(conn.state(), State::Closed { .. }) {
                        return true;
                    }
                }
                Ok(_) => {}
                Err(err) if err.is_closed() => return false,
                Err(_) => std::thread::yield_now(),
            }
        }
        false
    }

    static CLOSED_POLLS: AtomicUsize = AtomicUsize::new(0);
    static RESTART_POLLS: AtomicUsize = AtomicUsize::new(0);

    /// Stand-in for `ensure_running` that counts its polls and never finishes.
    fn ensure_never_after_close() -> EnsureFuture {
        Box::pin(async {
            CLOSED_POLLS.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        })
    }

    fn ensure_counting_restarts() -> EnsureFuture {
        Box::pin(async {
            RESTART_POLLS.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        })
    }

    fn reaches(count: &AtomicUsize, at_least: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if count.load(Ordering::SeqCst) >= at_least {
                return true;
            }
            std::thread::yield_now();
        }
        false
    }

    #[test]
    fn net_thread_starts_and_exits_when_channels_close() {
        let (view_tx, commands) = unbounded::<NetCommand>();
        drop(view_tx);
        let (events, _view_rx) = unbounded();

        run_thread(
            commands,
            events,
            deps(ensure_never_after_close),
            Target::Local,
            WELCOME_TIMEOUT,
        );

        assert_eq!(
            CLOSED_POLLS.load(Ordering::SeqCst),
            0,
            "ensure_running must not start once the command channel is closed"
        );
    }

    #[test]
    fn restart_during_ensure_restarts_the_attempt() {
        let (view_tx, commands) = unbounded();
        let (events, _view_rx) = unbounded();
        let net = std::thread::spawn(move || {
            run_thread(
                commands,
                events,
                deps(ensure_counting_restarts),
                Target::Local,
                WELCOME_TIMEOUT,
            );
        });

        assert!(
            reaches(&RESTART_POLLS, 1),
            "the first attempt never started"
        );
        view_tx
            .unbounded_send(NetCommand::Restart)
            .expect("net thread is running");
        assert!(
            reaches(&RESTART_POLLS, 2),
            "Restart during ensure did not start a new attempt"
        );
        drop(view_tx);
        net.join()
            .expect("net thread exits when the command channel closes");
    }

    #[test]
    fn missing_welcome_closes_and_reconnects() {
        let server = silent_server(2);
        let (view_tx, commands) = unbounded();
        let (events, mut view_rx) = unbounded();
        let net = std::thread::spawn(move || {
            run_thread(
                commands,
                events,
                deps(ensure_silent_server),
                Target::Local,
                Duration::from_millis(200),
            );
        });

        assert!(
            saw_closed(&mut view_rx, "no welcome from daemon"),
            "a daemon that never answers Hello was not dropped"
        );
        assert!(
            reaches(&SILENT_ACCEPTS, 2),
            "no reconnect after the welcome timeout"
        );
        let sockets = server.join().expect("silent server finishes");
        drop(view_tx);
        net.join()
            .expect("net thread exits when the command channel closes");
        drop(sockets);
    }

    #[test]
    fn native_hello_offers_only_the_current_version() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a test runtime");
        let hello = runtime.block_on(async {
            let (_commands_tx, commands) = unbounded();
            let (events, _events_rx) = unbounded();
            let net = Net::new(
                commands,
                events,
                deps(ensure_silent_server),
                Target::Local,
                WELCOME_TIMEOUT,
            );
            net.hello("test-token".to_owned())
        });
        let ClientMessage::Hello {
            protocol_version,
            protocol_versions,
            ..
        } = hello
        else {
            unreachable!("hello() builds a Hello, got {hello:?}");
        };
        assert_eq!(protocol_version, protocol::PROTOCOL_VERSION);
        assert_eq!(
            protocol_versions,
            protocol::SUPPORTED_PROTOCOL_VERSIONS.to_vec()
        );
    }

    /// The name the fake TLS daemon's certificate is issued for; it resolves
    /// to loopback, where the daemon listens.
    const TLS_HOST: &str = "localhost";
    /// The device token the tailnet tests hold for [`TLS_HOST`].
    const DEVICE_TOKEN: &str = "device-token";

    /// A test certificate authority.
    struct TestCa {
        der: CertificateDer<'static>,
        issuer: Issuer<'static, KeyPair>,
    }

    impl TestCa {
        fn new() -> Self {
            let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(DnType::CommonName, "rustling-tulip native test CA");
            params.key_usages = vec![
                KeyUsagePurpose::KeyCertSign,
                KeyUsagePurpose::DigitalSignature,
            ];
            let key = KeyPair::generate().expect("CA key");
            let cert = params.self_signed(&key).expect("self-signed CA");
            Self {
                der: cert.der().clone(),
                issuer: Issuer::new(params, key),
            }
        }

        /// A leaf for [`TLS_HOST`] with a fresh key, signed by this CA.
        fn leaf(&self) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
            let mut params =
                CertificateParams::new(vec![TLS_HOST.to_owned()]).expect("leaf params");
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            let key = KeyPair::generate().expect("leaf key");
            let cert = params
                .signed_by(&key, &self.issuer)
                .expect("leaf signed by the CA");
            let key = PrivatePkcs8KeyDer::from(key.serialize_der());
            (cert.der().clone(), key.into())
        }

        /// A client config whose only root is this CA.
        fn client_config(&self) -> Arc<rustls::ClientConfig> {
            tls_config_trusting(vec![self.der.clone()]).expect("a config trusting the test CA")
        }
    }

    /// What the fake TLS daemon saw.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Seen {
        /// A TCP connection was accepted.
        Accepted,
        /// The WebSocket upgrade request's `Host` and `Origin` headers.
        Upgrade {
            host: Option<String>,
            origin: Option<String>,
        },
        /// The TLS handshake or the WebSocket upgrade failed.
        TlsFailed,
        /// A `Hello` carrying this auth token.
        Hello(String),
        /// A `DevicePairRedeem` for this request with this code.
        Redeem { request_id: String, code: String },
        /// Any other client message, by its type tag.
        Message(String),
        /// An upgraded connection closed.
        Closed,
    }

    /// How the fake TLS daemon answers a `Hello`, whatever its token.
    #[derive(Debug, Clone, Copy)]
    enum Answer {
        /// `Welcome`, then `Repos`, as a daemon serving a paired device does.
        Served,
        /// `Welcome`, then `DevicePairFailed` "device not approved", keeping
        /// the socket open as a pairing connection.
        NotApproved,
        /// As `NotApproved`, then the daemon closes the socket, as its idle
        /// timer does.
        NotApprovedThenClose,
        /// As `NotApproved`, then the pairing steps: a `DevicePairRequest`
        /// gets `DevicePairPending` and at once `DevicePairDecided`
        /// approved; a redeem of [`GOOD_CODE`] gets `DeviceToken`
        /// [`ISSUED_TOKEN`] and the close, any other code "That code is not
        /// right." on the open socket; a cancel closes it.
        Pairing,
        /// As `Pairing`, but the daemon closes the socket right after its
        /// `DevicePairPending`, dropping the request.
        PairingThenDrop,
        /// As `Pairing`, but only the socket's first request is answered;
        /// later ones are only recorded.
        PairingOnce,
        /// `Hello` gets no answer until the client's next `ListRepos`, which
        /// is answered with `Welcome` and "device not approved": a test
        /// marker that holds the connection between socket open and "not
        /// approved".
        NotApprovedOnMarker,
    }

    impl Answer {
        /// Whether this daemon answers the pairing steps.
        fn pairs(self) -> bool {
            matches!(
                self,
                Self::Pairing | Self::PairingThenDrop | Self::PairingOnce
            )
        }
    }

    /// Send `Welcome`, then "device not approved".
    fn send_not_approved<S: std::io::Read + std::io::Write>(ws: &mut tungstenite::WebSocket<S>) {
        let welcome = DaemonMessage::Welcome {
            protocol_version: 23,
            supported_versions: vec![23],
            features: Vec::new(),
        };
        let refusal = DaemonMessage::DevicePairFailed {
            reason: DEVICE_NOT_APPROVED.to_owned(),
        };
        if send_daemon_message(ws, &welcome) {
            let _ = send_daemon_message(ws, &refusal);
        }
    }

    /// The token the fake daemon issues for [`GOOD_CODE`]; a `Hello`
    /// carrying it is served whatever the daemon's [`Answer`].
    const ISSUED_TOKEN: &str = "issued-device-token";
    /// The code the fake daemon accepts.
    const GOOD_CODE: &str = "135790";
    /// A code the fake daemon refuses.
    const WRONG_CODE: &str = "246810";
    /// The id of the fake daemon's pairing request.
    const REQUEST_ID: &str = "request-1";
    /// The fake daemon's refusal of a wrong code.
    const WRONG_CODE_REASON: &str = "That code is not right.";

    /// A loopback TLS WebSocket daemon whose certificate for [`TLS_HOST`] is
    /// signed by `ca`. It serves each `Hello` ([`Answer::Served`]) and
    /// reports what it sees on the returned receiver.
    fn tls_daemon(ca: &TestCa) -> (u16, mpsc::Receiver<Seen>) {
        tls_daemon_answering(ca, Answer::Served)
    }

    /// [`tls_daemon`], answering each `Hello` as `answer` says.
    fn tls_daemon_answering(ca: &TestCa, answer: Answer) -> (u16, mpsc::Receiver<Seen>) {
        tls_daemon_scripted(ca, vec![answer])
    }

    /// [`tls_daemon`], answering on its n-th connection as `answers[n]` says
    /// and on every connection past the list as its last entry says.
    fn tls_daemon_scripted(ca: &TestCa, answers: Vec<Answer>) -> (u16, mpsc::Receiver<Seen>) {
        let (cert, key) = ca.leaf();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("server protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .expect("server certificate");
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().expect("listener address").port();
        let (seen_tx, seen) = mpsc::channel();
        std::thread::spawn(move || {
            for (index, stream) in listener.incoming().enumerate() {
                let Ok(stream) = stream else { return };
                if seen_tx.send(Seen::Accepted).is_err() {
                    return;
                }
                let answer = answers[index.min(answers.len() - 1)];
                let config = Arc::clone(&config);
                let seen_tx = seen_tx.clone();
                std::thread::spawn(move || serve_tls(stream, config, &seen_tx, answer));
            }
        });
        (port, seen)
    }

    /// Send `msg` on the fake daemon's socket; false when the socket is gone.
    fn send_daemon_message<S: std::io::Read + std::io::Write>(
        ws: &mut tungstenite::WebSocket<S>,
        msg: &DaemonMessage,
    ) -> bool {
        let text = serde_json::to_string(msg).expect("encode a daemon message");
        ws.send(tungstenite::Message::Text(text.into())).is_ok()
    }

    /// One fake-daemon connection: TLS, the upgrade, then each message.
    fn serve_tls(
        stream: TcpStream,
        config: Arc<rustls::ServerConfig>,
        seen: &mpsc::Sender<Seen>,
        answer: Answer,
    ) {
        let conn = rustls::ServerConnection::new(config).expect("a TLS server connection");
        let header = |request: &Request, name: &str| {
            request
                .headers()
                .get(name)
                .map(|value| value.to_str().expect("an ASCII header").to_owned())
        };
        #[expect(
            clippy::result_large_err,
            reason = "tungstenite's upgrade callback type fixes the error as its ErrorResponse"
        )]
        let record_upgrade = |request: &Request, response: Response| {
            let _ = seen.send(Seen::Upgrade {
                host: header(request, "host"),
                origin: header(request, "origin"),
            });
            Ok(response)
        };
        let tls = rustls::StreamOwned::new(conn, stream);
        let Ok(mut ws) = tungstenite::accept_hdr(tls, record_upgrade) else {
            let _ = seen.send(Seen::TlsFailed);
            return;
        };
        // An `Answer::NotApprovedOnMarker` Hello not answered yet.
        let mut held_hello = false;
        // Whether this socket asked to pair before.
        let mut asked = false;
        loop {
            let text = match ws.read() {
                Ok(tungstenite::Message::Text(text)) => text,
                Ok(_) => continue,
                Err(_) => {
                    let _ = seen.send(Seen::Closed);
                    return;
                }
            };
            let value: serde_json::Value =
                serde_json::from_str(text.as_str()).expect("a JSON client message");
            let kind = value["type"].as_str().expect("a typed message").to_owned();
            let keep_open = match kind.as_str() {
                "hello" if matches!(answer, Answer::NotApprovedOnMarker) => {
                    let token = value["auth_token"].as_str().expect("Hello's token");
                    let _ = seen.send(Seen::Hello(token.to_owned()));
                    held_hello = true;
                    true
                }
                "list_repos" if held_hello => {
                    let _ = seen.send(Seen::Message(kind));
                    held_hello = false;
                    send_not_approved(&mut ws);
                    true
                }
                "device_pair_request" if matches!(answer, Answer::PairingOnce) && asked => {
                    let _ = seen.send(Seen::Message(kind));
                    true
                }
                "hello" => answer_hello(&mut ws, &value, seen, answer),
                "device_pair_request" if answer.pairs() => {
                    asked = true;
                    answer_pairing(&mut ws, &value, seen, answer)
                }
                "device_pair_request" | "device_pair_redeem" | "device_pair_cancel"
                    if answer.pairs() =>
                {
                    answer_pairing(&mut ws, &value, seen, answer)
                }
                _ => {
                    let _ = seen.send(Seen::Message(kind));
                    true
                }
            };
            if !keep_open {
                // The read loop reports the close once the client answers it.
                let _ = ws.close(None);
            }
        }
    }

    /// Answer a `Hello` as `answer` says; false when the daemon closes the
    /// socket after it. A failed send shows as the next read's close.
    fn answer_hello<S: std::io::Read + std::io::Write>(
        ws: &mut tungstenite::WebSocket<S>,
        hello: &serde_json::Value,
        seen: &mpsc::Sender<Seen>,
        answer: Answer,
    ) -> bool {
        let token = hello["auth_token"].as_str().expect("Hello's token");
        let _ = seen.send(Seen::Hello(token.to_owned()));
        let welcome = DaemonMessage::Welcome {
            protocol_version: 23,
            supported_versions: vec![23],
            features: Vec::new(),
        };
        let served = matches!(answer, Answer::Served) || token == ISSUED_TOKEN;
        let next = if served {
            DaemonMessage::Repos { repos: Vec::new() }
        } else {
            DaemonMessage::DevicePairFailed {
                reason: DEVICE_NOT_APPROVED.to_owned(),
            }
        };
        if send_daemon_message(ws, &welcome) {
            let _ = send_daemon_message(ws, &next);
        }
        served || !matches!(answer, Answer::NotApprovedThenClose)
    }

    /// Answer one pairing step as `answer` says; false when the daemon
    /// closes the socket after it.
    fn answer_pairing<S: std::io::Read + std::io::Write>(
        ws: &mut tungstenite::WebSocket<S>,
        msg: &serde_json::Value,
        seen: &mpsc::Sender<Seen>,
        answer: Answer,
    ) -> bool {
        let kind = msg["type"].as_str().expect("a typed message");
        if kind == "device_pair_redeem" {
            let code = msg["code"].as_str().expect("the redeem's code").to_owned();
            let request_id = msg["request_id"].as_str().expect("the redeem's request id");
            let _ = seen.send(Seen::Redeem {
                request_id: request_id.to_owned(),
                code: code.clone(),
            });
            if code == GOOD_CODE {
                let token = DaemonMessage::DeviceToken {
                    device_id: "device-1".to_owned(),
                    token: protocol::Secret(ISSUED_TOKEN.to_owned()),
                };
                let _ = send_daemon_message(ws, &token);
                return false;
            }
            let refusal = DaemonMessage::DevicePairFailed {
                reason: WRONG_CODE_REASON.to_owned(),
            };
            let _ = send_daemon_message(ws, &refusal);
            return true;
        }
        let _ = seen.send(Seen::Message(kind.to_owned()));
        if kind == "device_pair_cancel" {
            return false;
        }
        let pending = DaemonMessage::DevicePairPending {
            request_id: REQUEST_ID.to_owned(),
            expires_in_secs: 300,
        };
        if !send_daemon_message(ws, &pending) || matches!(answer, Answer::PairingThenDrop) {
            return false;
        }
        let approved = DaemonMessage::DevicePairDecided {
            request_id: REQUEST_ID.to_owned(),
            outcome: protocol::DevicePairOutcome::Approved,
        };
        send_daemon_message(ws, &approved)
    }

    /// What `seen` reports, up to and including the first event `done`
    /// accepts; `None` when [`BACKSTOP`] passes first.
    fn seen_until(
        seen: &mpsc::Receiver<Seen>,
        mut done: impl FnMut(&Seen) -> bool,
    ) -> Option<Vec<Seen>> {
        let deadline = Instant::now() + BACKSTOP;
        let mut log = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let event = seen.recv_timeout(left).ok()?;
            let finished = done(&event);
            log.push(event);
            if finished {
                return Some(log);
            }
        }
        None
    }

    /// The token of the next `Hello` the fake daemon sees.
    fn next_hello(seen: &mpsc::Receiver<Seen>) -> Option<String> {
        match seen_until(seen, |event| matches!(event, Seen::Hello(_)))?.pop() {
            Some(Seen::Hello(token)) => Some(token),
            _ => None,
        }
    }

    /// Wait for an event `want` accepts, up to [`BACKSTOP`].
    fn saw(events: &mut UnboundedReceiver<NetEvent>, want: impl Fn(&NetEvent) -> bool) -> bool {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build a test runtime");
        runtime.block_on(async {
            let found = async {
                while let Some(event) = events.next().await {
                    if want(&event) {
                        return true;
                    }
                }
                false
            };
            tokio::time::timeout(BACKSTOP, found).await.unwrap_or(false)
        })
    }

    /// Wait for a state event whose connection `want` accepts.
    fn saw_state(
        events: &mut UnboundedReceiver<NetEvent>,
        want: impl Fn(&Connection) -> bool,
    ) -> bool {
        saw(
            events,
            |event| matches!(event, NetEvent::State(conn) if want(conn)),
        )
    }

    /// The next state event, up to [`BACKSTOP`].
    fn next_state(events: &mut UnboundedReceiver<NetEvent>) -> Option<Connection> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build a test runtime");
        runtime.block_on(async {
            let found = async {
                while let Some(event) = events.next().await {
                    if let NetEvent::State(conn) = event {
                        return Some(conn);
                    }
                }
                None
            };
            tokio::time::timeout(BACKSTOP, found).await.ok().flatten()
        })
    }

    /// The network thread on `deps`, connecting to `target` first.
    fn run_net(
        deps: NetDeps,
        target: Target,
    ) -> (
        UnboundedSender<NetCommand>,
        UnboundedReceiver<NetEvent>,
        JoinHandle<()>,
    ) {
        let (view_tx, commands) = unbounded();
        let (events, view_rx) = unbounded();
        let net = std::thread::spawn(move || {
            run_thread(commands, events, deps, target, WELCOME_TIMEOUT);
        });
        (view_tx, view_rx, net)
    }

    fn stop_net(view_tx: UnboundedSender<NetCommand>, net: JoinHandle<()>) {
        drop(view_tx);
        net.join()
            .expect("net thread exits when the command channel closes");
    }

    static TAILNET_ENSURES: AtomicUsize = AtomicUsize::new(0);
    static TAILNET_STOPS: AtomicUsize = AtomicUsize::new(0);

    /// An `ensure_running` that counts its calls and fails: a tailnet
    /// target must never start a local daemon.
    fn ensure_must_not_run() -> EnsureFuture {
        TAILNET_ENSURES.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(anyhow::anyhow!("ensure ran for a tailnet target")) })
    }

    /// Deps for a tailnet target trusting only `ca`, holding `token` for
    /// [`TLS_HOST`] and none for any other host; a stop is counted.
    fn tailnet_deps(ca: &TestCa, token: Option<&'static str>) -> NetDeps {
        NetDeps {
            ensure: Box::new(ensure_must_not_run),
            identity: None,
            stop: Box::new(|| {
                TAILNET_STOPS.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }),
            tokens: Arc::new(token.map_or_else(MemoryTokens::default, |token| {
                MemoryTokens::with(TLS_HOST, token)
            })),
            tls: Some(ca.client_config()),
        }
    }

    #[test]
    #[expect(
        clippy::panic,
        reason = "a tailnet boot target that starts the local daemon fails the spec"
    )]
    fn net_starts_on_the_boot_target() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon(&ca);
        let target = Target::tailnet(TLS_HOST, Some(port));
        let deps = NetDeps {
            ensure: Box::new(|| panic!("ensure_running ran for a tailnet boot target")),
            ..tailnet_deps(&ca, Some(DEVICE_TOKEN))
        };
        let (view_tx, commands) = unbounded();
        let (events, mut view_rx) = unbounded();

        super::spawn_with(deps, target.clone(), commands, events);
        assert_eq!(
            next_hello(&seen).as_deref(),
            Some(DEVICE_TOKEN),
            "the first attempt did not dial the boot host"
        );
        assert!(
            saw_state(&mut view_rx, |conn| conn.is_open()
                && conn.target() == &target),
            "the boot host never opened"
        );
        drop(view_tx);
    }

    #[test]
    fn production_tls_config_builds() {
        let config = super::os_tls_config().expect("a config from the OS certificate store");
        assert!(
            !config.crypto_provider().cipher_suites.is_empty(),
            "the ring provider offers cipher suites"
        );
    }

    #[test]
    fn empty_root_store_fails_closed() {
        let err = tls_config_trusting(Vec::new()).expect_err("no roots must not build a config");
        assert!(
            format!("{err:#}").contains("no usable root certificates"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn tailnet_upgrade_sends_host_with_port_and_no_origin() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon(&ca);
        let (view_tx, _view_rx, net) = run_net(
            tailnet_deps(&ca, Some(DEVICE_TOKEN)),
            Target::tailnet(TLS_HOST, Some(port)),
        );

        let log = seen_until(&seen, |event| matches!(event, Seen::Upgrade { .. }))
            .expect("no upgrade request");
        assert_eq!(
            log.last(),
            Some(&Seen::Upgrade {
                host: Some(format!("{TLS_HOST}:{port}")),
                origin: None,
            }),
            "the tailnet listener needs Host = name:port and no Origin"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn switch_target_while_waiting_reconnects() {
        let ca = TestCa::new();
        let (pairing_port, pairing) = tls_daemon_answering(&ca, Answer::NotApprovedThenClose);
        let (port, seen) = tls_daemon(&ca);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, Some(DEVICE_TOKEN)),
            Target::tailnet(TLS_HOST, Some(pairing_port)),
        );
        assert!(
            saw_state(&mut view_rx, |conn| matches!(
                conn.state(),
                State::NotApproved { .. }
            )),
            "the host that does not approve this device never said so"
        );
        assert!(
            seen_until(&pairing, |event| *event == Seen::Closed).is_some(),
            "the pairing host never closed its socket"
        );
        assert!(
            next_state(&mut view_rx).is_some_and(|conn| is_not_approved(&conn, true)),
            "the close was not handled, so the switch would not land while waiting"
        );

        let switched = Target::tailnet(TLS_HOST, Some(port));
        view_tx
            .unbounded_send(NetCommand::SwitchTarget(switched.clone()))
            .expect("net thread is running");
        assert_eq!(
            next_hello(&seen).as_deref(),
            Some(DEVICE_TOKEN),
            "SwitchTarget while waiting did not dial the new host"
        );
        assert!(
            saw_state(&mut view_rx, |conn| conn.is_open()
                && conn.target() == &switched),
            "the new host never opened"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn tailnet_target_sends_hello_with_device_token_over_tls() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon(&ca);
        let target = Target::tailnet(TLS_HOST, Some(port));
        let (view_tx, mut view_rx, net) =
            run_net(tailnet_deps(&ca, Some(DEVICE_TOKEN)), target.clone());

        assert_eq!(
            next_hello(&seen).as_deref(),
            Some(DEVICE_TOKEN),
            "Hello must carry the device token for the host"
        );
        assert!(
            saw_state(&mut view_rx, |conn| conn.is_open()
                && conn.target() == &target),
            "the Welcome over TLS did not open the connection"
        );
        stop_net(view_tx, net);
    }

    /// True when `conn` is not approved, with `had_token` and the daemon's
    /// reason.
    fn is_not_approved(conn: &Connection, had_token: bool) -> bool {
        matches!(
            conn.state(),
            State::NotApproved { reason, had_token: held }
                if reason == DEVICE_NOT_APPROVED && *held == had_token
        )
    }

    /// Long enough for a reconnect on the backoff to have dialled again; the
    /// passing run always waits it out.
    fn wait_out_a_reconnect() {
        std::thread::sleep(crate::connection::backoff_delay(0) * 4);
    }

    #[test]
    fn untokened_tailnet_pick_dials_with_an_empty_token() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon_answering(&ca, Answer::NotApproved);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, None),
            Target::tailnet(TLS_HOST, Some(port)),
        );

        assert_eq!(
            next_hello(&seen).as_deref(),
            Some(""),
            "a host without a device token must be dialled with an empty one"
        );
        assert!(
            saw_state(&mut view_rx, |conn| is_not_approved(conn, false)),
            "the empty token did not end at not approved, without a token"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn a_stale_token_reports_not_approved_with_had_token() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon_answering(&ca, Answer::NotApproved);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, Some(DEVICE_TOKEN)),
            Target::tailnet(TLS_HOST, Some(port)),
        );

        assert_eq!(
            next_hello(&seen).as_deref(),
            Some(DEVICE_TOKEN),
            "the stored token goes in Hello"
        );
        assert!(
            saw_state(&mut view_rx, |conn| is_not_approved(conn, true)),
            "the stale token did not end at not approved, with a token"
        );
        assert!(
            saw(&mut view_rx, |event| matches!(
                event,
                NetEvent::Message(msg)
                    if matches!(&**msg, DaemonMessage::DevicePairFailed { .. })
            )),
            "the daemon's refusal was not forwarded"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn not_approved_welcome_never_reports_open() {
        let ca = TestCa::new();
        let (port, _seen) = tls_daemon_answering(&ca, Answer::NotApproved);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, None),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        let opened = Cell::new(false);
        let welcomed = Cell::new(false);

        let reached = saw(&mut view_rx, |event| match event {
            NetEvent::State(conn) => {
                opened.set(opened.get() || conn.is_open());
                is_not_approved(conn, false)
            }
            NetEvent::Message(msg) => {
                let welcome = matches!(&**msg, DaemonMessage::Welcome { .. });
                welcomed.set(welcomed.get() || welcome);
                false
            }
            _ => false,
        });
        assert!(reached, "never reached not approved");
        assert!(!opened.get(), "a pairing connection reported Open");
        assert!(
            !welcomed.get(),
            "the pairing connection's Welcome reached the view"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn not_approved_does_not_reconnect() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon_answering(&ca, Answer::NotApprovedThenClose);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, None),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        assert!(
            next_hello(&seen).is_some(),
            "the first attempt sent no Hello"
        );
        assert!(
            saw_state(&mut view_rx, |conn| is_not_approved(conn, false)),
            "never reached not approved"
        );
        assert!(
            seen_until(&seen, |event| *event == Seen::Closed).is_some(),
            "the daemon's close never landed"
        );
        assert!(
            next_state(&mut view_rx).is_some_and(|conn| is_not_approved(&conn, false)),
            "the close must keep the not-approved state"
        );

        wait_out_a_reconnect();
        assert_eq!(
            seen.try_recv().ok(),
            None,
            "a not-approved host was dialled again without a command"
        );
        view_tx
            .unbounded_send(NetCommand::Restart)
            .expect("net thread is running");
        assert_eq!(
            next_hello(&seen).as_deref(),
            Some(""),
            "Reconnect did not dial the host again"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn a_served_tailnet_connection_still_opens_after_welcome_hold_back() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon(&ca);
        let target = Target::tailnet(TLS_HOST, Some(port));
        let (view_tx, mut view_rx, net) =
            run_net(tailnet_deps(&ca, Some(DEVICE_TOKEN)), target.clone());
        assert!(
            next_hello(&seen).is_some(),
            "the first attempt sent no Hello"
        );
        let opened = Cell::new(false);
        let welcomed = Cell::new(false);

        let reached = saw(&mut view_rx, |event| match event {
            NetEvent::State(conn) => {
                opened.set(opened.get() || (conn.is_open() && conn.target() == &target));
                false
            }
            NetEvent::Message(msg) => match &**msg {
                DaemonMessage::Welcome { .. } => {
                    welcomed.set(true);
                    false
                }
                DaemonMessage::Repos { .. } => true,
                _ => false,
            },
            _ => false,
        });
        assert!(reached, "the frame after Welcome never reached the view");
        assert!(
            opened.get(),
            "the served host never opened before its Repos"
        );
        assert!(
            welcomed.get(),
            "the held Welcome never reached the view before the next frame"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn tailnet_target_never_runs_ensure() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon(&ca);
        let (view_tx, _view_rx, net) = run_net(
            tailnet_deps(&ca, Some(DEVICE_TOKEN)),
            Target::tailnet(TLS_HOST, Some(port)),
        );

        assert!(
            next_hello(&seen).is_some(),
            "the first attempt sent no Hello"
        );
        view_tx
            .unbounded_send(NetCommand::Restart)
            .expect("net thread is running");
        assert!(next_hello(&seen).is_some(), "Reconnect sent no Hello");
        stop_net(view_tx, net);
        assert_eq!(
            TAILNET_ENSURES.load(Ordering::SeqCst),
            0,
            "ensure_running ran for a tailnet target"
        );
    }

    #[test]
    fn untrusted_certificate_never_sends_hello() {
        let trusted = TestCa::new();
        let (port, seen) = tls_daemon(&TestCa::new());
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&trusted, Some(DEVICE_TOKEN)),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        let url = format!("connecting to wss://{TLS_HOST}:{port}/ws");

        assert!(
            saw_state(&mut view_rx, |conn| {
                matches!(conn.state(), State::Closed { reason } if reason.starts_with(&url))
            }),
            "the untrusted certificate did not fail the connect"
        );
        // A connection's TlsFailed comes from its own serve thread, so it may
        // land after the retry's accept: read on until both are in.
        let (mut accepts, mut failed) = (0, false);
        let log = seen_until(&seen, |event| {
            match event {
                Seen::Accepted => accepts += 1,
                Seen::TlsFailed => failed = true,
                _ => {}
            }
            accepts >= 2 && failed
        })
        .expect("no backoff retry and failed handshake after the untrusted certificate");
        assert!(
            !log.iter().any(|event| matches!(event, Seen::Hello(_))),
            "Hello went to an untrusted server: {log:?}"
        );
        assert!(
            log.contains(&Seen::TlsFailed),
            "no failed handshake: {log:?}"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn tailnet_target_never_sends_shutdown() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon(&ca);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, Some(DEVICE_TOKEN)),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        assert!(
            next_hello(&seen).is_some(),
            "the first attempt sent no Hello"
        );
        assert!(saw_state(&mut view_rx, Connection::is_open), "never opened");

        view_tx
            .unbounded_send(NetCommand::Restart)
            .expect("net thread is running");
        assert!(next_hello(&seen).is_some(), "Restart did not reconnect");
        assert!(
            saw_state(&mut view_rx, Connection::is_open),
            "never reopened"
        );
        for command in [
            NetCommand::Stop,
            NetCommand::Shutdown {
                before: vec![ClientMessage::ListRepos],
                drain: true,
            },
            NetCommand::Send(Box::new(ClientMessage::ListRepos)),
        ] {
            view_tx
                .unbounded_send(command)
                .expect("net thread is running");
        }

        let marker = Seen::Message("list_repos".to_owned());
        let log = seen_until(&seen, |event| *event == marker).expect("the marker never arrived");
        let messages: Vec<&Seen> = log
            .iter()
            .filter(|event| matches!(event, Seen::Message(_)))
            .collect();
        assert_eq!(
            messages,
            vec![&marker],
            "only the marker may reach a tailnet host"
        );
        assert!(
            saw(&mut view_rx, |event| matches!(
                event,
                NetEvent::ShutdownFailed
            )),
            "quit on a tailnet host must report that nothing was sent"
        );
        stop_net(view_tx, net);
        assert_eq!(
            TAILNET_STOPS.load(Ordering::SeqCst),
            0,
            "Stop ran on a tailnet host"
        );
    }

    #[test]
    fn switch_target_reconnects_at_once() {
        let ca = TestCa::new();
        let (first_port, first) = tls_daemon(&ca);
        let (second_port, second) = tls_daemon(&ca);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, Some(DEVICE_TOKEN)),
            Target::tailnet(TLS_HOST, Some(first_port)),
        );
        assert!(next_hello(&first).is_some(), "the first host got no Hello");
        assert!(saw_state(&mut view_rx, Connection::is_open), "never opened");

        let switched = Target::tailnet(TLS_HOST, Some(second_port));
        view_tx
            .unbounded_send(NetCommand::SwitchTarget(switched.clone()))
            .expect("net thread is running");
        assert_eq!(
            next_hello(&second).as_deref(),
            Some(DEVICE_TOKEN),
            "SwitchTarget did not dial the new host"
        );
        assert!(
            seen_until(&first, |event| *event == Seen::Closed).is_some(),
            "the old host's socket stayed open"
        );
        assert!(
            saw_state(&mut view_rx, |conn| conn.is_open()
                && conn.target() == &switched),
            "the new host never opened"
        );
        stop_net(view_tx, net);
    }

    /// A device token store that reads nothing and fails every write.
    struct FailingTokens;

    impl DeviceTokenStore for FailingTokens {
        fn get(&self, _host: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }

        fn set(&self, _host: &str, _token: &str) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("Credential Manager refused the write"))
        }

        fn delete(&self, _host: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn send_command(view_tx: &UnboundedSender<NetCommand>, command: NetCommand) {
        view_tx
            .unbounded_send(command)
            .expect("net thread is running");
    }

    /// Whether `event` forwards a daemon message `want` accepts.
    fn is_message(event: &NetEvent, want: impl Fn(&DaemonMessage) -> bool) -> bool {
        matches!(event, NetEvent::Message(msg) if want(msg))
    }

    fn is_approved(msg: &DaemonMessage) -> bool {
        matches!(
            msg,
            DaemonMessage::DevicePairDecided {
                outcome: DevicePairOutcome::Approved,
                ..
            }
        )
    }

    /// Wait for the pairing connection, ask to pair and wait for the
    /// approval, as a user does before typing the code.
    fn ask_and_get_approved(
        view_tx: &UnboundedSender<NetCommand>,
        view_rx: &mut UnboundedReceiver<NetEvent>,
    ) {
        assert!(
            saw_state(view_rx, |conn| is_not_approved(conn, false)),
            "never reached not approved"
        );
        send_command(view_tx, NetCommand::PairRequest);
        assert!(
            saw(view_rx, |event| is_message(event, is_approved)),
            "the approval never reached the view"
        );
    }

    fn redeem(view_tx: &UnboundedSender<NetCommand>, code: &str) {
        send_command(
            view_tx,
            NetCommand::PairRedeem {
                code: code.to_owned(),
            },
        );
    }

    #[test]
    fn pair_request_sends_device_pair_request_on_the_live_socket() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon_answering(&ca, Answer::Pairing);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, None),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        assert!(
            saw_state(&mut view_rx, |conn| is_not_approved(conn, false)),
            "never reached not approved"
        );

        send_command(&view_tx, NetCommand::PairRequest);
        let log = seen_until(&seen, |event| {
            *event == Seen::Message("device_pair_request".to_owned())
        })
        .expect("the daemon never got the pairing request");
        let hellos = log
            .iter()
            .filter(|event| matches!(event, Seen::Hello(_)))
            .count();
        assert_eq!(hellos, 1, "the request must ride the open socket: {log:?}");
        assert!(
            saw(&mut view_rx, |event| is_message(event, |msg| matches!(
                msg,
                DaemonMessage::DevicePairPending { request_id, .. } if request_id == REQUEST_ID
            ))),
            "the daemon's pending answer was not forwarded"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn pair_after_idle_close_redials_then_requests() {
        let ca = TestCa::new();
        let (port, seen) =
            tls_daemon_scripted(&ca, vec![Answer::NotApprovedThenClose, Answer::Pairing]);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, None),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        assert!(
            saw_state(&mut view_rx, |conn| is_not_approved(conn, false)),
            "never reached not approved"
        );
        assert!(
            seen_until(&seen, |event| *event == Seen::Closed).is_some(),
            "the daemon's idle close never landed"
        );
        assert!(
            next_state(&mut view_rx).is_some_and(|conn| is_not_approved(&conn, false)),
            "the idle close was not handled"
        );

        send_command(&view_tx, NetCommand::PairRequest);
        assert_eq!(
            next_hello(&seen).as_deref(),
            Some(""),
            "a request with the pairing socket gone must redial the host"
        );
        assert!(
            seen_until(&seen, |event| {
                *event == Seen::Message("device_pair_request".to_owned())
            })
            .is_some(),
            "the redialled socket never carried the request"
        );
        assert!(
            saw(&mut view_rx, |event| is_message(event, is_approved)),
            "the redialled socket's approval never reached the view"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn device_token_is_stored_for_the_host_and_next_hello_carries_it() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon_answering(&ca, Answer::Pairing);
        let target = Target::tailnet(TLS_HOST, Some(port));
        let store = Arc::new(MemoryTokens::default());
        let deps = NetDeps {
            tokens: store.clone(),
            ..tailnet_deps(&ca, None)
        };
        let (view_tx, mut view_rx, net) = run_net(deps, target.clone());
        ask_and_get_approved(&view_tx, &mut view_rx);

        redeem(&view_tx, GOOD_CODE);
        assert!(
            saw(&mut view_rx, |event| matches!(event, NetEvent::Paired)),
            "a landed token did not report the pairing"
        );
        assert_eq!(
            store.get(TLS_HOST).expect("reads").as_deref(),
            Some(ISSUED_TOKEN),
            "the issued token was not stored for the host"
        );
        assert_eq!(next_hello(&seen).as_deref(), Some(""), "the pairing Hello");
        assert_eq!(
            next_hello(&seen).as_deref(),
            Some(ISSUED_TOKEN),
            "the reconnect's Hello must carry the issued token"
        );
        assert!(
            saw_state(&mut view_rx, |conn| conn.is_open()
                && conn.target() == &target),
            "the paired host never opened"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn token_is_never_forwarded_to_the_view() {
        let ca = TestCa::new();
        let (port, _seen) = tls_daemon_answering(&ca, Answer::Pairing);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, None),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        ask_and_get_approved(&view_tx, &mut view_rx);
        let forwarded = Cell::new(false);

        redeem(&view_tx, GOOD_CODE);
        let opened = saw(&mut view_rx, |event| {
            let token = is_message(event, |msg| {
                matches!(msg, DaemonMessage::DeviceToken { .. })
            });
            forwarded.set(forwarded.get() || token);
            matches!(event, NetEvent::State(conn) if conn.is_open())
        });
        assert!(opened, "the paired host never opened");
        assert!(!forwarded.get(), "the device token reached the view");
        stop_net(view_tx, net);
    }

    #[test]
    fn failed_token_save_reports_and_does_not_reconnect_silently() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon_answering(&ca, Answer::Pairing);
        let deps = NetDeps {
            tokens: Arc::new(FailingTokens),
            ..tailnet_deps(&ca, None)
        };
        let (view_tx, mut view_rx, net) = run_net(deps, Target::tailnet(TLS_HOST, Some(port)));
        ask_and_get_approved(&view_tx, &mut view_rx);
        let paired = Cell::new(false);

        redeem(&view_tx, GOOD_CODE);
        let reported = saw(&mut view_rx, |event| {
            paired.set(paired.get() || matches!(event, NetEvent::Paired));
            matches!(event, NetEvent::TokenSaveFailed)
        });
        assert!(reported, "a failed token save was not reported");
        assert!(!paired.get(), "a token that was not saved reported paired");
        assert!(
            seen_until(&seen, |event| *event == Seen::Closed).is_some(),
            "the daemon never closed after issuing the token"
        );
        assert!(
            next_state(&mut view_rx).is_some_and(|conn| is_not_approved(&conn, false)),
            "the close after a failed save must leave the host not approved"
        );
        wait_out_a_reconnect();
        assert_eq!(
            seen.try_recv().ok(),
            None,
            "the host was dialled again after the token was lost"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn redeem_wrong_code_keeps_the_socket() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon_answering(&ca, Answer::Pairing);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, None),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        ask_and_get_approved(&view_tx, &mut view_rx);

        redeem(&view_tx, WRONG_CODE);
        assert!(
            saw(&mut view_rx, |event| is_message(event, |msg| matches!(
                msg,
                DaemonMessage::DevicePairFailed { reason } if reason == WRONG_CODE_REASON
            ))),
            "the refusal of the wrong code was not forwarded"
        );
        redeem(&view_tx, GOOD_CODE);
        assert!(
            saw(&mut view_rx, |event| matches!(event, NetEvent::Paired)),
            "the right code after a wrong one did not pair"
        );
        let good = Seen::Redeem {
            request_id: REQUEST_ID.to_owned(),
            code: GOOD_CODE.to_owned(),
        };
        let log =
            seen_until(&seen, |event| *event == good).expect("the second redeem never arrived");
        let after_first_hello: Vec<&Seen> = log
            .iter()
            .skip_while(|event| !matches!(event, Seen::Hello(_)))
            .skip(1)
            .collect();
        assert!(
            !after_first_hello
                .iter()
                .any(|event| matches!(event, Seen::Hello(_) | Seen::Closed)),
            "both redeems must ride the one pairing socket: {log:?}"
        );
        assert!(
            log.contains(&Seen::Redeem {
                request_id: REQUEST_ID.to_owned(),
                code: WRONG_CODE.to_owned(),
            }),
            "the wrong code was not redeemed for the pending request: {log:?}"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn a_socket_drop_with_a_pending_request_reports_pairing_lost() {
        let ca = TestCa::new();
        let (port, _seen) = tls_daemon_answering(&ca, Answer::PairingThenDrop);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, None),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        assert!(
            saw_state(&mut view_rx, |conn| is_not_approved(conn, false)),
            "never reached not approved"
        );

        send_command(&view_tx, NetCommand::PairRequest);
        assert!(
            saw(&mut view_rx, |event| matches!(event, NetEvent::PairingLost)),
            "a drop with the request pending did not report it lost"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn pair_commands_outside_not_approved_do_nothing() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon(&ca);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, Some(DEVICE_TOKEN)),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        assert!(saw_state(&mut view_rx, Connection::is_open), "never opened");

        send_command(&view_tx, NetCommand::PairRequest);
        redeem(&view_tx, GOOD_CODE);
        send_command(&view_tx, NetCommand::PairCancel);
        send_command(
            &view_tx,
            NetCommand::Send(Box::new(ClientMessage::ListRepos)),
        );

        let marker = Seen::Message("list_repos".to_owned());
        let log = seen_until(&seen, |event| *event == marker).expect("the marker never arrived");
        let sent: Vec<&Seen> = log
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    Seen::Message(_) | Seen::Redeem { .. } | Seen::Hello(_)
                )
            })
            .collect();
        assert_eq!(
            sent,
            vec![&Seen::Hello(DEVICE_TOKEN.to_owned()), &marker],
            "a pairing command reached a serving host or redialled it"
        );
        while let Ok(event) = view_rx.try_recv() {
            assert!(
                !matches!(
                    event,
                    NetEvent::PairingLost | NetEvent::Paired | NetEvent::TokenSaveFailed
                ),
                "a dropped pairing command reported a pairing event"
            );
        }
        stop_net(view_tx, net);
    }

    /// A log sink the tests read back.
    #[derive(Clone, Default)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturedLog {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
                .into_owned()
        }
    }

    #[test]
    fn the_redeem_code_never_appears_in_debug_or_logs() {
        let shown = format!(
            "{:?}",
            NetCommand::PairRedeem {
                code: WRONG_CODE.to_owned()
            }
        );
        assert!(!shown.contains(WRONG_CODE), "{shown}");
        assert!(shown.contains("PairRedeem"), "{shown}");

        let ca = TestCa::new();
        let (port, seen) = tls_daemon_answering(&ca, Answer::Pairing);
        let deps = tailnet_deps(&ca, None);
        let log = CapturedLog::default();
        let writer = log.clone();
        let (view_tx, commands) = unbounded();
        let (events, mut view_rx) = unbounded();
        let net = std::thread::spawn(move || {
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                run_thread(
                    commands,
                    events,
                    deps,
                    Target::tailnet(TLS_HOST, Some(port)),
                    WELCOME_TIMEOUT,
                );
            });
        });
        ask_and_get_approved(&view_tx, &mut view_rx);

        // On the live socket: sent, refused, the socket kept.
        redeem(&view_tx, WRONG_CODE);
        assert!(
            seen_until(&seen, |event| matches!(event, Seen::Redeem { .. })).is_some(),
            "the redeem never reached the daemon"
        );
        assert!(
            saw(&mut view_rx, |event| is_message(event, |msg| matches!(
                msg,
                DaemonMessage::DevicePairFailed { .. }
            ))),
            "the refusal never reached the view"
        );
        // With the socket gone: dropped and reported lost.
        send_command(&view_tx, NetCommand::PairCancel);
        assert!(
            next_state(&mut view_rx).is_some_and(|conn| is_not_approved(&conn, false)),
            "the cancel's close was not handled"
        );
        redeem(&view_tx, WRONG_CODE);
        assert!(
            saw(&mut view_rx, |event| matches!(event, NetEvent::PairingLost)),
            "a redeem with no socket was not reported lost"
        );
        stop_net(view_tx, net);

        let text = log.text();
        assert!(
            text.contains("PairRedeem"),
            "the dropped redeem was not logged by name: {text}"
        );
        assert!(
            !text.contains(WRONG_CODE),
            "the code reached the log: {text}"
        );
    }

    #[test]
    fn a_cancel_before_not_approved_on_the_redial_sends_no_request() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon_scripted(
            &ca,
            vec![Answer::NotApprovedThenClose, Answer::NotApprovedOnMarker],
        );
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, None),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        assert!(
            saw_state(&mut view_rx, |conn| is_not_approved(conn, false)),
            "never reached not approved"
        );
        assert!(
            seen_until(&seen, |event| *event == Seen::Closed).is_some(),
            "the daemon's idle close never landed"
        );
        assert!(
            next_state(&mut view_rx).is_some_and(|conn| is_not_approved(&conn, false)),
            "the idle close was not handled"
        );

        send_command(&view_tx, NetCommand::PairRequest);
        assert_eq!(
            next_hello(&seen).as_deref(),
            Some(""),
            "the request did not redial"
        );
        // The redial's socket is open and its host has not answered yet.
        send_command(&view_tx, NetCommand::PairCancel);
        send_command(
            &view_tx,
            NetCommand::Send(Box::new(ClientMessage::ListRepos)),
        );
        assert!(
            saw_state(&mut view_rx, |conn| is_not_approved(conn, false)),
            "the redial never reached not approved"
        );
        send_command(
            &view_tx,
            NetCommand::Send(Box::new(ClientMessage::ListRepos)),
        );

        let marker = Seen::Message("list_repos".to_owned());
        let mut markers = 0;
        let log = seen_until(&seen, |event| {
            markers += usize::from(*event == marker);
            markers == 2
        })
        .expect("the markers never arrived");
        assert!(
            !log.iter().any(
                |event| matches!(event, Seen::Message(kind) if kind.starts_with("device_pair"))
            ),
            "a cancelled request was still sent: {log:?}"
        );
        stop_net(view_tx, net);
    }

    #[test]
    fn a_new_request_forgets_the_old_request_id() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon_answering(&ca, Answer::PairingOnce);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, None),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        ask_and_get_approved(&view_tx, &mut view_rx);

        // The second request goes unanswered, so no new id ever arrives.
        send_command(&view_tx, NetCommand::PairRequest);
        redeem(&view_tx, WRONG_CODE);
        send_command(
            &view_tx,
            NetCommand::Send(Box::new(ClientMessage::ListRepos)),
        );

        let marker = Seen::Message("list_repos".to_owned());
        let log = seen_until(&seen, |event| *event == marker).expect("the marker never arrived");
        let requests = log
            .iter()
            .filter(|event| **event == Seen::Message("device_pair_request".to_owned()))
            .count();
        assert_eq!(requests, 2, "both requests reach the host: {log:?}");
        assert!(
            !log.iter().any(|event| matches!(event, Seen::Redeem { .. })),
            "a redeem named the replaced request: {log:?}"
        );
        stop_net(view_tx, net);
    }
}
