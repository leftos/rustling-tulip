//! OS smoke specs: the real `rustling-tulip-native` binary in a real window,
//! against an isolated daemon. The window opens cloaked and is never
//! activated (`RUSTLING_TULIP_OFFSCREEN_WINDOW`), so a run never covers the
//! user's work or takes their focus. The specs check that the window
//! connects, and that input posted to it as window messages reaches the
//! shell, read back from the daemon's scrollback. Ignored by default; run by
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
use protocol::ClientMessage;
use rustling_tulip_native::{NATIVE_PROTOCOL_VERSIONS, SIDEBAR_DEFAULT_WIDTH};
use serde_json::Value;
use support::live::{LiveDaemon, kill_tree, spawn_shell};
use tokio_tungstenite::tungstenite::stream::MaybeTlsStream;
use tokio_tungstenite::tungstenite::{self, Message, WebSocket};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MAPVK_VK_TO_VSC, MapVirtualKeyW, VIRTUAL_KEY, VK_OEM_MINUS, VK_OEM_PERIOD, VK_RETURN, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetForegroundWindow, GetWindowThreadProcessId, IsWindowVisible,
    PostMessageW, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
};
use windows::core::BOOL;

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// Makes the client open its window cloaked and never activate it.
const OFFSCREEN_ENV: &str = "RUSTLING_TULIP_OFFSCREEN_WINDOW";
/// `wParam` of a mouse message while the left button is down.
const MK_LBUTTON: usize = 0x0001;
/// The footer's height in logical pixels.
const FOOTER_HEIGHT: f32 = 22.0;
/// What the client logs once its handshake with the daemon succeeds
/// (`apps/native/src/net.rs`).
const CONNECTED_LINE: &str = "connected to daemon";
/// The line the shell prints for the typed command.
const MARKER: &str = "rt-smoke-marker";
const WINDOW_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const KEYS_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the test's socket waits for one reply among the broadcasts.
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long one typed command gets to show up before it is typed again.
const ECHO_WAIT: Duration = Duration::from_secs(5);

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

/// A short-lived socket of the test's own on `daemon`, past its handshake:
/// the client binary is a separate process with no sender to borrow.
fn connect(daemon: &LiveDaemon, client_id: &str) -> Socket {
    let handshake = daemon.handshake();
    let url = format!("ws://127.0.0.1:{}/ws", handshake.port);
    let (mut ws, _) = tungstenite::connect(url).expect("open a socket to the daemon");
    if let MaybeTlsStream::Plain(stream) = ws.get_ref() {
        TcpStream::set_read_timeout(stream, Some(Duration::from_secs(10)))
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

/// The middle of the window's pane area, inset from the sidebar, tab bar and
/// footer, in client pixels.
fn pane_center(hwnd: HWND) -> (i32, i32) {
    let scale = scale(hwnd);
    let (width, height) = client_size(hwnd);
    let left = physical(SIDEBAR_DEFAULT_WIDTH + 24.0, scale);
    let top = physical(40.0, scale);
    let right = width - physical(8.0, scale);
    let bottom = height - physical(FOOTER_HEIGHT + 8.0, scale);
    (i32::midpoint(left, right), i32::midpoint(top, bottom))
}

/// The real client window on an isolated daemon with one plain shell.
struct Smoke {
    hwnd: HWND,
    session: String,
    _client: Client,
    daemon: LiveDaemon,
}

fn open(test: &str) -> Smoke {
    // SAFETY: sets this process's DPI mode, so window sizes read in physical
    // pixels; a second call in the same process fails harmlessly because the
    // mode is already set.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let daemon = LiveDaemon::start(test);
    let session = seed_shell(&daemon);
    let client = launch_client(&daemon);
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
fn type_line(hwnd: HWND, text: &str) {
    for c in text.chars() {
        assert!(
            c == ' ' || c == '.' || c == '-' || c.is_ascii_alphanumeric(),
            "type_line types letters, digits, spaces, dots and hyphens only, not {c:?}"
        );
        let vk = match c {
            ' ' => VK_SPACE,
            '.' => VK_OEM_PERIOD,
            '-' => VK_OEM_MINUS,
            c => {
                let ascii = u8::try_from(c.to_ascii_uppercase()).expect("an ASCII character");
                VIRTUAL_KEY(u16::from(ascii))
            }
        };
        press(hwnd, vk);
    }
    press(hwnd, VK_RETURN);
}

#[test]
#[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
fn smoke_window_opens_cloaked_unfocused_and_connects() {
    let smoke = open("smoke-connect");
    wait_connected(&smoke);
    assert_out_of_the_way(smoke.hwnd);
}

#[test]
#[ignore = "smoke: run via .\\rt.ps1 native-smoke"]
fn smoke_posted_keys_reach_the_shell() {
    let smoke = open("smoke-keys");
    let hwnd = smoke.hwnd;
    wait_connected(&smoke);
    let mut shell = connect(&smoke.daemon, "rt-smoke-reader");
    let deadline = Instant::now() + KEYS_TIMEOUT;
    // Typed again until it lands: keys posted before the pane has attached
    // its session are dropped.
    loop {
        click_at(hwnd, pane_center(hwnd));
        type_line(hwnd, &format!("echo {MARKER}"));
        if wait_for_line(&mut shell, &smoke.session, MARKER, ECHO_WAIT) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no {MARKER:?} line in the shell's scrollback within {KEYS_TIMEOUT:?}; the posted \
             keys did not reach the shell"
        );
    }
    assert_out_of_the_way(hwnd);
}
