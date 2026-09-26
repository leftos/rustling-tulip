//! Daemon connection loop. On a dedicated thread with a current-thread tokio
//! runtime it ensures a daemon is running, opens the WebSocket, sends `Hello`
//! and pumps messages both ways until the socket closes, then reconnects after
//! the delay [`Connection`] sets. GPUI runs its own executor, so the view and
//! this thread talk over unbounded futures channels.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use daemon_client::{ClientIdentity, RetirePolicy};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::{SinkExt as _, StreamExt as _};
use protocol::{ClientMessage, DaemonHandshake, DaemonMessage, InboundDaemonMessage};
use tokio::net::TcpStream;
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};

use crate::connection::{Connection, PROBE_TIMEOUT, RestartAction, State, TICK, Watchdog};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Frame = Option<Result<Message, tungstenite::Error>>;
/// A pending `ensure_running`, boxed so tests can substitute one.
pub type EnsureFuture = Pin<Box<dyn Future<Output = Result<DaemonHandshake>>>>;
/// The protocol versions the native client speaks: only 23, the first whose
/// daemon echoes `request_id`. An older daemon is retired and replaced.
pub const NATIVE_PROTOCOL_VERSIONS: &[u32] = &[23];

/// The version `Hello` names as preferred: the highest native version, first
/// in [`NATIVE_PROTOCOL_VERSIONS`].
const NATIVE_PROTOCOL_VERSION: u32 = NATIVE_PROTOCOL_VERSIONS[0];

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
        .spawn(move || run_thread(commands, events, deps, WELCOME_TIMEOUT));
    if let Err(err) = spawned {
        error!("spawning the network thread: {err}");
    }
}

fn ensure_daemon() -> EnsureFuture {
    Box::pin(daemon_client::ensure_running(
        RetirePolicy::ReuseCompatible,
        NATIVE_PROTOCOL_VERSIONS,
    ))
}

/// The network thread's body: build the runtime and run the loop on it until
/// the command channel closes.
fn run_thread(
    commands: UnboundedReceiver<NetCommand>,
    events: UnboundedSender<NetEvent>,
    deps: NetDeps,
    welcome_timeout: Duration,
) {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        // `Net::new` builds a tokio interval, so it runs inside the runtime.
        Ok(rt) => rt.block_on(async move {
            Net::new(commands, events, deps, welcome_timeout)
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
        welcome_timeout: Duration,
    ) -> Self {
        let mut ticker = tokio::time::interval_at(Instant::now() + TICK, TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            commands,
            events,
            conn: Connection::new(),
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

    /// One connection attempt: ensure a daemon, open the socket, pump it. A
    /// command that arrives while the daemon or the socket is still coming up
    /// is handled at once and may cancel the attempt.
    async fn attempt(&mut self) -> Next {
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
            protocol_version: NATIVE_PROTOCOL_VERSION,
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
            Ok(InboundDaemonMessage::Unknown { .. }) => {}
            Err(err) => error!("undecodable daemon message: {err}"),
        }
        Step::Continue
    }

    fn on_daemon_message(&mut self, msg: Box<DaemonMessage>) {
        match &*msg {
            DaemonMessage::Welcome {
                protocol_version, ..
            } => {
                info!(protocol_version, "connected to daemon");
                self.conn.on_welcome(*protocol_version);
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
        }
    }

    /// Sends `before`, then the shutdown, and reports it sent. A failed send
    /// closes the socket; the view's wait then offers Force quit.
    async fn shutdown(&mut self, ws: &mut Socket, before: &[ClientMessage], drain: bool) -> Step {
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

    async fn stop(&mut self) {
        info!("stopping the daemon");
        self.conn.stop();
        self.emit_state();
        if let Err(err) = (self.deps.stop)().await {
            warn!("stopping the daemon: {err:#}");
        }
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
            Some(NetCommand::Stop) => {
                self.stop().await;
                Some(Next::Wait(None))
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
        EnsureFuture, NATIVE_PROTOCOL_VERSIONS, Net, NetCommand, NetDeps, NetEvent,
        WELCOME_TIMEOUT, run_thread,
    };
    use crate::connection::State;
    use futures::channel::mpsc::{UnboundedReceiver, unbounded};
    use protocol::ClientMessage;
    use protocol::DaemonHandshake;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio_tungstenite::tungstenite;

    /// Deps that ensure with `ensure`, send no identity and never stop.
    fn deps(ensure: fn() -> EnsureFuture) -> NetDeps {
        NetDeps {
            ensure: Box::new(ensure),
            identity: None,
            stop: Box::new(|| Box::pin(async { Ok(()) })),
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
                supported_versions: vec![23, 22],
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
    fn native_hello_offers_only_23() {
        assert_eq!(NATIVE_PROTOCOL_VERSIONS, &[23]);
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
        assert_eq!(protocol_version, 23);
        assert_eq!(protocol_versions, vec![23]);
    }
}
