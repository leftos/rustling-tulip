//! Pasting an image into a remote agent session: the upload to its host, the
//! returned path pasted back into the pane that pasted, and the toasts when
//! the host cannot take it.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use crate::support;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use gpui::{ImageFormat, Modifiers, TestAppContext};
use protocol::{
    ClientMessage, DaemonMessage, FEATURE_UPLOAD_FILE, ImagePaste, SessionSnapshot, SplitDirection,
    UPLOAD_MAX_BYTES,
};
use support::{Fixture, Harness, TestDir, pane, remote_pane, session, split, tab};

/// The title of the toast an upload that fails shows.
const FAILED_TITLE: &str = "Couldn't paste the image";

/// One `UploadFile` the client sent, its bytes decoded.
#[derive(Debug)]
struct Upload {
    request_id: String,
    session_id: String,
    name: String,
    bytes: Vec<u8>,
}

/// The remote agent session `b1`, its agent up.
fn agent_session() -> (SessionSnapshot, protocol::AgentDescriptor) {
    let descriptor = agent_api::test_agent::DESCRIPTOR.clone();
    let s = session("b1")
        .agent("test-agent")
        .program_name(&descriptor.tag)
        .build();
    (s, descriptor)
}

/// [`support::remote_in_pane_with_features`] with `b1`, an agent session
/// whose agent is up, in the focused pane `p2`, its host listing `features`.
fn remote_agent<'a>(cx: &'a mut TestAppContext, dir: &TestDir, features: &[&str]) -> Harness<'a> {
    let (s, descriptor) = agent_session();
    let mut h = support::remote_in_pane_with_features(
        cx,
        dir,
        &serde_json::json!({}),
        &[],
        s.clone(),
        features,
    );
    let mut host = h.host(&support::saved_remote());
    host.send(DaemonMessage::Agents {
        agents: vec![descriptor],
    });
    host.send(DaemonMessage::SessionUpdated {
        session: s,
        request_id: None,
    });
    host.sent();
    h
}

/// The uploads among `sent`.
fn uploads_in(sent: &[ClientMessage]) -> Vec<Upload> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::UploadFile {
                request_id,
                session_id,
                name,
                data_b64,
            } => Some(Upload {
                request_id: request_id.clone(),
                session_id: session_id.clone(),
                name: name.clone(),
                bytes: B64.decode(data_b64).expect("upload base64"),
            }),
            _ => None,
        })
        .collect()
}

/// Whether `sent` holds any input.
fn has_input(sent: &[ClientMessage]) -> bool {
    sent.iter()
        .any(|msg| matches!(msg, ClientMessage::SendInput { .. }))
}

/// The one upload the remote host was sent since the last read.
fn the_upload(h: &mut Harness<'_>) -> Upload {
    let sent = h.host(&support::saved_remote()).sent();
    let [upload]: [Upload; 1] = uploads_in(&sent).try_into().expect("exactly one upload");
    upload
}

/// The host answers `upload` with `path`.
fn answer(h: &mut Harness<'_>, upload: &Upload, path: &str) {
    h.host(&support::saved_remote())
        .send(DaemonMessage::FileUploaded {
            request_id: upload.request_id.clone(),
            session_id: upload.session_id.clone(),
            path: path.to_owned(),
        });
}

/// Every toast's title and detail, oldest first.
fn toasts(h: &mut Harness<'_>) -> Vec<(String, Option<String>)> {
    h.root(|root, _| {
        root.toasts()
            .iter()
            .map(|toast| (toast.title.clone(), toast.detail.clone()))
            .collect()
    })
}

#[gpui::test]
fn an_image_paste_into_a_remote_agent_uploads_it_and_pastes_the_returned_path(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
    h.set_clipboard_image(ImageFormat::Png, png.clone());
    h.keys("ctrl-v");
    let upload = the_upload(&mut h);
    assert_eq!(upload.session_id, "b1");
    assert_eq!(upload.name, "paste.png");
    assert_eq!(upload.bytes, png, "the clipboard's own bytes go up");

    answer(&mut h, &upload, "/work/.tmp/rt-uploads/paste-1.png");
    assert_eq!(
        h.host(&support::saved_remote()).sent_input("b1"),
        b"/work/.tmp/rt-uploads/paste-1.png",
        "the host path is pasted into the pane"
    );

    h.host(&support::saved_remote()).pty("b1", b"\x1b[?2004h");
    h.keys("ctrl-v");
    let upload = the_upload(&mut h);
    answer(&mut h, &upload, "/work/.tmp/rt-uploads/paste-2.png");
    assert_eq!(
        h.host(&support::saved_remote()).sent_input("b1"),
        b"\x1b[200~/work/.tmp/rt-uploads/paste-2.png\x1b[201~",
        "the path pastes bracketed when the program asked"
    );
}

#[gpui::test]
fn a_returned_path_with_a_space_is_pasted_in_double_quotes(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.set_clipboard_image(ImageFormat::Png, vec![1, 2, 3]);
    h.keys("ctrl-v");
    let upload = the_upload(&mut h);
    answer(
        &mut h,
        &upload,
        r"C:\Users\Some One\.tmp\rt-uploads\paste.png",
    );
    assert_eq!(
        h.host(&support::saved_remote()).sent_input("b1"),
        br#""C:\Users\Some One\.tmp\rt-uploads\paste.png""#,
    );
}

#[gpui::test]
fn the_path_is_pasted_into_the_pane_that_pasted_the_image(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.set_clipboard_image(ImageFormat::Png, vec![1, 2, 3]);
    h.keys("ctrl-v");
    let upload = the_upload(&mut h);
    // Focus moves to This PC's pane before the host answers.
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    answer(&mut h, &upload, "/work/one.png");
    assert_eq!(
        h.host(&support::saved_remote()).sent_input("b1"),
        b"/work/one.png",
        "the pane that pasted gets the path"
    );
    assert!(
        h.sent_input("a1").is_empty(),
        "the focused pane gets nothing"
    );

    // A second paste, then the pane is pointed at another session before
    // the answer comes.
    let at = h.cell_center("p2", 0, 0);
    h.click(at, Modifiers::none());
    h.keys("ctrl-v");
    let upload = the_upload(&mut h);
    let (b1, _) = agent_session();
    h.host(&support::saved_remote())
        .send(DaemonMessage::Sessions {
            sessions: vec![b1, session("b2").build()],
        });
    h.send(DaemonMessage::Tabs {
        tabs: vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                pane("p1", Some("a1")),
                remote_pane("p2", "b2"),
            ),
        )],
    });
    h.host(&support::saved_remote())
        .answer_scrollback("b2", b"");
    assert_eq!(
        h.root(|root, _| root.pane_session("p2")).as_deref(),
        Some("b2"),
        "the pane now shows b2"
    );
    answer(&mut h, &upload, "/work/two.png");
    let sent = h.host(&support::saved_remote()).sent();
    assert!(
        !has_input(&sent),
        "a re-pointed pane gets no path: {sent:?}"
    );
}

#[gpui::test]
fn a_jpeg_on_the_clipboard_uploads_under_its_own_extension(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.set_clipboard_image(ImageFormat::Jpeg, vec![0xFF, 0xD8, 0xFF]);
    h.keys("ctrl-v");
    let upload = the_upload(&mut h);
    assert_eq!(upload.name, "paste.jpg");
    assert_eq!(upload.bytes, [0xFF, 0xD8, 0xFF]);
}

#[gpui::test]
fn a_host_without_upload_file_toasts_that_it_cannot_receive_images(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[]);
    h.set_clipboard_image(ImageFormat::Png, vec![1, 2, 3]);
    h.keys("ctrl-v");
    let sent = h.host(&support::saved_remote()).sent();
    assert!(
        uploads_in(&sent).is_empty(),
        "nothing is uploaded: {sent:?}"
    );
    assert!(!has_input(&sent), "nothing is typed: {sent:?}");
    assert!(
        toasts(&mut h).contains(&(
            "This host can't receive pasted images or files until it is updated".to_owned(),
            None
        )),
        "{:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn an_image_over_the_upload_cap_is_refused_with_a_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.set_clipboard_image(ImageFormat::Png, vec![7; UPLOAD_MAX_BYTES + 1]);
    h.keys("ctrl-v");
    let sent = h.host(&support::saved_remote()).sent();
    assert!(uploads_in(&sent).is_empty(), "nothing is uploaded");
    assert!(!has_input(&sent), "nothing is typed");
    assert!(
        toasts(&mut h).contains(&(
            FAILED_TITLE.to_owned(),
            Some("The image is larger than 8 MiB.".to_owned())
        )),
        "{:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn an_error_reply_to_an_upload_toasts_its_message_and_pastes_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.set_clipboard_image(ImageFormat::Png, vec![1, 2, 3]);
    h.keys("ctrl-v");
    let upload = the_upload(&mut h);
    let message = "the file is larger than the host takes";
    h.host(&support::saved_remote()).send(DaemonMessage::Error {
        message: message.to_owned(),
        request_id: Some(upload.request_id.clone()),
        code: Some("too_large".to_owned()),
    });
    let shown = toasts(&mut h);
    assert_eq!(
        shown
            .iter()
            .filter(|toast| **toast == (FAILED_TITLE.to_owned(), Some(message.to_owned())))
            .count(),
        1,
        "{shown:?}"
    );
    assert!(
        !shown.iter().any(|(title, _)| title == "Daemon error"),
        "no generic toast as well: {shown:?}"
    );
    // A late answer for the refused request pastes nothing either.
    answer(&mut h, &upload, "/work/late.png");
    assert!(
        h.host(&support::saved_remote()).sent_input("b1").is_empty(),
        "a refused upload's late answer pastes nothing"
    );
}

#[gpui::test]
fn a_remote_shell_with_an_image_still_gets_ctrl_v(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let shell = session("b1").shell("D:/x").build();
    let mut h = support::remote_in_pane_with_features(
        cx,
        &dir,
        &serde_json::json!({}),
        &[],
        shell,
        &[FEATURE_UPLOAD_FILE],
    );
    h.set_clipboard_image(ImageFormat::Png, vec![1, 2, 3]);
    h.keys("ctrl-v");
    let sent = h.host(&support::saved_remote()).sent();
    assert!(
        uploads_in(&sent).is_empty(),
        "nothing is uploaded: {sent:?}"
    );
    assert_eq!(
        support::input_to(&sent, "b1"),
        [0x16],
        "a remote shell gets ^V"
    );
}

#[gpui::test]
fn a_local_agent_with_an_image_still_gets_the_image_paste_key(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut descriptor = agent_api::test_agent::DESCRIPTOR.clone();
    descriptor.image_paste = ImagePaste::AltV;
    let s = session("img")
        .agent("test-agent")
        .program_name(&descriptor.tag)
        .build();
    let mut h = Harness::with(cx, &dir, &Fixture::single(s.clone()));
    h.send(DaemonMessage::Agents {
        agents: vec![descriptor],
    });
    h.send(DaemonMessage::SessionUpdated {
        session: s.clone(),
        request_id: None,
    });
    h.answer_scrollback(&s.id, b"");
    let at = h.cell_center("p1", 0, 0);
    h.click(at, Modifiers::none());
    h.sent();
    h.set_clipboard_image(ImageFormat::Png, vec![1, 2, 3]);
    h.keys("ctrl-v");
    let sent = h.sent();
    assert!(
        uploads_in(&sent).is_empty(),
        "nothing is uploaded: {sent:?}"
    );
    assert_eq!(
        support::input_to(&sent, &s.id),
        b"\x1bv",
        "a local agent gets its image-paste key"
    );
}

#[gpui::test]
fn an_upload_whose_host_reconnects_is_forgotten(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.set_clipboard_image(ImageFormat::Png, vec![1, 2, 3]);
    h.keys("ctrl-v");
    let upload = the_upload(&mut h);

    let (s, descriptor) = agent_session();
    let mut host = h.host(&support::saved_remote());
    host.connect_with_features(&[FEATURE_UPLOAD_FILE]);
    host.send(DaemonMessage::Agents {
        agents: vec![descriptor],
    });
    host.send(DaemonMessage::Sessions {
        sessions: vec![s.clone()],
    });
    host.answer_scrollback("b1", b"");
    host.send(DaemonMessage::SessionUpdated {
        session: s,
        request_id: None,
    });
    host.sent();
    assert_eq!(
        h.root(|root, _| root.pane_session("p2")).as_deref(),
        Some("b1"),
        "the pane shows b1 again"
    );

    answer(&mut h, &upload, "/work/late.png");
    assert!(
        h.host(&support::saved_remote()).sent_input("b1").is_empty(),
        "an answer from before the reconnect pastes nothing"
    );
    h.set_clipboard("hi");
    h.keys("ctrl-v");
    assert_eq!(
        h.host(&support::saved_remote()).sent_input("b1"),
        b"hi",
        "the pane takes input again"
    );
}
