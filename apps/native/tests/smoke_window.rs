//! OS smoke specs: the real `rustling-tulip-native` binary in a real window,
//! against an isolated daemon. The window opens cloaked and is never
//! activated (`RUSTLING_TULIP_OFFSCREEN_WINDOW`), so a run never covers the
//! user's work or takes their focus. The specs check that the window
//! connects and paints, and that input posted to it as window messages
//! reaches the shell, read back from the daemon's scrollback, and redraws
//! the pane. Pixels are read with `PrintWindow(PW_RENDERFULLCONTENT)`, which
//! captures the cloaked window as rendered, at points computed from the
//! layout rather than against golden images. Ignored by default; run by
//! `.\rt.ps1 native-smoke`.

#![cfg(windows)]
#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

#[expect(dead_code, reason = "each spec file uses its own share of the helper")]
mod support;

use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use protocol::{ClientMessage, InitLayoutKind};
use rustling_tulip_native::palette::{ACCENT, PANEL_BG, SUNKEN};
use rustling_tulip_native::{
    DIVIDER_WIDTH, FOOTER_HEIGHT, NATIVE_PROTOCOL_VERSIONS, RAIL_ITEM_HEIGHT, RAIL_WIDTH,
    SIDEBAR_DEFAULT_WIDTH,
};
use serde_json::Value;
use support::live::{LiveDaemon, kill_tree, spawn_shell};
use tokio_tungstenite::tungstenite::stream::MaybeTlsStream;
use tokio_tungstenite::tungstenite::{self, Message, WebSocket};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PW_CLIENTONLY, PrintWindow};
use windows::Win32::System::StationsAndDesktops::{
    GetThreadDesktop, GetUserObjectInformationW, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MAPVK_VK_TO_VSC, MapVirtualKeyW, VIRTUAL_KEY, VK_OEM_MINUS, VK_OEM_PERIOD, VK_RETURN, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_HWNDPREV, GWL_EXSTYLE, GetClassNameW, GetClientRect, GetDlgItemTextW,
    GetForegroundWindow, GetWindow, GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible, PW_RENDERFULLCONTENT, PostMessageW, WM_CLOSE, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WS_EX_NOACTIVATE,
};
use windows::core::BOOL;

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// Makes the client open its window cloaked and never activate it.
const OFFSCREEN_ENV: &str = "RUSTLING_TULIP_OFFSCREEN_WINDOW";
/// `wParam` of a mouse message while the left button is down.
const MK_LBUTTON: usize = 0x0001;
/// A hang guard on the window's first paint of its sidebar and footer once
/// connected, not an expected duration.
const PAINT_TIMEOUT: Duration = Duration::from_secs(120);
/// The active rail button's accent bar in logical pixels
/// (`apps/native/src/activity_bar.rs`): its width at the window's left edge,
/// and its top and bottom on the first button, which sits at the top.
const RAIL_BAR_WIDTH: f32 = 2.0;
const RAIL_BAR_TOP: f32 = 10.0;
const RAIL_BAR_BOTTOM: f32 = 38.0;
/// The gap between two captures that must match for the pane to count as
/// settled.
const SETTLE_GAP: Duration = Duration::from_millis(250);
/// A hang guard on the pane drawing a command once the shell has echoed it,
/// not an expected duration.
const PANE_TIMEOUT: Duration = Duration::from_secs(120);
/// The fewest pane pixels, in square logical pixels, that typing
/// `echo rt-smoke-marker` and its output line must change: several times
/// the cursor cell a cursor move redraws, a quarter of the ~3,800 pixels the
/// two lines of text change at 100% scale.
const PANE_CHANGE_FLOOR: f32 = 1000.0;
/// What the client logs once its handshake with the daemon succeeds
/// (`apps/native/src/net.rs`).
const CONNECTED_LINE: &str = "connected to daemon";
/// The file the native client keeps its identity in, under the config dir
/// (`CLIENT_ID_FILE` in `apps/native/src/net.rs`).
const CLIENT_ID_FILE: &str = "client-id-native";
/// What the client logs when the daemon offers it the first-connect chooser
/// (`apps/native/src/layout_chooser_view.rs`). The chooser's backdrop dims
/// the probes, so a smoke run must never see this line.
const CHOOSER_LINE: &str = "first connect for this client";
/// The line the shell prints for the typed command.
const MARKER: &str = "rt-smoke-marker";
/// A hang guard on the client's first visible window, not an expected
/// duration.
const WINDOW_TIMEOUT: Duration = Duration::from_secs(120);
/// A hang guard on the client's own log recording its connection, not an
/// expected duration.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
/// A hang guard on posted keys reaching the shell and the command they type
/// landing, not an expected duration.
const KEYS_TIMEOUT: Duration = Duration::from_secs(120);
/// A hang guard on the seeded shell printing its first prompt, not an
/// expected duration.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// A hang guard on the test's socket waiting for one reply among the
/// broadcasts, not an expected duration.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);
/// A hang guard on one read from the test's socket, not an expected
/// duration.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(120);
/// The interval between two readings of the shell's scrollback while one
/// posted key's echo is awaited.
const ECHO_POLL: Duration = Duration::from_millis(250);
/// How long the attach probe waits for its posted Space to be echoed before
/// posting another; a repeated Space is harmless, since cmd ignores leading
/// blanks.
const PROBE_WAIT: Duration = Duration::from_secs(1);
/// How long the client's UI thread stays frozen after the stall spec
/// suspends it mid-line; the spec's thaw thread resumes it after this.
const STALL: Duration = Duration::from_secs(5);

/// The client binary, killed with everything under it on drop.
struct Client(Child);

impl Drop for Client {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            kill_tree(self.0.id());
        }
        let _ = self.0.wait();
    }
}

/// The client's UI thread, which drains the window's message queue, suspended
/// until dropped: a client stalled by load, for as long as the test says.
struct Frozen(HANDLE);

// SAFETY: a thread handle may be resumed and closed from any thread.
unsafe impl Send for Frozen {}

impl Frozen {
    fn ui_thread(hwnd: HWND) -> Self {
        // SAFETY: a plain query on a live window.
        let tid = unsafe { GetWindowThreadProcessId(hwnd, None) };
        // SAFETY: opens the client's UI thread for suspend and resume only.
        let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, tid) }
            .expect("open the client's UI thread");
        // SAFETY: suspends a thread of the client this test launched; Drop resumes it.
        let count = unsafe { SuspendThread(thread) };
        assert_ne!(count, u32::MAX, "suspend the client's UI thread");
        Self(thread)
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        // SAFETY: resumes the thread `ui_thread` suspended, then closes its handle.
        unsafe {
            ResumeThread(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// A short-lived socket of the test's own on `daemon`, past its handshake:
/// the client binary is a separate process with no sender to borrow.
fn connect(daemon: &LiveDaemon, client_id: &str) -> Socket {
    let handshake = daemon.handshake();
    let url = format!("ws://127.0.0.1:{}/ws", handshake.port);
    let (mut ws, _) = tungstenite::connect(url).expect("open a socket to the daemon");
    if let MaybeTlsStream::Plain(stream) = ws.get_ref() {
        TcpStream::set_read_timeout(stream, Some(SOCKET_TIMEOUT))
            .expect("set the socket's timeout");
    }
    send(
        &mut ws,
        &ClientMessage::Hello {
            protocol_version: NATIVE_PROTOCOL_VERSIONS[0],
            protocol_versions: NATIVE_PROTOCOL_VERSIONS.to_vec(),
            auth_token: handshake.auth_token.clone(),
            client_id: Some(client_id.to_owned()),
            client_name: None,
        },
    );
    read_until(&mut ws, "\"type\":\"welcome\"");
    ws
}

/// Seeds one plain shell and returns its session id.
fn seed_shell(daemon: &LiveDaemon) -> String {
    let mut ws = connect(daemon, "rt-smoke-seed");
    send(&mut ws, &spawn_shell(daemon.dir()));
    let updated = read_until(&mut ws, "\"type\":\"session_updated\"");
    let _ = ws.close(None);
    let _ = ws.flush();
    updated["session"]["id"]
        .as_str()
        .expect("the spawned session's id")
        .to_owned()
}

/// Seeds the identity the client binary will launch with, and a layout for it,
/// so the daemon answers its `Hello` with `Tabs` instead of
/// `LayoutInitRequired` and the client never opens the first-connect chooser,
/// whose full-window backdrop would dim the probes below.
///
/// The identity goes in the daemon's isolated config dir under the name the
/// client reads (`CLIENT_ID_FILE`), and the layout is created by the test's
/// own socket sending that identity's `InitLayout`, as `place_first_session`
/// does in the live tier.
fn seed_layout(daemon: &LiveDaemon) {
    let identity = daemon_client::client_identity_in(&daemon.config_dir(), CLIENT_ID_FILE)
        .expect("create the smoke client's identity");
    let mut ws = connect(daemon, &identity.client_id);
    read_until(&mut ws, "\"type\":\"layout_init_required\"");
    send(
        &mut ws,
        &ClientMessage::InitLayout {
            kind: InitLayoutKind::AllSessions,
        },
    );
    read_until(&mut ws, "\"type\":\"tabs\"");
    let _ = ws.close(None);
    let _ = ws.flush();
}

/// Selects the Contrast theme in the client's `native-ui.json`, so the
/// window draws the palette's colours the probes read.
fn pin_contrast(daemon: &LiveDaemon) {
    std::fs::write(
        daemon.config_dir().join("native-ui.json"),
        serde_json::json!({ "theme": "contrast" }).to_string(),
    )
    .expect("write the smoke client's native-ui.json");
}

fn send(ws: &mut Socket, msg: &ClientMessage) {
    let text = serde_json::to_string(msg).expect("encode a client message");
    ws.send(Message::Text(text.into()))
        .expect("write to the test's socket");
}

/// Reads frames until a text frame contains `needle`, and returns it parsed.
/// The daemon's broadcasts keep the socket busy, so the wait has its own
/// deadline rather than relying on the socket's read timeout.
fn read_until(ws: &mut Socket, needle: &str) -> Value {
    let deadline = Instant::now() + REPLY_TIMEOUT;
    loop {
        assert!(
            Instant::now() < deadline,
            "no daemon message containing {needle:?} within {REPLY_TIMEOUT:?}"
        );
        let frame = ws
            .read()
            .map_err(|err| format!("waiting for {needle:?}: {err}"))
            .expect("the daemon answers the test's socket");
        if let Message::Text(text) = frame
            && text.as_str().contains(needle)
        {
            return serde_json::from_str(text.as_str()).expect("a JSON daemon message");
        }
    }
}

/// `session`'s scrollback as the daemon holds it now.
fn scrollback(ws: &mut Socket, session: &str) -> Vec<u8> {
    send(
        ws,
        &ClientMessage::LoadScrollback {
            session_id: session.to_owned(),
            request_id: None,
        },
    );
    loop {
        let reply = read_until(ws, "\"type\":\"scrollback\"");
        if reply["session_id"] == session {
            let data = reply["data_b64"].as_str().expect("scrollback data");
            return B64.decode(data).expect("scrollback base64");
        }
    }
}

/// The text of terminal output, one entry per line, trimmed. Escape
/// sequences are dropped, and a cursor move (`CSI … H`) starts a new line.
fn plain_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                if skip_csi(&mut chars) == Some('H') {
                    out.push('\n');
                }
            }
            Some(']') => skip_osc(&mut chars),
            _ => {}
        }
    }
    out.split(['\n', '\r'])
        .map(|line| line.trim().to_owned())
        .collect()
}

/// Skips a CSI sequence's parameters and returns its final byte.
fn skip_csi(chars: &mut impl Iterator<Item = char>) -> Option<char> {
    chars.find(|c| ('@'..='~').contains(c))
}

/// Skips an OSC sequence up to its BEL or ST terminator.
fn skip_osc(chars: &mut impl Iterator<Item = char>) {
    while let Some(c) = chars.next() {
        if c == '\x07' {
            return;
        }
        if c == '\x1b' {
            chars.next();
            return;
        }
    }
}

/// Polls `session`'s scrollback until a line of it is exactly `line`, for
/// at most `within`.
fn wait_for_line(ws: &mut Socket, session: &str, line: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if plain_lines(&scrollback(ws, session))
            .iter()
            .any(|l| l == line)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Polls the seeded shell's scrollback until its prompt, a line in the
/// test's scratch dir ending in `>`, appears.
fn wait_for_prompt(ws: &mut Socket, smoke: &Smoke) {
    let dir = smoke
        .daemon
        .dir()
        .file_name()
        .expect("the scratch dir's name")
        .to_string_lossy()
        .into_owned();
    let deadline = Instant::now() + PROMPT_TIMEOUT;
    loop {
        let lines = plain_lines(&scrollback(ws, &smoke.session));
        if lines.iter().any(|l| l.ends_with('>') && l.contains(&dir)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no prompt in {dir} in the shell's scrollback within {PROMPT_TIMEOUT:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn launch_client(daemon: &LiveDaemon) -> Client {
    let exe = env!("CARGO_BIN_EXE_rustling-tulip-native");
    let child = Command::new(exe)
        .envs(
            daemon
                .envs()
                .iter()
                .map(|(key, value)| (*key, value.as_os_str())),
        )
        .env(OFFSCREEN_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch rustling-tulip-native");
    Client(child)
}

unsafe extern "system" fn collect_window(hwnd: HWND, found: LPARAM) -> BOOL {
    // SAFETY: `found` is the `&mut (u32, Option<HWND>)` `find_window` passes,
    // alive for the whole `EnumWindows` call.
    let found = unsafe { &mut *(found.0 as *mut (u32, Option<HWND>)) };
    let mut pid = 0;
    // SAFETY: plain queries on a window handle EnumWindows just gave us.
    let visible = unsafe {
        GetWindowThreadProcessId(hwnd, Some(&raw mut pid));
        IsWindowVisible(hwnd).as_bool()
    };
    if pid == found.0 && visible {
        found.1 = Some(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

/// The client's visible top-level window, once it has one.
fn find_window(pid: u32) -> HWND {
    let deadline = Instant::now() + WINDOW_TIMEOUT;
    loop {
        let mut found: (u32, Option<HWND>) = (pid, None);
        // SAFETY: the callback only writes through the pointer to `found`,
        // which outlives the call. EnumWindows reports an error when the
        // callback stops it early, so its result is not the signal.
        let _ = unsafe { EnumWindows(Some(collect_window), LPARAM(&raw mut found as isize)) };
        if let Some(hwnd) = found.1 {
            return hwnd;
        }
        assert!(
            Instant::now() < deadline,
            "no visible window for pid {pid} within {WINDOW_TIMEOUT:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The window must stay cloaked and never become the foreground window.
fn assert_out_of_the_way(hwnd: HWND) {
    let mut cloaked: u32 = 0;
    let size = u32::try_from(size_of::<u32>()).expect("u32 size");
    // SAFETY: an out-pointer to a local of the size passed, on a live window
    // handle.
    unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size)
            .expect("read the window's cloaked state");
    }
    // SAFETY: a plain query.
    let foreground = unsafe { GetForegroundWindow() };
    assert!(
        cloaked != 0,
        "the smoke window is not cloaked; {OFFSCREEN_ENV} must cloak it"
    );
    assert!(
        foreground != hwnd,
        "the smoke window became the foreground window"
    );
}

/// The window must carry `WS_EX_NOACTIVATE`, so nothing can make it the
/// foreground window.
fn assert_never_activates(hwnd: HWND) {
    // SAFETY: a plain query on a live window.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let noactivate = isize::try_from(WS_EX_NOACTIVATE.0).expect("an extended style bit");
    assert!(
        style & noactivate != 0,
        "the smoke window lacks WS_EX_NOACTIVATE (extended style {style:#x})"
    );
}

/// The window must not be the foreground window and must sit below it in
/// the Z order: walking up from the window through the windows above it
/// reaches the foreground window. With no foreground window at all (a
/// locked or disconnected session) there is nothing to sit below.
fn assert_below_foreground(hwnd: HWND) {
    // SAFETY: a plain query.
    let foreground = unsafe { GetForegroundWindow() };
    assert!(
        foreground != hwnd,
        "the smoke window became the foreground window"
    );
    if foreground.is_invalid() {
        return;
    }
    assert!(
        is_above(foreground, hwnd),
        "the smoke window sits above the foreground window {foreground:?} in the Z order"
    );
}

/// Whether `upper` is among the windows above `lower` in the Z order.
fn is_above(upper: HWND, lower: HWND) -> bool {
    // SAFETY: plain queries on window handles; one that has just closed
    // ends the walk with an error.
    let mut above = unsafe { GetWindow(lower, GW_HWNDPREV) };
    while let Ok(window) = above {
        if window == upper {
            return true;
        }
        // SAFETY: as above.
        above = unsafe { GetWindow(window, GW_HWNDPREV) };
    }
    false
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    // SAFETY: an out-pointer to a local, on a live window handle.
    unsafe { GetClientRect(hwnd, &raw mut rect) }.expect("read the client rect");
    (rect.right, rect.bottom)
}

fn scale(hwnd: HWND) -> f32 {
    // SAFETY: a plain query on a live window.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    f32::from(u16::try_from(dpi).unwrap_or(96)) / 96.0
}

/// `value` logical pixels in physical pixels at `scale`.
fn physical(value: f32, scale: f32) -> i32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "window coordinates are far inside i32; rounding is the intent"
    )]
    let pixels = (value * scale).round() as i32;
    pixels
}

/// The pane area of a `width` x `height` client area at `scale`, inset from
/// the rail, sidebar, divider, tab bar and footer, in client pixels.
fn pane_area((width, height): (i32, i32), scale: f32) -> RECT {
    RECT {
        left: physical(
            RAIL_WIDTH + SIDEBAR_DEFAULT_WIDTH + DIVIDER_WIDTH + 8.0,
            scale,
        ),
        top: physical(40.0, scale),
        right: width - physical(8.0, scale),
        bottom: height - physical(FOOTER_HEIGHT + 8.0, scale),
    }
}

/// The middle of the window's pane area.
fn pane_center(hwnd: HWND) -> (i32, i32) {
    let area = pane_area(client_size(hwnd), scale(hwnd));
    (
        i32::midpoint(area.left, area.right),
        i32::midpoint(area.top, area.bottom),
    )
}

/// The real client window on an isolated daemon with one plain shell.
struct Smoke {
    hwnd: HWND,
    session: String,
    _client: Client,
    daemon: LiveDaemon,
}

fn open(test: &str) -> Smoke {
    open_with(test, launch_client)
}

/// The smoke setup, with the client started by `launch`.
fn open_with(test: &str, launch: fn(&LiveDaemon) -> Client) -> Smoke {
    // SAFETY: sets this process's DPI mode, so window sizes read in physical
    // pixels; a second call in the same process fails harmlessly because the
    // mode is already set.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let daemon = LiveDaemon::start(test);
    let session = seed_shell(&daemon);
    seed_layout(&daemon);
    pin_contrast(&daemon);
    let client = launch(&daemon);
    let hwnd = find_window(client.0.id());
    assert_out_of_the_way(hwnd);
    Smoke {
        hwnd,
        session,
        _client: client,
        daemon,
    }
}

/// Waits for the client's own log, in the isolated config dir, to record
/// its connection to the daemon.
fn wait_connected(smoke: &Smoke) {
    let log = smoke.daemon.config_dir().join("logs").join("native.log");
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains(CONNECTED_LINE) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} has no {CONNECTED_LINE:?} line within {CONNECT_TIMEOUT:?}",
            log.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Asserts the client took the layout seeded before it launched instead of
/// opening the first-connect chooser. Called once the window has painted: the
/// chooser line is written while the client walks the daemon's opening state
/// burst, which the paint itself proves is over, so the check cannot read the
/// log before the line would have been written.
fn assert_no_layout_chooser(smoke: &Smoke) {
    let log = smoke.daemon.config_dir().join("logs").join("native.log");
    let text = std::fs::read_to_string(&log).expect("read the client's log");
    assert!(
        !text.contains(CHOOSER_LINE),
        "the client opened the first-connect layout chooser: {} contains {CHOOSER_LINE:?}",
        log.display()
    );
}

fn post(hwnd: HWND, message: u32, wparam: usize, lparam: isize) {
    // SAFETY: posts a plain input message to a live window.
    unsafe { PostMessageW(Some(hwnd), message, WPARAM(wparam), LPARAM(lparam)) }
        .expect("post a message to the smoke window");
}

/// A left click at client pixel (`x`, `y`): a move, a press and a release.
fn click_at(hwnd: HWND, (x, y): (i32, i32)) {
    let at = isize::try_from((y << 16) | (x & 0xffff)).expect("client coordinates");
    post(hwnd, WM_MOUSEMOVE, 0, at);
    post(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, at);
    post(hwnd, WM_LBUTTONUP, 0, at);
}

/// A key press and release of `vk`, with its scan code, as the keyboard
/// driver would post them. A posted key message carries no modifier state:
/// the client reads Ctrl, Shift and Alt from the physical keyboard, so
/// holding one down during a run can turn a letter into a shortcut and
/// flake the spec.
fn press(hwnd: HWND, vk: VIRTUAL_KEY) {
    // SAFETY: a pure table lookup.
    let scan = unsafe { MapVirtualKeyW(u32::from(vk.0), MAPVK_VK_TO_VSC) };
    let down = 1 | (scan << 16);
    let up = down | (1 << 30) | (1 << 31);
    let wparam = usize::from(vk.0);
    post(
        hwnd,
        WM_KEYDOWN,
        wparam,
        isize::try_from(down).expect("key lparam"),
    );
    post(
        hwnd,
        WM_KEYUP,
        wparam,
        isize::try_from(up).expect("key lparam"),
    );
}

/// Types lowercase letters, digits, spaces, dots and hyphens, then Enter.
/// Each posted key waits for the shell to echo it before the next key is
/// posted: a letter reaches the pane as the `WM_CHAR` that translating its
/// key-down posts, which queues behind every key-down already posted, while
/// Space and Enter are sent at the key-down itself, so keys posted without
/// waiting overtake one another whenever the client stalls.
fn type_line(
    hwnd: HWND,
    shell: &mut Socket,
    session: &str,
    text: &str,
    after_press: &mut dyn FnMut(usize),
) {
    let deadline = Instant::now() + KEYS_TIMEOUT;
    probe_attached(hwnd, shell, session, deadline);
    let mut expected = String::new();
    for (index, c) in text.chars().enumerate() {
        assert!(
            c == ' ' || c == '.' || c == '-' || c.is_ascii_alphanumeric(),
            "type_line types letters, digits, spaces, dots and hyphens only, not {c:?}"
        );
        expected.push(c);
        let vk = match c {
            ' ' => VK_SPACE,
            '.' => VK_OEM_PERIOD,
            '-' => VK_OEM_MINUS,
            c => {
                let ascii = u8::try_from(c.to_ascii_uppercase()).expect("an ASCII character");
                VIRTUAL_KEY(u16::from(ascii))
            }
        };
        if c == ' ' {
            // A Space is sent at its key-down like Enter; the key after it
            // proves by its own echo that the blank landed in place.
            let seen = scrollback(shell, session).len();
            press(hwnd, vk);
            after_press(index);
            wait_for_growth(shell, session, seen, deadline);
            continue;
        }
        press(hwnd, vk);
        after_press(index);
        wait_for_typed(shell, session, text, expected.trim_end(), deadline);
    }
    wait_for_typed(shell, session, text, text.trim_end(), deadline);
    press(hwnd, VK_RETURN);
}

/// Posts a Space until the shell echoes one, which shows the pane has
/// attached its session: a key posted before that is dropped. A repeated
/// Space is harmless, since cmd ignores leading blanks.
fn probe_attached(hwnd: HWND, shell: &mut Socket, session: &str, deadline: Instant) {
    loop {
        let seen = scrollback(shell, session).len();
        press(hwnd, VK_SPACE);
        let probe = (Instant::now() + PROBE_WAIT).min(deadline);
        let mut attached = false;
        while Instant::now() < probe {
            if scrollback(shell, session).len() > seen {
                attached = true;
                break;
            }
            std::thread::sleep(ECHO_POLL);
        }
        if attached {
            // The echoes of the probe's Spaces arrive in several frames: the
            // first key waits for a stream that has stopped changing.
            wait_for_still(shell, session, deadline);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no posted Space was echoed within {KEYS_TIMEOUT:?}: the pane never attached its \
             session"
        );
    }
}

/// Waits until the scrollback length is the same across two consecutive
/// polls, so a reply written in several frames has landed whole.
fn wait_for_still(shell: &mut Socket, session: &str, deadline: Instant) {
    let mut last = scrollback(shell, session).len();
    loop {
        assert!(
            Instant::now() < deadline,
            "the shell's scrollback never stopped growing within {KEYS_TIMEOUT:?}"
        );
        std::thread::sleep(ECHO_POLL);
        let len = scrollback(shell, session).len();
        if len == last {
            return;
        }
        last = len;
    }
}

/// Waits until `session`'s scrollback has grown past `len` bytes, the echo of
/// one posted key.
fn wait_for_growth(shell: &mut Socket, session: &str, len: usize, deadline: Instant) {
    loop {
        if scrollback(shell, session).len() > len {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "a posted key was not echoed within {KEYS_TIMEOUT:?}"
        );
        std::thread::sleep(ECHO_POLL);
    }
}

/// Polls until the shell has echoed the keys typed so far, `expected`, and
/// fails at once when what the shell has typed is not a prefix of the whole
/// line: a key that overtook another shows up as text out of order.
fn wait_for_typed(
    shell: &mut Socket,
    session: &str,
    whole: &str,
    expected: &str,
    deadline: Instant,
) {
    loop {
        let typed = typed_so_far(shell, session);
        assert!(
            whole.starts_with(&typed),
            "the shell had {typed:?} typed at its prompt, not a prefix of {whole:?}: the posted \
             keys reached it out of order"
        );
        if typed == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "a posted key was not echoed within {KEYS_TIMEOUT:?}: the shell had {typed:?} typed \
             at its prompt, not {expected:?}"
        );
        std::thread::sleep(ECHO_POLL);
    }
}

/// The text the shell has received at its prompt: the last three plain lines
/// joined with no separator, so a soft wrap between the prompt and the typed
/// text does not split them, from after the prompt's final `>`.
fn typed_so_far(shell: &mut Socket, session: &str) -> String {
    let lines = plain_lines(&scrollback(shell, session));
    let start = lines.len().saturating_sub(3);
    let mut tail = String::new();
    for line in &lines[start..] {
        tail.push_str(line);
    }
    tail.rsplit('>')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// A capture of the window's client area as the compositor renders it, in
/// top-down rows of BGRA pixels. The window is cloaked, so the capture is
/// the only place its pixels are ever shown.
struct Frame {
    width: i32,
    height: i32,
    bgra: Vec<u8>,
}

impl Frame {
    /// The pixel at client pixel `(x, y)` as `0xRRGGBB`.
    fn rgb(&self, (x, y): (i32, i32)) -> u32 {
        assert!(
            (0..self.width).contains(&x) && (0..self.height).contains(&y),
            "probe ({x}, {y}) is outside the {}x{} capture",
            self.width,
            self.height
        );
        let index = usize::try_from(y * self.width + x).expect("a pixel index") * 4;
        let pixel = &self.bgra[index..index + 4];
        (u32::from(pixel[2]) << 16) | (u32::from(pixel[1]) << 8) | u32::from(pixel[0])
    }

    fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    /// A point in the sidebar's empty middle, below its one session row.
    fn sidebar_probe(&self, scale: f32) -> (i32, i32) {
        (
            physical(SIDEBAR_DEFAULT_WIDTH / 2.0, scale),
            self.height / 2,
        )
    }

    /// A point at the footer's right end, in its padding past the status
    /// text.
    fn footer_probe(&self, scale: f32) -> (i32, i32) {
        (
            self.width - physical(8.0, scale),
            self.height - physical(FOOTER_HEIGHT / 2.0, scale),
        )
    }

    /// Whether the rail's first button, Sessions, shows its active bar:
    /// some pixel of the bar's strip reads [`ACCENT`]. The rail itself is
    /// [`SUNKEN`], close to the all-black capture the other probes guard
    /// against, so only the coral bar proves the rail painted.
    fn rail_bar_seen(&self, scale: f32) -> bool {
        self.rail_bar_seen_at(0, scale)
    }

    /// Whether the rail's button at `item`, counting from the top, shows its
    /// active bar: some pixel of that button's bar strip reads [`ACCENT`].
    fn rail_bar_seen_at(&self, item: usize, scale: f32) -> bool {
        let item = f32::from(u16::try_from(item).expect("a rail item index"));
        let top = physical(item * RAIL_ITEM_HEIGHT + RAIL_BAR_TOP, scale);
        let bottom = physical(item * RAIL_ITEM_HEIGHT + RAIL_BAR_BOTTOM, scale);
        (top..bottom)
            .any(|y| (0..physical(RAIL_BAR_WIDTH, scale)).any(|x| self.rgb((x, y)) == ACCENT))
    }

    /// How many pixels of the pane area differ between this capture and
    /// `other`, or `None` when the window changed size between the two.
    fn pane_changed(&self, other: &Self, scale: f32) -> Option<usize> {
        if self.size() != other.size() {
            return None;
        }
        let area = pane_area(self.size(), scale);
        let changed = (area.top..area.bottom)
            .flat_map(|y| (area.left..area.right).map(move |x| (x, y)))
            .filter(|&at| self.rgb(at) != other.rgb(at))
            .count();
        Some(changed)
    }
}

/// The window's client area through `PrintWindow` with
/// `PW_RENDERFULLCONTENT`, which reads the compositor's copy of a cloaked
/// window without showing, moving or activating it. Each capture also
/// checks the window is still cloaked and not in the foreground.
fn capture(hwnd: HWND) -> Frame {
    let (width, height) = client_size(hwnd);
    let size = usize::try_from(width * height).expect("a client size") * 4;
    let mut bgra = vec![0u8; size];
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).expect("header size"),
            biWidth: width,
            // Negative: top-down rows.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let flags = PRINT_WINDOW_FLAGS(PW_CLIENTONLY.0 | PW_RENDERFULLCONTENT);
    // SAFETY: the DCs and bitmap are created, used and released here, on a
    // live window; `bgra` holds the bitmap's full size in the format `info`
    // describes.
    let (printed, lines) = unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(dc, bitmap.into());
        let printed = PrintWindow(hwnd, dc, flags);
        SelectObject(dc, previous);
        let rows = u32::try_from(height).expect("a client height");
        let bits = Some(bgra.as_mut_ptr().cast());
        let lines = GetDIBits(dc, bitmap, 0, rows, bits, &raw mut info, DIB_RGB_COLORS);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        (printed.as_bool(), lines)
    };
    assert!(printed, "PrintWindow failed on the smoke window");
    assert_eq!(lines, height, "GetDIBits copied a partial capture");
    assert_out_of_the_way(hwnd);
    Frame {
        width,
        height,
        bgra,
    }
}

/// Captures the window until its sidebar and footer show their colours at
/// the probe points, and returns that capture. The points are computed from
/// each capture's own size, so a resize during startup is retried.
fn wait_painted(hwnd: HWND) -> Frame {
    let scale = scale(hwnd);
    let deadline = Instant::now() + PAINT_TIMEOUT;
    loop {
        let frame = capture(hwnd);
        let (sidebar, footer) = (frame.sidebar_probe(scale), frame.footer_probe(scale));
        let seen = (frame.rgb(sidebar), frame.rgb(footer));
        let rail_bar = frame.rail_bar_seen(scale);
        if seen == (PANEL_BG, SUNKEN) && rail_bar {
            return frame;
        }
        assert!(
            Instant::now() < deadline,
            "the window did not paint within {PAINT_TIMEOUT:?}: the sidebar at {sidebar:?} \
             reads {:06x}, not {PANEL_BG:06x}, the footer at {footer:?} reads {:06x}, not \
             {SUNKEN:06x}, and the rail's active bar in {ACCENT:06x} is {} (all black means the \
             capture sees no rendered content)",
            seen.0,
            seen.1,
            if rail_bar { "drawn" } else { "missing" }
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Captures the window until two captures in a row show the same pane
/// area, so the redraw a click causes (the focus border, the cursor) is
/// over, and returns the last one. A resize between two captures counts as
/// still redrawing.
fn wait_pane_settled(hwnd: HWND) -> Frame {
    let scale = scale(hwnd);
    let deadline = Instant::now() + PAINT_TIMEOUT;
    let mut last = capture(hwnd);
    loop {
        std::thread::sleep(SETTLE_GAP);
        let frame = capture(hwnd);
        let changed = frame.pane_changed(&last, scale);
        if changed == Some(0) {
            return frame;
        }
        assert!(
            Instant::now() < deadline,
            "the pane was still redrawing after {PAINT_TIMEOUT:?} ({changed:?} pixels changed \
             in the last {SETTLE_GAP:?}; None is a resize)"
        );
        last = frame;
    }
}

/// Captures the window until its pane area differs from `before` by at
/// least a typed line's worth of pixels.
fn wait_pane_changed(hwnd: HWND, before: &Frame) {
    let scale = scale(hwnd);
    let floor = physical(PANE_CHANGE_FLOOR, scale * scale);
    let floor = usize::try_from(floor).expect("a pixel count");
    let deadline = Instant::now() + PANE_TIMEOUT;
    loop {
        let changed = capture(hwnd).pane_changed(before, scale);
        if changed.is_some_and(|changed| changed >= floor) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the pane drew {changed:?} changed pixels within {PANE_TIMEOUT:?} (None is a \
             resize), fewer than the \
             {floor} a typed command and its output draw; the shell got the keys but the \
             window did not show them"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
fn smoke_window_opens_cloaked_unfocused_and_connects() {
    let smoke = open("smoke-connect");
    wait_connected(&smoke);
    wait_painted(smoke.hwnd);
    assert_no_layout_chooser(&smoke);
    assert_out_of_the_way(smoke.hwnd);
}

/// The class name of a Windows message box's dialog window.
const MESSAGE_BOX_CLASS: &str = "#32770";
/// The caption `start_guard::show_panic_box` gives its box.
const MESSAGE_BOX_TITLE: &str = "Rustling Tulip";
/// The dialog control id of a message box's message text.
const MESSAGE_BOX_ID: i32 = 0xFFFF;
/// The text the crash-box spec shows, so the box cannot be mistaken for a
/// real crash report.
const CRASH_BOX_TEXT: &str = "smoke: the crash box";
/// A hang guard on the crash box appearing or closing, not an expected
/// duration.
const CRASH_BOX_TIMEOUT: Duration = Duration::from_secs(120);

unsafe extern "system" fn collect_message_box(hwnd: HWND, found: LPARAM) -> BOOL {
    // SAFETY: `found` is the `(u32, Option<HWND>)` `find_message_box` passes,
    // alive for the whole `EnumWindows` call.
    let found = unsafe { &mut *(found.0 as *mut (u32, Option<HWND>)) };
    let mut pid = 0;
    // SAFETY: plain queries on a window handle EnumWindows just gave us.
    let visible = unsafe {
        GetWindowThreadProcessId(hwnd, Some(&raw mut pid));
        IsWindowVisible(hwnd).as_bool()
    };
    if pid != found.0 || !visible {
        return BOOL(1);
    }
    let mut class = [0u16; 64];
    // SAFETY: the buffer is sized for any class name; a longer one is
    // truncated rather than overrun.
    let len = unsafe { GetClassNameW(hwnd, &mut class) };
    let Ok(len) = usize::try_from(len) else {
        return BOOL(1);
    };
    if String::from_utf16_lossy(&class[..len]) != MESSAGE_BOX_CLASS {
        return BOOL(1);
    }
    let mut title = [0u16; 256];
    // SAFETY: the buffer is sized for the box's caption; a longer one is
    // truncated rather than overrun.
    let len = unsafe { GetWindowTextW(hwnd, &mut title) };
    let Ok(len) = usize::try_from(len) else {
        return BOOL(1);
    };
    if String::from_utf16_lossy(&title[..len]) == MESSAGE_BOX_TITLE {
        found.1 = Some(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

/// The crash message box this process shows, once it has one: a box thread
/// that ends without one fails at once, not with the hang guard.
fn find_message_box(pid: u32, shown: &std::thread::JoinHandle<()>) -> HWND {
    let deadline = Instant::now() + CRASH_BOX_TIMEOUT;
    loop {
        let mut found: (u32, Option<HWND>) = (pid, None);
        // SAFETY: the callback only writes through the pointer to `found`,
        // which outlives the call. EnumWindows reports an error when the
        // callback stops it early, so its result is not the signal.
        let _ = unsafe { EnumWindows(Some(collect_message_box), LPARAM(&raw mut found as isize)) };
        if let Some(hwnd) = found.1 {
            return hwnd;
        }
        assert!(
            !shown.is_finished(),
            "the crash message box thread ended without showing a box"
        );
        assert!(
            Instant::now() < deadline,
            "hung: no crash message box for pid {pid} within {CRASH_BOX_TIMEOUT:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The message the box shows, read from its text control.
fn message_box_text(hwnd: HWND) -> Option<String> {
    let mut text = [0u16; 1024];
    // SAFETY: the buffer is sized for a message box's text; a longer one is
    // truncated rather than overrun.
    let len = unsafe { GetDlgItemTextW(hwnd, MESSAGE_BOX_ID, &mut text) };
    let len = usize::try_from(len).ok()?;
    Some(String::from_utf16_lossy(&text[..len]))
}

/// The name of the desktop this thread's windows open on, such as `Default`.
fn desktop_name() -> String {
    let mut buf = [0u16; 128];
    // SAFETY: reads this thread's own desktop handle, which is not ours to
    // close, and copies its name into a buffer of the size passed.
    let name = unsafe {
        let desk = GetThreadDesktop(GetCurrentThreadId()).expect("read this thread's desktop");
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            u32::try_from(size_of::<[u16; 128]>()).expect("the buffer size fits a u32"),
            None,
        )
        .expect("read this thread's desktop name");
        buf
    };
    let end = name
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}

/// Waits for the box's thread to end, failing as a hang past the guard.
fn join_box_thread(shown: std::thread::JoinHandle<()>) {
    let deadline = Instant::now() + CRASH_BOX_TIMEOUT;
    while !shown.is_finished() {
        assert!(
            Instant::now() < deadline,
            "hung: the crash message box did not close within {CRASH_BOX_TIMEOUT:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    shown.join().expect("the crash box thread ended");
}

#[test]
#[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
fn the_crash_message_box_shows_its_text_and_closes() {
    assert!(
        !desktop_name().eq_ignore_ascii_case("Default"),
        "refused: the crash message box would open on the interactive desktop; run it through \
         .\\rt.ps1 native-smoke, which the gate routes to a hidden desktop"
    );
    let shown = std::thread::spawn(|| {
        rustling_tulip_native::start_guard::show_panic_box(CRASH_BOX_TEXT);
    });
    let hwnd = find_message_box(std::process::id(), &shown);
    let text = message_box_text(hwnd).expect("the crash box has no static text");
    assert!(
        text.contains(CRASH_BOX_TEXT),
        "the crash box showed {text:?}, not {CRASH_BOX_TEXT:?}"
    );
    // SAFETY: posts a close to a live dialog this process owns; its thread
    // then returns from `MessageBoxW`.
    unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }
        .expect("close the crash box");
    join_box_thread(shown);
}

#[test]
#[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
fn smoke_window_has_noactivate_and_sits_below_foreground() {
    let smoke = open("smoke-z-order");
    wait_connected(&smoke);
    assert_never_activates(smoke.hwnd);
    assert_below_foreground(smoke.hwnd);
    assert_out_of_the_way(smoke.hwnd);
}

#[test]
#[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
fn smoke_posted_keys_reach_the_shell() {
    let smoke = open("smoke-keys");
    let hwnd = smoke.hwnd;
    wait_connected(&smoke);
    wait_painted(hwnd);
    assert_no_layout_chooser(&smoke);
    let mut shell = connect(&smoke.daemon, "rt-smoke-reader");
    // The pane's baseline is taken after the prompt, so the shell's startup
    // output cannot pass for the typed command's redraw.
    wait_for_prompt(&mut shell, &smoke);
    click_at(hwnd, pane_center(hwnd));
    let before = wait_pane_settled(hwnd);
    type_line(
        hwnd,
        &mut shell,
        &smoke.session,
        &format!("echo {MARKER}"),
        &mut |_| {},
    );
    assert!(
        wait_for_line(&mut shell, &smoke.session, MARKER, KEYS_TIMEOUT),
        "no {MARKER:?} line in the shell's scrollback within {KEYS_TIMEOUT:?}; the posted keys \
         did not reach the shell"
    );
    wait_pane_changed(hwnd, &before);
    assert_out_of_the_way(hwnd);
}

#[test]
#[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
fn smoke_keys_posted_while_the_client_stalls_reach_the_shell_in_order() {
    let smoke = open("smoke-stall");
    let hwnd = smoke.hwnd;
    wait_connected(&smoke);
    wait_painted(hwnd);
    let mut shell = connect(&smoke.daemon, "rt-smoke-reader");
    wait_for_prompt(&mut shell, &smoke);
    click_at(hwnd, pane_center(hwnd));
    wait_pane_settled(hwnd);
    // The freeze lands right after the `o` of `echo` and lasts STALL: the
    // Space that follows waits for the `o`'s echo, so a harness with no
    // per-key wait queues the Space behind the letters and garbles the
    // command.
    let (froze, frozen) = std::sync::mpsc::channel::<Frozen>();
    let (posted, all_posted) = std::sync::mpsc::channel::<()>();
    let thaw = std::thread::spawn(move || {
        let Ok(frozen) = frozen.recv() else {
            return;
        };
        let _ = all_posted.recv_timeout(STALL);
        drop(frozen);
    });
    let mut stall = move |index: usize| {
        if index == 3 {
            let _ = froze.send(Frozen::ui_thread(hwnd));
        }
    };
    type_line(
        hwnd,
        &mut shell,
        &smoke.session,
        &format!("echo {MARKER}"),
        &mut stall,
    );
    drop(stall);
    let _ = posted.send(());
    thaw.join().expect("resume the client's UI thread");
    assert!(
        wait_for_line(&mut shell, &smoke.session, MARKER, KEYS_TIMEOUT),
        "no {MARKER:?} line within {KEYS_TIMEOUT:?}: keys posted while the client stalled reached \
         the shell out of order"
    );
}

#[test]
#[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
fn smoke_posted_click_on_the_rail_switches_the_panel() {
    let smoke = open("smoke-click");
    let hwnd = smoke.hwnd;
    wait_connected(&smoke);
    wait_painted(hwnd);
    assert_no_layout_chooser(&smoke);
    let scale = scale(hwnd);
    // The rail stacks Sessions, Needs You and Source control from its top with
    // no gap, so the third button's middle is two and a half items down.
    let at = (
        physical(RAIL_WIDTH / 2.0, scale),
        physical(RAIL_ITEM_HEIGHT * 2.5, scale),
    );
    click_at(hwnd, at);
    wait_source_control_active(hwnd, &format!("the posted click at {at:?}"));
    assert_out_of_the_way(smoke.hwnd);
}

/// Captures the window until the rail's active bar has moved from Sessions
/// to Source control, the effect of `click`, a description of the click.
fn wait_source_control_active(hwnd: HWND, click: &str) {
    let scale = scale(hwnd);
    let deadline = Instant::now() + PAINT_TIMEOUT;
    loop {
        let frame = capture(hwnd);
        let (third, first) = (
            frame.rail_bar_seen_at(2, scale),
            frame.rail_bar_seen_at(0, scale),
        );
        if third && !first {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{click} (scale {scale}) did not move the rail's active bar within \
             {PAINT_TIMEOUT:?}: Source control's strip shows {} and Sessions' strip shows {} (the \
             active bar is the {ACCENT:06x} accent)",
            if third { "the accent" } else { "no accent" },
            if first { "the accent" } else { "no accent" },
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The driven smoke specs: the client as a driven run, steered through its
/// driver pipe, whose pointer methods post window messages to its window.
#[cfg(feature = "driver")]
mod driven {
    use std::fs::{File, OpenOptions};
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use base64::Engine as _;
    use protocol::ClientMessage;
    use rustling_tulip_native::SIDEBAR_DEFAULT_WIDTH;
    use rustling_tulip_native::driver_gate::{DRIVER_ENV, DRIVER_TOASTS_ENV};
    use serde_json::{Value, json};
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GWL_EXSTYLE, GetCursorPos, GetForegroundWindow, GetWindowLongPtrW,
        GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, WS_EX_NOACTIVATE, WS_EX_TOPMOST,
    };
    use windows::core::BOOL;

    use super::{
        B64, Client, LiveDaemon, OFFSCREEN_ENV, PROMPT_TIMEOUT, Smoke, Socket,
        assert_no_layout_chooser, assert_out_of_the_way, connect, open_with, plain_lines,
        read_until, scrollback, send, wait_connected, wait_painted, wait_source_control_active,
    };

    /// A hang guard on each wait for the driver: its discovery file, one
    /// reply, or an effect read back through it; not an expected duration.
    const DRIVER_TIMEOUT: Duration = Duration::from_secs(60);
    /// A hang guard on a panicked client exiting, and on a next start after
    /// two panics closing its tabs: not an expected duration.
    const SAFE_START_TIMEOUT: Duration = Duration::from_secs(120);
    /// How long a spec's `wait_for` waits at most, inside [`DRIVER_TIMEOUT`].
    const WAIT_FOR_MS: u64 = 30_000;
    /// The sidebar's narrowest width (`MIN_WIDTH` in
    /// `apps/native/src/sidebar.rs`): `clamp_width` keeps the panes at least
    /// this wide too.
    const SIDEBAR_MIN_WIDTH: f64 = 200.0;
    /// How far the drag spec moves the sidebar divider, in logical pixels.
    const DRAG_BY: f64 = 80.0;
    /// How close the divider must land to where the drag aimed.
    const DRAG_SLACK: f64 = 4.0;

    /// The client binary as a driven run: [`super::launch_client`]'s cloaked
    /// window with the driver endpoint on. The driver gate
    /// (`apps/native/src/driver_gate.rs`) also needs the isolated folders,
    /// which the daemon's variables carry, and every registered agent's
    /// override variable, set to the fake the daemon runs.
    fn launch_driven_client(daemon: &LiveDaemon) -> Client {
        spawn_driven(driven_command(daemon))
    }

    /// [`launch_driven_client`] with the app's own toast windows shown
    /// (`DRIVER_TOASTS_ENV`): the main window stays cloaked, its toasts show
    /// on the desktop the run sits on.
    fn launch_driven_client_with_toasts(daemon: &LiveDaemon) -> Client {
        let mut command = driven_command(daemon);
        command.env(DRIVER_TOASTS_ENV, "1");
        spawn_driven(command)
    }

    /// The driven run's command: the daemon's variables, the cloaked window,
    /// the driver endpoint and every registered agent's override.
    fn driven_command(daemon: &LiveDaemon) -> Command {
        let exe = env!("CARGO_BIN_EXE_rustling-tulip-native");
        let envs = daemon.envs();
        let mut command = Command::new(exe);
        command
            .envs(envs.iter().map(|(key, value)| (*key, value.as_os_str())))
            .env(OFFSCREEN_ENV, "1")
            .env(DRIVER_ENV, "1")
            .env(
                daemon_client::isolation::REMOTE_FILES_DIR_VAR,
                daemon.dir().join("remote-files"),
            );
        for plugin in agent_registry::all() {
            let variable = plugin.program_env_var();
            let (_, fake) = envs
                .iter()
                .find(|(key, _)| *key == variable)
                .expect("the daemon's variables carry every agent's override");
            command.env(variable, fake);
        }
        command
    }

    /// Starts `command` as the driven client, its standard streams closed.
    fn spawn_driven(mut command: Command) -> Client {
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("launch rustling-tulip-native as a driven run");
        Client(child)
    }

    /// The id of the process that owns `hwnd`.
    fn window_pid(hwnd: HWND) -> u32 {
        let mut pid = 0;
        // SAFETY: a plain query on a live window, into a local.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
        pid
    }

    /// One connection to a driven client's pipe, one request at a time.
    struct Driver {
        /// The pipe, held here between calls; a call lends it to the thread
        /// that reads its reply.
        pipe: Option<BufReader<File>>,
        next_id: u64,
    }

    impl Driver {
        /// Connects to client `pid`'s pipe once its discovery file shows
        /// the endpoint is up.
        fn connect(pid: u32) -> Self {
            let discovery = std::env::temp_dir()
                .join("rustling-tulip-driver")
                .join(format!("{pid}.json"));
            let deadline = Instant::now() + DRIVER_TIMEOUT;
            while !discovery.exists() {
                assert!(
                    Instant::now() < deadline,
                    "hung: no discovery file {} within {DRIVER_TIMEOUT:?}",
                    discovery.display()
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            let pipe = OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!(r"\\.\pipe\rt-driver-{pid}"))
                .expect("open the driver pipe");
            Self {
                pipe: Some(BufReader::new(pipe)),
                next_id: 1,
            }
        }

        /// The reply to `method` with `params`. The reply is read on a
        /// thread of its own, so a client that never answers fails the spec
        /// after [`DRIVER_TIMEOUT`]; the write happens first, since a
        /// synchronous pipe handle runs one read or write at a time.
        fn call(&mut self, method: &str, params: &Value) -> Value {
            let id = self.next_id;
            self.next_id += 1;
            let mut pipe = self.pipe.take().expect("no call is in flight");
            let request = json!({"id": id, "method": method, "params": params});
            let mut line = serde_json::to_string(&request).expect("encode a driver request");
            line.push('\n');
            pipe.get_mut()
                .write_all(line.as_bytes())
                .expect("write to the driver pipe");
            let (sent, replied) = mpsc::channel();
            std::thread::spawn(move || {
                let mut reply = String::new();
                let read = pipe.read_line(&mut reply).map(|_| reply);
                let _ = sent.send((pipe, read));
            });
            let (pipe, read) = replied
                .recv_timeout(DRIVER_TIMEOUT)
                .map_err(|_| format!("hung: no reply to {request} within {DRIVER_TIMEOUT:?}"))
                .expect("the driver answers");
            self.pipe = Some(pipe);
            let reply: Value = serde_json::from_str(&read.expect("read the driver pipe"))
                .expect("a JSON driver reply");
            assert_eq!(
                reply["id"],
                json!(id),
                "the reply answers {request}: {reply}"
            );
            reply
        }

        /// Sends `method` and reads no reply, for a call the client does not
        /// live to answer.
        fn fire(&mut self, method: &str) {
            let id = self.next_id;
            self.next_id += 1;
            let request = json!({"id": id, "method": method, "params": {}});
            let mut line = serde_json::to_string(&request).expect("encode a driver request");
            line.push('\n');
            self.pipe
                .as_mut()
                .expect("no call is in flight")
                .get_mut()
                .write_all(line.as_bytes())
                .expect("write to the driver pipe");
        }

        /// The result of `method` with `params`, failing the spec with the
        /// error it got instead.
        fn ok(&mut self, method: &str, params: &Value) -> Value {
            let reply = self.call(method, params);
            assert!(
                reply.get("error").is_none(),
                "{method} {params} failed: {reply}"
            );
            reply["result"].clone()
        }

        /// Waits with `wait_for` until `until` holds, failing the spec with
        /// the last value read when it does not within [`WAIT_FOR_MS`].
        fn wait(&mut self, until: &Value, what: &str) {
            let waited = self.ok(
                "wait_for",
                &json!({"until": until, "timeout_ms": WAIT_FOR_MS}),
            );
            assert_eq!(
                waited["met"],
                json!(true),
                "{what}: {until} did not hold within {WAIT_FOR_MS} ms; last read {}",
                waited["last"]
            );
        }

        /// The element `name` as `find` lists it.
        fn element(&mut self, name: &str) -> Value {
            let found = self.ok("find", &json!({"prefix": name}));
            found
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["name"] == json!(name)))
                .cloned()
                .ok_or_else(|| format!("find lists no {name}: {found}"))
                .expect("the element is painted")
        }
    }

    /// The smoke setup with a driven client, painted and connected, and a
    /// connection to its driver pipe.
    fn open_driven(test: &str) -> (Smoke, Driver) {
        open_driven_with(test, launch_driven_client)
    }

    /// [`open_driven`] with the client started by `launch`.
    fn open_driven_with(test: &str, launch: fn(&LiveDaemon) -> Client) -> (Smoke, Driver) {
        let smoke = open_with(test, launch);
        wait_connected(&smoke);
        wait_painted(smoke.hwnd);
        assert_no_layout_chooser(&smoke);
        let driver = Driver::connect(window_pid(smoke.hwnd));
        (smoke, driver)
    }

    /// Stops `daemon` and starts it again on a saved layout holding one
    /// Dashboard tab, which the daemon's start keeps, for the identity the
    /// client binary will launch with. Returns that identity and the tab's id.
    fn seed_dashboard_tab(daemon: &mut LiveDaemon) -> (String, String) {
        let tab_id = "smoke-safe-start-tab".to_owned();
        let client_id =
            daemon_client::client_identity_in(&daemon.config_dir(), super::CLIENT_ID_FILE)
                .expect("create the smoke client's identity")
                .client_id;
        daemon.kill();
        let state = json!({
            "repos": [],
            "workspaces": [],
            "layouts": { client_id.as_str(): { "tabs": [{
                "id": tab_id,
                "name": "Dashboard",
                "content": { "kind": "dashboard" },
                "created_at": "2026-01-01T00:00:00Z",
            }] } },
        });
        std::fs::write(daemon.config_dir().join("state.json"), state.to_string())
            .expect("seed state.json");
        daemon.restart_after_reboot();
        (client_id, tab_id)
    }

    /// Waits for `client` to exit, under a hang guard, and returns its status.
    fn wait_exit(client: &mut Client) -> std::process::ExitStatus {
        let deadline = Instant::now() + SAFE_START_TIMEOUT;
        loop {
            if let Some(status) = client.0.try_wait().expect("read the client's exit") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "hung: the client is still running {SAFE_START_TIMEOUT:?} after its panic"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Whether `state.json` in `config` holds an empty layout for `client_id`.
    /// A file caught mid-replace reads as not yet.
    fn layout_is_empty(config: &std::path::Path, client_id: &str) -> bool {
        std::fs::read_to_string(config.join("state.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|state| state["layouts"][client_id]["tabs"].as_array().cloned())
            .is_some_and(|tabs| tabs.is_empty())
    }

    /// The text of every kept copy of a closed layout in `config`.
    fn layout_copies(config: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(config)
            .expect("list the config dir")
            .map(|entry| entry.expect("an entry").path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("layout-set-aside-"))
            })
            .map(|path| std::fs::read_to_string(path).expect("read a kept copy"))
            .collect()
    }

    #[test]
    #[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
    fn two_panics_at_start_ask_about_the_tabs_on_the_next_start() {
        let mut daemon = LiveDaemon::start("smoke-safe-start");
        let (client_id, tab_id) = seed_dashboard_tab(&mut daemon);
        let config = daemon.config_dir();

        let mut crashed = launch_driven_client(&daemon);
        let mut driver = Driver::connect(crashed.0.id());
        driver.ok("ping", &json!({}));
        driver.fire("panic");
        let status = wait_exit(&mut crashed);
        assert!(
            !status.success(),
            "the panicked client exited with {status}"
        );

        let guard: Value = serde_json::from_str(
            &std::fs::read_to_string(config.join("start-guard.json")).expect("read the guard"),
        )
        .expect("the guard is valid JSON");
        assert_eq!(guard["failed_starts"], json!(1), "{guard}");
        assert!(
            guard["last_panic"]
                .as_str()
                .is_some_and(|text| text.contains("driver asked for a panic")),
            "{guard}"
        );
        assert!(
            layout_copies(&config).is_empty(),
            "the first crash sets nothing aside"
        );

        let mut retried = launch_driven_client(&daemon);
        let mut retried_driver = Driver::connect(retried.0.id());
        retried_driver.ok("ping", &json!({}));
        wait_start_prompt(&mut retried_driver, &mut retried);
        assert!(
            !layout_is_empty(&config, &client_id),
            "the retried start keeps the tab while it asks"
        );
        assert!(
            layout_copies(&config).is_empty(),
            "the retried start sets no layout aside"
        );
        retried_driver.fire("panic");
        let status = wait_exit(&mut retried);
        assert!(
            !status.success(),
            "the second panicked client exited with {status}"
        );

        let guard: Value = serde_json::from_str(
            &std::fs::read_to_string(config.join("start-guard.json")).expect("read the guard"),
        )
        .expect("the guard is valid JSON");
        assert_eq!(guard["failed_starts"], json!(2), "{guard}");

        let mut third = launch_driven_client(&daemon);
        let mut third_driver = Driver::connect(third.0.id());
        third_driver.ok("ping", &json!({}));
        wait_start_prompt(&mut third_driver, &mut third);
        assert!(
            !layout_is_empty(&config, &client_id),
            "nothing is closed before the user chooses"
        );
        assert!(
            std::fs::read_to_string(config.join("state.json"))
                .expect("read state.json")
                .contains(&tab_id),
            "the saved layout still holds the seeded tab"
        );
        assert!(
            layout_copies(&config).is_empty(),
            "nothing is set aside before the user chooses"
        );
    }

    /// Waits until the start-after-a-crash prompt is painted, then checks
    /// that no tab draws behind it.
    #[expect(
        clippy::panic,
        reason = "a client that exits now fails the spec at once"
    )]
    fn wait_start_prompt(driver: &mut Driver, client: &mut Client) {
        let deadline = Instant::now() + SAFE_START_TIMEOUT;
        loop {
            let found = driver.ok("find", &json!({"prefix": "start-prompt-without"}));
            let shown = found.as_array().is_some_and(|rows| {
                rows.iter()
                    .any(|row| row["name"] == json!("start-prompt-without"))
            });
            if shown {
                break;
            }
            if let Some(status) = client.0.try_wait().expect("poll the client") {
                panic!("the client exited before it asked about the tabs: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "hung: no prompt {SAFE_START_TIMEOUT:?} into the start"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        let state = driver.ok("state", &json!({}));
        assert_eq!(state["tabs"], json!([]), "no tab draws behind the prompt");
    }

    fn number(value: &Value) -> f64 {
        value.as_f64().expect("a number")
    }

    #[test]
    #[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
    fn smoke_driver_click_switches_the_rail_panel() {
        let (smoke, mut driver) = open_driven("smoke-driver-click");
        driver.element("activity-source-control");

        driver.ok("click", &json!({"element": "activity-source-control"}));

        wait_source_control_active(smoke.hwnd, "the driver's click on activity-source-control");
        assert_out_of_the_way(smoke.hwnd);
    }

    /// Drags the sidebar divider by [`DRAG_BY`] and waits until it lands
    /// where the drag aimed, within [`DRAG_SLACK`], the panes keeping their
    /// minimum width.
    fn drag_sidebar_divider(driver: &mut Driver) {
        let bounds = driver.element("sidebar-divider")["bounds"].clone();
        let x0 = number(&bounds["x"]);
        let centre = (
            x0 + number(&bounds["w"]) / 2.0,
            number(&bounds["y"]) + number(&bounds["h"]) / 2.0,
        );
        let width = number(&driver.ok("state", &json!({}))["window"]["width"]);
        // The sidebar starts at its default width and stops where the panes
        // keep their minimum.
        let room = width - SIDEBAR_MIN_WIDTH - f64::from(SIDEBAR_DEFAULT_WIDTH);
        let expected = x0 + DRAG_BY.min(room);

        driver.ok(
            "drag",
            &json!({
                "from": {"element": "sidebar-divider"},
                "to": {"x": centre.0 + DRAG_BY, "y": centre.1},
            }),
        );

        let deadline = Instant::now() + DRIVER_TIMEOUT;
        loop {
            let x = number(&driver.element("sidebar-divider")["bounds"]["x"]);
            if (x - expected).abs() <= DRAG_SLACK {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the drag from x {x0} by {DRAG_BY} left the divider at x {x}, not within \
                 {DRAG_SLACK} px of {expected}, after {DRIVER_TIMEOUT:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    #[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
    fn smoke_driver_drag_moves_the_sidebar_divider() {
        let (smoke, mut driver) = open_driven("smoke-driver-drag");
        drag_sidebar_divider(&mut driver);
        assert_out_of_the_way(smoke.hwnd);
    }

    /// The `n` of the first visible row that reads `rt-<n>-`.
    fn first_counted_row(lines: &Value) -> Option<u32> {
        lines.as_array()?.iter().find_map(|line| {
            line.as_str()?
                .trim()
                .strip_prefix("rt-")?
                .strip_suffix('-')?
                .parse()
                .ok()
        })
    }

    /// The loop typed into a pane to fill its scrollback past the first
    /// row it shows.
    const SCROLLBACK_LOOP: &str = "for /l %i in (1,1,200) do @echo rt-%i-";

    /// Types [`SCROLLBACK_LOOP`] into `pane` and waits until the prompt row
    /// shows it; [`submit_scrollback_loop`] submits it.
    fn type_scrollback_loop(driver: &mut Driver, pane: &str) {
        driver.ok("type_text", &json!({"text": SCROLLBACK_LOOP}));
        driver.wait(
            &json!({"pane_text": {"pane": pane, "contains": "do @echo rt-"}}),
            "the typed loop in the pane",
        );
    }

    /// Submits the loop [`type_scrollback_loop`] typed into `pane` and waits
    /// for its last line, returning the `n` of the pane's first counted row:
    /// the loop's output fills the pane, so it has scrolled past its first
    /// line.
    fn submit_scrollback_loop(driver: &mut Driver, pane: &str) -> u32 {
        driver.ok("key", &json!({"keys": "enter"}));
        driver.wait(
            &json!({"pane_text": {"pane": pane, "contains": "rt-200-"}}),
            "the loop's last line",
        );
        let read = driver.ok("pane_text", &json!({"pane": pane}));
        let first = first_counted_row(&read["lines"])
            .ok_or_else(|| format!("no rt-<n>- row in the pane: {}", read["text"]))
            .expect("the loop's output fills the pane");
        assert!(
            first > 1,
            "the pane shows the loop's first line: {}",
            read["text"]
        );
        first
    }

    /// Turns the wheel three notches up over `pane` and waits for the row
    /// above the pane's first, which was `first`.
    fn wheel_pane_up(driver: &mut Driver, pane: &str, first: u32) {
        driver.ok(
            "wheel",
            &json!({"element": format!("pane-grid-{pane}"), "notches": 3}),
        );
        driver.wait(
            &json!({"pane_text": {"pane": pane, "contains": format!("rt-{}-", first - 1)}}),
            "the row above the pane's first after three notches up",
        );
    }

    #[test]
    #[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
    fn smoke_driver_wheel_scrolls_a_pane_back() {
        let (smoke, mut driver) = open_driven("smoke-driver-wheel");
        let pane = driver.ok("state", &json!({}))["panes"][0]["id"]
            .as_str()
            .expect("the shell's pane")
            .to_owned();
        driver.ok(
            "action",
            &json!({"name": "focus_pane", "args": {"id": pane}}),
        );
        // The prompt names the scratch dir: once the pane shows it, the pane
        // has attached its session and takes keys.
        let dir = smoke
            .daemon
            .dir()
            .file_name()
            .expect("the scratch dir's name")
            .to_string_lossy()
            .into_owned();
        driver.wait(
            &json!({"pane_text": {"pane": pane, "contains": dir}}),
            "the shell's prompt",
        );
        type_scrollback_loop(&mut driver, &pane);
        let first = submit_scrollback_loop(&mut driver, &pane);
        wheel_pane_up(&mut driver, &pane, first);
        assert_out_of_the_way(smoke.hwnd);
    }

    #[test]
    #[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
    fn smoke_driver_right_click_opens_a_menu() {
        let (smoke, mut driver) = open_driven("smoke-driver-menu");
        let row = format!("leaf-{}", smoke.session);
        driver.element(&row);

        driver.ok("click", &json!({"element": row, "button": "right"}));

        driver.wait(
            &json!({"state": "overlay", "equals": "menu"}),
            "the session row's menu",
        );
        assert_out_of_the_way(smoke.hwnd);
    }

    /// The input state of the desktop a run sits on, which a driven run must
    /// never move: its foreground window and its cursor position.
    #[derive(Debug, PartialEq, Eq)]
    struct DesktopInput {
        foreground: HWND,
        cursor: (i32, i32),
    }

    impl DesktopInput {
        fn read() -> Self {
            // SAFETY: a plain query.
            let foreground = unsafe { GetForegroundWindow() };
            let mut cursor = POINT::default();
            // SAFETY: an out-pointer to a local.
            unsafe { GetCursorPos(&raw mut cursor) }.expect("read the cursor position");
            Self {
                foreground,
                cursor: (cursor.x, cursor.y),
            }
        }
    }

    /// The desktop's foreground window and cursor are still `before`'s after
    /// `step`, and the client's window is still out of the way.
    fn assert_unmoved(before: &DesktopInput, hwnd: HWND, step: &str) {
        let now = DesktopInput::read();
        assert_eq!(
            &now, before,
            "the drive's {step} moved the desktop: before the drive {before:?}, after the step {now:?}"
        );
        assert_out_of_the_way(hwnd);
    }

    #[test]
    #[ignore = "live: run via .\\rt.ps1 native-drive-check"]
    fn live_drive_leaves_the_foreground_and_cursor_alone() {
        let before = DesktopInput::read();
        assert!(
            !before.foreground.is_invalid(),
            "no foreground window to compare against; run this on a logged-on desktop \
             (rt.ps1 native-drive-check routes there)"
        );
        let (smoke, mut driver) = open_driven("live-drive-check");
        assert_unmoved(&before, smoke.hwnd, "launch");

        driver.element("activity-source-control");
        driver.ok("click", &json!({"element": "activity-source-control"}));
        wait_source_control_active(smoke.hwnd, "the driver's click on activity-source-control");
        assert_unmoved(&before, smoke.hwnd, "click on activity-source-control");

        let pane = driver.ok("state", &json!({}))["panes"][0]["id"]
            .as_str()
            .expect("the shell's pane")
            .to_owned();
        driver.ok(
            "action",
            &json!({"name": "focus_pane", "args": {"id": pane}}),
        );
        // The prompt names the scratch dir: once the pane shows it, the pane
        // has attached its session and takes keys.
        let dir = smoke
            .daemon
            .dir()
            .file_name()
            .expect("the scratch dir's name")
            .to_string_lossy()
            .into_owned();
        driver.wait(
            &json!({"pane_text": {"pane": pane, "contains": dir}}),
            "the shell's prompt",
        );
        driver.ok("type_text", &json!({"text": "echo rt-live-drive"}));
        driver.wait(
            &json!({"pane_text": {"pane": pane, "contains": "rt-live-drive"}}),
            "the typed text in the pane",
        );
        assert_unmoved(&before, smoke.hwnd, "type_text into the focused pane");

        // Submit the typed line so the loop below starts on a clean prompt:
        // its output row, not the typed command, is what shows it ran.
        driver.ok("key", &json!({"keys": "enter"}));
        driver.wait(
            &json!({"pane_text": {"pane": pane, "contains": "\nrt-live-drive\n"}}),
            "the echo's output row",
        );
        assert_unmoved(&before, smoke.hwnd, "submit of the typed line");

        drag_sidebar_divider(&mut driver);
        assert_unmoved(&before, smoke.hwnd, "drag of the sidebar divider");

        type_scrollback_loop(&mut driver, &pane);
        assert_unmoved(&before, smoke.hwnd, "type_text of the loop into the pane");
        let first = submit_scrollback_loop(&mut driver, &pane);
        assert_unmoved(&before, smoke.hwnd, "submit of the loop");
        wheel_pane_up(&mut driver, &pane, first);
        assert_unmoved(&before, smoke.hwnd, "wheel over the pane");

        driver.ok("screenshot", &json!({}));
        assert_unmoved(&before, smoke.hwnd, "screenshot");
    }

    /// A hang guard on the app's own toast window showing once the session
    /// asks, not an expected duration.
    const TOAST_TIMEOUT: Duration = Duration::from_secs(30);
    /// What fake-claude's permission dialog asks
    /// (`crates/agents/claude/fake/README.md`).
    const PERMISSION_QUESTION: &str = "Do you want to proceed?";

    /// Spawns a session of the default agent, whose CLI is the fake the
    /// daemon runs, in the daemon's scratch dir over the test's own socket,
    /// and returns its session id.
    fn seed_fake_agent(daemon: &LiveDaemon) -> String {
        let mut ws = connect(daemon, "rt-smoke-agent");
        let spawn: ClientMessage = serde_json::from_value(json!({
            "type": "spawn_session",
            "label": null,
            "target": {"kind": "standalone", "cwd": daemon.dir()},
            "mode": "interactive",
            "initial_prompt": null,
            "dangerously_skip_permissions": false,
            "agent": {"agent": agent_registry::default_agent(), "options": {}},
        }))
        .expect("a spawn request for the default agent");
        send(&mut ws, &spawn);
        let id = loop {
            let updated = read_until(&mut ws, "\"type\":\"session_updated\"");
            if updated["session"]["mode"] == json!("interactive") {
                break updated["session"]["id"]
                    .as_str()
                    .expect("the spawned session's id")
                    .to_owned();
            }
        };
        let _ = ws.close(None);
        let _ = ws.flush();
        id
    }

    /// Polls `session`'s scrollback until a line of it passes `seen`,
    /// failing as hung with `what` after [`PROMPT_TIMEOUT`].
    fn wait_scrollback(ws: &mut Socket, session: &str, what: &str, seen: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + PROMPT_TIMEOUT;
        loop {
            if plain_lines(&scrollback(ws, session))
                .iter()
                .any(|line| seen(line))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "hung: no {what} in session {session}'s scrollback within {PROMPT_TIMEOUT:?}"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Writes `bytes` to `session` as the owner's keys.
    fn type_into(ws: &mut Socket, session: &str, bytes: &[u8]) {
        send(
            ws,
            &ClientMessage::SendInput {
                session_id: session.to_owned(),
                data_b64: B64.encode(bytes),
                automatic: false,
            },
        );
    }

    unsafe extern "system" fn collect_visible(hwnd: HWND, found: LPARAM) -> BOOL {
        // SAFETY: `found` is the `&mut (u32, Vec<HWND>)` `find_toast` passes,
        // alive for the whole `EnumWindows` call.
        let found = unsafe { &mut *(found.0 as *mut (u32, Vec<HWND>)) };
        let mut pid = 0;
        // SAFETY: plain queries on a window handle EnumWindows just gave us.
        let visible = unsafe {
            GetWindowThreadProcessId(hwnd, Some(&raw mut pid));
            IsWindowVisible(hwnd).as_bool()
        };
        if pid == found.0 && visible {
            found.1.push(hwnd);
        }
        BOOL(1)
    }

    /// The client's visible top-level window other than its main one: the
    /// app's own toast, once one shows.
    fn find_toast(smoke: &Smoke) -> HWND {
        let pid = window_pid(smoke.hwnd);
        let log = smoke.daemon.config_dir().join("logs").join("native.log");
        let deadline = Instant::now() + TOAST_TIMEOUT;
        loop {
            let mut found: (u32, Vec<HWND>) = (pid, Vec::new());
            // SAFETY: the callback only writes through the pointer to
            // `found`, which outlives the call.
            let _ = unsafe { EnumWindows(Some(collect_visible), LPARAM(&raw mut found as isize)) };
            if let Some(toast) = found.1.into_iter().find(|hwnd| *hwnd != smoke.hwnd) {
                return toast;
            }
            assert!(
                Instant::now() < deadline,
                "hung: no toast window of the client appeared within {TOAST_TIMEOUT:?}; see {}",
                log.display()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The toast shows on top, never activates, is not cloaked and has an
    /// area.
    fn assert_toast_on_top(toast: HWND) {
        // SAFETY: a plain query on a live window.
        let style = unsafe { GetWindowLongPtrW(toast, GWL_EXSTYLE) };
        let bit = |flag: u32| isize::try_from(flag).expect("an extended style bit");
        assert!(
            style & bit(WS_EX_TOPMOST.0) != 0,
            "the toast lacks WS_EX_TOPMOST (extended style {style:#x})"
        );
        assert!(
            style & bit(WS_EX_NOACTIVATE.0) != 0,
            "the toast lacks WS_EX_NOACTIVATE (extended style {style:#x})"
        );
        let mut cloaked: u32 = 0;
        let size = u32::try_from(size_of::<u32>()).expect("u32 size");
        // SAFETY: an out-pointer to a local of the size passed, on a live
        // window handle.
        unsafe { DwmGetWindowAttribute(toast, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size) }
            .expect("read the toast's cloaked state");
        assert_eq!(cloaked, 0, "the toast window is cloaked");
        let mut rect = RECT::default();
        // SAFETY: an out-pointer to a local, on a live window handle.
        unsafe { GetWindowRect(toast, &raw mut rect) }.expect("read the toast's rect");
        assert!(
            rect.right > rect.left && rect.bottom > rect.top,
            "the toast window has no area: {rect:?}"
        );
    }

    /// The Notifications page's "the app's own" choice under Show toasts as.
    const TOAST_STYLE_APP: &str = "settings-toast-style-app";
    /// The most wheel turns [`scroll_into_view`] makes before it gives up.
    const SCROLL_TRIES: usize = 10;

    /// The element `name` as `find` lists it now, if it does.
    fn listed(driver: &mut Driver, name: &str) -> Option<Value> {
        driver
            .ok("find", &json!({"prefix": name}))
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["name"] == json!(name)))
            .cloned()
    }

    /// Whether `row`, as `find` lists it, shows some part of itself.
    fn shows(row: &Value) -> bool {
        let side = |key: &str| row["visible"][key].as_f64().unwrap_or(0.0);
        side("w") > 0.0 && side("h") > 0.0
    }

    /// Turns the wheel down over the Settings page until element `name`
    /// shows, at most [`SCROLL_TRIES`] times, each turn followed by a short
    /// wait for the next frame to show it; fails naming the last read.
    fn scroll_into_view(driver: &mut Driver, name: &str) {
        let height = number(&driver.ok("state", &json!({}))["window"]["height"]);
        let mut last = None;
        for _ in 0..SCROLL_TRIES {
            let row = listed(driver, name);
            if row.as_ref().is_some_and(shows) {
                return;
            }
            if let Some(row) = &row {
                let bounds = &row["bounds"];
                let x = number(&bounds["x"]) + number(&bounds["w"]) / 2.0;
                driver.ok("wheel", &json!({"x": x, "y": height / 2.0, "notches": -3}));
            }
            // The next frame, or the page's first: its outcome is read above.
            driver.ok(
                "wait_for",
                &json!({"until": {"element": name, "visible": true}, "timeout_ms": 1000}),
            );
            last = row;
        }
        let row = listed(driver, name);
        assert!(
            row.as_ref().is_some_and(shows),
            "{name} is still not visible after {SCROLL_TRIES} wheel turns down; last read \
             {last:?}, now {row:?}"
        );
    }

    /// Chooses the app's own toasts in Settings › Notifications through the
    /// driver, waits for the choice to be saved, and closes Settings.
    fn choose_app_toasts(smoke: &Smoke, driver: &mut Driver) {
        driver.ok("action", &json!({"name": "open_settings"}));
        driver.wait(
            &json!({"state": "overlay", "equals": "settings"}),
            "Settings open",
        );
        driver.wait(
            &json!({"element": "settings-tab-notifications", "visible": true}),
            "the Notifications tab",
        );
        driver.ok("click", &json!({"element": "settings-tab-notifications"}));
        scroll_into_view(driver, TOAST_STYLE_APP);
        driver.ok("click", &json!({"element": TOAST_STYLE_APP}));
        scroll_into_view(driver, "settings-apply");
        driver.ok("click", &json!({"element": "settings-apply"}));
        let saved = smoke.daemon.config_dir().join("native-ui.json");
        let deadline = Instant::now() + super::CONNECT_TIMEOUT;
        loop {
            let ui: Value = std::fs::read_to_string(&saved)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default();
            if ui["notifications"]["toast_style"] == json!("app") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "hung: {} never saved the app's own toasts: {ui}",
                saved.display()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        driver.ok("key", &json!({"keys": "escape"}));
        driver.wait(
            &json!({"state": "overlay", "exists": false}),
            "Settings closed",
        );
    }

    #[test]
    #[ignore = "live: run via .\\rt.ps1 native-drive-check"]
    fn live_drive_app_toasts_show_on_top_without_focus() {
        let before = DesktopInput::read();
        assert!(
            !before.foreground.is_invalid(),
            "no foreground window to compare against; run this on a logged-on desktop \
             (rt.ps1 native-drive-check routes there)"
        );
        let (smoke, mut driver) =
            open_driven_with("live-drive-toasts", launch_driven_client_with_toasts);
        assert_unmoved(&before, smoke.hwnd, "launch");
        choose_app_toasts(&smoke, &mut driver);
        assert_unmoved(&before, smoke.hwnd, "the Settings choice");

        let session = seed_fake_agent(&smoke.daemon);
        let mut ws = connect(&smoke.daemon, "rt-smoke-agent");
        wait_scrollback(&mut ws, &session, "idle prompt box", |line| {
            line.starts_with('\u{276f}')
        });
        type_into(&mut ws, &session, b"please rt-perm\r");
        wait_scrollback(&mut ws, &session, "permission dialog", |line| {
            line.contains(PERMISSION_QUESTION)
        });

        let toast = find_toast(&smoke);
        assert_toast_on_top(toast);
        assert_unmoved(&before, smoke.hwnd, "the app toast");

        type_into(&mut ws, &session, b"1");
        let _ = ws.close(None);
        let _ = ws.flush();
    }
}
