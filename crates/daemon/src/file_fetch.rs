//! Confinement of client-supplied repo-relative paths, and chunked streaming of
//! a confined file to one client connection.

use anyhow::Context as _;
use base64::Engine as _;
use protocol::DaemonMessage;
use std::io;
use std::path::{Component, Path, PathBuf};
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Bytes read from disk per [`DaemonMessage::FileChunk`].
pub const CHUNK_SIZE: usize = 256 * 1024;

/// Why [`check_relative`] or [`confine_path`] refused a client-supplied path.
#[derive(Debug, thiserror::Error)]
pub enum ConfineError {
    #[error("path is empty")]
    Empty,
    #[error("path {rel:?} contains `..`")]
    ParentDir { rel: String },
    #[error("path {rel:?} is absolute")]
    Absolute { rel: String },
    #[error("path {rel:?} resolves outside the repo {root} (to {resolved})")]
    Outside {
        rel: String,
        root: String,
        resolved: String,
    },
    #[error("path {rel:?} does not exist under {root}")]
    NotFound {
        rel: String,
        root: String,
        #[source]
        source: io::Error,
    },
}

/// Reject, without touching the filesystem, a path that is empty, absolute
/// (root, drive or UNC prefix) or contains a `..` component.
///
/// # Errors
/// A [`ConfineError`] naming `rel` and the reason.
pub fn check_relative(rel: &str) -> anyhow::Result<()> {
    if rel.is_empty() {
        return Err(ConfineError::Empty.into());
    }
    for component in Path::new(rel).components() {
        match component {
            Component::ParentDir => {
                return Err(ConfineError::ParentDir {
                    rel: rel.to_string(),
                }
                .into());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ConfineError::Absolute {
                    rel: rel.to_string(),
                }
                .into());
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(())
}

/// Resolve `rel` against `root` and return the canonical target, refusing
/// anything that is not a relative path resolving (through symlinks and
/// junctions) to a location inside the canonical `root`.
///
/// # Errors
/// Every error names `rel`, `root` and the reason. A target that does not
/// exist is reported as [`ConfineError::NotFound`]; see [`is_not_found`].
pub fn confine_path(root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    check_relative(rel).with_context(|| format!("refusing {rel:?} under {}", root.display()))?;
    let canon_root = std::fs::canonicalize(root)
        .with_context(|| format!("resolving root {} for {rel:?}", root.display()))?;
    let target = match std::fs::canonicalize(root.join(rel)) {
        Ok(target) => target,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ConfineError::NotFound {
                rel: rel.to_string(),
                root: root.display().to_string(),
                source,
            }
            .into());
        }
        Err(source) => {
            return Err(anyhow::Error::new(source)
                .context(format!("resolving {rel:?} under {}", root.display())));
        }
    };
    if !target.starts_with(&canon_root) {
        return Err(ConfineError::Outside {
            rel: rel.to_string(),
            root: root.display().to_string(),
            resolved: target.display().to_string(),
        }
        .into());
    }
    Ok(target)
}

/// Whether `err` came from [`confine_path`] finding no file at the path.
#[must_use]
pub fn is_not_found(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<ConfineError>(),
        Some(ConfineError::NotFound { .. })
    )
}

/// Stream the file at `path` over `tx` as `FileFetchStarted`, one
/// `FileChunk` per [`CHUNK_SIZE`] bytes, then `FileFetchDone`; or a single
/// `FileFetchError` when the path is a directory or cannot be opened or read.
/// `tx` is bounded, so a slow consumer stalls the reads. Once `cancel` fires,
/// nothing further is sent.
pub async fn stream_file(
    id: String,
    path: PathBuf,
    tx: mpsc::Sender<DaemonMessage>,
    cancel: CancellationToken,
) {
    let opened = tokio::select! {
        biased;
        () = cancel.cancelled() => return,
        opened = open_for_streaming(&path) => opened,
    };
    let (mut file, size) = match opened {
        Ok(opened) => opened,
        Err(err) => {
            let error = format!("{err:#}");
            send_unless_cancelled(&tx, &cancel, DaemonMessage::FileFetchError { id, error }).await;
            return;
        }
    };
    let started = DaemonMessage::FileFetchStarted {
        id: id.clone(),
        resolved_path: path.display().to_string(),
        size,
    };
    if !send_unless_cancelled(&tx, &cancel, started).await {
        return;
    }
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut seq: u64 = 0;
    loop {
        let filled = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            filled = fill_buf(&mut file, &mut buf) => filled,
        };
        let n = match filled {
            Ok(0) => {
                send_unless_cancelled(&tx, &cancel, DaemonMessage::FileFetchDone { id }).await;
                return;
            }
            Ok(n) => n,
            Err(err) => {
                let error = format!("reading {}: {err}", path.display());
                send_unless_cancelled(&tx, &cancel, DaemonMessage::FileFetchError { id, error })
                    .await;
                return;
            }
        };
        let chunk = DaemonMessage::FileChunk {
            id: id.clone(),
            seq,
            data_b64: base64::engine::general_purpose::STANDARD.encode(&buf[..n]),
        };
        if !send_unless_cancelled(&tx, &cancel, chunk).await {
            return;
        }
        seq += 1;
    }
}

async fn open_for_streaming(path: &Path) -> anyhow::Result<(tokio::fs::File, u64)> {
    let meta = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("reading metadata of {}", path.display()))?;
    if meta.is_dir() {
        anyhow::bail!("{} is a directory", path.display());
    }
    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    Ok((file, meta.len()))
}

/// Read until `buf` is full or the file ends; returns the bytes read.
async fn fill_buf(file: &mut tokio::fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Send `msg` unless `cancel` fires first. Returns whether it was sent.
async fn send_unless_cancelled(
    tx: &mpsc::Sender<DaemonMessage>,
    cancel: &CancellationToken,
    msg: DaemonMessage,
) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        sent = tx.send(msg) => sent.is_ok(),
    }
}

/// Scratch directories for tests, removed on drop.
#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; panic messages aid debugging"
)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    pub(crate) struct TestDir(PathBuf);

    impl TestDir {
        pub(crate) fn new(tag: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("rt-file-fetch-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create test dir");
            Self(path)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; panic messages aid debugging"
)]
mod tests {
    use super::test_support::TestDir;
    use super::*;
    use std::time::Duration;

    /// `base/root` as the confinement root, beside `base/outside.txt`.
    fn root_with_sibling(tag: &str) -> (TestDir, PathBuf, PathBuf) {
        let base = TestDir::new(tag);
        let root = base.path().join("root");
        std::fs::create_dir_all(root.join("a")).expect("create root");
        let outside = base.path().join("outside.txt");
        std::fs::write(&outside, "secret").expect("write sibling");
        (base, root, outside)
    }

    fn confine_error(err: &anyhow::Error) -> &ConfineError {
        err.downcast_ref::<ConfineError>()
            .expect("error carries a ConfineError")
    }

    #[test]
    fn nested_relative_path_is_accepted_and_canonical() {
        let (_base, root, _) = root_with_sibling("ok");
        std::fs::write(root.join("a").join("b.txt"), "hi").expect("write file");
        let resolved = confine_path(&root, "a/b.txt").expect("confined");
        let expected = std::fs::canonicalize(root.join("a").join("b.txt")).expect("canonical");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn parent_dir_is_rejected_lexically() {
        let (_base, root, outside) = root_with_sibling("parent");
        assert!(root.join("../outside.txt").exists() && outside.exists());
        let err = confine_path(&root, "../outside.txt").expect_err("rejected");
        assert!(matches!(
            confine_error(&err),
            ConfineError::ParentDir { .. }
        ));
        let message = format!("{err:#}");
        assert!(message.contains("../outside.txt"), "{message}");
        assert!(message.contains(&root.display().to_string()), "{message}");
        assert!(message.contains("contains `..`"), "{message}");
    }

    #[test]
    fn nested_parent_dir_escape_is_rejected() {
        let (_base, root, _) = root_with_sibling("nested");
        let err = confine_path(&root, "a/../../outside.txt").expect_err("rejected");
        assert!(matches!(
            confine_error(&err),
            ConfineError::ParentDir { .. }
        ));
    }

    #[test]
    fn absolute_path_outside_root_is_rejected() {
        let (_base, root, outside) = root_with_sibling("absolute");
        let abs = std::fs::canonicalize(&outside).expect("canonical outside");
        let abs = abs.to_str().expect("utf-8 temp path");
        let err = confine_path(&root, abs).expect_err("rejected");
        assert!(matches!(confine_error(&err), ConfineError::Absolute { .. }));
    }

    #[cfg(unix)]
    fn make_dir_link(link: &Path, target: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn make_dir_link(link: &Path, target: &Path) -> bool {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return true;
        }
        std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|out| out.status.success())
    }

    #[test]
    fn link_pointing_outside_root_is_rejected() {
        let (base, root, _) = root_with_sibling("link");
        let target = base.path().join("elsewhere");
        std::fs::create_dir_all(&target).expect("create link target");
        std::fs::write(target.join("secret.txt"), "secret").expect("write target file");
        // Symlinks need developer mode or elevation on Windows and junctions
        // need `cmd`; with neither available there is no link to test.
        if !make_dir_link(&root.join("link"), &target) {
            return;
        }
        let err = confine_path(&root, "link/secret.txt").expect_err("rejected");
        assert!(matches!(confine_error(&err), ConfineError::Outside { .. }));
        assert!(format!("{err:#}").contains("resolves outside the repo"));
    }

    #[test]
    fn missing_file_reports_not_found() {
        let (_base, root, _) = root_with_sibling("missing");
        let err = confine_path(&root, "a/nope.txt").expect_err("missing");
        assert!(is_not_found(&err));
        assert!(format!("{err:#}").contains("does not exist"));
    }

    #[test]
    fn empty_path_is_rejected() {
        let (_base, root, _) = root_with_sibling("empty");
        let err = confine_path(&root, "").expect_err("rejected");
        assert!(matches!(confine_error(&err), ConfineError::Empty));
        assert!(!is_not_found(&err));
    }

    async fn collect(mut rx: mpsc::Receiver<DaemonMessage>) -> Vec<DaemonMessage> {
        let mut out = Vec::new();
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("stream progresses")
        {
            out.push(msg);
        }
        out
    }

    #[tokio::test]
    async fn multi_chunk_file_streams_byte_exact() {
        let dir = TestDir::new("stream");
        let path = dir.path().join("data.bin");
        #[expect(clippy::cast_possible_truncation, reason = "value is < 251")]
        let source: Vec<u8> = (0..CHUNK_SIZE * 2 + 17).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &source).expect("write source");
        let (tx, rx) = mpsc::channel(2);
        let task = tokio::spawn(stream_file(
            "f1".into(),
            path.clone(),
            tx,
            CancellationToken::new(),
        ));
        let msgs = collect(rx).await;
        task.await.expect("stream task");
        assert_eq!(msgs.len(), 5, "{msgs:?}");
        assert!(matches!(
            &msgs[0],
            DaemonMessage::FileFetchStarted { id, size, .. } if id == "f1" && *size == source.len() as u64
        ));
        let mut joined = Vec::new();
        for (expected_seq, msg) in (0u64..).zip(&msgs[1..4]) {
            let DaemonMessage::FileChunk { id, seq, data_b64 } = msg else {
                unreachable!("expected a chunk, got {msg:?}");
            };
            assert_eq!((id.as_str(), *seq), ("f1", expected_seq));
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .expect("valid base64");
            joined.extend_from_slice(&bytes);
        }
        assert_eq!(joined, source);
        assert!(matches!(&msgs[4], DaemonMessage::FileFetchDone { id } if id == "f1"));
    }

    #[tokio::test]
    async fn empty_file_sends_started_then_done() {
        let dir = TestDir::new("stream-empty");
        let path = dir.path().join("empty.bin");
        std::fs::write(&path, b"").expect("write empty");
        let (tx, rx) = mpsc::channel(2);
        stream_file("e".into(), path, tx, CancellationToken::new()).await;
        let msgs = collect(rx).await;
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert!(matches!(
            &msgs[0],
            DaemonMessage::FileFetchStarted { size: 0, .. }
        ));
        assert!(matches!(&msgs[1], DaemonMessage::FileFetchDone { .. }));
    }

    #[tokio::test]
    async fn directory_sends_single_error_naming_path() {
        let dir = TestDir::new("stream-dir");
        let (tx, rx) = mpsc::channel(2);
        stream_file(
            "d".into(),
            dir.path().to_path_buf(),
            tx,
            CancellationToken::new(),
        )
        .await;
        let msgs = collect(rx).await;
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        let DaemonMessage::FileFetchError { id, error } = &msgs[0] else {
            unreachable!("expected an error, got {:?}", msgs[0]);
        };
        assert_eq!(id, "d");
        assert!(error.contains(&dir.path().display().to_string()), "{error}");
    }

    #[tokio::test]
    async fn cancel_stops_stream_without_done() {
        let dir = TestDir::new("stream-cancel");
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![7u8; CHUNK_SIZE * 4]).expect("write source");
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(stream_file("c".into(), path, tx, cancel.clone()));
        let first = rx.recv().await.expect("started");
        assert!(matches!(first, DaemonMessage::FileFetchStarted { .. }));
        cancel.cancel();
        let rest = collect(rx).await;
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("stream task finishes after cancel")
            .expect("stream task");
        assert!(
            !rest
                .iter()
                .any(|m| matches!(m, DaemonMessage::FileFetchDone { .. })),
            "{rest:?}"
        );
        assert!(rest.len() <= 1, "{rest:?}");
    }
}
