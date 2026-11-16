//! Follow tab specs: a follow tab tails its file through the session's
//! host daemon, shows a refusal, shares one follow with a Chat Tail box on
//! the same session and path, asks again after a reconnect, and asks
//! nothing of a host that cannot follow files.

use crate::support;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use gpui::TestAppContext;
use protocol::{ClientMessage, DaemonMessage, FEATURE_AGENT_COMMANDS, FEATURE_FILE_TAIL, TabEntry};
use rustling_tulip_native::target::{HostKey, Target};
use rustling_tulip_native::{Connection, NetEvent, ToastKind};
use serde_json::json;
use support::{Fixture, Harness, TestDir, session};

/// The file session `s1` writes.
const LOG: &str = "C:/work/build.log";

/// A follow tab `id` of [`LOG`] in session `s1` of This PC.
fn follow_tab(id: &str) -> TabEntry {
    let tab = serde_json::from_value(json!({
        "id": id,
        "name": "build.log",
        "content": {
            "kind": "follow",
            "source": { "kind": "file", "session_id": "s1", "path": LOG },
        },
        "created_at": "2026-01-01T00:00:00Z",
    }));
    tab.unwrap_or_else(|err| unreachable!("follow tab fixture: {err}"))
}

/// Session `s1` in pane `p1` of tab `t1`, and `tabs` after it.
fn with_tabs(tabs: &[TabEntry]) -> Fixture {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs.extend(tabs.iter().cloned());
    fixture
}

/// Reports the connection as a daemon that follows files.
fn file_tail_daemon(h: &mut Harness<'_>) {
    let mut conn = Connection::for_target(Target::Local);
    conn.on_connecting();
    conn.on_socket_open(4242);
    let features = vec![
        FEATURE_AGENT_COMMANDS.to_owned(),
        FEATURE_FILE_TAIL.to_owned(),
    ];
    conn.on_welcome(support::PROTOCOL, features, None);
    h.event(NetEvent::State(conn));
}

/// `s1` attached over a daemon that follows files, everything sent so far
/// drained.
fn tail_daemon<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let mut h = Harness::with(cx, dir, &with_tabs(&[]));
    h.answer_scrollback("s1", b"");
    file_tail_daemon(&mut h);
    h.sent();
    h
}

/// The request ids of the `TailFile`s for [`LOG`] among `sent`.
fn tail_requests(sent: &[ClientMessage]) -> Vec<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::TailFile {
                session_id,
                path,
                request_id,
            } if session_id == "s1" && path == LOG => Some(request_id.clone().unwrap_or_default()),
            _ => None,
        })
        .collect()
}

fn untails(sent: &[ClientMessage]) -> usize {
    sent.iter()
        .filter(|msg| {
            matches!(msg, ClientMessage::UntailFile { session_id, path }
                if session_id == "s1" && path == LOG)
        })
        .count()
}

/// Follow tab `f1` arrives: the request id of the one `TailFile` it sends.
fn tab_arrives(h: &mut Harness<'_>) -> String {
    h.send(DaemonMessage::TabUpdated {
        tab: follow_tab("f1"),
    });
    let sent = h.sent();
    let requests = tail_requests(&sent);
    assert_eq!(requests.len(), 1, "one TailFile: {sent:?}");
    requests.into_iter().next().unwrap_or_default()
}

fn file_tail(data: &[u8], reset: bool) -> DaemonMessage {
    DaemonMessage::FileTail {
        session_id: "s1".to_owned(),
        path: LOG.to_owned(),
        data_b64: B64.encode(data),
        offset: 0,
        reset,
    }
}

fn lines(h: &mut Harness<'_>) -> Option<Vec<String>> {
    h.root(|root, _| root.follow_tab_lines("f1"))
}

fn note(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.follow_tab_note("f1"))
}

fn strings(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| (*line).to_owned()).collect()
}

/// Whether an element tagged `selector` was painted.
fn drawn(h: &mut Harness<'_>, selector: &'static str) -> bool {
    h.cx.run_until_parked();
    h.cx.debug_bounds(selector).is_some()
}

#[gpui::test]
fn a_follow_tab_tails_its_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_daemon(cx, &dir);
    let request = tab_arrives(&mut h);
    assert!(!request.is_empty(), "the TailFile carries a request id");
    h.click_on("tab-f1");
    assert!(drawn(&mut h, "follow-tab-f1"), "the body is drawn");
    assert_eq!(
        note(&mut h).as_deref(),
        Some("Waiting for build.log\u{2026}")
    );
    h.send(file_tail(b"\x1b[32mone\x1b[0m\r\ntwo\n", true));
    h.send(file_tail(b"three\n", false));
    assert_eq!(lines(&mut h), Some(strings(&["one", "two", "three"])));
    assert_eq!(note(&mut h), None);
    assert!(drawn(&mut h, "follow-tab-lines-f1"), "the lines are drawn");
    assert_eq!(
        h.root(|root, _| root.follow_tab_following("f1")),
        Some(true)
    );
    h.send(file_tail(b"fresh\n", true));
    assert_eq!(
        lines(&mut h),
        Some(strings(&["fresh"])),
        "a reset replaces them"
    );
    assert!(tail_requests(&h.sent()).is_empty(), "nothing more is asked");
}

#[gpui::test]
fn a_follow_tab_shows_a_refusal(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_daemon(cx, &dir);
    let request = tab_arrives(&mut h);
    let message = "refusing to follow \"C:/work/build.log\": outside the session's folders";
    h.send(DaemonMessage::Error {
        message: message.to_owned(),
        code: Some("invalid_request".to_owned()),
        request_id: Some(request),
    });
    h.click_on("tab-f1");
    assert_eq!(note(&mut h).as_deref(), Some(message));
    assert_eq!(lines(&mut h), None);
    assert!(
        drawn(&mut h, "follow-tab-refused-f1"),
        "the refusal is drawn"
    );
    h.send(file_tail(b"x\n", true));
    assert_eq!(
        note(&mut h).as_deref(),
        Some(message),
        "a refused tab takes no data"
    );
    assert!(
        tail_requests(&h.sent()).is_empty(),
        "nothing is asked again"
    );
}

#[gpui::test]
fn a_follow_tab_and_a_chat_tail_box_share_one_follow(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_daemon(cx, &dir);
    tab_arrives(&mut h);
    h.send(file_tail(b"one\n", true));
    h.root_update(|root, _, cx| root.toggle_chat_tail("s1", "t1", 0, LOG, cx));
    let sent = h.sent();
    assert!(
        tail_requests(&sent).is_empty(),
        "the box reuses the tab's follow: {sent:?}"
    );
    assert_eq!(
        h.root(|root, _| root.chat_tail_lines("s1", LOG)),
        Some(strings(&["one"])),
        "the box shows the lines so far"
    );
    h.send(file_tail(b"two\n", false));
    assert_eq!(
        h.root(|root, _| root.chat_tail_lines("s1", LOG)),
        Some(strings(&["one", "two"]))
    );
    assert_eq!(lines(&mut h), Some(strings(&["one", "two"])));

    h.root_update(|root, _, cx| root.toggle_chat_tail("s1", "t1", 0, LOG, cx));
    let sent = h.sent();
    assert_eq!(
        untails(&sent),
        0,
        "the tab still reads the follow: {sent:?}"
    );
    assert_eq!(
        h.root(|root, _| root.chat_tail_lines("s1", LOG)),
        None,
        "the box closed"
    );
    h.send(file_tail(b"three\n", false));
    assert_eq!(lines(&mut h), Some(strings(&["one", "two", "three"])));

    h.send(DaemonMessage::TabRemoved {
        tab_id: "f1".to_owned(),
    });
    let sent = h.sent();
    assert_eq!(
        untails(&sent),
        1,
        "the last reader stops the follow: {sent:?}"
    );
}

#[gpui::test]
fn a_follow_tab_asks_again_after_a_reconnect(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_daemon(cx, &dir);
    let first = tab_arrives(&mut h);
    h.send(file_tail(b"before\n", true));
    h.send_welcome(support::PROTOCOL);
    h.load(&with_tabs(&[follow_tab("f1")]));
    let sent = h.sent();
    let again = tail_requests(&sent);
    assert_eq!(again.len(), 1, "the tab asks again: {sent:?}");
    assert!(
        again.iter().all(|id| *id != first),
        "under a fresh request id"
    );
    assert_eq!(untails(&sent), 0, "a new connection untails nothing");
    h.send(file_tail(b"late from the old follow\n", false));
    assert_eq!(lines(&mut h), Some(strings(&["before"])));
    h.send(file_tail(b"after\n", true));
    assert_eq!(lines(&mut h), Some(strings(&["after"])));
}

#[gpui::test]
fn a_follow_tab_on_a_host_without_file_tail_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_tabs(&[]));
    h.answer_scrollback("s1", b"");
    h.sent();
    h.send(DaemonMessage::TabUpdated {
        tab: follow_tab("f1"),
    });
    let sent = h.sent();
    assert!(
        tail_requests(&sent).is_empty(),
        "nothing is asked: {sent:?}"
    );
    h.click_on("tab-f1");
    assert!(drawn(&mut h, "follow-tab-f1"));
    assert_eq!(
        note(&mut h).as_deref(),
        Some("This host can't follow files.")
    );
}

/// The refusal [`refuse`] sends.
const REFUSED: &str = "refusing to follow: five follows already run on this connection";

/// The daemon refuses the `TailFile` sent under `request`.
fn refuse(h: &mut Harness<'_>, request: String) {
    h.send(DaemonMessage::Error {
        message: REFUSED.to_owned(),
        code: Some("limit_reached".to_owned()),
        request_id: Some(request),
    });
    assert_eq!(note(h).as_deref(), Some(REFUSED));
}

#[gpui::test]
fn a_refused_follow_tab_asks_again_after_a_reconnect(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_daemon(cx, &dir);
    let first = tab_arrives(&mut h);
    refuse(&mut h, first.clone());
    h.send_welcome(support::PROTOCOL);
    h.load(&with_tabs(&[follow_tab("f1")]));
    let sent = h.sent();
    let again = tail_requests(&sent);
    assert_eq!(again.len(), 1, "the refused tab asks again: {sent:?}");
    assert!(
        again.iter().all(|id| *id != first),
        "under a fresh request id"
    );
    assert_eq!(
        note(&mut h).as_deref(),
        Some("Waiting for build.log\u{2026}"),
        "the refusal is cleared"
    );
    h.send(file_tail(b"back\n", true));
    assert_eq!(lines(&mut h), Some(strings(&["back"])));
}

#[gpui::test]
fn reopening_a_tail_box_on_a_refused_file_asks_again(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_daemon(cx, &dir);
    let first = tab_arrives(&mut h);
    refuse(&mut h, first);
    h.root_update(|root, _, cx| root.toggle_chat_tail("s1", "t1", 0, LOG, cx));
    let sent = h.sent();
    assert_eq!(
        tail_requests(&sent).len(),
        1,
        "the box asks again: {sent:?}"
    );
    assert_eq!(
        h.root(|root, _| root.chat_tail_lines("s1", LOG)),
        Some(strings(&["No output yet"]))
    );
    assert_eq!(
        note(&mut h).as_deref(),
        Some("Waiting for build.log\u{2026}"),
        "the tab's refusal is cleared"
    );
}

#[gpui::test]
fn a_refused_follow_open_raises_a_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = tail_daemon(cx, &dir);
    h.root_update(|root, _, _| root.open_follow_tab(&HostKey::Local, "s1", LOG));
    let sent = h.sent();
    let id = sent
        .iter()
        .find_map(|msg| match msg {
            ClientMessage::OpenFollowTab { id, .. } => Some(id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| unreachable!("an OpenFollowTab: {sent:?}"));
    let message = "open follow tab failed: the path is empty";
    h.send(DaemonMessage::Error {
        message: message.to_owned(),
        code: None,
        request_id: Some(id),
    });
    let toasts: Vec<(ToastKind, String, Option<String>)> = h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.kind, toast.title.clone(), toast.detail.clone()))
            .collect()
    });
    assert_eq!(
        toasts,
        [(
            ToastKind::Warning,
            "Couldn't open the follow tab".to_owned(),
            Some(message.to_owned())
        )]
    );
}
