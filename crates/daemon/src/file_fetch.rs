//! Confinement of client-supplied repo-relative paths, and chunked streaming of
//! a confined file to one client connection. A folder fetch's target is
//! confined here too ([`confine_dir`]); its walk is [`crate::folder_pack`].

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
    #[error("path {rel:?} is not a folder (it resolves to {resolved})")]
    NotADir { rel: String, resolved: String },
}

/// A folder [`confine_dir`] found inside its root: its canonical path,
/// verbatim (`\\?\`) on Windows.
#[derive(Clone, Debug)]
pub struct CanonicalRoot(PathBuf);

impl CanonicalRoot {
    /// The folder's canonical path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Resolve the folder `rel` names under `root`, as [`resolve_inside`] does,
/// refusing a target that is not a folder.
///
/// # Errors
/// Every error names `rel` and the reason: those of [`resolve_inside`], or
/// [`ConfineError::NotADir`].
pub fn confine_dir(root: &Path, rel: &str) -> anyhow::Result<CanonicalRoot> {
    let target = resolve_inside(root, rel)?;
    let meta = std::fs::metadata(&target)
        .with_context(|| format!("reading metadata of {}", target.display()))?;
    if !meta.is_dir() {
        return Err(ConfineError::NotADir {
            rel: rel.to_string(),
            resolved: target.display().to_string(),
        }
        .into());
    }
    Ok(CanonicalRoot(target))
}

/// Resolve `rel` against the canonical `root` (verbatim, `\\?\`, on
/// Windows) and return the canonical target, refusing what [`confine_path`]
/// refuses. `""` and `"."` name the root itself. The request is joined to the
/// verbatim root one component at a time, so a name with a trailing dot or
/// space, or a reserved name such as `CON`, names exactly that entry.
///
/// # Errors
/// Every error names `rel`, `root` and the reason. A target that does not
/// exist is reported as [`ConfineError::NotFound`], before any
/// [`ConfineError::Outside`]: never pass the error on to a client that must
/// not learn what lies outside the root.
pub fn resolve_inside(root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let request = if rel.is_empty() { "." } else { rel };
    confine_joined(root, rel, request, |canon_root| {
        let mut joined = canon_root.to_path_buf();
        for component in Path::new(request).components() {
            if let Component::Normal(name) = component {
                joined.push(name);
            }
        }
        joined
    })
}

/// The one confinement both [`resolve_inside`] and [`confine_path`] run:
/// refuse `request` (the client's `rel`, as the caller reads it) unless it
/// is relative, then canonicalize the path `join` makes from the canonical
/// root and refuse a target outside that root. Every error names `rel`.
fn confine_joined(
    root: &Path,
    rel: &str,
    request: &str,
    join: impl FnOnce(&Path) -> PathBuf,
) -> anyhow::Result<PathBuf> {
    check_relative(request)
        .with_context(|| format!("refusing {rel:?} under {}", root.display()))?;
    let canon_root = std::fs::canonicalize(root)
        .with_context(|| format!("resolving root {} for {rel:?}", root.display()))?;
    let target = match std::fs::canonicalize(join(&canon_root)) {
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
    confine_joined(root, rel, rel, |_| root.join(rel))
}

/// Whether `err` came from [`confine_path`] finding no file at the path.
#[must_use]
pub fn is_not_found(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<ConfineError>(),
        Some(ConfineError::NotFound { .. })
    )
}

/// Stream the file at `path` over `tx` as `FileFetchStarted` and one
/// `FileChunk` per [`CHUNK_SIZE`] bytes, and return the final message,
/// unsent: `FileFetchDone`, or a `FileFetchError` when the path is a
/// directory or cannot be opened or read (then with nothing sent before it).
/// `tx` is bounded, so a slow consumer stalls the reads. Once `cancel` fires,
/// or a send fails, nothing further is sent and `None` is returned.
pub async fn stream_file(
    id: String,
    path: PathBuf,
    tx: mpsc::Sender<DaemonMessage>,
    cancel: CancellationToken,
) -> Option<DaemonMessage> {
    let opened = tokio::select! {
        biased;
        () = cancel.cancelled() => return None,
        opened = open_for_streaming(&path) => opened,
    };
    let (mut file, size) = match opened {
        Ok(opened) => opened,
        Err(err) => {
            let error = format!("{err:#}");
            return Some(DaemonMessage::FileFetchError { id, error });
        }
    };
    let started = DaemonMessage::FileFetchStarted {
        id: id.clone(),
        resolved_path: path.display().to_string(),
        size,
    };
    if !send_unless_cancelled(&tx, &cancel, started).await {
        return None;
    }
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut seq: u64 = 0;
    loop {
        let filled = tokio::select! {
            biased;
            () = cancel.cancelled() => return None,
            filled = fill_buf(&mut file, &mut buf) => filled,
        };
        let n = match filled {
            Ok(0) => return Some(DaemonMessage::FileFetchDone { id }),
            Ok(n) => n,
            Err(err) => {
                let error = format!("reading {}: {err}", path.display());
                return Some(DaemonMessage::FileFetchError { id, error });
            }
        };
        let chunk = DaemonMessage::FileChunk {
            id: id.clone(),
            seq,
            data_b64: base64::engine::general_purpose::STANDARD.encode(&buf[..n]),
        };
        if !send_unless_cancelled(&tx, &cancel, chunk).await {
            return None;
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

/// Send `msg` unless `cancel` fires first. Returns whether it was sent. Every
/// async transfer (file fetch, file follow, a fetch's final message) sends
/// with it; the folder pack's blocking thread uses
/// [`send_unless_cancelled_blocking`].
pub(crate) async fn send_unless_cancelled(
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

/// [`send_unless_cancelled`] for a blocking thread: waits for room in `tx`
/// unless `cancel` fires first, so a cancel always frees a sender parked on
/// a full queue (which `blocking_send` would not). Returns whether `msg` was
/// sent. Never call it from an async task.
pub(crate) fn send_unless_cancelled_blocking(
    tx: &mpsc::Sender<DaemonMessage>,
    cancel: &CancellationToken,
    msg: DaemonMessage,
) -> bool {
    futures::executor::block_on(async {
        tokio::select! {
            biased;
            () = cancel.cancelled() => false,
            sent = tx.send(msg) => sent.is_ok(),
        }
    })
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

    /// The 8.3 short form of `path`'s last part, when the volume made one
    /// that differs from it.
    #[cfg(windows)]
    pub(crate) fn short_name(path: &Path) -> Option<std::ffi::OsString> {
        use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
        use windows::Win32::Storage::FileSystem::GetShortPathNameW;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut buf = vec![0u16; 1024];
        // SAFETY: `wide` is null-terminated and outlives the call, which
        // writes at most `buf.len()` units into `buf`.
        let len = unsafe { GetShortPathNameW(windows::core::PCWSTR(wide.as_ptr()), Some(&mut buf)) }
            as usize;
        if len == 0 || len >= buf.len() {
            return None;
        }
        buf.truncate(len);
        let short = PathBuf::from(std::ffi::OsString::from_wide(&buf));
        let short = short.file_name()?.to_os_string();
        (Some(short.as_os_str()) != path.file_name()).then_some(short)
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
    use crate::file_tail::hooks::make_dir_link;
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

    #[test]
    fn fetch_folder_of_empty_and_dot_is_the_root() {
        let (_base, root, _) = root_with_sibling("folder-root");
        let canonical = std::fs::canonicalize(&root).expect("canonical root");
        for rel in ["", ".", "./", "a/.."] {
            if rel == "a/.." {
                // `..` stays refused, even when it would land on the root.
                assert!(confine_dir(&root, rel).is_err(), "{rel:?}");
                continue;
            }
            let dir = confine_dir(&root, rel).expect("the root is a folder");
            assert_eq!(dir.path(), canonical.as_path(), "{rel:?}");
        }
    }

    #[test]
    fn folder_fetch_refuses_dotdot_absolute_and_outside_links() {
        let (base, root, outside) = root_with_sibling("folder-refuse");
        let err = confine_dir(&root, "..").expect_err("`..` refused");
        assert!(matches!(
            confine_error(&err),
            ConfineError::ParentDir { .. }
        ));
        let err = confine_dir(&root, "a/../..").expect_err("nested `..` refused");
        assert!(matches!(
            confine_error(&err),
            ConfineError::ParentDir { .. }
        ));
        let abs = std::fs::canonicalize(base.path()).expect("canonical base");
        let abs = abs.to_str().expect("utf-8 temp path");
        let err = confine_dir(&root, abs).expect_err("absolute refused");
        assert!(matches!(confine_error(&err), ConfineError::Absolute { .. }));
        assert!(outside.exists());

        let elsewhere = base.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("create link target");
        // Symlinks need developer mode or elevation on Windows and junctions
        // need `cmd`; with neither available there is no link to test.
        if !make_dir_link(&root.join("link"), &elsewhere) {
            return;
        }
        let err = confine_dir(&root, "link").expect_err("outside link refused");
        assert!(matches!(confine_error(&err), ConfineError::Outside { .. }));
    }

    #[test]
    fn folder_fetch_of_a_file_is_an_error() {
        let (_base, root, _) = root_with_sibling("folder-file");
        std::fs::write(root.join("a").join("b.txt"), "hi").expect("write file");
        let err = confine_dir(&root, "a/b.txt").expect_err("a file is no folder");
        assert!(matches!(confine_error(&err), ConfineError::NotADir { .. }));
        assert!(format!("{err:#}").contains("is not a folder"), "{err:#}");
        let err = confine_dir(&root, "a/nope").expect_err("missing");
        assert!(is_not_found(&err));
    }

    #[test]
    fn every_prefixed_or_rooted_request_is_refused() {
        let (_base, root, _) = root_with_sibling("folder-prefixed");
        let mut requests = vec!["/x", "/"];
        if cfg!(windows) {
            requests.extend([
                r"\\?\C:\x",
                r"\??\C:\x",
                "//?/C:/x",
                r"\\.\C:\x",
                r"\\.\pipe\x",
                r"\\server\share\x",
                "//server/share/x",
                "C:x",
                r"C:\x",
                r"\x",
            ]);
        }
        for rel in requests {
            let err = check_relative(rel).expect_err(rel);
            assert!(
                matches!(confine_error(&err), ConfineError::Absolute { .. }),
                "{rel:?}: {err:#}"
            );
            assert!(confine_dir(&root, rel).is_err(), "{rel:?}");
        }
    }

    #[test]
    fn a_request_with_a_trailing_dot_names_that_exact_folder() {
        let (_base, root, _) = root_with_sibling("folder-trailing-dot");
        let canonical = std::fs::canonicalize(&root).expect("canonical root");
        std::fs::create_dir_all(canonical.join("d")).expect("create d");
        std::fs::create_dir_all(canonical.join("d.")).expect("create d.");
        std::fs::write(canonical.join("d").join("plain.txt"), "1").expect("write");
        std::fs::write(canonical.join("d.").join("dotted.txt"), "22").expect("write");
        let dir = confine_dir(&root, "d.").expect("the dotted folder");
        assert_eq!(
            dir.path().file_name().and_then(|name| name.to_str()),
            Some("d.")
        );
        assert!(dir.path().join("dotted.txt").exists());
        assert!(!dir.path().join("plain.txt").exists());
        let plain = confine_dir(&root, "d").expect("the plain folder");
        assert_ne!(plain.path(), dir.path());
        std::fs::remove_file(canonical.join("d.").join("dotted.txt")).expect("clean up");
        std::fs::remove_dir(canonical.join("d.")).expect("clean up");
    }

    #[cfg(windows)]
    #[test]
    fn a_short_name_request_resolves_to_the_long_folder() {
        use super::test_support::short_name;
        let (_base, root, _) = root_with_sibling("folder-short-request");
        let long = root.join("a-rather-long-folder-name");
        std::fs::create_dir_all(&long).expect("create long folder");
        // ReFS and the Dev Drive make no short names.
        let Some(short) = short_name(&long) else {
            return;
        };
        let short = short.to_str().expect("an ASCII short name");
        let dir = confine_dir(&root, short).expect("the short name resolves");
        assert_eq!(
            dir.path().file_name().and_then(|name| name.to_str()),
            Some("a-rather-long-folder-name")
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_root_registered_in_another_case_still_confines() {
        let (base, root, _) = root_with_sibling("folder-case");
        let upper = PathBuf::from(root.to_string_lossy().to_uppercase());
        if upper == root || !upper.exists() {
            return;
        }
        let dir = confine_dir(&upper, "a").expect("the folder under the upper-case root");
        assert_eq!(
            dir.path(),
            confine_dir(&root, "a").expect("the same folder").path()
        );
        let err = confine_dir(&upper, "..").expect_err("`..` refused");
        assert!(matches!(
            confine_error(&err),
            ConfineError::ParentDir { .. }
        ));
        let elsewhere = base.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("create link target");
        if !make_dir_link(&root.join("link"), &elsewhere) {
            return;
        }
        let err = confine_dir(&upper, "link").expect_err("outside link refused");
        assert!(matches!(confine_error(&err), ConfineError::Outside { .. }));
    }

    /// The hang guard on each wait for the stream, failing as "hung".
    const HANG_GUARD: Duration = Duration::from_secs(120);

    async fn collect(mut rx: mpsc::Receiver<DaemonMessage>) -> Vec<DaemonMessage> {
        let mut out = Vec::new();
        while let Some(msg) = tokio::time::timeout(HANG_GUARD, rx.recv())
            .await
            .expect("hung: the stream stopped before closing")
        {
            out.push(msg);
        }
        out
    }

    /// [`stream_file`], then its final message sent as `start_fetch` sends
    /// it.
    async fn stream_to_end(
        id: String,
        path: PathBuf,
        tx: mpsc::Sender<DaemonMessage>,
        cancel: CancellationToken,
    ) {
        if let Some(last) = stream_file(id, path, tx.clone(), cancel.clone()).await {
            send_unless_cancelled(&tx, &cancel, last).await;
        }
    }

    #[tokio::test]
    async fn multi_chunk_file_streams_byte_exact() {
        let dir = TestDir::new("stream");
        let path = dir.path().join("data.bin");
        #[expect(clippy::cast_possible_truncation, reason = "value is < 251")]
        let source: Vec<u8> = (0..CHUNK_SIZE * 2 + 17).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &source).expect("write source");
        let (tx, rx) = mpsc::channel(2);
        let task = tokio::spawn(stream_to_end(
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
        stream_to_end("e".into(), path, tx, CancellationToken::new()).await;
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
        stream_to_end(
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
        let task = tokio::spawn(stream_to_end("c".into(), path, tx, cancel.clone()));
        let first = rx.recv().await.expect("started");
        assert!(matches!(first, DaemonMessage::FileFetchStarted { .. }));
        cancel.cancel();
        let rest = collect(rx).await;
        tokio::time::timeout(HANG_GUARD, task)
            .await
            .expect("hung: the stream task never finished after cancel")
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
