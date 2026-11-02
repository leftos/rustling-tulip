//! The driver endpoint of a driven run: the UI thread's dispatcher for the
//! pipe's requests, the drawn frame each reply waits for, `screenshot`, and
//! the discovery file that tells a driver client where the pipe is.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{Either, select};
use gpui::{AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, WeakEntity, Window};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::RootView;
use crate::driver::capture;
use crate::driver::handlers;
use crate::driver::pipe::{self, Call};
use crate::driver::protocol::{ErrorCode, Outcome, Reply, Request};

/// How long a reply waits for the frame drawn after its request.
const FRAME_WAIT: Duration = Duration::from_secs(5);

/// How long after start the discovery file waits for the window to be
/// cloaked before it gives up.
const CLOAK_WAIT: Duration = Duration::from_secs(5);

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

/// How long a `screenshot` waits for its capture thread.
const CAPTURE_WAIT: Duration = Duration::from_secs(10);

/// What the capture thread answers: the PNG's path and its size.
type Shot = Result<(PathBuf, u32, u32)>;

/// How the wait for a drawn frame ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    Drawn,
    Closed,
    TimedOut,
}

/// Starts the endpoint on `window`, which `root` draws in: the pipe thread,
/// the dispatcher loop, and the discovery file once a frame has drawn with
/// the window cloaked.
pub fn start(window: &mut Window, root: &Entity<RootView>, cx: &mut App) {
    let handle = window.window_handle();
    let root = root.downgrade();
    let (calls, mut received) = mpsc::unbounded::<Call>();
    let listening = pipe::spawn(calls);
    cx.spawn(async move |cx| {
        let mut shots = 0u32;
        while let Some(call) = received.next().await {
            let reply = answer(&call.request, handle, &root, &mut shots, cx).await;
            if call.reply.send(reply).is_err() {
                tracing::debug!("driver: the connection closed before its reply");
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        // The pipe thread drops the sender only when the listener could not
        // be created, and then it ends the process.
        if listening.await.is_ok() {
            publish_when_cloaked(handle, cx).await;
        }
    })
    .detach();
}

/// The reply to `request`, sent once a frame has drawn after it.
async fn answer(
    request: &Request,
    handle: AnyWindowHandle,
    root: &WeakEntity<RootView>,
    shots: &mut u32,
    cx: &mut AsyncApp,
) -> Reply {
    let id = request.id;
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
    let frame = wait_for_frame(frame, cx).await;
    match handled {
        Handled::Reply(reply) => after_frame(reply, frame),
        Handled::Shot(_) if frame == Frame::Closed => refused(id, "the window is closed"),
        Handled::Shot(_) if frame == Frame::TimedOut => {
            refused(id, "no frame was drawn within 5 s")
        }
        Handled::Shot(Err(reason)) => refused(id, &reason),
        Handled::Shot(Ok(address)) => {
            *shots += 1;
            let timeout = cx.background_executor().timer(CAPTURE_WAIT);
            screenshot(id, address, *shots, timeout).await
        }
    }
}

/// The reply to a request whose handler has run, once the frame wait ended
/// as `frame`. The handler's effect has happened, so its result is sent
/// whatever the wait came to; an ok object result says `"drawn": false`
/// when no frame was drawn after it.
fn after_frame(reply: Reply, frame: Frame) -> Reply {
    match (frame, reply.outcome) {
        (Frame::TimedOut | Frame::Closed, Outcome::Result(Value::Object(mut result))) => {
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

/// Waits for `frame`, at most [`FRAME_WAIT`].
async fn wait_for_frame(frame: oneshot::Receiver<()>, cx: &AsyncApp) -> Frame {
    let timer = cx.background_executor().timer(FRAME_WAIT);
    match select(frame, timer).await {
        Either::Left((Ok(()), _)) => Frame::Drawn,
        Either::Left((Err(_), _)) => Frame::Closed,
        Either::Right(((), _)) => Frame::TimedOut,
    }
}

/// The address of `window`'s Win32 handle, read fresh, which crosses
/// threads where an `HWND` cannot.
fn window_address(window: &Window) -> Result<usize, String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // `Window::window_handle` is gpui's own handle; the raw one is the trait's.
    match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => Ok(handle.hwnd.get().cast_unsigned()),
        other => Err(format!("the window has no Win32 handle ({other:?})")),
    }
}

/// Captures the window at `address` to
/// `<config dir>/driver-shots/<pid>-<n>.png` on a worker thread, awaited
/// without blocking the UI thread until `timeout` ends.
async fn screenshot(id: u64, address: usize, n: u32, timeout: impl Future<Output = ()>) -> Reply {
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
    capture_reply(id, rx, timeout).await
}

/// The reply to request `id` from the capture thread's `answer`, or a
/// refusal when `timeout` ends first. A late answer is dropped.
async fn capture_reply(
    id: u64,
    answer: oneshot::Receiver<Shot>,
    timeout: impl Future<Output = ()>,
) -> Reply {
    match select(answer, std::pin::pin!(timeout)).await {
        Either::Left((Ok(Ok((path, width, height))), _)) => Reply::ok(
            id,
            json!({"path": path.display().to_string(), "width": width, "height": height}),
        ),
        Either::Left((Ok(Err(err)), _)) => refused(id, &format!("{err:#}")),
        Either::Left((Err(_), _)) => refused(id, "the capture thread ended without an answer"),
        Either::Right(((), _)) => refused(id, "the capture did not finish within 10 s"),
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
    let started = Instant::now();
    loop {
        let (drawn, frame) = oneshot::channel();
        if cx
            .update_window(handle, |_, window, _| request_frame(window, drawn))
            .is_err()
        {
            return;
        }
        match wait_for_frame(frame, cx).await {
            Frame::Drawn => {}
            Frame::Closed => return,
            Frame::TimedOut => {
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
            Ok(Ok(false)) if started.elapsed() < CLOAK_WAIT => {}
            Ok(Ok(false)) => {
                tracing::error!(
                    "driver: the window was not shown and cloaked within 5 s; no discovery file \
                     is written"
                );
                return;
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

    use futures::channel::oneshot;
    use serde_json::json;

    use super::{
        Discovery, Frame, Shot, after_frame, capture_reply, process_started_at, sweep,
        write_discovery,
    };
    use crate::driver::protocol::{ErrorCode, Reply};

    #[test]
    fn a_stuck_capture_is_answered_refused() {
        // The sender is held and never answers, as a stuck capture thread.
        let (_stuck, answer) = oneshot::channel::<Shot>();
        let reply = futures::executor::block_on(capture_reply(4, answer, async {}));
        assert_eq!(
            reply,
            Reply::error(
                4,
                ErrorCode::Refused,
                "the capture did not finish within 10 s".to_owned(),
                None,
            )
        );
    }

    #[test]
    fn a_frame_timeout_keeps_the_handlers_result() {
        let pressed = Reply::ok(5, json!({"pressed": "ctrl-,"}));
        assert_eq!(after_frame(pressed.clone(), Frame::Drawn), pressed);
        for frame in [Frame::TimedOut, Frame::Closed] {
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
