//! Per-session scrollback ring buffer on disk.
//!
//! Each PTY chunk (or each headless stream-json line) is appended to
//! `<sessions_dir>/<id>/scrollback.bin`. When the file exceeds the cap, the
//! oldest bytes are dropped and a `<sessions_dir>/<id>/scrollback.truncated`
//! flag is touched so the UI can show "earlier output discarded" on replay.
//!
//! On attach, the frontend issues a one-shot `LoadScrollback` request before
//! subscribing to live PTY events; this lets users see their previous session
//! state across app/daemon restarts without reconnecting to a vanished PTY.
//!
//! All ops are best-effort: failures are logged but never abort session
//! handling. Synchronization: writes are sequential (one writer per session),
//! reads run in dispatch tasks; a brief partial read during a trim operation
//! is acceptable — terminal rendering tolerates it.

use crate::paths::Dirs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tracing::warn;

/// 2 MB hard cap on scrollback file size. Tunable; balances "users see
/// enough history" against "we don't waste disk when claude prints a lot".
const MAX_SIZE: u64 = 2 * 1024 * 1024;
/// When trimming, keep this many bytes (the most recent ones). The 1.5 MB
/// floor + 2 MB ceiling means a trim happens at most once per ~512 KB of
/// fresh output, amortizing the cost of the rewrite.
const TRIM_TO: usize = 1_500_000;

fn scrollback_path(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir.join(session_id).join("scrollback.bin")
}

fn truncated_flag_path(dirs: &Dirs, session_id: &str) -> PathBuf {
    dirs.sessions_dir
        .join(session_id)
        .join("scrollback.truncated")
}

/// Append `data` to the session's scrollback file, creating parent dirs as
/// needed. Trims and sets the truncated flag if the file exceeds `MAX_SIZE`.
pub fn append(dirs: &Dirs, session_id: &str, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    if let Err(err) = append_inner(dirs, session_id, data) {
        warn!(?err, %session_id, "scrollback append failed");
    }
}

fn append_inner(dirs: &Dirs, session_id: &str, data: &[u8]) -> std::io::Result<()> {
    let path = scrollback_path(dirs, session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        f.write_all(data)?;
    }
    let size = std::fs::metadata(&path)?.len();
    if size > MAX_SIZE {
        trim(dirs, session_id, &path)?;
    }
    Ok(())
}

fn trim(dirs: &Dirs, session_id: &str, path: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    let keep_from = bytes.len().saturating_sub(TRIM_TO);
    let keep = &bytes[keep_from..];
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, keep)?;
    std::fs::rename(&tmp, path)?;
    // Touch the truncated flag (idempotent — already-existing is fine).
    let flag = truncated_flag_path(dirs, session_id);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&flag);
    Ok(())
}

/// Load the full scrollback for a session. Returns `(bytes, truncated)` —
/// `truncated == true` means the buffer overflowed at some point and the
/// caller should surface a "earlier output discarded" hint to the user.
/// Missing file is not an error; returns empty + false.
pub fn load(dirs: &Dirs, session_id: &str) -> (Vec<u8>, bool) {
    let path = scrollback_path(dirs, session_id);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            warn!(?err, %session_id, "scrollback load failed");
            return (Vec::new(), false);
        }
    };
    let truncated = truncated_flag_path(dirs, session_id).exists();
    (bytes, truncated)
}
