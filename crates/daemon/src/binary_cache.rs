//! Content-addressed cache of executable templates.
//!
//! Windows refuses to overwrite a `.exe` while a process is running it. That
//! makes both `cargo build` (during dev) and the NSIS installer (in
//! production) destructive: the only way to free the file is to kill every
//! process holding it. For `rt-tracer.exe` that means killing every
//! supervisor, which kills every PTY child — every active claude/codex/shell
//! session is gone.
//!
//! The fix here is to spawn from a content-addressed copy of the template
//! rather than the template itself. The shipped `rt-tracer.exe` (and
//! `rustling-tulipd.exe`) are never executed directly; on first use they're
//! hashed and copied into `<binaries_dir>/<stem>-<sha256-prefix>.exe`. Each
//! running process holds a handle on its own private copy. Replacing the
//! shipped template never collides with a live process.
//!
//! [`gc`] runs at daemon startup and prunes cached copies that no live
//! sidecar references — the cache doesn't grow unbounded across rebuilds.

use anyhow::Context as _;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// First N hex chars of the SHA-256 retained in the cached filename. 16 chars
/// = 8 bytes = 64 bits of entropy, more than enough to avoid accidental
/// collisions between different builds of the same template.
const HASH_PREFIX_LEN: usize = 16;

/// Ensure a content-addressed copy of `template` exists in `cache_dir` and
/// return the cached path.
///
/// `name_hint` is the stem of the cached filename (typically the template's
/// own file stem) — e.g. `"rt-tracer"` produces
/// `<cache_dir>/rt-tracer-<hex16>.exe`. The extension is preserved from the
/// template; templates without an extension produce extensionless cache
/// entries (Linux/macOS).
///
/// Idempotent: if a cache entry with the matching hash already exists, no
/// copy happens. The returned path is always absolute when `cache_dir` is
/// absolute (which it is in production — see [`crate::paths::Dirs`]).
pub fn ensure_cached(template: &Path, cache_dir: &Path, name_hint: &str) -> anyhow::Result<PathBuf> {
    let bytes = fs::read(template)
        .with_context(|| format!("reading template {}", template.display()))?;
    let hash = hash_prefix(&bytes);

    let extension = template
        .extension()
        .map(|e| e.to_string_lossy().into_owned());
    let filename = match extension.as_deref() {
        Some(ext) if !ext.is_empty() => format!("{name_hint}-{hash}.{ext}"),
        _ => format!("{name_hint}-{hash}"),
    };
    let cached = cache_dir.join(&filename);

    if cached.exists() {
        debug!(
            template = %template.display(),
            cached = %cached.display(),
            "binary_cache: hit"
        );
        return Ok(cached);
    }

    fs::create_dir_all(cache_dir)
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

    let tmp = cache_dir.join(format!("{filename}.tmp"));
    fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(err) = fs::rename(&tmp, &cached) {
        // Another concurrent caller may have populated the cache between our
        // existence check and rename. Retry the existence check before
        // surfacing the error.
        if cached.exists() {
            let _ = fs::remove_file(&tmp);
            debug!(
                template = %template.display(),
                cached = %cached.display(),
                "binary_cache: lost rename race; using existing entry"
            );
            return Ok(cached);
        }
        return Err(err).with_context(|| {
            format!("renaming {} to {}", tmp.display(), cached.display())
        });
    }

    info!(
        template = %template.display(),
        cached = %cached.display(),
        "binary_cache: populated"
    );
    Ok(cached)
}

/// Delete every cached binary that is not present in `in_use`. Failures on
/// individual entries are logged and skipped — a stale lock or missing perm
/// must not crash the daemon at startup.
pub fn gc(cache_dir: &Path, in_use: &HashSet<PathBuf>) -> anyhow::Result<GcReport> {
    let mut report = GcReport::default();
    let entries = match fs::read_dir(cache_dir) {
        Ok(it) => it,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(err) => return Err(err).context("reading binaries cache dir"),
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                warn!(?err, "binary_cache: skipping unreadable entry");
                continue;
            }
        };
        let path = entry.path();
        if !entry.file_type().is_ok_and(|ft| ft.is_file()) {
            continue;
        }
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("tmp"))
        {
            // Stranded `.tmp` from a crashed copy — safe to remove.
            if let Err(err) = fs::remove_file(&path) {
                warn!(?err, path = %path.display(), "binary_cache: failed to remove tmp");
            } else {
                report.tmp_removed += 1;
            }
            continue;
        }
        if in_use.contains(&path) {
            report.kept += 1;
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                report.removed += 1;
                debug!(path = %path.display(), "binary_cache: gc removed");
            }
            Err(err) => {
                warn!(?err, path = %path.display(), "binary_cache: gc could not remove");
                report.skipped += 1;
            }
        }
    }
    Ok(report)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct GcReport {
    pub kept: usize,
    pub removed: usize,
    pub skipped: usize,
    pub tmp_removed: usize,
}

fn hash_prefix(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex.truncate(HASH_PREFIX_LEN);
    hex
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests assert preconditions with expect/unwrap; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// RAII-style scratch dir under the OS temp root. Drop removes the tree.
    /// Lightweight stand-in for `tempfile`/`tempdir` — we don't want to pull
    /// in another dep just for tests.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rt-binary-cache-{label}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write_template(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn ensure_cached_copies_and_is_idempotent() {
        let scratch = Scratch::new("idempotent");
        let template = write_template(scratch.path(), "rt-tracer.exe", b"hello world");
        let cache = scratch.path().join("cache");

        let first = ensure_cached(&template, &cache, "rt-tracer").expect("first ensure");
        assert!(first.exists());
        assert!(
            first
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("rt-tracer-")
        );
        assert!(first.extension().is_some_and(|e| e == "exe"));
        assert_eq!(fs::read(&first).unwrap(), b"hello world");

        let second = ensure_cached(&template, &cache, "rt-tracer").expect("second ensure");
        assert_eq!(first, second);
    }

    #[test]
    fn ensure_cached_changes_path_when_template_bytes_change() {
        let scratch = Scratch::new("changed");
        let template = write_template(scratch.path(), "rt-tracer.exe", b"v1");
        let cache = scratch.path().join("cache");

        let v1 = ensure_cached(&template, &cache, "rt-tracer").expect("v1");
        fs::write(&template, b"v2").unwrap();
        let v2 = ensure_cached(&template, &cache, "rt-tracer").expect("v2");

        assert_ne!(v1, v2);
        assert!(
            v1.exists() && v2.exists(),
            "GC is separate; both linger until prune"
        );
    }

    #[test]
    fn ensure_cached_handles_extensionless_templates() {
        let scratch = Scratch::new("noext");
        let template = write_template(scratch.path(), "rt-tracer", b"unix");
        let cache = scratch.path().join("cache");

        let p = ensure_cached(&template, &cache, "rt-tracer").expect("ensure");
        assert!(p.extension().is_none(), "{p:?}");
    }

    #[test]
    fn gc_keeps_in_use_and_removes_orphans() {
        let scratch = Scratch::new("gc");
        let cache = scratch.path().to_path_buf();
        let alive = write_template(&cache, "rt-tracer-aaaaaaaaaaaaaaaa.exe", b"a");
        let dead = write_template(&cache, "rt-tracer-bbbbbbbbbbbbbbbb.exe", b"b");
        let stale_tmp = write_template(&cache, "rt-tracer-cccccccccccccccc.exe.tmp", b"c");

        let mut in_use = HashSet::new();
        in_use.insert(alive.clone());

        let report = gc(&cache, &in_use).expect("gc");
        assert_eq!(report.kept, 1);
        assert_eq!(report.removed, 1);
        assert_eq!(report.tmp_removed, 1);

        assert!(alive.exists(), "in-use entry kept");
        assert!(!dead.exists(), "orphan entry removed");
        assert!(!stale_tmp.exists(), "stale tmp removed");
    }

    #[test]
    fn gc_returns_empty_when_cache_dir_missing() {
        let scratch = Scratch::new("missing");
        let cache = scratch.path().join("does-not-exist");
        let report = gc(&cache, &HashSet::new()).expect("gc on missing dir is ok");
        assert_eq!(report.kept, 0);
        assert_eq!(report.removed, 0);
    }
}
