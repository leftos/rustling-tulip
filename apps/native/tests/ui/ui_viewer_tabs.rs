//! Viewer tab specs: the pill, the placeholder body on a remote host, an
//! open the daemon answers in either order, a refused open, the merge menu,
//! undoing the close of a file that is open again, and a target switch.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use gpui::{Modifiers, TestAppContext};
use protocol::{ClientMessage, DaemonMessage, TabEntry};
use rustling_tulip_native::target::Target;
use rustling_tulip_native::{
    Connection, ToastKind, VIEWER_OPEN_FAILED_TITLE, ViewerLoad, ViewerOrigin,
};
use serde_json::json;
use support::{Fixture, Harness, TestDir, pane, session, tab};

const HOST: &str = "box.tail1234.ts.net";
const PATH: &str = r"C:\notes\a.md";
const REMOTE_NOTE: &str = "Not available on a remote host: this opens files on this PC";

/// A viewer tab of `path`, which the daemon named `name`.
fn viewer_tab_named(id: &str, path: &str, name: &str) -> TabEntry {
    serde_json::from_value(json!({
        "id": id,
        "name": name,
        "content": { "kind": "viewer", "path": path },
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .expect("viewer tab fixture")
}

fn viewer_tab(id: &str, path: &str) -> TabEntry {
    viewer_tab_named(id, path, "a.md")
}

/// Session `s1` in pane `p1` of tab `t1`, and `tabs` after it.
fn with_tabs(tabs: &[TabEntry]) -> Fixture {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.tabs.extend(tabs.iter().cloned());
    fixture
}

fn updated(tab: &TabEntry) -> DaemonMessage {
    DaemonMessage::TabUpdated { tab: tab.clone() }
}

fn opened(id: &str, tab_id: &str) -> DaemonMessage {
    DaemonMessage::ViewerTabOpened {
        id: id.to_owned(),
        tab_id: tab_id.to_owned(),
    }
}

/// The one `OpenViewerTab` the client sent: (id, path).
fn one_open(h: &mut Harness<'_>) -> (String, String) {
    let mut opens: Vec<(String, String)> = h
        .sent()
        .into_iter()
        .filter_map(|msg| match msg {
            ClientMessage::OpenViewerTab { id, path } => Some((id, path)),
            _ => None,
        })
        .collect();
    assert_eq!(opens.len(), 1, "one open: {opens:?}");
    opens.remove(0)
}

fn open_viewer(h: &mut Harness<'_>, path: &str, line: Option<u32>) -> (String, String) {
    let path = path.to_owned();
    h.root_update(move |root, _, _| root.open_viewer(&path, line));
    one_open(h)
}

fn active(h: &mut Harness<'_>) -> Option<String> {
    h.root(|root, _| root.active_tab_id().map(str::to_owned))
}

fn focused(h: &mut Harness<'_>, tab_id: &str) -> bool {
    let root = h.root.clone();
    h.cx.update(|window, cx| root.read(cx).viewer_tab_focused(tab_id, window))
}

/// The viewer tab's origin, load state, note and line target.
type Shown = (ViewerOrigin, ViewerLoad, Option<&'static str>, Option<u32>);

fn shown(h: &mut Harness<'_>, tab_id: &str) -> Option<Shown> {
    h.root(|root, _| {
        root.viewer_tab(tab_id)
            .map(|tab| (tab.origin(), tab.load(), tab.note(), tab.line_target()))
    })
}

/// Whether an element tagged `selector` was painted. A spec's window paints
/// a viewer tab's body once, so bounds kept from an earlier frame cannot
/// mislead here.
fn drawn(h: &mut Harness<'_>, selector: &'static str) -> bool {
    h.cx.run_until_parked();
    h.cx.debug_bounds(selector).is_some()
}

fn ctrl_click(h: &mut Harness<'_>, selector: &str) {
    let at = h.center(selector);
    h.click(at, Modifiers::control());
}

/// An open connection to the tailnet host [`HOST`].
fn remote() -> Connection {
    let mut conn = Connection::for_target(Target::tailnet(HOST, None));
    conn.on_connecting();
    conn.on_socket_open(8787);
    conn.on_welcome(support::PROTOCOL, support::daemon_features());
    conn
}

#[gpui::test]
fn a_viewer_tab_shows_its_file_name_on_the_pill(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = with_tabs(&[viewer_tab_named("v1", PATH, "renamed")]);
    let mut h = Harness::with(cx, &dir, &fixture);
    assert_eq!(
        h.root(|root, _| root.tab_pill_label("v1")).as_deref(),
        Some("a.md"),
        "the file name, with no glyph"
    );
    assert_eq!(
        h.root(|root, _| root.tab_pill_label("t1")).as_deref(),
        Some("t1")
    );

    h.click_on("tab-v1");
    assert!(
        drawn(&mut h, "viewer-placeholder-v1"),
        "the body names the file"
    );
    assert!(
        !drawn(&mut h, "viewer-note-v1"),
        "a file of this PC has no note"
    );
    assert_eq!(
        shown(&mut h, "v1"),
        Some((ViewerOrigin::Link, ViewerLoad::Unread, None, None))
    );
}

#[gpui::test]
fn a_restored_viewer_tab_on_a_remote_host_reads_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let fixture = Fixture {
        tabs: vec![tab("ht1", &pane("hp1", None)), viewer_tab("v1", PATH)],
        ..Fixture::default()
    };
    let mut h = Harness::on_target(cx, &dir, Target::tailnet(HOST, None), &fixture);
    h.click_on("tab-v1");
    assert_eq!(active(&mut h).as_deref(), Some("v1"));
    assert_eq!(
        shown(&mut h, "v1"),
        Some((
            ViewerOrigin::NotAvailable,
            ViewerLoad::NotAvailable,
            Some(REMOTE_NOTE),
            None
        ))
    );
    assert!(drawn(&mut h, "viewer-placeholder-v1"));
    assert!(
        drawn(&mut h, "viewer-note-v1"),
        "the remote-host note shows"
    );
}

#[gpui::test]
fn viewer_tab_opened_activates_and_focuses_the_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_tabs(&[]));
    h.sent();
    let (id, path) = open_viewer(&mut h, "  C:/notes/a.md \n", Some(7));
    assert_eq!(id, "viewer-open-1");
    assert_eq!(path, PATH, "trimmed, absolute, with this OS's separators");

    h.send(opened(&id, "v1"));
    assert_eq!(
        active(&mut h).as_deref(),
        Some("t1"),
        "v1 is not listed yet"
    );
    h.send(updated(&viewer_tab("v1", PATH)));
    assert_eq!(active(&mut h).as_deref(), Some("v1"));
    assert!(focused(&mut h, "v1"), "the viewer tab takes the keyboard");
    assert_eq!(shown(&mut h, "v1").and_then(|shown| shown.3), Some(7));

    h.click_on("tab-t1");
    assert!(!focused(&mut h, "v1"));
    let (id, _) = open_viewer(&mut h, PATH, Some(30));
    h.send(opened(&id, "v1"));
    assert_eq!(
        active(&mut h).as_deref(),
        Some("v1"),
        "an open of a listed tab"
    );
    assert!(focused(&mut h, "v1"));
    assert_eq!(
        shown(&mut h, "v1").and_then(|shown| shown.3),
        Some(30),
        "the second open moves the line"
    );
}

#[gpui::test]
fn an_open_error_ends_the_wait_and_toasts(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_tabs(&[]));
    h.sent();
    let (id, _) = open_viewer(&mut h, PATH, None);
    h.send(DaemonMessage::Error {
        message: "open viewer tab failed: the path is empty".to_owned(),
        request_id: Some(id.clone()),
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
            ToastKind::Error,
            VIEWER_OPEN_FAILED_TITLE.to_owned(),
            Some("open viewer tab failed: the path is empty".to_owned())
        )]
    );
    h.send(updated(&viewer_tab("v1", PATH)));
    h.send(opened(&id, "v1"));
    assert_eq!(active(&mut h).as_deref(), Some("t1"), "the id is forgotten");
}

#[gpui::test]
fn a_viewer_tab_is_not_offered_for_merge(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_tabs(&[viewer_tab("v1", PATH)]));
    h.sent();
    ctrl_click(&mut h, "tab-t1");
    ctrl_click(&mut h, "tab-v1");

    h.right_click_on("tab-t1");
    assert!(
        h.root(|root, _| root.tab_menu_text())
            .contains(&"Viewer tabs can't be merged".to_owned())
    );
    h.click_on("tab-menu-merge-horizontal");
    assert!(
        !h.sent()
            .iter()
            .any(|msg| matches!(msg, ClientMessage::MergeTabs { .. })),
        "the rows are inert"
    );
    assert!(h.in_model("tab-menu"), "and the menu stays");
}

#[gpui::test]
fn undo_of_a_closed_viewer_tab_whose_path_is_open_focuses_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_tabs(&[viewer_tab("v1", PATH)]));
    h.click_on("tab-v1");
    h.sent();
    h.click_on("tab-close-v1");
    let sent = h.sent();
    assert!(
        matches!(sent.as_slice(), [ClientMessage::CloseTab { tab_id }] if tab_id == "v1"),
        "sent {sent:?}"
    );
    let id = h
        .root(|root, _| root.undo_entries().first().map(|entry| entry.id))
        .expect("an undo entry");

    h.send(DaemonMessage::TabRemoved {
        tab_id: "v1".to_owned(),
    });
    h.send(updated(&viewer_tab("v2", PATH)));
    h.sent();
    assert_eq!(active(&mut h).as_deref(), Some("t1"));

    h.click_on(&format!("undo-action-{id}"));
    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::RestoreTab { .. } | ClientMessage::RestoreTabSnapshot { .. }
        )),
        "no second tab of the same file: sent {sent:?}"
    );
    assert_eq!(active(&mut h).as_deref(), Some("v2"), "the open one shows");
    assert!(focused(&mut h, "v2"));
}

#[gpui::test]
fn undo_of_a_background_viewer_close_keeps_the_shown_tab(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_tabs(&[viewer_tab("v1", PATH)]));
    assert_eq!(active(&mut h).as_deref(), Some("t1"));
    h.sent();
    h.click_on("tab-close-v1");
    let id = h
        .root(|root, _| root.undo_entries().first().map(|entry| entry.id))
        .expect("an undo entry");
    h.send(DaemonMessage::TabRemoved {
        tab_id: "v1".to_owned(),
    });
    h.send(updated(&viewer_tab("v2", PATH)));
    h.sent();

    h.click_on(&format!("undo-action-{id}"));
    let sent = h.sent();
    assert!(
        !sent.iter().any(|msg| matches!(
            msg,
            ClientMessage::RestoreTab { .. } | ClientMessage::RestoreTabSnapshot { .. }
        )),
        "the file is open already: sent {sent:?}"
    );
    assert_eq!(
        active(&mut h).as_deref(),
        Some("t1"),
        "the closed tab was not the one shown, so the shown tab stays"
    );
}

#[gpui::test]
fn a_viewer_tab_offers_no_rename(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_tabs(&[viewer_tab("v1", PATH)]));
    h.right_click_on("tab-t1");
    assert!(h.in_model("tab-menu-rename"), "a terminal tab renames");
    h.keys("escape");
    h.right_click_on("tab-v1");
    assert!(h.in_model("tab-menu"));
    assert!(
        !h.in_model("tab-menu-rename"),
        "a viewer tab's pill shows its file name, so a rename would show nothing"
    );
}

#[gpui::test]
fn target_switch_clears_viewer_tabs_without_sending(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = Harness::with(cx, &dir, &with_tabs(&[viewer_tab("v1", PATH)]));
    let (open_id, _) = open_viewer(&mut h, r"C:\notes\b.md", None);
    assert!(shown(&mut h, "v1").is_some());

    h.set_connection(remote());
    let sent = h.sent();
    assert!(sent.is_empty(), "the switch sends nothing: {sent:?}");
    assert_eq!(
        shown(&mut h, "v1"),
        None,
        "the local daemon's viewer tab is gone"
    );

    h.load(&Fixture {
        tabs: vec![
            tab("ht1", &pane("hp1", None)),
            viewer_tab_named("v2", r"C:\notes\b.md", "b.md"),
        ],
        ..Fixture::default()
    });
    h.send(opened(&open_id, "v2"));
    assert_eq!(
        active(&mut h).as_deref(),
        Some("ht1"),
        "the old daemon's open activates nothing on the host"
    );
    assert_eq!(
        shown(&mut h, "v2").map(|shown| shown.0),
        Some(ViewerOrigin::NotAvailable)
    );
}
