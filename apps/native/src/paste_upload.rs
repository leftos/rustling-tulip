//! Pasting an image into an agent session on another host.
//!
//! That agent would read its own host's clipboard, not this one's, so the
//! pane hands the image up ([`crate::term_view::PaneEvent::PasteImage`]) and
//! the root uploads it to the host (`UploadFile`). The host saves it in the
//! session's folder and answers with its path (`FileUploaded`), which the
//! root pastes into the pane that pasted, as text the agent reads.

use std::collections::HashMap;

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

/// An upload sent and not yet answered: which pane pasted it, showing
/// which session on which host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingUpload {
    host: HostKey,
    pane_id: String,
    session_id: String,
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
        self.uploads.0.insert(
            request_id.clone(),
            PendingUpload {
                host: host.clone(),
                pane_id: slot_id.to_owned(),
                session_id: session_id.to_owned(),
            },
        );
        self.send_to(
            &host,
            ClientMessage::UploadFile {
                request_id,
                session_id: session_id.to_owned(),
                name: name.to_owned(),
                data_b64: B64.encode(bytes),
            },
        );
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
        let Some(pending) = self.uploads.take(host, request_id) else {
            tracing::debug!(%request_id, "paste upload: an answer to no pending upload");
            return;
        };
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
        let text = pasted_path(path);
        view.update(cx, |pane, cx| {
            pane.paste_text(&text, &pending.session_id, cx);
        });
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
        if self.uploads.take(host, request_id).is_none() {
            return false;
        }
        tracing::info!(?host, %request_id, "paste upload: the host refused the image");
        self.push_toast(ToastKind::Error, FAILED_TITLE, Some(message.to_owned()), cx);
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
    use gpui::ImageFormat;

    use super::{extension_of, pasted_path};

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
