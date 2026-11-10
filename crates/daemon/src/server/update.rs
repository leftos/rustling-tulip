//! The host update request: `RemoteUpdate` asks this host to update itself to
//! the newest official release. The request names nothing; the daemon checks,
//! in a fixed order, that it can update the install it runs from, and answers
//! an `Error` carrying the request's id with the first reason it cannot.
//!
//! The checks, first that applies: not Windows, a dev build, an elevated
//! daemon, no install found, no updates folder, an install that is not this
//! daemon's own, a process other than the app that would block the swap,
//! another update already running in this daemon. The checks run on a
//! blocking thread, since the process list and the registry read are slow.
//! The app being open refuses nothing: it is asked to quit instead.
//!
//! When every check passes, the update runs on a plain thread of its own (a
//! blocking task would hold up the daemon's exit for a long download): it
//! takes the update mutex, clears old downloads, downloads and verifies the
//! release, waits for the go-ahead, and starts its installer with fixed
//! arguments: the plain set, or the set that also relaunches the app when it
//! still ran from the install folder as the launch began. The mutex is
//! taken, held and dropped on that one thread, and
//! the verified file stays open until the installer has started. The answer
//! is `RemoteUpdateStarted` once the installer runs, else an `Error`;
//! exactly one per request.
//!
//! The go-ahead: with no local connection attached, at once. Otherwise every
//! local connection is told `HostUpdateScheduled` and the update counts down
//! on the runtime; a local `CancelHostUpdate` within it aborts the update
//! (`HostUpdateCanceled` to every local connection, `update_canceled` to the
//! requester). At its end every local connection is told
//! `HostUpdateLaunched`, and the launch waits a while for the app to leave
//! the install folder. A daemon shutting down during the countdown refuses
//! the update. The phase is read and published under one lock, so a
//! connection that connects or lags is sent the current one.

use super::connection::{AuthKind, ConnCtx};
use super::{Hub, StateEvent};
use crate::cli_apply_update::{CLIENT_EXE, ProcessList, RunningExe};
use crate::sync::lock;
use protocol::error_codes::{BUSY, INTERNAL, NOT_FOUND, UPDATE_CANCELED, UPDATE_REFUSED};
use protocol::{ClientMessage, DaemonMessage};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// How long a host update counts down on the host before it goes ahead.
const COUNTDOWN: Duration = Duration::from_secs(10);

/// How long the launch waits for the app to leave the install folder.
const APP_QUIT_WAIT: Duration = Duration::from_secs(30);

/// How often the launch looks for the app while it waits.
const APP_POLL: Duration = Duration::from_millis(500);

/// Who asked, for a request that came from this PC.
const THIS_PC: &str = "a connection on this PC";

/// The longest device name shown as who asked.
const REQUESTER_CHARS: usize = 64;

/// The number in the next update id.
static NEXT_UPDATE: AtomicU64 = AtomicU64::new(1);

/// The daemon's file name in an install folder: an install holds one.
#[cfg(windows)]
const DAEMON_EXE: &str = "rustling-tulipd.exe";
#[cfg(not(windows))]
const DAEMON_EXE: &str = "rustling-tulipd";

/// The folder in an install that the installer stages an update into.
const STAGING_DIR: &str = ".staging";

/// The installer's arguments: silent, in its update mode, restarting the
/// daemon after the swap. Nothing in them comes from the request.
const INSTALLER_ARGS: [&str; 3] = ["/S", "/UPDATE", "/RESTARTDAEMON"];

/// The installer's arguments when the app ran from the install folder as the
/// launch began: [`INSTALLER_ARGS`] plus `/RELAUNCH`, which starts the app
/// again after the swap. Nothing in them comes from the request.
const INSTALLER_ARGS_RELAUNCH: [&str; 4] = ["/S", "/UPDATE", "/RELAUNCH", "/RESTARTDAEMON"];

const NOT_WINDOWS: &str = "This host can't update itself remotely yet on this system.";
const DEV_BUILD: &str = "This host runs a development build, which doesn't update itself.";
const ELEVATED: &str = "This host's daemon runs elevated; update it from the host.";
const NO_INSTALL: &str = "No installed copy of rustling-tulip was found on this host.";
const NO_UPDATES_DIR: &str = "This host has no updates folder.";
const NOT_OWN_INSTALL: &str =
    "This daemon doesn't run from the installed copy, so it can't update it.";
const ANOTHER_UPDATE: &str = "Another update is running on this host.";
const SOMETHING_RUNS: &str =
    "Something else is running from this host's install folder; close it and try again.";
const NO_LOCK: &str = "This host couldn't take the update lock.";
const ALREADY_NEWEST: &str = "This host already runs the newest release.";
const DOWNLOAD_FAILED: &str = "This host couldn't download the update; try again later.";
const CANCELED: &str = "The update was cancelled on the host.";
const STOPPING: &str = "The host's daemon is stopping; try again once it is back.";
const NO_INSTALLER: &str = "This host couldn't start the installer.";
const STARTED: &str = "The update has already started on this host.";
const NO_COUNTDOWN: &str = "No host update with that id is counting down.";

/// A refusal's error code and message.
type Refusal = (&'static str, &'static str);

/// Where this host's installed copy is.
pub(crate) trait InstallLocator {
    /// The install folder, when an installed copy (one holding the daemon) is
    /// found.
    fn install_folder(&self) -> Option<PathBuf>;
}

/// The update mutex the installer's `apply-update` also takes.
pub(crate) trait UpdateLock: Send + Sync {
    /// Takes the mutex without waiting: the hold keeps it until dropped, on
    /// the thread that took it; `None` when another process holds it.
    ///
    /// # Errors
    ///
    /// Fails when the mutex cannot be created or waited on.
    fn try_lock(&self) -> anyhow::Result<Option<Box<dyn std::any::Any>>>;
}

/// Where a release newer than this host's comes from.
pub(crate) trait UpdateSource: Send + Sync {
    /// The release newer than `current`, downloaded and verified under
    /// `root`; `None` when this host runs the newest.
    ///
    /// # Errors
    ///
    /// Fails when the feed, the download or the verification fails.
    fn fetch(
        &self,
        current: &semver::Version,
        root: &Path,
    ) -> anyhow::Result<Option<update_core::Ready>>;
}

/// Starts the verified installer.
pub(crate) trait Launch: Send + Sync {
    /// Starts `program` with `args`, detached from this daemon.
    ///
    /// # Errors
    ///
    /// Fails when the program does not start.
    fn launch(&self, program: &Path, args: &[OsString]) -> anyhow::Result<()>;
}

/// The update's own seams: the mutex, the release source, the installer
/// launch, and the updates folder downloads go under.
pub(crate) struct Seams {
    pub(crate) lock: Box<dyn UpdateLock>,
    pub(crate) source: Box<dyn UpdateSource>,
    pub(crate) launcher: Box<dyn Launch>,
    pub(crate) updates_dir: Box<dyn Fn() -> anyhow::Result<PathBuf> + Send + Sync>,
}

impl Seams {
    /// This machine's: the named mutex, the release feed, a detached launch
    /// and the client's updates folder.
    pub(crate) fn system() -> Self {
        Self {
            lock: Box::new(NamedMutex),
            source: Box::new(ReleaseFeed),
            launcher: Box::new(DetachedLaunch),
            updates_dir: Box::new(daemon_client::updates_dir),
        }
    }
}

/// What a remote update checks the host against, the seams it runs through,
/// and the guard that keeps one update at a time in this daemon.
pub(crate) struct HostUpdate {
    install: Box<dyn InstallLocator + Send + Sync>,
    processes: Box<dyn ProcessList + Send + Sync>,
    seams: Seams,
    /// True while an update runs in this daemon; checked and set in one lock
    /// hold, never held across an `.await`. It stays set once the installer
    /// has started, which is about to stop this daemon.
    in_flight: std::sync::Mutex<bool>,
    /// True from the update's start until it answers anything but started:
    /// the idle-exit watcher's hold-off for it.
    pub(crate) update_in_flight: Arc<AtomicBool>,
    /// How long an update counts down while a local connection is attached.
    pub(crate) countdown: Duration,
    /// How long the launch waits for the app to leave the install folder.
    pub(crate) app_quit_wait: Duration,
    /// The local connections attached now ([`HostUpdate::attach_local`]).
    local_connections: Arc<AtomicUsize>,
    /// The countdown's phase. Every host update notice is published under
    /// this lock, and a snapshot is read and sent under it.
    phase: std::sync::Mutex<Countdown>,
}

/// Where the host update's countdown stands, and how the last one ended.
#[derive(Default)]
struct Countdown {
    phase: Phase,
    last: Option<Ended>,
}

#[derive(Default)]
enum Phase {
    /// No countdown and no launch.
    #[default]
    Idle,
    Counting(Counting),
    /// From `HostUpdateLaunched` until the installer starts, by update id.
    Launching(String),
}

/// An update counting down.
struct Counting {
    update_id: String,
    requested_by: String,
    version: String,
    deadline: Instant,
    /// Wakes the countdown when a cancel wins.
    cancel: oneshot::Sender<()>,
}

impl Counting {
    /// The notice for this countdown, with the seconds left.
    fn scheduled(&self) -> DaemonMessage {
        let left = self.deadline.saturating_duration_since(Instant::now());
        let seconds = left.as_secs() + u64::from(left.subsec_nanos() > 0);
        DaemonMessage::HostUpdateScheduled {
            update_id: self.update_id.clone(),
            requested_by: self.requested_by.clone(),
            version: self.version.clone(),
            seconds: u32::try_from(seconds).unwrap_or(u32::MAX),
        }
    }
}

/// How a counted-down update ended, by update id.
enum Ended {
    Canceled(String),
    Launched(String),
}

/// Keeps a local connection counted while it is served.
pub(crate) struct LocalAttach(Arc<AtomicUsize>);

impl Drop for LocalAttach {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Publishes a host update notice; only local connections forward it.
fn publish(events: &broadcast::Sender<StateEvent>, message: DaemonMessage) {
    // No subscriber is no connection to tell.
    let _ = events.send(StateEvent::HostUpdate(Box::new(message)));
}

/// A fresh update id: unique in this daemon run, and carrying the process id
/// so a later daemon's ids differ.
fn mint_update_id() -> String {
    let number = NEXT_UPDATE.fetch_add(1, Ordering::Relaxed);
    format!("host-update-{}-{number}", std::process::id())
}

impl HostUpdate {
    /// Checks against `install` and `processes`, updating through `seams`, no
    /// update running.
    pub(crate) fn new(
        install: Box<dyn InstallLocator + Send + Sync>,
        processes: Box<dyn ProcessList + Send + Sync>,
        seams: Seams,
    ) -> Self {
        Self {
            install,
            processes,
            seams,
            in_flight: std::sync::Mutex::new(false),
            update_in_flight: Arc::default(),
            countdown: COUNTDOWN,
            app_quit_wait: APP_QUIT_WAIT,
            local_connections: Arc::default(),
            phase: std::sync::Mutex::default(),
        }
    }

    /// Counts a local connection as attached until the hold is dropped.
    pub(crate) fn attach_local(&self) -> LocalAttach {
        self.local_connections.fetch_add(1, Ordering::SeqCst);
        LocalAttach(Arc::clone(&self.local_connections))
    }

    /// Sends `out_tx` the current phase: `HostUpdateScheduled` with the
    /// seconds left while counting, `HostUpdateLaunched` while launching,
    /// `HostUpdateCanceled` when the last update ended in a cancel. Read and
    /// sent under the lock every notice is published under.
    pub(crate) fn send_phase(&self, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
        let state = lock(&self.phase);
        let message = match (&state.phase, &state.last) {
            (Phase::Counting(counting), _) => Some(counting.scheduled()),
            (Phase::Launching(update_id), _) => Some(DaemonMessage::HostUpdateLaunched {
                update_id: update_id.clone(),
            }),
            (Phase::Idle, Some(Ended::Canceled(update_id))) => {
                Some(DaemonMessage::HostUpdateCanceled {
                    update_id: update_id.clone(),
                })
            }
            (Phase::Idle, _) => None,
        };
        if let Some(message) = message {
            // A closed outbox is a connection already ending.
            let _ = out_tx.send(message);
        }
        drop(state);
    }

    /// Starts counting down `update_id` and tells every local connection;
    /// the receiver wakes when a cancel wins.
    fn begin_counting(
        &self,
        events: &broadcast::Sender<StateEvent>,
        update_id: &str,
        requested_by: &str,
        version: &str,
    ) -> oneshot::Receiver<()> {
        let (cancel, canceled) = oneshot::channel();
        let counting = Counting {
            update_id: update_id.to_owned(),
            requested_by: requested_by.to_owned(),
            version: version.to_owned(),
            deadline: Instant::now() + self.countdown,
            cancel,
        };
        let mut state = lock(&self.phase);
        publish(events, counting.scheduled());
        *state = Countdown {
            phase: Phase::Counting(counting),
            last: None,
        };
        drop(state);
        canceled
    }

    /// A countdown as an update starts one, for a test of what connections
    /// are sent; its id.
    #[cfg(test)]
    pub(crate) fn begin_test_countdown(&self, events: &broadcast::Sender<StateEvent>) -> String {
        let update_id = mint_update_id();
        let _canceled = self.begin_counting(events, &update_id, THIS_PC, "0.99.0");
        update_id
    }

    /// Cancels `update_id` when it is counting down, telling every local
    /// connection; else why not. Decided under the phase lock, so a cancel
    /// and the countdown's end never both win.
    fn cancel(&self, events: &broadcast::Sender<StateEvent>, update_id: &str) -> Option<Refusal> {
        let mut state = lock(&self.phase);
        match &state.phase {
            Phase::Counting(counting) if counting.update_id == update_id => {}
            Phase::Launching(id) if id == update_id => return Some((BUSY, STARTED)),
            _ => {
                let launched = matches!(&state.last, Some(Ended::Launched(id)) if id == update_id);
                return Some(if launched {
                    (BUSY, STARTED)
                } else {
                    (NOT_FOUND, NO_COUNTDOWN)
                });
            }
        }
        if let Phase::Counting(counting) = std::mem::take(&mut state.phase) {
            // The countdown may already be ending; it reads the phase then.
            let _ = counting.cancel.send(());
            publish(
                events,
                DaemonMessage::HostUpdateCanceled {
                    update_id: update_id.to_owned(),
                },
            );
            state.last = Some(Ended::Canceled(update_id.to_owned()));
        }
        drop(state);
        None
    }

    /// Moves `update_id` from counting to launching and tells every local
    /// connection; false when a cancel won first.
    fn start_launching(&self, events: &broadcast::Sender<StateEvent>, update_id: &str) -> bool {
        let mut state = lock(&self.phase);
        if !matches!(&state.phase, Phase::Counting(counting) if counting.update_id == update_id) {
            return false;
        }
        state.phase = Phase::Launching(update_id.to_owned());
        publish(
            events,
            DaemonMessage::HostUpdateLaunched {
                update_id: update_id.to_owned(),
            },
        );
        drop(state);
        true
    }

    /// Ends `update_id`'s countdown without a notice: the daemon is going.
    fn abandon(&self, update_id: &str) {
        let mut state = lock(&self.phase);
        if matches!(&state.phase, Phase::Counting(counting) if counting.update_id == update_id) {
            state.phase = Phase::Idle;
        }
    }

    /// Ends `update_id`'s launching phase once the worker answered; a
    /// started installer is remembered, so a late cancel is told so.
    fn end_launching(&self, update_id: &str, launched: bool) {
        let mut state = lock(&self.phase);
        if matches!(&state.phase, Phase::Launching(id) if id == update_id) {
            state.phase = Phase::Idle;
            if launched {
                state.last = Some(Ended::Launched(update_id.to_owned()));
            }
        }
    }

    /// Checks against this machine, the installer's registry entry and the
    /// running processes, and updates through its seams.
    pub(crate) fn system() -> Self {
        Self::new(
            Box::new(RegistryInstall),
            Box::new(SystemProcesses),
            Seams::system(),
        )
    }

    /// A test hub's checks: no install, no processes.
    #[cfg(test)]
    pub(crate) fn nothing_installed() -> Self {
        struct Nothing;
        impl InstallLocator for Nothing {
            fn install_folder(&self) -> Option<PathBuf> {
                None
            }
        }
        impl ProcessList for Nothing {
            fn running(&self) -> Vec<RunningExe> {
                Vec::new()
            }
        }
        Self::new(Box::new(Nothing), Box::new(Nothing), Seams::system())
    }

    /// Claims the guard: false when an update already holds it.
    fn try_begin(&self) -> bool {
        let mut in_flight = lock(&self.in_flight);
        if *in_flight {
            return false;
        }
        *in_flight = true;
        true
    }

    /// Releases the guard and the idle-exit hold-off.
    fn finish(&self) {
        self.update_in_flight.store(false, Ordering::SeqCst);
        *lock(&self.in_flight) = false;
    }
}

/// What the checks hand the update: the version this host runs, the
/// updates folder, and the install folder the app runs from.
struct Plan {
    current: semver::Version,
    updates_root: PathBuf,
    install: PathBuf,
}

/// Who asked for an update, and where its answer goes.
struct Requester {
    request_id: String,
    requested_by: String,
    out_tx: mpsc::UnboundedSender<DaemonMessage>,
}

/// Where the countdown's notices go, and the daemon's shutdown signal.
struct Notices {
    events: broadcast::Sender<StateEvent>,
    shutdown: watch::Receiver<bool>,
}

/// The runtime's word on the verified release.
enum Decision {
    /// Go ahead; whether the app ran from the install folder as the launch
    /// began.
    Launch { app_running: bool },
    /// A local connection cancelled the update on the host.
    Cancel,
    /// The daemon is stopping.
    Stopping,
}

/// Handle the host update messages `dispatch` routes here.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    match msg {
        ClientMessage::RemoteUpdate { request_id } => request(hub, request_id, ctx).await,
        ClientMessage::CancelHostUpdate { update_id } => cancel(hub, &update_id, ctx.out_tx),
        other => debug!(
            ?other,
            "remote update: a message that is not a host update message was routed here"
        ),
    }
    Ok(())
}

/// A local connection's cancel: refused with no request id when it does not
/// win.
fn cancel(hub: &Hub, update_id: &str, out_tx: &mpsc::UnboundedSender<DaemonMessage>) {
    let Some((code, message)) = hub.host_update.cancel(&hub.state_events, update_id) else {
        info!(%update_id, "remote update: cancelled by a local connection");
        return;
    };
    info!(%update_id, code, "remote update: cancel refused");
    send(
        out_tx,
        DaemonMessage::Error {
            message: message.to_owned(),
            request_id: None,
            code: Some(code.to_owned()),
        },
    );
}

/// Who asked, as display text: a tailnet device's name (bounded, control
/// characters dropped), else this PC.
fn requested_by(auth: &AuthKind) -> String {
    match auth {
        AuthKind::Local => THIS_PC.to_owned(),
        AuthKind::Tailnet(peer) => peer
            .name
            .chars()
            .filter(|c| !c.is_control())
            .take(REQUESTER_CHARS)
            .collect(),
    }
}

/// An update request: the checks, then the update.
async fn request(hub: &Hub, request_id: String, ctx: &ConnCtx<'_>) {
    let checking = hub.clone();
    let checked = tokio::task::spawn_blocking(move || begin(&checking)).await;
    let plan = match checked {
        Ok(Ok(plan)) => plan,
        Ok(Err((code, message))) => {
            send(
                ctx.out_tx,
                refusal_reply(request_id, code, message.to_owned()),
            );
            return;
        }
        Err(err) => {
            let message = format!("remote update: the checks failed to run: {err}");
            send(ctx.out_tx, refusal_reply(request_id, INTERNAL, message));
            return;
        }
    };
    let requester = Requester {
        request_id,
        requested_by: requested_by(ctx.auth),
        out_tx: ctx.out_tx.clone(),
    };
    let notices = Notices {
        events: hub.state_events.clone(),
        shutdown: hub.shutdown_tx.subscribe(),
    };
    start(Arc::clone(&hub.host_update), plan, requester, notices);
}

/// The checks, then the guard: the plan when the update may start. Blocking
/// work.
fn begin(hub: &Hub) -> Result<Plan, Refusal> {
    let plan = host_check(hub)?;
    if !hub.host_update.try_begin() {
        return Err((BUSY, ANOTHER_UPDATE));
    }
    Ok(plan)
}

/// Starts the update on its own thread, and a task that runs its go-ahead
/// and sends its one answer to the requester. The guard is already held.
fn start(update: Arc<HostUpdate>, plan: Plan, requester: Requester, notices: Notices) {
    let (answer_tx, answer_rx) = oneshot::channel();
    let (ready_tx, ready_rx) = oneshot::channel::<semver::Version>();
    let (decision_tx, decision_rx) = std::sync::mpsc::channel::<Decision>();
    update.update_in_flight.store(true, Ordering::SeqCst);
    let install = plan.install.clone();
    let worker = Arc::clone(&update);
    let spawned = std::thread::Builder::new()
        .name("remote-update".to_owned())
        .spawn(move || {
            // The verified version goes to the runtime, which answers
            // Launch, Cancel or Stopping; a dropped sender, a daemon shutting
            // down, reads as a cancel.
            let go = move |version: &semver::Version| {
                if ready_tx.send(version.clone()).is_err() {
                    return Decision::Cancel;
                }
                decision_rx.recv().unwrap_or(Decision::Cancel)
            };
            let outcome = run(&worker, &plan, go);
            if outcome.is_err() {
                worker.finish();
            }
            // A dropped receiver is a daemon shutting down.
            let _ = answer_tx.send(outcome);
        });
    if let Err(err) = spawned {
        warn!(error = %err, "remote update: the update thread didn't start");
        update.finish();
        let message = "remote update: the update thread didn't start".to_owned();
        send(
            &requester.out_tx,
            refusal_reply(requester.request_id, INTERNAL, message),
        );
        return;
    }
    tokio::spawn(async move {
        let mut counted = None;
        if let Ok(version) = ready_rx.await {
            let (decision, update_id) = decide(
                &update,
                &install,
                &version,
                &requester.requested_by,
                notices,
            )
            .await;
            counted = update_id;
            // A worker that is gone has already answered.
            let _ = decision_tx.send(decision);
        }
        drop(decision_tx);
        let outcome = answer_rx.await;
        if let Some(update_id) = &counted {
            update.end_launching(update_id, matches!(outcome, Ok(Ok(_))));
        }
        let request_id = requester.request_id;
        let reply = match outcome {
            Ok(Ok(version)) => {
                info!(%request_id, %version, "remote update: installer started");
                DaemonMessage::RemoteUpdateStarted {
                    request_id,
                    version: version.to_string(),
                }
            }
            Ok(Err((code, message))) => refusal_reply(request_id, code, message.to_owned()),
            Err(_) => {
                update.finish();
                let message = "remote update: the update ended without an answer".to_owned();
                refusal_reply(request_id, INTERNAL, message)
            }
        };
        send(&requester.out_tx, reply);
    });
}

/// The go-ahead for the verified `version`: at once with no local
/// connection attached, probing once for the app; else the countdown, a
/// cancel or a shutdown ending it, then the look for the app before
/// `HostUpdateLaunched` goes out and the wait for it to leave. The decision,
/// and the id the countdown ran under.
async fn decide(
    update: &Arc<HostUpdate>,
    install: &Path,
    version: &semver::Version,
    requested_by: &str,
    mut notices: Notices,
) -> (Decision, Option<String>) {
    if update.local_connections.load(Ordering::SeqCst) == 0 {
        info!("remote update: no local connection; launching at once");
        let app_running = probe_app(update, install).await;
        return (Decision::Launch { app_running }, None);
    }
    let update_id = mint_update_id();
    let canceled = update.begin_counting(
        &notices.events,
        &update_id,
        requested_by,
        &version.to_string(),
    );
    info!(%update_id, %version, "remote update: counting down on the host");
    // A cancel wins a tie with a shutdown, and a stopping daemon wins a tie
    // with the countdown's end.
    tokio::select! {
        biased;
        _ = canceled => return (Decision::Cancel, Some(update_id)),
        _ = notices.shutdown.wait_for(|stopping| *stopping) => {
            info!(%update_id, "remote update: the daemon is stopping; update refused");
            update.abandon(&update_id);
            return (Decision::Stopping, Some(update_id));
        }
        () = tokio::time::sleep(update.countdown) => {}
    }
    // The app quits the moment `HostUpdateLaunched` reaches it, so it is
    // looked for before that notice goes out; a cancel still wins inside
    // `start_launching`.
    let seen = probe_app(update, install).await;
    if !update.start_launching(&notices.events, &update_id) {
        return (Decision::Cancel, Some(update_id));
    }
    let app_running = wait_for_app(update, install, seen).await;
    (Decision::Launch { app_running }, Some(update_id))
}

/// Waits until the app no longer runs from `install`, looking every
/// [`APP_POLL`], for at most the update's app-quit wait; the launch goes
/// ahead either way. Whether the app ran from `install` as the launch began,
/// `seen` being the look taken before `HostUpdateLaunched` went out.
async fn wait_for_app(update: &Arc<HostUpdate>, install: &Path, mut open: bool) -> bool {
    if !open {
        return false;
    }
    let deadline = Instant::now() + update.app_quit_wait;
    while open {
        let now = Instant::now();
        if now >= deadline {
            info!("remote update: the app is still open; launching anyway");
            break;
        }
        tokio::time::sleep(APP_POLL.min(deadline - now)).await;
        open = probe_app(update, install).await;
    }
    true
}

/// Whether the app runs from `install` right now.
async fn probe_app(update: &Arc<HostUpdate>, install: &Path) -> bool {
    let probe = Arc::clone(update);
    let folder = install.to_path_buf();
    let looked = tokio::task::spawn_blocking(move || app_runs(&probe.processes.running(), &folder));
    match looked.await {
        Ok(running) => running,
        Err(err) => {
            tracing::warn!(error = %err, "remote update: the app probe failed; treating the app as not running");
            false
        }
    }
}

/// Whether the app, other than this process, runs from `install`.
fn app_runs(running: &[RunningExe], install: &Path) -> bool {
    let folder = crate::normalize_process_path(install);
    let own_pid = std::process::id();
    running.iter().any(|process| {
        process.pid != own_pid && is_client(&process.exe) && runs_from(&process.exe, &folder)
    })
}

/// Whether `exe` sits directly in the folder whose normalized path is
/// `folder`.
fn runs_from(exe: &Path, folder: &str) -> bool {
    exe.parent()
        .is_some_and(|parent| crate::normalize_process_path(parent) == folder)
}

/// The update, on its own thread: the mutex, old downloads cleared, the
/// download, `go`'s word on the verified version, the launch. The version
/// started, or why not.
fn run(
    update: &HostUpdate,
    plan: &Plan,
    go: impl FnOnce(&semver::Version) -> Decision,
) -> Result<semver::Version, Refusal> {
    let seams = &update.seams;
    let hold = take_lock(seams.lock.as_ref())?;
    update_core::clean_old(&plan.updates_root, None);
    let ready = download(seams.source.as_ref(), plan)?;
    let app_running = match go(&ready.version) {
        Decision::Launch { app_running } => app_running,
        Decision::Cancel => {
            info!(version = %ready.version, "remote update: cancelled on the host");
            return Err((UPDATE_CANCELED, CANCELED));
        }
        Decision::Stopping => {
            info!(version = %ready.version, "remote update: the daemon is stopping");
            return Err((UPDATE_REFUSED, STOPPING));
        }
    };
    launch(seams.launcher.as_ref(), &ready, app_running)?;
    // The installer's `apply-update` takes the mutex next; the verified file
    // stays open until the installer has started.
    drop(hold);
    let version = ready.version.clone();
    drop(ready);
    Ok(version)
}

/// The update mutex's hold, or why it can't be taken.
fn take_lock(lock: &dyn UpdateLock) -> Result<Box<dyn std::any::Any>, Refusal> {
    match lock.try_lock() {
        Ok(Some(hold)) => Ok(hold),
        Ok(None) => Err((BUSY, ANOTHER_UPDATE)),
        Err(err) => {
            warn!(error = %format!("{err:#}"), "remote update: taking the update lock failed");
            Err((UPDATE_REFUSED, NO_LOCK))
        }
    }
}

/// The verified release newer than this host's, or why there is none. A
/// failure's text stays in the log: it can name paths.
fn download(source: &dyn UpdateSource, plan: &Plan) -> Result<update_core::Ready, Refusal> {
    match source.fetch(&plan.current, &plan.updates_root) {
        Ok(Some(ready)) => Ok(ready),
        Ok(None) => Err((UPDATE_REFUSED, ALREADY_NEWEST)),
        Err(err) => {
            warn!(error = %format!("{err:#}"), "remote update: the download failed");
            Err((UPDATE_REFUSED, DOWNLOAD_FAILED))
        }
    }
}

/// Starts `ready`'s installer with [`INSTALLER_ARGS`], or with
/// [`INSTALLER_ARGS_RELAUNCH`] when the app ran from the install folder as
/// the launch began.
fn launch(
    launcher: &dyn Launch,
    ready: &update_core::Ready,
    app_running: bool,
) -> Result<(), Refusal> {
    let set: &[&str] = if app_running {
        &INSTALLER_ARGS_RELAUNCH
    } else {
        &INSTALLER_ARGS
    };
    let args: Vec<OsString> = set.iter().map(OsString::from).collect();
    launcher.launch(&ready.path, &args).map_err(|err| {
        warn!(error = %format!("{err:#}"), "remote update: the installer didn't start");
        (UPDATE_REFUSED, NO_INSTALLER)
    })
}

/// An `Error` answering `request_id` with `code` and `message`.
fn refusal_reply(request_id: String, code: &str, message: String) -> DaemonMessage {
    info!(%request_id, code, "remote update: refused");
    DaemonMessage::Error {
        message,
        request_id: Some(request_id),
        code: Some(code.to_owned()),
    }
}

fn send(out_tx: &mpsc::UnboundedSender<DaemonMessage>, reply: DaemonMessage) {
    // A closed outbox is a connection already ending.
    let _ = out_tx.send(reply);
}

/// The first check that refuses this host's update; the plan when every
/// check passes.
fn host_check(hub: &Hub) -> Result<Plan, Refusal> {
    if let Some(refusal) = platform_refusal()
        .or_else(dev_build_refusal)
        .or_else(|| elevated_refusal(hub))
    {
        return Err(refusal);
    }
    let current =
        semver::Version::parse(protocol::BUILD_VERSION).map_err(|_| (UPDATE_REFUSED, DEV_BUILD))?;
    let update = &hub.host_update;
    let Some(install) = update.install.install_folder() else {
        return Err((UPDATE_REFUSED, NO_INSTALL));
    };
    let updates_root = (update.seams.updates_dir)().map_err(|err| {
        warn!(error = %format!("{err:#}"), "remote update: no updates folder");
        (UPDATE_REFUSED, NO_UPDATES_DIR)
    })?;
    if let Some(refusal) = own_install_refusal(&install, hub.dirs.bin_templates.as_deref())
        .or_else(|| process_refusal(&install, &update.processes.running(), &updates_root))
    {
        return Err(refusal);
    }
    Ok(Plan {
        current,
        updates_root,
        install,
    })
}

fn platform_refusal() -> Option<Refusal> {
    (!cfg!(windows)).then_some((UPDATE_REFUSED, NOT_WINDOWS))
}

fn dev_build_refusal() -> Option<Refusal> {
    (!update_core::checks_allowed()).then_some((UPDATE_REFUSED, DEV_BUILD))
}

fn elevated_refusal(hub: &Hub) -> Option<Refusal> {
    hub.integrity
        .is_elevated()
        .then_some((UPDATE_REFUSED, ELEVATED))
}

/// Refused unless this daemon's own folder (`bin_templates`) is `install`.
fn own_install_refusal(install: &Path, own: Option<&Path>) -> Option<Refusal> {
    let own = own.map(crate::normalize_process_path);
    (own != Some(crate::normalize_process_path(install)))
        .then_some((UPDATE_REFUSED, NOT_OWN_INSTALL))
}

/// Busy while a process other than this one runs from `install`'s staging
/// folder or from under `updates`; refused while anything but the app runs
/// from `install` itself.
fn process_refusal(install: &Path, running: &[RunningExe], updates: &Path) -> Option<Refusal> {
    let own_pid = std::process::id();
    let others = || running.iter().filter(move |process| process.pid != own_pid);
    let staging = install.join(STAGING_DIR);
    let updating = others().any(|process| {
        crate::path_is_under(&process.exe, &staging) || crate::path_is_under(&process.exe, updates)
    });
    if updating {
        return Some((BUSY, ANOTHER_UPDATE));
    }
    // The app is asked to quit before the launch, so it refuses nothing.
    let folder = crate::normalize_process_path(install);
    let blocking =
        others().any(|process| !is_client(&process.exe) && runs_from(&process.exe, &folder));
    blocking.then_some((UPDATE_REFUSED, SOMETHING_RUNS))
}

/// Whether `exe` is the app, which the launch waits for to quit.
fn is_client(exe: &Path) -> bool {
    exe.file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(CLIENT_EXE))
}

/// The installer's named update mutex.
struct NamedMutex;

impl UpdateLock for NamedMutex {
    fn try_lock(&self) -> anyhow::Result<Option<Box<dyn std::any::Any>>> {
        update_core::try_lock(update_core::UPDATE_MUTEX)
    }
}

/// The published releases' feed.
struct ReleaseFeed;

impl UpdateSource for ReleaseFeed {
    fn fetch(
        &self,
        current: &semver::Version,
        root: &Path,
    ) -> anyhow::Result<Option<update_core::Ready>> {
        update_core::check_and_download(current, root, &update_core::feed_base())
    }
}

/// A launch detached from this daemon, as the app starts its installer.
struct DetachedLaunch;

impl Launch for DetachedLaunch {
    #[cfg(any(windows, target_os = "macos"))]
    fn launch(&self, program: &Path, args: &[OsString]) -> anyhow::Result<()> {
        update_core::launch_detached(program, args)
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn launch(&self, program: &Path, _args: &[OsString]) -> anyhow::Result<()> {
        anyhow::bail!(
            "no installer launch on this system for {}",
            program.display()
        )
    }
}

/// The installer's uninstall entry: its `InstallLocation`, when the folder
/// holds the daemon.
struct RegistryInstall;

impl InstallLocator for RegistryInstall {
    fn install_folder(&self) -> Option<PathBuf> {
        let folder = PathBuf::from(registry::install_location()?);
        folder.join(DAEMON_EXE).is_file().then_some(folder)
    }
}

#[cfg(windows)]
mod registry {
    use tracing::warn;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::w;

    /// Reads of a value whose size can change between the size query and the
    /// read, the size query included.
    const ATTEMPTS: usize = 4;

    /// The installer's `InstallLocation` under its per-user uninstall key
    /// (`UNINSTKEY` in `tools/installer/rustling-tulip.nsi`). `None` when the
    /// key or value is absent or can't be read (logged).
    pub(super) fn install_location() -> Option<String> {
        let mut size: u32 = 0;
        for _ in 0..ATTEMPTS {
            let mut buf = vec![0u16; (size as usize).div_ceil(2)];
            let mut written = u32::try_from(buf.len() * 2).unwrap_or(u32::MAX);
            let data = (!buf.is_empty()).then(|| buf.as_mut_ptr().cast());
            // SAFETY: the key and value names are valid null-terminated wide
            // strings; `buf` holds `written` bytes and outlives the call, which
            // writes at most that many bytes into it and the byte count it
            // needs or wrote into `written`; with no buffer it writes only the
            // size.
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\rustling-tulip"),
                    w!("InstallLocation"),
                    RRF_RT_REG_SZ,
                    None,
                    data,
                    Some(&raw mut written),
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return None;
            }
            if status == ERROR_MORE_DATA || (status == ERROR_SUCCESS && buf.is_empty()) {
                size = written;
                continue;
            }
            if status != ERROR_SUCCESS {
                warn!(
                    code = status.0,
                    "remote update: reading the install location failed"
                );
                return None;
            }
            let chars = (written as usize / 2).min(buf.len());
            let text = &buf[..chars];
            let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
            return Some(String::from_utf16_lossy(&text[..end]));
        }
        warn!("remote update: the install location kept growing while being read");
        None
    }
}

#[cfg(not(windows))]
mod registry {
    /// No installer registry off Windows.
    pub(super) fn install_location() -> Option<String> {
        None
    }
}

/// This machine's processes, read through sysinfo.
struct SystemProcesses;

impl ProcessList for SystemProcesses {
    fn running(&self) -> Vec<RunningExe> {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

        let refresh = ProcessRefreshKind::new().with_exe(UpdateKind::Always);
        let mut sys = System::new_with_specifics(RefreshKind::new().with_processes(refresh));
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
        sys.processes()
            .iter()
            .filter_map(|(pid, process)| {
                Some(RunningExe {
                    pid: pid.as_u32(),
                    exe: process.exe()?.to_path_buf(),
                })
            })
            .collect()
    }
}

#[cfg(all(test, windows))]
#[expect(
    clippy::panic,
    reason = "tests fail with the reply they got; panic messages aid debugging"
)]
mod tests {
    use super::super::test_support::{
        HANG_GUARD, ScratchDir, dispatch_on, dispatch_one, test_hub_in,
    };
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, OnceLock, Weak};
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc};
    use tokio::time::Instant;

    /// The folder under the scratch dir the hub's daemon runs from.
    const OWN: &str = "install";

    /// Text the failing download's error carries, which no reply may.
    const DOWNLOAD_ERROR: &str = r"C:\private\updates\0.99.0-x\setup.exe: connection reset";

    struct FakeInstall(Option<PathBuf>);

    impl InstallLocator for FakeInstall {
        fn install_folder(&self) -> Option<PathBuf> {
            self.0.clone()
        }
    }

    /// The processes running, which a test may change mid-update, and how
    /// many times they were read.
    #[derive(Clone, Default)]
    struct FakeProcesses {
        running: Arc<std::sync::Mutex<Vec<RunningExe>>>,
        reads: Arc<AtomicUsize>,
        /// Whether the app quits the moment the update launches, as a client
        /// does when the launch notice reaches it.
        quits_on_launch: Arc<AtomicBool>,
        /// The update these processes are read for, which tells whether it
        /// has launched.
        update: Arc<OnceLock<Weak<HostUpdate>>>,
    }

    impl ProcessList for FakeProcesses {
        fn running(&self) -> Vec<RunningExe> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.quits_on_launch.load(Ordering::SeqCst) && self.launching() {
                return Vec::new();
            }
            lock(&self.running).clone()
        }
    }

    impl FakeProcesses {
        /// Whether the update has left its countdown for the launch, which is
        /// when a client that quits on the notice is gone.
        fn launching(&self) -> bool {
            let Some(update) = self.update.get().and_then(Weak::upgrade) else {
                return false;
            };
            matches!(lock(&update.phase).phase, Phase::Launching(_))
        }
    }

    /// The update lock: free, or held by another process.
    struct FakeLock {
        held: bool,
    }

    impl UpdateLock for FakeLock {
        fn try_lock(&self) -> anyhow::Result<Option<Box<dyn std::any::Any>>> {
            Ok((!self.held).then(|| Box::new(()) as Box<dyn std::any::Any>))
        }
    }

    /// What the release feed has.
    #[derive(Clone, Copy)]
    enum Feed {
        /// Nothing newer than this host.
        Newest,
        /// A download that fails.
        Failing,
        /// Release `0.99.0`, downloaded and verified.
        Release,
    }

    struct FakeSource {
        feed: Feed,
        /// The verified installer a release hands back.
        installer: PathBuf,
    }

    impl UpdateSource for FakeSource {
        fn fetch(
            &self,
            _current: &semver::Version,
            _root: &Path,
        ) -> anyhow::Result<Option<update_core::Ready>> {
            match self.feed {
                Feed::Newest => Ok(None),
                Feed::Failing => anyhow::bail!("{DOWNLOAD_ERROR}"),
                Feed::Release => Ok(Some(update_core::Ready {
                    version: semver::Version::new(0, 99, 0),
                    path: self.installer.clone(),
                    file: std::fs::File::open(&self.installer)?,
                    notes: String::new(),
                })),
            }
        }
    }

    /// Every launch: the program and its arguments.
    type Launches = Arc<std::sync::Mutex<Vec<(PathBuf, Vec<OsString>)>>>;

    struct FakeLauncher {
        fails: bool,
        seen: Launches,
    }

    impl Launch for FakeLauncher {
        fn launch(&self, program: &Path, args: &[OsString]) -> anyhow::Result<()> {
            lock(&self.seen).push((program.to_path_buf(), args.to_vec()));
            if self.fails {
                anyhow::bail!("the installer would not start");
            }
            Ok(())
        }
    }

    /// How the fakes behind the checks answer.
    #[derive(Clone, Copy)]
    struct Setup {
        lock_held: bool,
        feed: Feed,
        launch_fails: bool,
    }

    const SETUP: Setup = Setup {
        lock_held: false,
        feed: Feed::Release,
        launch_fails: false,
    };

    /// A hub over the fakes, and what its launcher saw.
    struct Rig {
        hub: Hub,
        _scratch: ScratchDir,
        launches: Launches,
        installer: PathBuf,
        processes: FakeProcesses,
    }

    /// A hub whose daemon runs from `<scratch>/install`, the locator finding
    /// `<scratch>/<install>`, other processes running the exes at
    /// `<scratch>/<running>`, and the update seams answering as `setup` says.
    fn rig(tag: &str, install: Option<&str>, running: &[&str], setup: Setup) -> Rig {
        let scratch = ScratchDir::new(tag);
        let root = scratch.path().to_path_buf();
        let downloads = root.join("downloads");
        std::fs::create_dir_all(&downloads).unwrap_or_else(|err| panic!("downloads folder: {err}"));
        let installer = downloads.join("rustling-tulip-0.99.0-setup.exe");
        std::fs::write(&installer, b"installer").unwrap_or_else(|err| panic!("installer: {err}"));
        let (mut hub, scratch) = test_hub_in(scratch, Some(root.join(OWN)));
        let other = std::process::id().wrapping_add(1);
        let processes = FakeProcesses::default();
        *lock(&processes.running) = running
            .iter()
            .map(|exe| RunningExe {
                pid: other,
                exe: root.join(exe),
            })
            .collect();
        let launches = Launches::default();
        let updates_root = root.join("update-root");
        let seams = Seams {
            lock: Box::new(FakeLock {
                held: setup.lock_held,
            }),
            source: Box::new(FakeSource {
                feed: setup.feed,
                installer: installer.clone(),
            }),
            launcher: Box::new(FakeLauncher {
                fails: setup.launch_fails,
                seen: Arc::clone(&launches),
            }),
            updates_dir: Box::new(move || Ok(updates_root.clone())),
        };
        let update = Arc::new(HostUpdate::new(
            Box::new(FakeInstall(install.map(|folder| root.join(folder)))),
            Box::new(processes.clone()),
            seams,
        ));
        let _ = processes.update.set(Arc::downgrade(&update));
        hub.host_update = update;
        Rig {
            hub,
            _scratch: scratch,
            launches,
            installer,
            processes,
        }
    }

    /// A rig whose every check passes, its seams answering as `setup` says.
    fn passing(tag: &str, setup: Setup) -> Rig {
        rig(tag, Some(OWN), &["elsewhere/rustling-tulipd.exe"], setup)
    }

    fn request() -> ClientMessage {
        ClientMessage::RemoteUpdate {
            request_id: "u1".to_owned(),
        }
    }

    /// Sends the request and returns its one reply, failing when a second
    /// comes or none does.
    async fn ask(hub: &Hub) -> DaemonMessage {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        dispatch_on(hub, request(), 1, &out_tx).await;
        drop(out_tx);
        let reply = tokio::time::timeout(HANG_GUARD, out_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("hung: no reply to the update request"))
            .unwrap_or_else(|| panic!("the connection closed with no reply"));
        let more = tokio::time::timeout(HANG_GUARD, out_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("hung: the reply's sender was never dropped"));
        assert!(more.is_none(), "a second reply: {more:?}");
        reply
    }

    /// The code and message of a refusal carrying the request's id.
    fn refusal(reply: DaemonMessage) -> (String, String) {
        let DaemonMessage::Error {
            message,
            request_id,
            code,
        } = reply
        else {
            panic!("expected a refusal, got {reply:?}");
        };
        assert_eq!(request_id.as_deref(), Some("u1"));
        (code.unwrap_or_default(), message)
    }

    fn expected(code: &str, message: &str) -> (String, String) {
        (code.to_owned(), message.to_owned())
    }

    /// Whether this daemon's update guard is set.
    fn guard_set(hub: &Hub) -> bool {
        *lock(&hub.host_update.in_flight)
    }

    /// Whether the idle-exit hold-off for an update is set.
    fn held_off(hub: &Hub) -> bool {
        hub.host_update.update_in_flight.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn remote_update_refuses_a_host_with_no_install() {
        let rig = rig("update-no-install", None, &[], SETUP);

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, NO_INSTALL));
    }

    #[tokio::test]
    async fn remote_update_refuses_an_install_that_is_not_this_daemons() {
        let rig = rig("update-not-own", Some("elsewhere"), &[], SETUP);

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, NOT_OWN_INSTALL));
    }

    #[tokio::test]
    async fn remote_update_is_busy_while_a_process_runs_from_staging() {
        let rig = rig(
            "update-staging",
            Some(OWN),
            &["install/.staging/rustling-tulipd.exe"],
            SETUP,
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(BUSY, ANOTHER_UPDATE));
    }

    #[tokio::test]
    async fn remote_update_refuses_while_a_bridge_runs_from_the_install() {
        let rig = rig(
            "update-bridge",
            Some(OWN),
            &["install/rustling-tulipd.exe"],
            SETUP,
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, SOMETHING_RUNS));
    }

    #[tokio::test]
    async fn remote_update_is_busy_while_another_update_runs() {
        let rig = passing("update-in-flight", SETUP);
        *lock(&rig.hub.host_update.in_flight) = true;

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(BUSY, ANOTHER_UPDATE));
        assert!(guard_set(&rig.hub), "the running update keeps its guard");
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
    }

    #[tokio::test]
    async fn the_app_running_from_the_install_no_longer_refuses() {
        let app = rig(
            "update-app-open",
            Some(OWN),
            &[
                "install/rustling-tulip-native.exe",
                "elsewhere/rustling-tulipd.exe",
            ],
            SETUP,
        );

        let reply = ask(&app.hub).await;

        assert!(
            matches!(reply, DaemonMessage::RemoteUpdateStarted { .. }),
            "{reply:?}"
        );
        assert_eq!(lock(&app.launches).len(), 1, "launched once");

        let other = rig(
            "update-app-and-other",
            Some(OWN),
            &[
                "install/rustling-tulip-native.exe",
                "install/other-tool.exe",
            ],
            SETUP,
        );

        let reply = refusal(ask(&other.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, SOMETHING_RUNS));
        assert!(
            !guard_set(&other.hub),
            "a refusal before the guard takes none"
        );
        assert!(lock(&other.launches).is_empty(), "nothing launched");
    }

    /// Sends the request on a requester's outbox of its own and returns the
    /// receiver its answer comes on. No timer, so a paused clock does not
    /// move while the update's thread works.
    async fn send_request(hub: &Hub) -> mpsc::UnboundedReceiver<DaemonMessage> {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        dispatch_on(hub, request(), 1, &out_tx).await;
        out_rx
    }

    /// The requester's one answer, with no timer.
    async fn answer(replies: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> DaemonMessage {
        replies
            .recv()
            .await
            .unwrap_or_else(|| panic!("the requester's outbox closed with no answer"))
    }

    /// The next host update notice broadcast on `events`.
    async fn next_notice(events: &mut broadcast::Receiver<StateEvent>) -> DaemonMessage {
        loop {
            match events.recv().await {
                Ok(StateEvent::HostUpdate(message)) => return *message,
                Ok(_) => {}
                Err(err) => panic!("the state events failed: {err}"),
            }
        }
    }

    /// The update id a `HostUpdateScheduled` notice carries.
    fn scheduled_id(notice: &DaemonMessage) -> String {
        let DaemonMessage::HostUpdateScheduled { update_id, .. } = notice else {
            panic!("expected the countdown's start, got {notice:?}");
        };
        update_id.clone()
    }

    /// The code and message of a refusal that carries no request id.
    fn bare_refusal(reply: DaemonMessage) -> (String, String) {
        let DaemonMessage::Error {
            message,
            request_id,
            code,
        } = reply
        else {
            panic!("expected a refusal, got {reply:?}");
        };
        assert_eq!(request_id, None);
        (code.unwrap_or_default(), message)
    }

    fn cancel(update_id: &str) -> ClientMessage {
        ClientMessage::CancelHostUpdate {
            update_id: update_id.to_owned(),
        }
    }

    #[tokio::test]
    async fn with_no_local_connection_the_update_launches_at_once() {
        let rig = passing("update-no-local", SETUP);
        let mut events = rig.hub.state_events.subscribe();

        let reply = ask(&rig.hub).await;

        assert!(
            matches!(reply, DaemonMessage::RemoteUpdateStarted { .. }),
            "{reply:?}"
        );
        assert_eq!(lock(&rig.launches).len(), 1, "launched once");
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, StateEvent::HostUpdate(_)),
                "no countdown with no local connection: {event:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_local_connection_is_told_the_countdown_then_launched() {
        // The app runs from the install folder, which holds the launch until
        // it leaves: the notice is seen before the installer starts.
        let rig = rig(
            "update-countdown",
            Some(OWN),
            &["install/rustling-tulip-native.exe"],
            SETUP,
        );
        let _local = rig.hub.host_update.attach_local();
        let mut events = rig.hub.state_events.subscribe();

        let mut replies = send_request(&rig.hub).await;
        let scheduled = next_notice(&mut events).await;
        let counting_from = Instant::now();

        let DaemonMessage::HostUpdateScheduled {
            update_id,
            requested_by,
            version,
            seconds,
        } = scheduled
        else {
            panic!("expected the countdown's start, got {scheduled:?}");
        };
        assert_eq!(
            (requested_by.as_str(), version.as_str(), seconds),
            (THIS_PC, "0.99.0", 10)
        );
        assert_ne!(update_id, "u1", "the daemon's own id, not the request's");
        let launched = next_notice(&mut events).await;
        assert!(
            matches!(&launched, DaemonMessage::HostUpdateLaunched { update_id: id } if *id == update_id),
            "{launched:?}"
        );
        assert!(
            counting_from.elapsed() >= Duration::from_secs(10),
            "launched after the countdown"
        );
        assert!(
            lock(&rig.launches).is_empty(),
            "the notice goes out before the installer starts"
        );
        lock(&rig.processes.running).clear();
        let reply = answer(&mut replies).await;
        assert!(
            matches!(reply, DaemonMessage::RemoteUpdateStarted { .. }),
            "{reply:?}"
        );
        assert_eq!(lock(&rig.launches).len(), 1, "launched once");
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_within_the_countdown_aborts_the_update() {
        let rig = passing("update-cancel", SETUP);
        let local = rig.hub.host_update.attach_local();
        let mut events = rig.hub.state_events.subscribe();
        let mut replies = send_request(&rig.hub).await;
        let update_id = scheduled_id(&next_notice(&mut events).await);

        let (canceller_tx, mut canceller_rx) = mpsc::unbounded_channel();
        dispatch_on(&rig.hub, cancel(&update_id), 2, &canceller_tx).await;

        let canceled = next_notice(&mut events).await;
        assert!(
            matches!(&canceled, DaemonMessage::HostUpdateCanceled { update_id: id } if *id == update_id),
            "{canceled:?}"
        );
        let reply = refusal(answer(&mut replies).await);
        assert_eq!(reply, expected(UPDATE_CANCELED, CANCELED));
        assert!(canceller_rx.try_recv().is_err(), "the cancel has no reply");
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
        assert!(!guard_set(&rig.hub), "the guard is released");
        assert!(!held_off(&rig.hub), "idle exit is no longer held off");

        drop(local);
        let mut again = send_request(&rig.hub).await;
        let reply = answer(&mut again).await;
        assert!(
            matches!(reply, DaemonMessage::RemoteUpdateStarted { .. }),
            "the next update is not busy: {reply:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_after_the_countdown_is_refused_as_started() {
        let rig = passing("update-cancel-late", SETUP);
        let _local = rig.hub.host_update.attach_local();
        let mut events = rig.hub.state_events.subscribe();
        let mut replies = send_request(&rig.hub).await;
        let update_id = scheduled_id(&next_notice(&mut events).await);
        let reply = answer(&mut replies).await;
        assert!(
            matches!(reply, DaemonMessage::RemoteUpdateStarted { .. }),
            "{reply:?}"
        );

        let reply = bare_refusal(dispatch_one(&rig.hub, cancel(&update_id)).await);

        assert_eq!(
            reply,
            expected(BUSY, "The update has already started on this host.")
        );
        assert_eq!(lock(&rig.launches).len(), 1, "launched once");
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_cancel_or_an_unknown_id_is_not_found() {
        let rig = passing("update-cancel-twice", SETUP);
        let _local = rig.hub.host_update.attach_local();
        let mut events = rig.hub.state_events.subscribe();
        let mut replies = send_request(&rig.hub).await;
        let update_id = scheduled_id(&next_notice(&mut events).await);
        let (canceller_tx, _canceller_rx) = mpsc::unbounded_channel();
        dispatch_on(&rig.hub, cancel(&update_id), 2, &canceller_tx).await;

        let second = bare_refusal(dispatch_one(&rig.hub, cancel(&update_id)).await);
        let unknown = bare_refusal(dispatch_one(&rig.hub, cancel("no-such-update")).await);

        let not_found = expected(
            protocol::error_codes::NOT_FOUND,
            "No host update with that id is counting down.",
        );
        assert_eq!(second, not_found);
        assert_eq!(unknown, not_found);
        let reply = refusal(answer(&mut replies).await);
        assert_eq!(reply, expected(UPDATE_CANCELED, CANCELED));
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_stopping_mid_countdown_refuses_with_why() {
        let rig = passing("update-stopping", SETUP);
        let _local = rig.hub.host_update.attach_local();
        let mut events = rig.hub.state_events.subscribe();
        let mut replies = send_request(&rig.hub).await;
        let _scheduled = next_notice(&mut events).await;

        rig.hub.shutdown_tx.send_replace(true);

        let reply = refusal(answer(&mut replies).await);
        assert_eq!(
            reply,
            expected(
                UPDATE_REFUSED,
                "The host's daemon is stopping; try again once it is back.",
            )
        );
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
        assert!(!guard_set(&rig.hub), "the guard is released");
        assert!(!held_off(&rig.hub), "idle exit is no longer held off");
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_tied_with_a_shutdown_answers_canceled() {
        let rig = passing("update-cancel-tie", SETUP);
        let _local = rig.hub.host_update.attach_local();
        let mut events = rig.hub.state_events.subscribe();
        let mut replies = send_request(&rig.hub).await;
        let update_id = scheduled_id(&next_notice(&mut events).await);

        // Both are ready before the countdown task is polled again; the
        // cancel, sent second, must win the tie.
        rig.hub.shutdown_tx.send_replace(true);
        let (canceller_tx, mut canceller_rx) = mpsc::unbounded_channel();
        dispatch_on(&rig.hub, cancel(&update_id), 2, &canceller_tx).await;

        let reply = refusal(answer(&mut replies).await);
        assert_eq!(reply, expected(UPDATE_CANCELED, CANCELED));
        assert!(canceller_rx.try_recv().is_err(), "the cancel has no reply");
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
        assert!(!guard_set(&rig.hub), "the guard is released");
        assert!(!held_off(&rig.hub), "idle exit is no longer held off");
    }

    #[tokio::test(start_paused = true)]
    async fn the_launch_waits_for_the_app_to_leave() {
        let rig = rig(
            "update-app-leaves",
            Some(OWN),
            &["install/rustling-tulip-native.exe"],
            SETUP,
        );
        let _local = rig.hub.host_update.attach_local();
        let mut events = rig.hub.state_events.subscribe();
        let mut replies = send_request(&rig.hub).await;
        let _scheduled = next_notice(&mut events).await;
        let _launched = next_notice(&mut events).await;
        let reads_at_launched = rig.processes.reads.load(Ordering::SeqCst);

        tokio::time::sleep(Duration::from_secs(5)).await;

        assert!(
            lock(&rig.launches).is_empty(),
            "no launch while the app runs"
        );
        assert!(
            rig.processes.reads.load(Ordering::SeqCst) >= reads_at_launched + 2,
            "the launch keeps looking for the app"
        );
        lock(&rig.processes.running).clear();
        let reply = answer(&mut replies).await;
        assert!(
            matches!(reply, DaemonMessage::RemoteUpdateStarted { .. }),
            "{reply:?}"
        );
        assert_eq!(lock(&rig.launches).len(), 1, "launched once the app left");
    }

    #[tokio::test(start_paused = true)]
    async fn the_launch_goes_ahead_when_the_app_never_leaves() {
        let rig = rig(
            "update-app-stays",
            Some(OWN),
            &["install/rustling-tulip-native.exe"],
            SETUP,
        );
        let _local = rig.hub.host_update.attach_local();
        let mut events = rig.hub.state_events.subscribe();
        let mut replies = send_request(&rig.hub).await;
        let _scheduled = next_notice(&mut events).await;
        let _launched = next_notice(&mut events).await;
        let waiting_from = Instant::now();

        let reply = answer(&mut replies).await;

        assert!(
            matches!(reply, DaemonMessage::RemoteUpdateStarted { .. }),
            "{reply:?}"
        );
        assert!(
            waiting_from.elapsed() >= rig.hub.host_update.app_quit_wait,
            "the launch waited out the app"
        );
        assert_eq!(lock(&rig.launches).len(), 1, "launched anyway");
    }

    #[tokio::test(start_paused = true)]
    async fn the_app_seen_before_launched_is_relaunched() {
        let rig = rig(
            "update-app-seen",
            Some(OWN),
            &["install/rustling-tulip-native.exe"],
            SETUP,
        );
        // The app quits the moment the launch notice reaches it.
        rig.processes.quits_on_launch.store(true, Ordering::SeqCst);
        let _local = rig.hub.host_update.attach_local();
        let mut events = rig.hub.state_events.subscribe();
        let mut replies = send_request(&rig.hub).await;
        let _scheduled = next_notice(&mut events).await;
        let _launched = next_notice(&mut events).await;

        let reply = answer(&mut replies).await;
        assert!(
            matches!(reply, DaemonMessage::RemoteUpdateStarted { .. }),
            "{reply:?}"
        );
        let args: Vec<OsString> = ["/S", "/UPDATE", "/RELAUNCH", "/RESTARTDAEMON"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(*lock(&rig.launches), vec![(rig.installer.clone(), args)]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_installer_relaunches_the_app_it_saw_running() {
        let rig = rig(
            "update-relaunch",
            Some(OWN),
            &["install/rustling-tulip-native.exe"],
            SETUP,
        );
        let _local = rig.hub.host_update.attach_local();
        let mut events = rig.hub.state_events.subscribe();
        let mut replies = send_request(&rig.hub).await;
        let _scheduled = next_notice(&mut events).await;
        let _launched = next_notice(&mut events).await;
        let reads_at_launched = rig.processes.reads.load(Ordering::SeqCst);

        tokio::time::sleep(Duration::from_secs(5)).await;

        assert!(
            rig.processes.reads.load(Ordering::SeqCst) >= reads_at_launched + 2,
            "the launch looked and saw the app running"
        );
        assert!(
            lock(&rig.launches).is_empty(),
            "no launch while the app runs"
        );
        lock(&rig.processes.running).clear();
        let reply = answer(&mut replies).await;
        assert!(
            matches!(reply, DaemonMessage::RemoteUpdateStarted { .. }),
            "{reply:?}"
        );
        let args: Vec<OsString> = ["/S", "/UPDATE", "/RELAUNCH", "/RESTARTDAEMON"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(*lock(&rig.launches), vec![(rig.installer.clone(), args)]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_installer_restarts_only_the_daemon_when_no_app_ran() {
        let rig = passing("update-no-app", SETUP);
        let _local = rig.hub.host_update.attach_local();
        let mut replies = send_request(&rig.hub).await;

        let reply = answer(&mut replies).await;

        assert!(
            matches!(reply, DaemonMessage::RemoteUpdateStarted { .. }),
            "{reply:?}"
        );
        let args: Vec<OsString> = ["/S", "/UPDATE", "/RESTARTDAEMON"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(*lock(&rig.launches), vec![(rig.installer.clone(), args)]);
    }

    #[tokio::test]
    async fn remote_update_launches_the_verified_installer_and_answers_started() {
        let rig = passing("update-started", SETUP);

        let reply = ask(&rig.hub).await;

        let DaemonMessage::RemoteUpdateStarted {
            request_id,
            version,
        } = reply
        else {
            panic!("expected the update to start, got {reply:?}");
        };
        assert_eq!((request_id.as_str(), version.as_str()), ("u1", "0.99.0"));
        let args: Vec<OsString> = ["/S", "/UPDATE", "/RESTARTDAEMON"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(*lock(&rig.launches), vec![(rig.installer.clone(), args)]);
        assert!(guard_set(&rig.hub), "a launched update keeps its guard");
        assert!(held_off(&rig.hub), "a launched update holds off idle exit");
    }

    #[tokio::test]
    async fn remote_update_is_busy_when_the_update_lock_is_held() {
        let rig = passing(
            "update-lock-held",
            Setup {
                lock_held: true,
                ..SETUP
            },
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(BUSY, ANOTHER_UPDATE));
        assert!(!guard_set(&rig.hub), "the guard is released");
        assert!(!held_off(&rig.hub), "idle exit is no longer held off");
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
    }

    #[tokio::test]
    async fn remote_update_refuses_when_the_host_is_on_the_newest_release() {
        let rig = passing(
            "update-newest",
            Setup {
                feed: Feed::Newest,
                ..SETUP
            },
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, ALREADY_NEWEST));
        assert!(!guard_set(&rig.hub), "the guard is released");
        assert!(!held_off(&rig.hub), "idle exit is no longer held off");
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
    }

    #[tokio::test]
    async fn remote_update_refuses_when_the_download_fails() {
        let rig = passing(
            "update-download-fails",
            Setup {
                feed: Feed::Failing,
                ..SETUP
            },
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, DOWNLOAD_FAILED));
        assert!(
            !reply.1.contains(DOWNLOAD_ERROR),
            "the error's text stays on the host: {}",
            reply.1
        );
        assert!(!guard_set(&rig.hub), "the guard is released");
        assert!(lock(&rig.launches).is_empty(), "nothing launched");
    }

    #[tokio::test]
    async fn remote_update_refuses_when_the_installer_does_not_start() {
        let rig = passing(
            "update-launch-fails",
            Setup {
                launch_fails: true,
                ..SETUP
            },
        );

        let reply = refusal(ask(&rig.hub).await);

        assert_eq!(reply, expected(UPDATE_REFUSED, NO_INSTALLER));
        assert!(!guard_set(&rig.hub), "the guard is released");
        assert!(!held_off(&rig.hub), "idle exit is no longer held off");
    }

    #[tokio::test]
    async fn remote_update_answers_once_per_request() {
        let outcomes = [
            ("update-once-started", SETUP),
            (
                "update-once-busy",
                Setup {
                    lock_held: true,
                    ..SETUP
                },
            ),
            (
                "update-once-newest",
                Setup {
                    feed: Feed::Newest,
                    ..SETUP
                },
            ),
            (
                "update-once-download",
                Setup {
                    feed: Feed::Failing,
                    ..SETUP
                },
            ),
            (
                "update-once-launch",
                Setup {
                    launch_fails: true,
                    ..SETUP
                },
            ),
        ];
        for (tag, setup) in outcomes {
            let rig = passing(tag, setup);

            // `ask` fails on a second reply, and on none.
            let _reply = ask(&rig.hub).await;
        }
        let refused = rig(
            "update-once-refused",
            Some(OWN),
            &["install/rustling-tulipd.exe"],
            SETUP,
        );
        let _reply = ask(&refused.hub).await;
    }
}
