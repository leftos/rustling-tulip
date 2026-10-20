//! Daemon connection loop. On a dedicated thread with a current-thread tokio
//! runtime it ensures a daemon is running, opens the WebSocket, sends `Hello`
//! and pumps messages both ways until the socket closes, then reconnects after
//! the delay [`Connection`] sets. GPUI runs its own executor, so the view and
//! this thread talk over unbounded futures channels.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use daemon_client::ClientIdentity;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::{SinkExt as _, StreamExt as _};
use protocol::{ClientMessage, DaemonHandshake, DaemonMessage, InboundDaemonMessage};
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

/// Looks up the device token this client holds for a tailnet host.
pub type TokenFor = Box<dyn Fn(&str) -> Option<String> + Send>;

/// How the network thread reaches its daemon.
pub struct NetDeps {
    /// Starts an `ensure_running`: the daemon step of each connection
    /// attempt.
    pub ensure: Box<dyn Fn() -> EnsureFuture + Send>,
    /// The identity sent in `Hello`; `None` connects without one.
    pub identity: Option<ClientIdentity>,
    /// Force-stops the daemon, for [`NetCommand::Stop`].
    pub stop: Box<dyn Fn() -> StopFuture + Send>,
    /// The device token this client holds for a tailnet host, sent in its
    /// `Hello`; `None` when it holds none, and then no socket is opened.
    pub token_for: TokenFor,
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
            token_for: Box::new(|_| None),
            tls: None,
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
}

/// Spawns the network thread on the production [`NetDeps`]. It runs until
/// the view drops its command sender.
pub fn spawn(commands: UnboundedReceiver<NetCommand>, events: UnboundedSender<NetEvent>) {
    spawn_with(NetDeps::production(), commands, events);
}

/// Spawns the network thread on `deps`. It runs until the view drops its
/// command sender.
pub fn spawn_with(
    deps: NetDeps,
    commands: UnboundedReceiver<NetCommand>,
    events: UnboundedSender<NetEvent>,
) {
    let spawned = std::thread::Builder::new()
        .name("net".to_owned())
        .spawn(move || run_thread(commands, events, deps, Target::Local, WELCOME_TIMEOUT));
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
            let mut conn = Connection::new();
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
        Next::Wait(delay)
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

    /// A tailnet host: never a local daemon step. Without a device token for
    /// the host no socket is opened (the `daemon.json` token never leaves
    /// this machine); with one, dial its listener over TLS and pump it.
    async fn attempt_tailnet(&mut self, host: &TailnetHost) -> Next {
        let Some(token) = (self.deps.token_for)(host.name()) else {
            let reason = format!("no device token for {}", host.name());
            warn!(%reason, "not connecting to the tailnet host");
            return self.failed(reason);
        };
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
        self.pump(ws, token).await
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
            // Welcome or AuthFailed moves the machine out of Connecting.
            if !matches!(self.conn.state(), State::Connecting) {
                welcome_deadline = None;
            }
            match step {
                Step::Continue => {}
                Step::Close(reason) => return self.closed(reason),
                Step::Leave(next) => return next,
            }
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
            Ok(InboundDaemonMessage::Known(msg)) => self.on_daemon_message(msg),
            Ok(InboundDaemonMessage::Unknown { type_tag, .. }) => {
                warn!("unknown or undecodable daemon message type: {type_tag}");
            }
            Err(err) => error!("undecodable daemon message: {err}"),
        }
        Step::Continue
    }

    fn on_daemon_message(&mut self, msg: Box<DaemonMessage>) {
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
            Some(NetCommand::Restart) => match self.conn.restart() {
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
        }
    }

    fn switch_target(&mut self, target: Target) {
        info!(daemon = ?target, "switching the daemon target; connecting from scratch");
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
                let _ = self.conn.restart();
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
        }
    }
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
    use futures::StreamExt as _;
    use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
    use protocol::ClientMessage;
    use protocol::DaemonHandshake;
    use protocol::DaemonMessage;
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
    use std::sync::mpsc;
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
            token_for: Box::new(|_| None),
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
        /// Any other client message, by its type tag.
        Message(String),
        /// An upgraded connection closed.
        Closed,
    }

    /// A loopback TLS WebSocket daemon whose certificate for [`TLS_HOST`] is
    /// signed by `ca`. It answers each `Hello` with `Welcome` and reports
    /// what it sees on the returned receiver.
    fn tls_daemon(ca: &TestCa) -> (u16, mpsc::Receiver<Seen>) {
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
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                if seen_tx.send(Seen::Accepted).is_err() {
                    return;
                }
                let config = Arc::clone(&config);
                let seen_tx = seen_tx.clone();
                std::thread::spawn(move || serve_tls(stream, config, &seen_tx));
            }
        });
        (port, seen)
    }

    /// One fake-daemon connection: TLS, the upgrade, then each message.
    fn serve_tls(stream: TcpStream, config: Arc<rustls::ServerConfig>, seen: &mpsc::Sender<Seen>) {
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
            if kind != "hello" {
                let _ = seen.send(Seen::Message(kind));
                continue;
            }
            let token = value["auth_token"].as_str().expect("Hello's token");
            let _ = seen.send(Seen::Hello(token.to_owned()));
            let welcome = DaemonMessage::Welcome {
                protocol_version: 23,
                supported_versions: vec![23],
                features: Vec::new(),
            };
            let welcome = serde_json::to_string(&welcome).expect("encode Welcome");
            if ws.send(tungstenite::Message::Text(welcome.into())).is_err() {
                let _ = seen.send(Seen::Closed);
                return;
            }
        }
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
            token_for: Box::new(move |host| token.filter(|_| host == TLS_HOST).map(str::to_owned)),
            tls: Some(ca.client_config()),
        }
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
        let (port, seen) = tls_daemon(&ca);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, Some(DEVICE_TOKEN)),
            Target::tailnet("elsewhere.tail1234.ts.net", None),
        );
        assert!(
            saw_state(&mut view_rx, |conn| matches!(
                conn.state(),
                State::Error { .. }
            )),
            "the host without a token did not fail"
        );

        let switched = Target::tailnet(TLS_HOST, Some(port));
        view_tx
            .unbounded_send(NetCommand::SwitchTarget(switched.clone()))
            .expect("net thread is running");
        assert_eq!(
            next_hello(&seen).as_deref(),
            Some(DEVICE_TOKEN),
            "SwitchTarget during the backoff did not dial the new host"
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

    #[test]
    fn tailnet_target_without_token_fails_closed_without_hello() {
        let ca = TestCa::new();
        let (port, seen) = tls_daemon(&ca);
        let (view_tx, mut view_rx, net) = run_net(
            tailnet_deps(&ca, None),
            Target::tailnet(TLS_HOST, Some(port)),
        );
        let names_host = |conn: &Connection| matches!(conn.state(), State::Error { reason } if reason.contains(TLS_HOST));

        assert!(
            saw_state(&mut view_rx, names_host),
            "no Error naming the host"
        );
        assert!(
            saw_state(&mut view_rx, names_host),
            "the missing token was not retried after the backoff"
        );
        assert_eq!(
            seen.try_recv().ok(),
            None,
            "a socket was opened without a device token"
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
}
