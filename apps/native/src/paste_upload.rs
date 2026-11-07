//! Pasting an image into an agent session on another host.
//!
//! That agent would read its own host's clipboard, not this one's, so the
//! pane hands the image up ([`crate::term_view::PaneEvent::PasteImage`]) and
//! the root uploads it to the host (`UploadFile`). The host saves it in the
//! session's folder and answers with its path (`FileUploaded`), which the
//! root pastes into the pane that pasted, as text the agent reads.
//!
//! Files dropped on a pane go the same way: on a remote session each one
//! uploads in turn and the returned paths paste together; on a local one
//! their own paths paste.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use gpui::{Context, ImageFormat};
use protocol::{ClientMessage, DaemonMessage, FEATURE_UPLOAD_FILE, UPLOAD_MAX_BYTES};

use crate::RootView;
use crate::notices::ToastKind;
use crate::target::HostKey;

/// The toast a host that cannot take an upload shows.
const UNSUPPORTED_TITLE: &str =
    "This host can't receive pasted images or files until it is updated";
/// The toast an image that could not be pasted shows.
const FAILED_TITLE: &str = "Couldn't paste the image";
/// The toast a drop holding folders shows on a remote session.
const FOLDERS_TITLE: &str = "Folders can't be dropped into a remote session";
/// The toast a dropped file that could not go up shows.
const DROP_FAILED_TITLE: &str = "Couldn't drop the file";

/// What an upload carries: a pasted image, or a file of a drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadKind {
    Image,
    Drop,
}

impl UploadKind {
    /// The title of the toast a refused upload of this kind shows.
    fn failed_title(self) -> &'static str {
        match self {
            Self::Image => FAILED_TITLE,
            Self::Drop => DROP_FAILED_TITLE,
        }
    }
}

/// An upload sent and not yet answered: which pane pasted it, showing
/// which session on which host, the drop's files not yet sent and the host
/// paths returned so far. An image paste is a drop of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingUpload {
    host: HostKey,
    pane_id: String,
    session_id: String,
    kind: UploadKind,
    remaining: Vec<PathBuf>,
    uploaded: Vec<String>,
}

/// The uploads waiting for their answer, by request id.
#[derive(Debug, Default)]
pub(crate) struct PendingUploads(HashMap<String, PendingUpload>);

impl PendingUploads {
    /// Takes the upload `request_id` names when `host` sent it.
    fn take(&mut self, host: &HostKey, request_id: &str) -> Option<PendingUpload> {
        if self
            .0
            .get(request_id)
            .is_none_or(|pending| pending.host != *host)
        {
            return None;
        }
        self.0.remove(request_id)
    }
}

/// The file extension an image of `format` is uploaded under.
pub(crate) fn extension_of(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
    }
}

/// `path` as it is pasted: in double quotes when it holds a space, so the
/// agent reads it as one path; as it is otherwise.
pub(crate) fn pasted_path(path: &str) -> String {
    if path.contains(' ') {
        format!("\"{path}\"")
    } else {
        path.to_owned()
    }
}

/// The detail of the toast an image over the cap shows.
fn too_large_detail() -> String {
    format!(
        "The image is larger than {} MiB.",
        UPLOAD_MAX_BYTES / (1024 * 1024)
    )
}

/// The detail of the toast a dropped file `name` over the cap shows.
fn file_too_large_detail(name: &str) -> String {
    format!(
        "{name} is larger than {} MiB.",
        UPLOAD_MAX_BYTES / (1024 * 1024)
    )
}

/// The files of a drop, in drop order, and how many of its paths were
/// folders.
pub(crate) fn split_drop(
    paths: &[PathBuf],
    is_dir: impl Fn(&Path) -> bool,
) -> (Vec<PathBuf>, usize) {
    let (folders, files): (Vec<&PathBuf>, Vec<&PathBuf>) =
        paths.iter().partition(|path| is_dir(path));
    (files.into_iter().cloned().collect(), folders.len())
}

/// `paths` as one paste: each as [`pasted_path`] pastes it, one space
/// between.
pub(crate) fn joined_paths(paths: &[String]) -> String {
    paths
        .iter()
        .map(|path| pasted_path(path))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The bytes of dropped file `path`, named `name`; or the toast kind and
/// detail that say why it cannot go up.
fn read_dropped(path: &Path, name: &str) -> Result<Vec<u8>, (ToastKind, String)> {
    let over_cap = std::fs::metadata(path)
        .is_ok_and(|meta| !usize::try_from(meta.len()).is_ok_and(|len| len <= UPLOAD_MAX_BYTES));
    if over_cap {
        return Err((ToastKind::Warning, file_too_large_detail(name)));
    }
    std::fs::read(path).map_err(|err| (ToastKind::Error, err.to_string()))
}

impl RootView {
    /// Pane `slot_id`, showing `session_id`, pasted the image `bytes` named
    /// `name`: it goes up to the pane's host, unless that host cannot take
    /// it or it is over the cap, which a toast says instead.
    pub(crate) fn on_paste_image(
        &mut self,
        slot_id: &str,
        session_id: &str,
        name: &str,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.panes.get(slot_id).map(|slot| slot.host().clone()) else {
            tracing::debug!(%slot_id, "a gone pane's image paste; not uploaded");
            return;
        };
        if host == HostKey::Local {
            tracing::debug!(%slot_id, "a local pane's image paste; not uploaded");
            return;
        }
        if !self
            .conn_of(&host)
            .is_some_and(|conn| conn.supports(FEATURE_UPLOAD_FILE))
        {
            tracing::info!(?host, "paste upload: the host does not take uploads");
            self.push_toast(ToastKind::Warning, UNSUPPORTED_TITLE, None, cx);
            return;
        }
        if bytes.len() > UPLOAD_MAX_BYTES {
            tracing::info!(
                ?host,
                bytes = bytes.len(),
                "paste upload: the image is over the cap"
            );
            self.push_toast(
                ToastKind::Warning,
                FAILED_TITLE,
                Some(too_large_detail()),
                cx,
            );
            return;
        }
        let request_id = crate::new_request_id();
        tracing::info!(
            ?host,
            %session_id,
            %name,
            bytes = bytes.len(),
            %request_id,
            "paste upload: sending the image"
        );
        let pending = PendingUpload {
            host,
            pane_id: slot_id.to_owned(),
            session_id: session_id.to_owned(),
            kind: UploadKind::Image,
            remaining: Vec::new(),
            uploaded: Vec::new(),
        };
        self.send_upload(request_id, pending, name, bytes);
    }

    /// Sends `bytes` named `name` up as `request_id`, waiting as `pending`.
    fn send_upload(
        &mut self,
        request_id: String,
        pending: PendingUpload,
        name: &str,
        bytes: &[u8],
    ) {
        let host = pending.host.clone();
        let session_id = pending.session_id.clone();
        self.uploads.0.insert(request_id.clone(), pending);
        self.send_to(
            &host,
            ClientMessage::UploadFile {
                request_id,
                session_id,
                name: name.to_owned(),
                data_b64: B64.encode(bytes),
            },
        );
    }

    /// `paths` were dropped on pane `pane_id`. On a local session their own
    /// paths paste; on a remote one each file uploads in turn and the
    /// returned paths paste once the last is answered. Folders are skipped
    /// there with a toast.
    #[doc(hidden)]
    pub fn drop_files(&mut self, pane_id: &str, paths: &[PathBuf], cx: &mut Context<Self>) {
        let Some((host, session_id, view)) = self.panes.get(pane_id).and_then(|slot| {
            let session = slot.session()?.to_owned();
            Some((slot.host().clone(), session, slot.view().clone()))
        }) else {
            tracing::debug!(%pane_id, "a drop on a pane with no session; ignored");
            return;
        };
        if paths.is_empty() {
            return;
        }
        if host == HostKey::Local {
            let local: Vec<String> = paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            let text = joined_paths(&local);
            view.update(cx, |pane, cx| pane.paste_text(&text, &session_id, cx));
            return;
        }
        if !self
            .conn_of(&host)
            .is_some_and(|conn| conn.supports(FEATURE_UPLOAD_FILE))
        {
            tracing::info!(?host, "drop upload: the host does not take uploads");
            self.push_toast(ToastKind::Warning, UNSUPPORTED_TITLE, None, cx);
            return;
        }
        let (files, folders) = split_drop(paths, Path::is_dir);
        if folders > 0 {
            tracing::info!(?host, folders, "drop upload: folders skipped");
            self.push_toast(ToastKind::Warning, FOLDERS_TITLE, None, cx);
        }
        let pending = PendingUpload {
            host,
            pane_id: pane_id.to_owned(),
            session_id,
            kind: UploadKind::Drop,
            remaining: files,
            uploaded: Vec::new(),
        };
        self.send_next_file(pending, cx);
    }

    /// Sends the first of `pending`'s remaining files that can go up,
    /// toasting each one skipped; with none left, pastes the paths
    /// returned so far.
    fn send_next_file(&mut self, mut pending: PendingUpload, cx: &mut Context<Self>) {
        while !pending.remaining.is_empty() {
            let path = pending.remaining.remove(0);
            let name = path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
            match read_dropped(&path, &name) {
                Ok(bytes) => {
                    let request_id = crate::new_request_id();
                    tracing::info!(
                        host = ?pending.host,
                        session_id = %pending.session_id,
                        %name,
                        bytes = bytes.len(),
                        %request_id,
                        left = pending.remaining.len(),
                        "drop upload: sending a file"
                    );
                    self.send_upload(request_id, pending, &name, &bytes);
                    return;
                }
                Err((kind, detail)) => {
                    tracing::info!(%name, %detail, "drop upload: a file skipped");
                    self.push_toast(kind, DROP_FAILED_TITLE, Some(detail), cx);
                }
            }
        }
        self.paste_uploaded(&pending, cx);
    }

    /// Pastes `pending`'s returned paths into the pane it came from, while
    /// that pane still shows that session on that host.
    fn paste_uploaded(&self, pending: &PendingUpload, cx: &mut Context<Self>) {
        if pending.uploaded.is_empty() {
            return;
        }
        let Some(view) = self
            .panes
            .get(&pending.pane_id)
            .filter(|slot| {
                *slot.host() == pending.host && slot.session() == Some(&pending.session_id)
            })
            .map(|slot| slot.view().clone())
        else {
            tracing::debug!(
                pane = %pending.pane_id,
                session = %pending.session_id,
                "paste upload: the pane no longer shows the session; path dropped"
            );
            return;
        };
        let text = joined_paths(&pending.uploaded);
        view.update(cx, |pane, cx| {
            pane.paste_text(&text, &pending.session_id, cx);
        });
    }

    /// Takes `host`'s `FileUploaded` answer to [`Self::on_file_uploaded`].
    /// Returns whether `msg` was one.
    pub(crate) fn on_upload_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        cx: &mut Context<Self>,
    ) -> bool {
        let DaemonMessage::FileUploaded {
            request_id, path, ..
        } = msg
        else {
            return false;
        };
        self.on_file_uploaded(host, request_id, path, cx);
        true
    }

    /// `host` saved upload `request_id` at `path`: the path is pasted into
    /// the pane that pasted the image, while it still shows that session on
    /// that host. An id this client did not send there is ignored.
    fn on_file_uploaded(
        &mut self,
        host: &HostKey,
        request_id: &str,
        path: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(mut pending) = self.uploads.take(host, request_id) else {
            tracing::debug!(%request_id, "paste upload: an answer to no pending upload");
            return;
        };
        pending.uploaded.push(path.to_owned());
        if pending.remaining.is_empty() {
            self.paste_uploaded(&pending, cx);
        } else {
            self.send_next_file(pending, cx);
        }
    }

    /// A refusal from `host` carrying `request_id`: when it answers a
    /// pending upload, a toast shows `message`. Returns whether it did.
    pub(crate) fn on_upload_refused(
        &mut self,
        host: &HostKey,
        request_id: &str,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pending) = self.uploads.take(host, request_id) else {
            return false;
        };
        tracing::info!(
            ?host,
            %request_id,
            kind = ?pending.kind,
            abandoned = pending.remaining.len(),
            "paste upload: the host refused the upload"
        );
        self.push_toast(
            ToastKind::Error,
            pending.kind.failed_title(),
            Some(message.to_owned()),
            cx,
        );
        if pending.kind == UploadKind::Drop {
            self.paste_uploaded(&pending, cx);
        }
        true
    }

    /// `host` opened a new connection, which answers nothing the old one
    /// was sent: its pending uploads are forgotten.
    pub(crate) fn forget_uploads_of(&mut self, host: &HostKey) {
        self.uploads.0.retain(|_, pending| pending.host != *host);
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use gpui::ImageFormat;

    use super::{extension_of, joined_paths, pasted_path, split_drop};

    #[test]
    fn split_drop_keeps_files_in_order_and_counts_folders() {
        let paths: Vec<PathBuf> = ["a.txt", "dir1", "b.txt", "dir2", "c.txt"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let (files, folders) = split_drop(&paths, |path: &Path| {
            path.to_string_lossy().starts_with("dir")
        });
        assert_eq!(
            files,
            [
                PathBuf::from("a.txt"),
                PathBuf::from("b.txt"),
                PathBuf::from("c.txt")
            ]
        );
        assert_eq!(folders, 2);
        let (files, folders) = split_drop(&[], |_: &Path| true);
        assert_eq!(files, [] as [PathBuf; 0]);
        assert_eq!(folders, 0);
    }

    #[test]
    fn joined_paths_quotes_spaced_paths_and_joins_with_a_space() {
        assert_eq!(joined_paths(&["/work/a.txt".to_owned()]), "/work/a.txt");
        assert_eq!(
            joined_paths(&["/work/a.txt".to_owned(), "/work/b.txt".to_owned()]),
            "/work/a.txt /work/b.txt"
        );
        assert_eq!(
            joined_paths(&["/work/a.txt".to_owned(), "/work/my file.txt".to_owned()]),
            r#"/work/a.txt "/work/my file.txt""#
        );
    }

    #[test]
    fn pasted_path_quotes_only_a_path_with_a_space() {
        assert_eq!(
            pasted_path("/work/.tmp/rt-uploads/paste.png"),
            "/work/.tmp/rt-uploads/paste.png"
        );
        assert_eq!(
            pasted_path(r"C:\Users\Some One\paste.png"),
            r#""C:\Users\Some One\paste.png""#
        );
        assert_eq!(
            pasted_path(r#"/work/it's"odd".png"#),
            r#"/work/it's"odd".png"#,
            "nothing else is escaped"
        );
    }

    #[test]
    fn extension_of_each_image_format() {
        let cases = [
            (ImageFormat::Png, "png"),
            (ImageFormat::Jpeg, "jpg"),
            (ImageFormat::Webp, "webp"),
            (ImageFormat::Gif, "gif"),
            (ImageFormat::Svg, "svg"),
            (ImageFormat::Bmp, "bmp"),
            (ImageFormat::Tiff, "tiff"),
        ];
        for (format, extension) in cases {
            assert_eq!(extension_of(format), extension, "{format:?}");
        }
    }
}
