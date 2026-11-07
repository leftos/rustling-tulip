//! The file upload message: `UploadFile` decodes the file and writes it into
//! an `rt-uploads` folder under the session's own folder, then answers
//! `FileUploaded` with the saved path; a refusal answers `Error` carrying the
//! request's id. The disk work runs on a blocking thread.
//!
//! A file's name is the daemon's own: the request's name loses everything but
//! its last component, every character outside `A-Z a-z 0-9 . _ -` becomes
//! `_`, and a timestamp and a count lead it, so two uploads never share a
//! name. The write is atomic (a `.part` file renamed over the final name), and
//! a sweep after it keeps each folder to the last [`RETENTION`].
//!
//! The folder is `{session folder}/.tmp/rt-uploads/`, the session folder being
//! its first member's worktree, else its current cwd. The folder's
//! `.gitignore` holds `*`, so an upload never shows up in the session's repo.

use super::Hub;
use super::connection::ConnCtx;
use crate::sync::lock;
use base64::Engine as _;
use protocol::error_codes::{INTERNAL, INVALID_REQUEST, NOT_FOUND, TOO_LARGE};
use protocol::{ClientMessage, DaemonMessage, UPLOAD_MAX_BYTES};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};

/// The folder under a session's folder that uploads land in.
const UPLOAD_DIR: &str = "rt-uploads";

/// What an `rt-uploads` folder's `.gitignore` holds.
const GITIGNORE: &str = "*\n";

/// How long an upload stays in its folder before a sweep deletes it.
const RETENTION: Duration = Duration::from_hours(7 * 24);

/// The longest name an upload keeps, before its stamp and count are added.
const NAME_MAX_CHARS: usize = 80;

/// Counts uploads over the daemon's life, so two uploads never share a name.
static NEXT_UPLOAD: AtomicU64 = AtomicU64::new(0);

/// Handle the upload message `dispatch` routes here.
pub(super) async fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) -> anyhow::Result<()> {
    let ClientMessage::UploadFile {
        request_id,
        session_id,
        name,
        data_b64,
    } = msg
    else {
        debug!(
            ?msg,
            "upload: a message that is not an upload was routed here"
        );
        return Ok(());
    };
    let hub = hub.clone();
    let session = session_id.clone();
    let saved = tokio::task::spawn_blocking(move || save(&hub, &session, &name, &data_b64)).await;
    let reply = match saved {
        Ok(Ok(path)) => DaemonMessage::FileUploaded {
            request_id,
            session_id,
            path,
        },
        Ok(Err((code, message))) => DaemonMessage::Error {
            message,
            request_id: Some(request_id),
            code: Some(code.to_owned()),
        },
        Err(err) => DaemonMessage::Error {
            message: format!("upload: the save failed to run: {err}"),
            request_id: Some(request_id),
            code: Some(INTERNAL.to_owned()),
        },
    };
    // A closed outbox is a connection already ending.
    let _ = ctx.out_tx.send(reply);
    Ok(())
}

/// Decode and write one upload: the saved file's absolute path, or the
/// refusal's code and message. Disk work.
fn save(
    hub: &Hub,
    session_id: &str,
    name: &str,
    data_b64: &str,
) -> Result<String, (&'static str, String)> {
    // Refused before decoding, so an oversized message is never decoded.
    if data_b64.len() > UPLOAD_MAX_BYTES.div_ceil(3) * 4 {
        return Err((TOO_LARGE, too_large()));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64.as_bytes())
        .map_err(|_| {
            (
                INVALID_REQUEST,
                "upload: the file is not valid base64".to_owned(),
            )
        })?;
    if bytes.len() > UPLOAD_MAX_BYTES {
        return Err((TOO_LARGE, too_large()));
    }
    let Some(record) = hub.sessions.get(session_id) else {
        return Err((NOT_FOUND, format!("upload: no session {session_id}")));
    };
    let folder = {
        let record = lock(&record);
        let member = record
            .members
            .first()
            .map(|member| member.worktree_path.clone());
        member
            .filter(|path| !path.is_empty())
            .or_else(|| record.current_cwd.clone().filter(|path| !path.is_empty()))
    };
    let Some(folder) = folder else {
        return Err((
            INVALID_REQUEST,
            "upload: the session has no folder".to_owned(),
        ));
    };
    let dir = PathBuf::from(folder).join(".tmp").join(UPLOAD_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|err| internal(format!("upload: creating {}: {err}", dir.display())))?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, GITIGNORE)
            .map_err(|err| internal(format!("upload: writing {}: {err}", ignore.display())))?;
    }
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let count = NEXT_UPLOAD.fetch_add(1, Ordering::Relaxed);
    let file_name = format!("{stamp}-{count}-{}", safe_name(name));
    // Written beside the final name and renamed over it, so a reader never
    // sees a half-written upload under its own name.
    let part = dir.join(format!("{file_name}.part"));
    std::fs::write(&part, &bytes)
        .map_err(|err| internal(format!("upload: writing {}: {err}", part.display())))?;
    let saved = dir.join(&file_name);
    std::fs::rename(&part, &saved).map_err(|err| {
        internal(format!(
            "upload: renaming {} to {}: {err}",
            part.display(),
            saved.display()
        ))
    })?;
    sweep(&dir, SystemTime::now());
    Ok(saved.display().to_string())
}

/// The refusal of a file over [`UPLOAD_MAX_BYTES`].
fn too_large() -> String {
    format!(
        "upload: the file is over {} MiB",
        UPLOAD_MAX_BYTES / (1024 * 1024)
    )
}

/// The refusal of a write that failed.
fn internal(message: String) -> (&'static str, String) {
    (INTERNAL, message)
}

/// The name an upload keeps, from the name the client sent: its last
/// component, every character outside `A-Z a-z 0-9 . _ -` replaced by `_`,
/// cut to [`NAME_MAX_CHARS`] characters, and `upload` when nothing usable is
/// left. A folder named `..` therefore cannot leave the folder.
fn safe_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let safe: String = last
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect();
    let cut: String = safe.chars().take(NAME_MAX_CHARS).collect();
    if cut.is_empty() || cut.chars().all(|c| c == '.') {
        "upload".to_owned()
    } else {
        cut
    }
}

/// Delete the files in `dir` whose modified time is more than [`RETENTION`]
/// before `now`: folders and the folder's `.gitignore` stay, and a file the
/// upload just wrote is newer than the cutoff by construction, so it stays
/// too. A delete that fails is logged and never fails the upload.
fn sweep(dir: &Path, now: SystemTime) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            warn!(
                dir = %dir.display(),
                error = %err,
                "upload: the rt-uploads folder could not be read to sweep it"
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name() == ".gitignore" {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                warn!(path = %path.display(), error = %err, "upload: an entry's type could not be read");
                continue;
            }
        };
        if file_type.is_dir() {
            continue;
        }
        let modified = match entry.metadata().and_then(|meta| meta.modified()) {
            Ok(modified) => modified,
            Err(err) => {
                warn!(path = %path.display(), error = %err, "upload: an entry's time could not be read");
                continue;
            }
        };
        if !now
            .duration_since(modified)
            .is_ok_and(|age| age > RETENTION)
        {
            continue;
        }
        if let Err(err) = std::fs::remove_file(&path) {
            warn!(path = %path.display(), error = %err, "upload: an old upload could not be deleted");
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect; panic messages aid debugging"
)]
mod tests {
    use super::super::test_support::{
        ScratchDir, WsClient, dispatch_one, hello, idle_record, loopback_ws_client, test_hub,
        ws_next, ws_send,
    };
    use super::*;
    use protocol::SessionMember;

    /// An `UploadFile` of `bytes` under `name` for session `session_id`.
    fn upload(session_id: &str, name: &str, bytes: &[u8]) -> ClientMessage {
        ClientMessage::UploadFile {
            request_id: "req".to_owned(),
            session_id: session_id.to_owned(),
            name: name.to_owned(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    /// A `ScratchDir` with an `rt-uploads` folder holding a `.gitignore`.
    fn swept_dir(tag: &str) -> (ScratchDir, PathBuf) {
        let scratch = ScratchDir::new(tag);
        let dir = scratch.path().join("rt-uploads");
        std::fs::create_dir_all(&dir).expect("create the rt-uploads folder");
        std::fs::write(dir.join(".gitignore"), GITIGNORE).expect("write the gitignore");
        (scratch, dir)
    }

    /// A hub holding session `s1`, whose first member's worktree is
    /// `<scratch>/worktree`.
    fn hub_with_folder(tag: &str) -> (Hub, ScratchDir, PathBuf) {
        let (hub, scratch) = test_hub(tag);
        let worktree = scratch.path().join("worktree");
        std::fs::create_dir_all(&worktree).expect("create the worktree");
        let mut record = idle_record("s1");
        record.members.push(SessionMember {
            repo_id: "r1".to_owned(),
            repo_name: "r1".to_owned(),
            branch: "main".to_owned(),
            worktree_path: worktree.display().to_string(),
        });
        hub.sessions.insert(record);
        (hub, scratch, worktree)
    }

    /// The saved path an answer names, failing on anything but `FileUploaded`.
    fn saved_path(reply: DaemonMessage) -> PathBuf {
        let DaemonMessage::FileUploaded {
            request_id,
            session_id,
            path,
        } = reply
        else {
            panic!("expected a FileUploaded, got {reply:?}");
        };
        assert_eq!(request_id, "req");
        assert_eq!(session_id, "s1");
        PathBuf::from(path)
    }

    /// The code and message of a refusal.
    fn refusal(reply: DaemonMessage) -> (String, String) {
        let DaemonMessage::Error {
            message,
            request_id,
            code,
        } = reply
        else {
            panic!("expected a refusal, got {reply:?}");
        };
        assert_eq!(request_id.as_deref(), Some("req"));
        (code.expect("a refusal carries a code"), message)
    }

    /// The next `FileUploaded` or `Error` on `client`, skipping what the
    /// connection's initial state sent.
    async fn until_answered(client: &mut WsClient) -> DaemonMessage {
        for _ in 0..100 {
            let message = ws_next(client).await.expect("the connection stays open");
            if matches!(
                message,
                DaemonMessage::FileUploaded { .. } | DaemonMessage::Error { .. }
            ) {
                return message;
            }
        }
        panic!("no upload answer came");
    }

    #[tokio::test]
    async fn an_upload_lands_in_the_sessions_rt_uploads_folder() {
        let (hub, _scratch, worktree) = hub_with_folder("upload-lands");

        let path = saved_path(dispatch_one(&hub, upload("s1", "shot.png", b"png bytes")).await);

        assert!(
            path.starts_with(worktree.join(".tmp").join(UPLOAD_DIR)),
            "{}",
            path.display()
        );
        assert_eq!(std::fs::read(&path).expect("read the upload"), b"png bytes");
        let ignore = path
            .parent()
            .expect("an upload has a folder")
            .join(".gitignore");
        assert_eq!(
            std::fs::read_to_string(&ignore).expect("read the gitignore"),
            "*\n"
        );
    }

    #[tokio::test]
    async fn an_upload_without_a_member_uses_the_sessions_cwd() {
        let (hub, scratch) = test_hub("upload-cwd");
        let cwd = scratch.path().join("cwd");
        std::fs::create_dir_all(&cwd).expect("create the cwd");
        let mut record = idle_record("s1");
        record.current_cwd = Some(cwd.display().to_string());
        hub.sessions.insert(record);

        let path = saved_path(dispatch_one(&hub, upload("s1", "a.txt", b"hi")).await);

        assert!(
            path.starts_with(cwd.join(".tmp").join(UPLOAD_DIR)),
            "{}",
            path.display()
        );
        assert_eq!(std::fs::read(&path).expect("read the upload"), b"hi");
    }

    #[tokio::test]
    async fn the_name_keeps_only_its_last_component_and_safe_characters() {
        let (hub, _scratch, _worktree) = hub_with_folder("upload-name");

        for (sent, want) in [
            ("../a b/ü.png", "_.png"),
            (r"..\x y.png", "x_y.png"),
            ("..", "upload"),
            ("", "upload"),
        ] {
            let path = saved_path(dispatch_one(&hub, upload("s1", sent, b"x")).await);
            let name = path
                .file_name()
                .expect("an upload has a file name")
                .to_string_lossy()
                .into_owned();
            assert!(
                name.ends_with(&format!("-{want}")),
                "{sent:?} saved as {name}"
            );
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
                "{sent:?} saved as {name}"
            );
        }
    }

    #[tokio::test]
    async fn two_uploads_with_one_name_never_collide() {
        let (hub, _scratch, _worktree) = hub_with_folder("upload-collide");

        let first = saved_path(dispatch_one(&hub, upload("s1", "shot.png", b"one")).await);
        let second = saved_path(dispatch_one(&hub, upload("s1", "shot.png", b"two")).await);

        assert_ne!(first, second, "{}", first.display());
        assert_eq!(std::fs::read(&first).expect("read the first"), b"one");
        assert_eq!(std::fs::read(&second).expect("read the second"), b"two");
    }

    #[tokio::test]
    async fn an_upload_over_the_cap_is_refused_as_too_large() {
        let (hub, _scratch, _worktree) = hub_with_folder("upload-cap");
        let bytes = vec![7u8; UPLOAD_MAX_BYTES + 1];
        let (code, message) = refusal(dispatch_one(&hub, upload("s1", "big.bin", &bytes)).await);
        assert_eq!(code, TOO_LARGE);
        assert_eq!(message, "upload: the file is over 8 MiB");

        // Over the base64 bound, refused before it is decoded at all.
        let oversized = ClientMessage::UploadFile {
            request_id: "req".to_owned(),
            session_id: "s1".to_owned(),
            name: "big.bin".to_owned(),
            data_b64: "!".repeat(UPLOAD_MAX_BYTES.div_ceil(3) * 4 + 1),
        };
        let (code, message) = refusal(dispatch_one(&hub, oversized).await);
        assert_eq!(code, TOO_LARGE);
        assert_eq!(message, "upload: the file is over 8 MiB");
    }

    #[tokio::test]
    async fn bad_base64_is_an_invalid_request() {
        let (hub, _scratch, _worktree) = hub_with_folder("upload-bad-b64");
        let mut msg = upload("s1", "a.png", b"x");
        let ClientMessage::UploadFile { data_b64, .. } = &mut msg else {
            panic!("an upload message");
        };
        *data_b64 = "not base64!".to_owned();

        let (code, message) = refusal(dispatch_one(&hub, msg).await);

        assert_eq!(code, INVALID_REQUEST);
        assert_eq!(message, "upload: the file is not valid base64");
    }

    #[tokio::test]
    async fn an_unknown_session_is_not_found() {
        let (hub, _scratch, _worktree) = hub_with_folder("upload-unknown");

        let (code, message) = refusal(dispatch_one(&hub, upload("nope", "a.png", b"x")).await);

        assert_eq!(code, NOT_FOUND);
        assert_eq!(message, "upload: no session nope");
    }

    #[tokio::test]
    async fn a_session_without_a_folder_is_an_invalid_request() {
        let (hub, _scratch) = test_hub("upload-no-folder");
        hub.sessions.insert(idle_record("s1"));

        let (code, message) = refusal(dispatch_one(&hub, upload("s1", "a.png", b"x")).await);

        assert_eq!(code, INVALID_REQUEST);
        assert_eq!(message, "upload: the session has no folder");
    }

    #[test]
    fn the_sweep_removes_files_older_than_seven_days() {
        let (_scratch, dir) = swept_dir("upload-sweep-old");
        let old = dir.join("old.png");
        std::fs::write(&old, b"old").expect("write the old upload");

        sweep(&dir, SystemTime::now() + Duration::from_hours(8 * 24));

        assert!(!old.exists(), "an eight-day-old upload survived the sweep");
        assert!(
            dir.join(".gitignore").exists(),
            "the sweep removed the gitignore"
        );
    }

    #[test]
    fn the_sweep_keeps_newer_files() {
        let (_scratch, dir) = swept_dir("upload-sweep-new");
        let fresh = dir.join("fresh.png");
        std::fs::write(&fresh, b"fresh").expect("write the new upload");

        sweep(&dir, SystemTime::now() + Duration::from_hours(6 * 24));

        assert!(fresh.exists(), "a six-day-old upload was swept");
    }

    #[tokio::test]
    async fn a_full_size_upload_crosses_a_real_socket() {
        let (hub, _scratch, _worktree) = hub_with_folder("upload-socket");
        let mut client = loopback_ws_client(&hub).await;
        ws_send(&mut client, &hello(&hub.auth_token, None)).await;
        let welcome = ws_next(&mut client)
            .await
            .expect("a Welcome before anything else");
        assert!(
            matches!(welcome, DaemonMessage::Welcome { .. }),
            "{welcome:?}"
        );

        let bytes = vec![9u8; UPLOAD_MAX_BYTES];
        ws_send(&mut client, &upload("s1", "full.bin", &bytes)).await;

        let path = saved_path(until_answered(&mut client).await);
        assert_eq!(std::fs::read(&path).expect("read the upload"), bytes);
    }
}
