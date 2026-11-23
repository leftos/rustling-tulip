//! Pasting an image into a remote agent session: the upload to its host, the
//! returned path pasted back into the pane that pasted, and the toasts when
//! the host cannot take it. The chat composer's image paste, drop and Attach
//! upload to the session's host, a local one included, and insert the
//! returned paths at the composer's caret.

#![expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the precondition it lost"
)]

use std::path::{Path, PathBuf};

use crate::support;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use gpui::{
    ClipboardEntry, ClipboardItem, ClipboardString, ExternalPaths, FileDropEvent, Image,
    ImageFormat, Modifiers, TestAppContext,
};
use protocol::{
    ChatScreen, ClientMessage, ComposerState, DaemonMessage, FEATURE_UPLOAD_FILE, ImagePaste,
    SessionSnapshot, SplitDirection, UPLOAD_MAX_BYTES,
};
use support::{Fixture, Harness, TestDir, pane, remote_pane, session, split, tab};

/// The title of the toast an upload that fails shows.
const FAILED_TITLE: &str = "Couldn't paste the image";

/// The title of the toast a file sent into a chat that fails shows.
const FILE_FAILED_TITLE: &str = "Couldn't send the file";

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
    h.keys("secondary-v");
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
    h.keys("secondary-v");
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
    h.keys("secondary-v");
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
    h.keys("secondary-v");
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
    h.keys("secondary-v");
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
    h.keys("secondary-v");
    let upload = the_upload(&mut h);
    assert_eq!(upload.name, "paste.jpg");
    assert_eq!(upload.bytes, [0xFF, 0xD8, 0xFF]);
}

/// The PNG the stubbed bitmap reader finds on the clipboard.
const READER_PNG: [u8; 7] = [0x89, b'P', b'N', b'G', 9, 8, 7];

/// A bitmap reader that finds [`READER_PNG`].
#[expect(clippy::unnecessary_wraps, reason = "a bitmap reader returns a Result")]
fn reads_a_png() -> anyhow::Result<Option<Vec<u8>>> {
    Ok(Some(READER_PNG.to_vec()))
}

#[gpui::test]
fn a_copied_bitmap_reported_as_bmp_pastes_a_png(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.cx.update(|_, cx| rustling_tulip_native::set_bitmap_reader(cx, reads_a_png));
    let bmp = vec![b'B', b'M', 1, 2, 3];
    h.set_clipboard_image(ImageFormat::Bmp, bmp.clone());
    h.keys("secondary-v");
    let upload = the_upload(&mut h);
    assert_eq!(upload.name, "paste.png");
    assert_eq!(upload.bytes, READER_PNG, "the reader's PNG goes up");
    assert_ne!(upload.bytes, bmp, "not the BMP entry's bytes");
}

#[gpui::test]
fn a_bmp_the_reader_cannot_read_pastes_as_a_bmp(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let bmp = vec![b'B', b'M', 1, 2, 3];
    h.set_clipboard_image(ImageFormat::Bmp, bmp.clone());
    h.keys("secondary-v");
    let upload = the_upload(&mut h);
    assert_eq!(upload.name, "paste.bmp");
    assert_eq!(upload.bytes, bmp);
}

/// A bitmap reader that fails, as it does on a bitmap it cannot convert.
fn fails_to_read() -> anyhow::Result<Option<Vec<u8>>> {
    Err(anyhow::anyhow!("an 8-bit bitmap"))
}

#[gpui::test]
fn a_bmp_whose_reader_fails_pastes_as_a_bmp(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.cx.update(|_, cx| rustling_tulip_native::set_bitmap_reader(cx, fails_to_read));
    let bmp = vec![b'B', b'M', 1, 2, 3];
    h.set_clipboard_image(ImageFormat::Bmp, bmp.clone());
    h.keys("secondary-v");
    let upload = the_upload(&mut h);
    assert_eq!(upload.name, "paste.bmp");
    assert_eq!(upload.bytes, bmp);
}

#[gpui::test]
fn a_host_without_upload_file_toasts_that_it_cannot_receive_images(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[]);
    h.set_clipboard_image(ImageFormat::Png, vec![1, 2, 3]);
    h.keys("secondary-v");
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
    h.keys("secondary-v");
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
    h.keys("secondary-v");
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
    h.keys("secondary-v");
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
    h.keys("secondary-v");
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
    h.keys("secondary-v");
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
    h.keys("secondary-v");
    assert_eq!(
        h.host(&support::saved_remote()).sent_input("b1"),
        b"hi",
        "the pane takes input again"
    );
}

/// The title of the toast a dropped file that cannot go up shows.
const DROP_FAILED_TITLE: &str = "Couldn't drop the file";

/// `name` written under `dir` with `bytes`, its path returned.
fn file_in(dir: &TestDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).expect("write the dropped file");
    path
}

/// `path` as a drop pastes it: quoted when it holds a space.
fn quoted(path: &Path) -> String {
    let path = path.to_string_lossy();
    if path.contains(' ') {
        format!("\"{path}\"")
    } else {
        path.into_owned()
    }
}

/// Drops `paths` on pane `pane_id`.
fn drop_on(h: &mut Harness<'_>, pane_id: &str, paths: &[PathBuf]) {
    h.root_update(|root, _window, cx| root.drop_files(pane_id, paths, cx));
}

#[gpui::test]
fn dropping_files_on_a_local_session_pastes_their_paths(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.sent();
    let files = TestDir::new();
    let one = file_in(&files, "one.txt", b"1");
    let spaced = file_in(&files, "with space.txt", b"2");
    drop_on(&mut h, "p1", &[one.clone(), spaced.clone()]);
    let sent = h.sent();
    assert!(
        uploads_in(&sent).is_empty(),
        "nothing is uploaded: {sent:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&support::input_to(&sent, "a1")),
        format!("{} {}", quoted(&one), quoted(&spaced)),
        "both local paths paste, the spaced one quoted"
    );
    assert!(
        uploads_in(&h.host(&support::saved_remote()).sent()).is_empty(),
        "nothing goes to the remote host"
    );
}

#[gpui::test]
fn dropping_files_on_a_remote_session_uploads_them_one_after_another_and_pastes_the_paths(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let files = TestDir::new();
    let one = file_in(&files, "one.txt", b"first");
    let two = file_in(&files, "two.txt", b"second");
    drop_on(&mut h, "p2", &[one, two]);
    let sent = h.host(&support::saved_remote()).sent();
    let [first]: [Upload; 1] = uploads_in(&sent).try_into().expect("one upload at a time");
    assert_eq!(first.session_id, "b1");
    assert_eq!(first.name, "one.txt");
    assert_eq!(first.bytes, b"first");
    assert!(!has_input(&sent), "nothing pasted yet: {sent:?}");

    answer(&mut h, &first, "/work/one.txt");
    let sent = h.host(&support::saved_remote()).sent();
    let [second]: [Upload; 1] = uploads_in(&sent)
        .try_into()
        .expect("the answer sends the next file");
    assert_eq!(second.name, "two.txt");
    assert_eq!(second.bytes, b"second");
    assert!(
        !has_input(&sent),
        "nothing pasted before the last answer: {sent:?}"
    );

    answer(&mut h, &second, "/work/two.txt");
    assert_eq!(
        h.host(&support::saved_remote()).sent_input("b1"),
        b"/work/one.txt /work/two.txt",
        "both host paths paste together"
    );
}

#[gpui::test]
fn a_dropped_folder_on_a_remote_session_is_skipped_with_a_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let files = TestDir::new();
    let folder = files.path().join("sub");
    std::fs::create_dir(&folder).expect("make the dropped folder");
    let file = file_in(&files, "file.txt", b"f");
    drop_on(&mut h, "p2", &[folder, file]);
    let shown = toasts(&mut h);
    assert_eq!(
        shown
            .iter()
            .filter(|toast| {
                **toast
                    == (
                        "Folders can't be dropped into a remote session".to_owned(),
                        None,
                    )
            })
            .count(),
        1,
        "{shown:?}"
    );
    let upload = the_upload(&mut h);
    assert_eq!(upload.name, "file.txt");
}

#[gpui::test]
fn a_dropped_file_over_the_cap_is_skipped_with_a_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let files = TestDir::new();
    let big = file_in(&files, "big.bin", &vec![7; UPLOAD_MAX_BYTES + 1]);
    let small = file_in(&files, "small.txt", b"s");
    drop_on(&mut h, "p2", &[big, small]);
    assert!(
        toasts(&mut h).contains(&(
            DROP_FAILED_TITLE.to_owned(),
            Some("big.bin is larger than 8 MiB.".to_owned())
        )),
        "{:?}",
        toasts(&mut h)
    );
    let upload = the_upload(&mut h);
    assert_eq!(upload.name, "small.txt");
}

#[gpui::test]
fn a_drop_on_a_host_without_upload_file_toasts_and_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[]);
    let files = TestDir::new();
    let file = file_in(&files, "file.txt", b"f");
    drop_on(&mut h, "p2", &[file]);
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
fn a_refused_upload_mid_drop_pastes_what_was_uploaded_and_stops(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_agent(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let files = TestDir::new();
    let paths = [
        file_in(&files, "one.txt", b"1"),
        file_in(&files, "two.txt", b"2"),
        file_in(&files, "three.txt", b"3"),
    ];
    drop_on(&mut h, "p2", &paths);
    let first = the_upload(&mut h);
    answer(&mut h, &first, "/work/one.txt");
    let second = the_upload(&mut h);
    assert_eq!(second.name, "two.txt");
    let message = "the host's disk is full";
    h.host(&support::saved_remote()).send(DaemonMessage::Error {
        message: message.to_owned(),
        request_id: Some(second.request_id.clone()),
        code: None,
    });
    assert!(
        toasts(&mut h).contains(&(DROP_FAILED_TITLE.to_owned(), Some(message.to_owned()))),
        "{:?}",
        toasts(&mut h)
    );
    let sent = h.host(&support::saved_remote()).sent();
    assert!(
        uploads_in(&sent).is_empty(),
        "the rest of the drop is abandoned: {sent:?}"
    );
    assert_eq!(
        support::input_to(&sent, "b1"),
        b"/work/one.txt",
        "the path already returned is pasted"
    );
}

/// The title of the toast a host that cannot take an upload shows.
const UNSUPPORTED_TITLE: &str =
    "This host can't receive pasted images or files until it is updated";

/// The request id of the last `LoadChat` for `session_id` among `sent`.
fn chat_load_request(sent: &[ClientMessage], session_id: &str) -> Option<String> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::LoadChat {
                session_id: id,
                request_id,
            } if id == session_id => request_id.clone(),
            _ => None,
        })
        .next_back()
}

/// `session_id`'s chat loaded with no rows, the reply echoing `request_id`.
fn chat_reply(session_id: &str, request_id: &str) -> DaemonMessage {
    DaemonMessage::Chat {
        session_id: session_id.to_owned(),
        entries: Vec::new(),
        hidden: 0,
        screen: ChatScreen {
            composer: ComposerState::Ready,
            prompt: None,
        },
        request_id: Some(request_id.to_owned()),
    }
}

/// The remote session `b1` in pane `p2` switched to its chat, loaded, on a
/// host listing `features`, its composer focused and everything sent so far
/// drained.
fn remote_chat<'a>(cx: &'a mut TestAppContext, dir: &TestDir, features: &[&str]) -> Harness<'a> {
    let mut h = support::remote_in_pane_with_features(
        cx,
        dir,
        &serde_json::json!({}),
        &[],
        session("b1").build(),
        features,
    );
    h.click_on("pane-view-chat-p2");
    let sent = h.host(&support::saved_remote()).sent();
    let id = chat_load_request(&sent, "b1").expect("the pane asks its host for the chat");
    h.host(&support::saved_remote()).send(chat_reply("b1", &id));
    h.click_on("chat-composer");
    h.host(&support::saved_remote()).sent();
    h.sent();
    h
}

/// This PC's session `s1` in pane `p1` switched to its chat, loaded, on a
/// daemon listing `upload_file`, its composer focused and everything sent so
/// far drained.
fn local_chat<'a>(cx: &'a mut TestAppContext, dir: &TestDir) -> Harness<'a> {
    let fixture = Fixture::single(session("s1").build());
    let mut h = Harness::with(cx, dir, &fixture);
    h.answer_scrollback("s1", b"");
    h.reconnect_with_features(&[FEATURE_UPLOAD_FILE]);
    h.load(&fixture);
    h.answer_scrollback("s1", b"");
    h.sent();
    h.click_on("pane-view-chat-p1");
    let id = chat_load_request(&h.sent(), "s1").expect("the toggle asks for the chat");
    h.send(chat_reply("s1", &id));
    h.click_on("chat-composer");
    h.sent();
    h
}

/// The text in session `session_id`'s chat composer.
fn composer_of(h: &mut Harness<'_>, session_id: &str) -> Option<String> {
    h.root(|root, cx| root.chat_composer_text(session_id, cx))
}

#[gpui::test]
fn an_image_pasted_in_the_chat_composer_uploads_and_inserts_its_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
    h.set_clipboard_image(ImageFormat::Png, png.clone());
    h.keys("secondary-v");
    let upload = the_upload(&mut h);
    assert_eq!(upload.session_id, "b1");
    assert_eq!(upload.name, "paste.png");
    assert_eq!(upload.bytes, png, "the clipboard's own bytes go up");
    assert_eq!(
        composer_of(&mut h, "b1").as_deref(),
        Some(""),
        "nothing is inserted before the host answers"
    );

    answer(&mut h, &upload, "/work/.tmp/rt-uploads/paste-1.png");
    assert_eq!(
        composer_of(&mut h, "b1").as_deref(),
        Some("/work/.tmp/rt-uploads/paste-1.png "),
        "the host path and one space are inserted at the caret"
    );
    let sent = h.host(&support::saved_remote()).sent();
    assert!(
        !has_input(&sent),
        "nothing is typed into the terminal: {sent:?}"
    );
}

#[gpui::test]
fn a_local_chat_paste_also_uploads(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = local_chat(cx, &dir);
    h.set_clipboard_image(ImageFormat::Png, vec![1, 2, 3]);
    h.keys("secondary-v");
    let sent = h.sent();
    let [upload]: [Upload; 1] = uploads_in(&sent)
        .try_into()
        .expect("a local chat's image paste uploads too");
    assert_eq!(upload.session_id, "s1");
    assert_eq!(upload.name, "paste.png");
    assert_eq!(upload.bytes, [1, 2, 3]);

    h.send(DaemonMessage::FileUploaded {
        request_id: upload.request_id.clone(),
        session_id: upload.session_id.clone(),
        path: "C:/work/.tmp/rt-uploads/paste.png".to_owned(),
    });
    assert_eq!(
        composer_of(&mut h, "s1").as_deref(),
        Some("C:/work/.tmp/rt-uploads/paste.png "),
        "the returned path is inserted"
    );
    let sent = h.sent();
    assert!(
        !has_input(&sent),
        "nothing is typed into the terminal: {sent:?}"
    );
}

#[gpui::test]
fn a_file_sent_into_the_chat_uploads_and_inserts_its_path(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = local_chat(cx, &dir);
    h.root_update(|root, _, cx| {
        root.on_chat_upload_file("s1", "notes.pdf", b"%PDF-1.7", cx);
    });
    let sent = h.sent();
    let [upload]: [Upload; 1] = uploads_in(&sent).try_into().expect("exactly one upload");
    assert_eq!(upload.session_id, "s1");
    assert_eq!(upload.name, "notes.pdf");
    assert_eq!(upload.bytes, b"%PDF-1.7");
    assert_eq!(
        composer_of(&mut h, "s1").as_deref(),
        Some(""),
        "nothing is inserted before the daemon answers"
    );

    h.send(DaemonMessage::FileUploaded {
        request_id: upload.request_id.clone(),
        session_id: upload.session_id.clone(),
        path: "/h/.tmp/rt-uploads/notes.pdf".to_owned(),
    });
    assert_eq!(
        composer_of(&mut h, "s1").as_deref(),
        Some("/h/.tmp/rt-uploads/notes.pdf "),
        "the host path and one space are inserted at the caret"
    );
}

#[gpui::test]
fn a_file_over_the_cap_is_refused_naming_it(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = local_chat(cx, &dir);
    let bytes = vec![7; UPLOAD_MAX_BYTES + 1];
    h.root_update(|root, _, cx| {
        root.on_chat_upload_file("s1", "notes.pdf", &bytes, cx);
    });
    let sent = h.sent();
    assert!(
        uploads_in(&sent).is_empty(),
        "nothing is uploaded: {sent:?}"
    );
    assert!(
        toasts(&mut h).contains(&(
            FILE_FAILED_TITLE.to_owned(),
            Some("notes.pdf is larger than 8 MiB.".to_owned())
        )),
        "{:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn a_host_without_upload_file_refuses_a_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat(cx, &dir, &[]);
    h.root_update(|root, _, cx| {
        root.on_chat_upload_file("b1", "notes.pdf", b"%PDF-1.7", cx);
    });
    let sent = h.host(&support::saved_remote()).sent();
    assert!(
        uploads_in(&sent).is_empty(),
        "nothing is uploaded: {sent:?}"
    );
    assert!(
        toasts(&mut h).contains(&(UNSUPPORTED_TITLE.to_owned(), None)),
        "{:?}",
        toasts(&mut h)
    );
}

#[gpui::test]
fn an_error_reply_to_a_file_upload_names_a_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = local_chat(cx, &dir);
    h.root_update(|root, _, cx| {
        root.on_chat_upload_file("s1", "notes.pdf", b"%PDF-1.7", cx);
    });
    let sent = h.sent();
    let [upload]: [Upload; 1] = uploads_in(&sent).try_into().expect("exactly one upload");
    let message = "the file is larger than the host takes";
    h.send(DaemonMessage::Error {
        message: message.to_owned(),
        request_id: Some(upload.request_id.clone()),
        code: Some("too_large".to_owned()),
    });
    let shown = toasts(&mut h);
    assert_eq!(
        shown
            .iter()
            .filter(|toast| **toast == (FILE_FAILED_TITLE.to_owned(), Some(message.to_owned())))
            .count(),
        1,
        "{shown:?}"
    );
    assert!(
        !shown.iter().any(|(title, _)| title == "Daemon error"),
        "no generic toast as well: {shown:?}"
    );
    assert_eq!(
        composer_of(&mut h, "s1").as_deref(),
        Some(""),
        "nothing is inserted"
    );
}

#[gpui::test]
fn a_drop_on_the_chat_uploads_each_file(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let files = TestDir::new();
    let one = file_in(&files, "one.txt", b"first");
    let two = file_in(&files, "two.txt", b"second");
    h.root_update(|root, _window, cx| root.drop_files_on_chat("b1", &[one, two], cx));
    let first = the_upload(&mut h);
    assert_eq!(first.session_id, "b1");
    assert_eq!(first.name, "one.txt");
    assert_eq!(first.bytes, b"first");

    answer(&mut h, &first, "/work/one.txt");
    let second = the_upload(&mut h);
    assert_eq!(second.name, "two.txt");
    assert_eq!(second.bytes, b"second");
    assert_eq!(
        composer_of(&mut h, "b1").as_deref(),
        Some(""),
        "nothing is inserted before the last answer"
    );

    answer(&mut h, &second, "/work/my two.txt");
    assert_eq!(
        composer_of(&mut h, "b1").as_deref(),
        Some(r#"/work/one.txt "/work/my two.txt" "#),
        "both host paths are inserted together, the spaced one quoted"
    );
}

#[gpui::test]
fn attach_uploads_the_picked_files(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.set_picked_file(None);
    h.click_on("chat-attach");
    assert_eq!(h.file_asks(), 1, "Attach asks for a file");
    let sent = h.host(&support::saved_remote()).sent();
    assert!(
        uploads_in(&sent).is_empty(),
        "a cancelled pick uploads nothing: {sent:?}"
    );

    let files = TestDir::new();
    let picked = file_in(&files, "notes.md", b"# notes");
    h.set_picked_file(picked.to_str());
    h.click_on("chat-attach");
    let upload = the_upload(&mut h);
    assert_eq!(upload.session_id, "b1");
    assert_eq!(upload.name, "notes.md");
    assert_eq!(upload.bytes, b"# notes");
    answer(&mut h, &upload, "/work/notes.md");
    assert_eq!(
        composer_of(&mut h, "b1").as_deref(),
        Some("/work/notes.md ")
    );
}

#[gpui::test]
fn a_chat_paste_on_a_host_without_uploads_shows_the_toast(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat(cx, &dir, &[]);
    h.set_clipboard_image(ImageFormat::Png, vec![1, 2, 3]);
    h.keys("secondary-v");
    let sent = h.host(&support::saved_remote()).sent();
    assert!(
        uploads_in(&sent).is_empty(),
        "nothing is uploaded: {sent:?}"
    );
    assert!(
        toasts(&mut h).contains(&(UNSUPPORTED_TITLE.to_owned(), None)),
        "{:?}",
        toasts(&mut h)
    );
    assert_eq!(
        composer_of(&mut h, "b1").as_deref(),
        Some(""),
        "nothing is inserted"
    );
}

#[gpui::test]
fn a_real_drop_on_a_chat_pane_uploads_to_the_composer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let files = TestDir::new();
    let file = file_in(&files, "drop.txt", b"dropped");
    let over = h.bounds("pane-chat-p2").center();
    h.cx.simulate_event(FileDropEvent::Entered {
        position: over,
        paths: ExternalPaths(vec![file].into()),
    });
    h.cx.simulate_event(FileDropEvent::Submit { position: over });
    let upload = the_upload(&mut h);
    assert_eq!(upload.session_id, "b1");
    assert_eq!(upload.name, "drop.txt");
    answer(&mut h, &upload, "/work/drop.txt");
    assert_eq!(
        composer_of(&mut h, "b1").as_deref(),
        Some("/work/drop.txt "),
        "the dropped file's host path is in the composer"
    );
    let sent = h.host(&support::saved_remote()).sent();
    assert!(
        !has_input(&sent),
        "nothing is pasted to the terminal: {sent:?}"
    );
}

#[gpui::test]
fn a_paste_with_text_and_an_image_pastes_the_text(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.cx.write_to_clipboard(ClipboardItem {
        entries: vec![
            ClipboardEntry::String(ClipboardString::new("some text".to_owned())),
            ClipboardEntry::Image(Image::from_bytes(ImageFormat::Png, vec![1, 2, 3])),
        ],
    });
    h.keys("secondary-v");
    let sent = h.host(&support::saved_remote()).sent();
    assert!(
        uploads_in(&sent).is_empty(),
        "nothing is uploaded: {sent:?}"
    );
    assert_eq!(
        composer_of(&mut h, "b1").as_deref(),
        Some("some text"),
        "the clipboard's text wins over its image"
    );
}

#[gpui::test]
fn a_folder_dropped_on_a_chat_is_refused_with_the_attach_wording(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    let files = TestDir::new();
    let folder = files.path().join("sub");
    std::fs::create_dir(&folder).expect("make the dropped folder");
    let file = file_in(&files, "file.txt", b"f");
    h.root_update(|root, _window, cx| root.drop_files_on_chat("b1", &[folder, file], cx));
    let shown = toasts(&mut h);
    assert!(
        shown.contains(&("Folders can't be attached".to_owned(), None)),
        "{shown:?}"
    );
    assert!(
        !shown
            .iter()
            .any(|(title, _)| title == "Folders can't be dropped into a remote session"),
        "not the pane's wording: {shown:?}"
    );
    let upload = the_upload(&mut h);
    assert_eq!(upload.name, "file.txt");
}

#[gpui::test]
fn an_attach_for_a_chat_no_longer_loaded_sends_nothing(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.click_on("pane-view-terminal-p2");
    h.host(&support::saved_remote()).sent();
    h.sent();
    let before = toasts(&mut h);
    let files = TestDir::new();
    let file = file_in(&files, "late.txt", b"late");
    // What Attach does once its pick resolves, after the chat unloaded.
    h.root_update(|root, _window, cx| root.drop_files_on_chat("b1", &[file], cx));
    let remote = h.host(&support::saved_remote()).sent();
    assert!(
        uploads_in(&remote).is_empty(),
        "nothing goes to the session's host: {remote:?}"
    );
    let local = h.sent();
    assert!(
        uploads_in(&local).is_empty(),
        "nothing goes to This PC: {local:?}"
    );
    assert_eq!(toasts(&mut h), before, "no toast either");
}

#[gpui::test]
fn paste_without_an_image_still_pastes_text_in_the_composer(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = remote_chat(cx, &dir, &[FEATURE_UPLOAD_FILE]);
    h.set_clipboard("hello there");
    h.keys("secondary-v");
    let sent = h.host(&support::saved_remote()).sent();
    assert!(
        uploads_in(&sent).is_empty(),
        "nothing is uploaded: {sent:?}"
    );
    assert_eq!(
        composer_of(&mut h, "b1").as_deref(),
        Some("hello there"),
        "the clipboard's text is pasted"
    );
}
