//! The driver endpoint of a driven run: the UI thread's dispatcher for the
//! pipe's requests, the drawn frame each reply waits for, `screenshot`, and
//! the discovery file that tells a driver client where the pipe is.

use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{Either, select};
use gpui::{AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, WeakEntity, Window};
use load_clock::{Deadline, Expiry, LoadClock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::RootView;
use crate::driver::capture;
use crate::driver::handlers;
use crate::driver::pipe::{self, Call};
use crate::driver::pointer::window_address;
use crate::driver::protocol::{ErrorCode, Outcome, Reply, Request};
use crate::driver::wait::{Step, WaitLoop};

/// How long a reply waits for the frame drawn after its request, in
/// load-adjusted time.
const FRAME_WAIT: Duration = Duration::from_secs(5);

/// How long after start the discovery file waits for the window to be
/// cloaked before it gives up, in load-adjusted time.
const CLOAK_WAIT: Duration = Duration::from_secs(5);

/// The longest a deadline wait sleeps before checking its deadline again.
const SLICE: Duration = Duration::from_millis(250);

/// The shortest a deadline wait sleeps: the load-adjusted time left is not
/// wall time, so near the end a shorter slice would poll at the timer's tick
/// rate. A wait may overshoot its ceiling by this much wall time.
const MIN_SLICE: Duration = Duration::from_millis(20);

/// The process's one load-adjusted clock, which every endpoint wait counts on.
fn clock() -> Arc<LoadClock> {
    static CLOCK: OnceLock<Arc<LoadClock>> = OnceLock::new();
    Arc::clone(CLOCK.get_or_init(LoadClock::system))
}

/// Ends once `deadline` has expired, sleeping on background timers between
/// checks; answers its expiry.
async fn until_expired(deadline: &Deadline, cx: &AsyncApp) -> Expiry {
    loop {
        if let Some(expiry) = deadline.expired() {
            return expiry;
        }
        cx.background_executor()
            .timer(deadline.wait_slice(SLICE).max(MIN_SLICE))
            .await;
    }
}

/// The folder under the temp dir that holds one discovery file per driven
/// client.
const DISCOVERY_FOLDER: &str = "rustling-tulip-driver";

/// One driven client's discovery file: `%TEMP%\rustling-tulip-driver\<pid>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovery {
    pub pid: u32,
    pub pipe: String,
    /// The process's creation time, as a Windows `FILETIME` in one u64.
    pub started_at: u64,
    pub version: String,
}

/// `screenshot`'s params: none.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScreenshotParams {}

/// What a request came to inside the window update.
enum Handled {
    Reply(Reply),
    /// A screenshot, with the window's handle address, captured after the
    /// frame wait.
    Shot(Result<usize, String>),
}

/// How long a `screenshot` waits for its capture thread, in load-adjusted
/// time.
const CAPTURE_WAIT: Duration = Duration::from_secs(10);

/// What the capture thread answers: the PNG's path and its size.
type Shot = Result<(PathBuf, u32, u32)>;

/// How the wait for a drawn frame ended.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Frame {
    Drawn,
    Closed,
    /// The request's token fired first.
    Cancelled,
    TimedOut(Expiry),
}

/// Starts the endpoint on `window`, which `root` draws in: the pipe thread,
/// the dispatcher loop, and the discovery file once a frame has drawn with
/// the window cloaked.
pub fn start(window: &mut Window, root: &Entity<RootView>, cx: &mut App) {
    let handle = window.window_handle();
    let (calls, received) = mpsc::unbounded::<Call>();
    let listening = pipe::spawn(calls);
    dispatch(received, handle, root.downgrade(), cx);
    cx.spawn(async move |cx| {
        // The pipe thread drops the sender only when the listener could not
        // be created, and then it ends the process.
        if listening.await.is_ok() {
            publish_when_cloaked(handle, cx).await;
        }
    })
    .detach();
}

/// Answers each call `received` brings, one at a time, against `handle`,
/// the window `root` draws in. A call whose token has fired before its turn
/// is refused "cancelled" without touching the window.
pub fn dispatch(
    mut received: mpsc::UnboundedReceiver<Call>,
    handle: AnyWindowHandle,
    root: WeakEntity<RootView>,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        let mut shots = 0u32;
        while let Some(call) = received.next().await {
            let reply = if call.cancel.is_cancelled() {
                cancelled(call.request.id)
            } else {
                answer(&call, handle, &root, &mut shots, cx).await
            };
            if call.reply.send(reply).is_err() {
                tracing::debug!("driver: the connection closed before its reply");
            }
        }
    })
    .detach();
}

/// The reply to `call`'s request, sent once a frame has drawn after it.
async fn answer(
    call: &Call,
    handle: AnyWindowHandle,
    root: &WeakEntity<RootView>,
    shots: &mut u32,
    cx: &mut AsyncApp,
) -> Reply {
    let request = &call.request;
    let id = request.id;
    if request.method == "wait_for" {
        return wait_for(request, &call.cancel, handle, root, cx).await;
    }
    let (drawn, frame) = oneshot::channel();
    let handled = cx.update_window(handle, |_, window, cx| {
        let root = root.upgrade()?;
        let handled = if request.method == "screenshot" {
            match request.params::<ScreenshotParams>() {
                Ok(ScreenshotParams {}) => Handled::Shot(window_address(window)),
                Err(reply) => Handled::Reply(reply),
            }
        } else {
            Handled::Reply(handlers::handle(request, &root, window, cx))
        };
        request_frame(window, drawn);
        Some(handled)
    });
    let Ok(Some(handled)) = handled else {
        return refused(id, "the window is closed");
    };
    let frame = wait_for_frame(frame, call.cancel.cancelled(), cx).await;
    let shot = match handled {
        Handled::Reply(reply) => return after_frame(reply, frame),
        Handled::Shot(shot) => shot,
    };
    match next_shot(id, frame, shot, shots) {
        Err(reply) => reply,
        Ok((address, n)) => {
            let deadline = Deadline::start(clock(), CAPTURE_WAIT);
            let timeout = until_expired(&deadline, cx);
            screenshot(id, address, n, call.cancel.cancelled(), timeout).await
        }
    }
}

/// The window address and shot number a `screenshot` (request `id`) captures
/// once its frame wait ended as `frame`, counting the shot in `shots`; or
/// its refusal, counting none. A cancel before the frame takes no capture.
fn next_shot(
    id: u64,
    frame: Frame,
    shot: Result<usize, String>,
    shots: &mut u32,
) -> Result<(usize, u32), Reply> {
    match (frame, shot) {
        (Frame::Closed, _) => Err(refused(id, "the window is closed")),
        (Frame::Cancelled, _) => Err(cancelled(id)),
        (Frame::TimedOut(expiry), _) => Err(refused(id, &format!("no frame was drawn: {expiry}"))),
        (Frame::Drawn, Err(reason)) => Err(refused(id, &reason)),
        (Frame::Drawn, Ok(address)) => {
            *shots += 1;
            Ok((address, *shots))
        }
    }
}

/// The reply to a `wait_for`: its predicate read in one window update per
/// poll, with a background timer between polls, so the UI thread is never
/// blocked. A read-only poll changes nothing, so no frame is awaited. A
/// fired `cancel` ends it at the top of its next poll, its sleep included,
/// refused "cancelled".
async fn wait_for(
    request: &Request,
    cancel: &CancellationToken,
    handle: AnyWindowHandle,
    root: &WeakEntity<RootView>,
    cx: &mut AsyncApp,
) -> Reply {
    let (params, predicate) = match handlers::wait_request(request) {
        Ok(parsed) => parsed,
        Err(reply) => return reply,
    };
    let wait = WaitLoop::new(&params);
    let deadline = Deadline::start(clock(), wait.timeout());
    loop {
        if cancel.is_cancelled() {
            return cancelled(request.id);
        }
        let read = cx.update_window(handle, |_, window, cx| {
            let root = root.upgrade()?;
            Some(handlers::wait_read(&predicate, &root, window, cx))
        });
        let Ok(Some((met, last))) = read else {
            return refused(request.id, "the window is closed");
        };
        match wait.step(met, last, deadline.figures()) {
            Step::Done(result) => return Reply::ok(request.id, result),
            Step::Again(sleep) => {
                let timer = cx.background_executor().timer(sleep);
                select(pin!(timer), pin!(cancel.cancelled())).await;
            }
        }
    }
}

/// The reply to a request whose handler has run, once the frame wait ended
/// as `frame`. The handler's effect has happened, so its result is sent
/// whatever the wait came to; an ok object result says `"drawn": false`
/// when no frame was drawn after it.
fn after_frame(reply: Reply, frame: Frame) -> Reply {
    match (frame, reply.outcome) {
        (
            Frame::TimedOut(_) | Frame::Closed | Frame::Cancelled,
            Outcome::Result(Value::Object(mut result)),
        ) => {
            result.insert("drawn".to_owned(), Value::Bool(false));
            Reply::ok(reply.id, Value::Object(result))
        }
        (_, outcome) => Reply {
            id: reply.id,
            outcome,
        },
    }
}

fn refused(id: u64, message: &str) -> Reply {
    Reply::error(id, ErrorCode::Refused, message.to_owned(), None)
}

/// The reply to request `id` once its token fired before its effect.
fn cancelled(id: u64) -> Reply {
    refused(id, "cancelled")
}

/// Asks `window` for a frame and sends on `drawn` once it has drawn. Two
/// hops, because a next-frame callback runs before that frame draws: the
/// second runs at the start of the frame after it.
fn request_frame(window: &mut Window, drawn: oneshot::Sender<()>) {
    window.refresh();
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |_, _| {
            let _ = drawn.send(());
        });
    });
}

/// Waits for `frame`, at most [`FRAME_WAIT`] of load-adjusted time, or
/// until `cancel` ends.
async fn wait_for_frame(
    frame: oneshot::Receiver<()>,
    cancel: impl Future<Output = ()>,
    cx: &AsyncApp,
) -> Frame {
    let deadline = Deadline::start(clock(), FRAME_WAIT);
    frame_outcome(frame, cancel, until_expired(&deadline, cx)).await
}

/// How a wait for `frame` ends against `cancel` and `expiry`: the frame's
/// own outcome wins over a cancel, and a cancel over the expiry, when more
/// than one is ready.
async fn frame_outcome(
    frame: oneshot::Receiver<()>,
    cancel: impl Future<Output = ()>,
    expiry: impl Future<Output = Expiry>,
) -> Frame {
    match select(frame, select(pin!(cancel), pin!(expiry))).await {
        Either::Left((Ok(()), _)) => Frame::Drawn,
        Either::Left((Err(_), _)) => Frame::Closed,
        Either::Right((Either::Left(((), _)), _)) => Frame::Cancelled,
        Either::Right((Either::Right((expiry, _)), _)) => Frame::TimedOut(expiry),
    }
}

/// Captures the window at `address` to
/// `<config dir>/driver-shots/<pid>-<n>.png` on a worker thread, awaited
/// without blocking the UI thread until `cancel` or `timeout` ends.
async fn screenshot(
    id: u64,
    address: usize,
    n: u32,
    cancel: impl Future<Output = ()>,
    timeout: impl Future<Output = Expiry>,
) -> Reply {
    let (tx, rx) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("driver-capture".to_owned())
        .spawn(move || {
            let shot = shot_path(n).and_then(|path| {
                let (width, height) = capture::capture_png(address, &path)?;
                Ok((path, width, height))
            });
            if tx.send(shot).is_err() {
                tracing::warn!("driver: the capture finished after the dispatcher stopped waiting");
            }
        });
    if let Err(err) = spawned {
        return refused(id, &format!("starting the capture thread: {err}"));
    }
    capture_reply(id, rx, cancel, timeout).await
}

/// The reply to request `id` from the capture thread's `answer`, or a
/// refusal when `cancel` or `timeout` ends first; a finished capture wins
/// over both, and a cancel over the timeout. A late answer is dropped (the
/// PNG may still be written).
async fn capture_reply(
    id: u64,
    answer: oneshot::Receiver<Shot>,
    cancel: impl Future<Output = ()>,
    timeout: impl Future<Output = Expiry>,
) -> Reply {
    let (cancel, timeout) = (pin!(cancel), pin!(timeout));
    match select(answer, select(cancel, timeout)).await {
        Either::Right((Either::Left(((), _)), _)) => cancelled(id),
        Either::Right((Either::Right((expiry, _)), _)) => {
            refused(id, &format!("the capture did not finish: {expiry}"))
        }
        Either::Left((Ok(Ok((path, width, height))), _)) => Reply::ok(
            id,
            json!({"path": path.display().to_string(), "width": width, "height": height}),
        ),
        Either::Left((Ok(Err(err)), _)) => refused(id, &format!("{err:#}")),
        Either::Left((Err(_), _)) => refused(id, "the capture thread ended without an answer"),
    }
}

/// `<config dir>/driver-shots/<pid>-<n>.png`, its folder created when
/// missing.
fn shot_path(n: u32) -> Result<PathBuf> {
    let dir = daemon_client::config_dir()?.join("driver-shots");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dir = std::path::absolute(&dir).with_context(|| format!("resolving {}", dir.display()))?;
    Ok(dir.join(format!("{}-{n}.png", std::process::id())))
}

/// Writes the discovery file once a frame has drawn with the window shown
/// and cloaked by the app, retrying on each following frame until
/// [`CLOAK_WAIT`] passes: another thread cloaks and shows the window after
/// it opens.
async fn publish_when_cloaked(handle: AnyWindowHandle, cx: &mut AsyncApp) {
    let deadline = Deadline::start(clock(), CLOAK_WAIT);
    loop {
        let (drawn, frame) = oneshot::channel();
        if cx
            .update_window(handle, |_, window, _| request_frame(window, drawn))
            .is_err()
        {
            return;
        }
        // Publishing is never cancelled.
        match wait_for_frame(frame, std::future::pending(), cx).await {
            Frame::Drawn => {}
            Frame::Closed | Frame::Cancelled => return,
            Frame::TimedOut(_) => {
                tracing::error!("driver: no frame was drawn; no discovery file is written");
                return;
            }
        }
        let published = cx.update_window(handle, |_, window, cx| {
            let address = window_address(window).map_err(|reason| anyhow!(reason))?;
            if !capture::is_out_of_sight(address)? {
                return Ok(false);
            }
            publish(cx)?;
            Ok::<bool, anyhow::Error>(true)
        });
        match published {
            // The window closed, or the file is written.
            Err(_) | Ok(Ok(true)) => return,
            Ok(Ok(false)) => {
                if let Some(expiry) = deadline.expired() {
                    tracing::error!(
                        "driver: the window was not shown and cloaked: {expiry}; no discovery \
                         file is written"
                    );
                    return;
                }
            }
            Ok(Err(err)) => {
                tracing::error!("driver: writing the discovery file: {err:#}");
                return;
            }
        }
    }
}

/// Sweeps stale discovery files, writes this process's, and removes it when
/// the app quits.
fn publish(cx: &mut App) -> Result<()> {
    let dir = std::env::temp_dir().join(DISCOVERY_FOLDER);
    sweep(&dir);
    let pid = std::process::id();
    let discovery = Discovery {
        pid,
        pipe: pipe::pipe_name(pid),
        started_at: process_started_at(pid)?
            .ok_or_else(|| anyhow!("this process has no start time"))?,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let path = write_discovery(&dir, &discovery)?;
    tracing::info!(path = %path.display(), "driver: wrote the discovery file");
    cx.on_app_quit(move |_| {
        if let Err(err) = std::fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), "driver: removing the discovery file: {err}");
        }
        async {}
    })
    .detach();
    Ok(())
}

/// Writes `discovery` to `<dir>/<pid>.json` through a temp file and a
/// rename, creating `dir` when missing, and returns its path.
fn write_discovery(dir: &Path, discovery: &Discovery) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{}.json", discovery.pid));
    let temp = dir.join(format!(".{}.json.tmp", discovery.pid));
    let text = serde_json::to_vec(discovery).context("encoding the discovery file")?;
    std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &path)
        .with_context(|| format!("renaming {} into place", temp.display()))?;
    Ok(path)
}

/// Deletes each `*.json` in `dir` whose pid is not running, or whose
/// `started_at` is not that pid's creation time (a reused pid). A process
/// this one may not open counts as alive. A file that cannot be read is
/// left in place.
pub fn sweep(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            tracing::warn!(dir = %dir.display(), "driver: listing discovery files: {err}");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let discovery = std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| serde_json::from_slice::<Discovery>(&text).map_err(Into::into));
        let discovery = match discovery {
            Ok(discovery) => discovery,
            Err(err) => {
                tracing::warn!(path = %path.display(), "driver: reading a discovery file: {err:#}");
                continue;
            }
        };
        if is_live(&discovery) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!(
                path = %path.display(),
                pid = discovery.pid,
                "driver: deleted a stale discovery file"
            ),
            Err(err) => tracing::warn!(
                path = %path.display(),
                "driver: deleting a stale discovery file: {err}"
            ),
        }
    }
}

/// Whether the process `discovery` names is running and started when it
/// says; a process this one may not open counts as running.
fn is_live(discovery: &Discovery) -> bool {
    match process_started_at(discovery.pid) {
        Ok(Some(started_at)) => started_at == discovery.started_at,
        Ok(None) => true,
        Err(_) => false,
    }
}

/// The creation time of process `pid` as a `FILETIME` in one u64: `None`
/// when access is denied, an error when it is not running or its times
/// cannot be read.
fn process_started_at(pid: u32) -> Result<Option<u64>> {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: opens a query-only handle, closed below.
    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(process) => process,
        Err(err) if err.code() == ERROR_ACCESS_DENIED.to_hresult() => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("opening process {pid}")),
    };
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the handle opened above and out-pointers to locals.
    let times = unsafe {
        GetProcessTimes(
            process,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    };
    // SAFETY: the handle opened above, closed once.
    let _ = unsafe { CloseHandle(process) };
    times.with_context(|| format!("reading process {pid}'s times"))?;
    Ok(Some(
        (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
    ))
}

#[cfg(all(test, windows))]
#[expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the step it lost"
)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use futures::channel::oneshot;
    use load_clock::{Expiry, ExpiryKind, Figures};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::{
        Discovery, Frame, Shot, after_frame, capture_reply, frame_outcome, next_shot,
        process_started_at, sweep, write_discovery,
    };
    use crate::driver::protocol::{ErrorCode, Reply};

    #[test]
    fn a_stuck_capture_is_answered_refused() {
        // The sender is held and never answers, as a stuck capture thread.
        let (_stuck, answer) = oneshot::channel::<Shot>();
        let expiry = ceiling(Duration::from_secs(10));
        let reply = futures::executor::block_on(capture_reply(
            4,
            answer,
            std::future::pending(),
            async move { expiry },
        ));
        assert_eq!(
            reply,
            Reply::error(
                4,
                ErrorCode::Refused,
                format!("the capture did not finish: {expiry}"),
                None,
            )
        );
    }

    /// A ceiling expiry of `budget`, spent on a free machine.
    fn ceiling(budget: Duration) -> Expiry {
        Expiry {
            kind: ExpiryKind::Ceiling,
            figures: Figures {
                budget,
                wall: budget,
                adjusted: budget,
            },
        }
    }

    #[test]
    fn a_frame_timeout_keeps_the_handlers_result() {
        let pressed = Reply::ok(5, json!({"pressed": "ctrl-,"}));
        assert_eq!(after_frame(pressed.clone(), Frame::Drawn), pressed);
        let timed_out = Frame::TimedOut(ceiling(Duration::from_secs(5)));
        for frame in [timed_out, Frame::Closed] {
            assert_eq!(
                after_frame(pressed.clone(), frame),
                Reply::ok(5, json!({"pressed": "ctrl-,", "drawn": false})),
                "{frame:?}"
            );
            let failed = Reply::error(6, ErrorCode::BadParams, "bad".to_owned(), None);
            assert_eq!(after_frame(failed.clone(), frame), failed, "{frame:?}");
            let listed = Reply::ok(7, json!([]));
            assert_eq!(after_frame(listed.clone(), frame), listed, "{frame:?}");
        }
    }

    /// A token already fired, as a cancel that has landed.
    fn fired() -> CancellationToken {
        let token = CancellationToken::new();
        token.cancel();
        token
    }

    /// The refusal a cancelled request `id` gets when its effect is absent.
    fn cancelled(id: u64) -> Reply {
        Reply::error(id, ErrorCode::Refused, "cancelled".to_owned(), None)
    }

    #[test]
    fn a_frame_wait_ends_on_cancel() {
        // The frame's sender is held and never sends, as a frame not drawn.
        let (_drawing, frame) = oneshot::channel::<()>();
        let token = fired();
        let ended = futures::executor::block_on(frame_outcome(
            frame,
            token.cancelled(),
            std::future::pending(),
        ));
        assert_eq!(ended, Frame::Cancelled);
    }

    #[test]
    fn a_drawn_frame_beats_a_cancel() {
        let (drawn, frame) = oneshot::channel::<()>();
        drawn.send(()).expect("the wait holds its receiver");
        let token = fired();
        let ended = futures::executor::block_on(frame_outcome(
            frame,
            token.cancelled(),
            std::future::pending(),
        ));
        assert_eq!(ended, Frame::Drawn);
    }

    #[test]
    fn a_cancel_after_the_handler_keeps_its_result() {
        let (_drawing, frame) = oneshot::channel::<()>();
        let token = fired();
        let ended = futures::executor::block_on(frame_outcome(
            frame,
            token.cancelled(),
            std::future::pending(),
        ));
        let opened = Reply::ok(5, json!({"action": "open_settings"}));
        assert_eq!(
            after_frame(opened, ended),
            Reply::ok(5, json!({"action": "open_settings", "drawn": false}))
        );
    }

    #[test]
    fn a_screenshot_cancelled_before_its_frame_takes_no_capture() {
        let mut shots = 0u32;
        assert_eq!(
            next_shot(4, Frame::Cancelled, Ok(0x1234), &mut shots),
            Err(cancelled(4))
        );
        assert_eq!(shots, 0, "a cancelled screenshot counts no shot");
        assert_eq!(
            next_shot(4, Frame::Drawn, Ok(0x1234), &mut shots),
            Ok((0x1234, 1))
        );
        assert_eq!(shots, 1, "a drawn screenshot counts its shot");
    }

    #[test]
    fn a_capture_wait_ends_on_cancel() {
        // The sender is held and never answers, as a capture still running.
        let (_capturing, answer) = oneshot::channel::<Shot>();
        let token = fired();
        let reply = futures::executor::block_on(capture_reply(
            4,
            answer,
            token.cancelled(),
            std::future::pending(),
        ));
        assert_eq!(reply, cancelled(4));
    }

    #[test]
    fn a_finished_capture_beats_a_cancel() {
        let (captured, answer) = oneshot::channel::<Shot>();
        let path = PathBuf::from("shot.png");
        assert!(
            captured.send(Ok((path.clone(), 10, 20))).is_ok(),
            "the wait holds its receiver"
        );
        let token = fired();
        let reply = futures::executor::block_on(capture_reply(
            4,
            answer,
            token.cancelled(),
            std::future::pending(),
        ));
        assert_eq!(
            reply,
            Reply::ok(
                4,
                json!({"path": path.display().to_string(), "width": 10, "height": 20})
            )
        );
    }

    /// A pid no process has: Windows pids are multiples of 4 well below it.
    const NO_SUCH_PID: u32 = !3;

    /// An empty folder no other spec uses.
    fn folder() -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rt-driver-sweep-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates the folder");
        dir
    }

    fn discovery(pid: u32, started_at: u64) -> Discovery {
        Discovery {
            pid,
            pipe: format!("rt-driver-{pid}"),
            started_at,
            version: "0.0.0".to_owned(),
        }
    }

    fn own_start() -> u64 {
        process_started_at(std::process::id())
            .expect("reads this process's times")
            .expect("this process can open itself")
    }

    #[test]
    fn a_stale_discovery_file_is_swept() {
        let dir = folder();
        let gone = write_discovery(&dir, &discovery(NO_SUCH_PID, 1)).expect("writes");
        let reused =
            write_discovery(&dir, &discovery(std::process::id(), own_start() + 1)).expect("writes");

        sweep(&dir);

        assert!(!gone.exists(), "a file naming a pid not running is deleted");
        assert!(
            !reused.exists(),
            "a file whose start time is not the pid's is deleted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_discovery_file_is_kept() {
        let dir = folder();
        let live =
            write_discovery(&dir, &discovery(std::process::id(), own_start())).expect("writes");

        sweep(&dir);

        assert!(live.exists(), "this process's file is kept");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
