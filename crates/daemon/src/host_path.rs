//! Judging a host path a client names, by its text alone, before any disk
//! work: [`check`] refuses what no request may name and gives the absolute
//! path with `.` and `..` applied, and [`verbatim`] spells a checked path so
//! the operating system takes each of its names literally.

use std::path::{Component, Path, PathBuf, Prefix};

/// The longest path a request takes, in characters.
const MAX_PATH_CHARS: usize = 1024;
/// The most components a requested path may have.
const MAX_PATH_DEPTH: usize = 64;

/// The path `path` names, absolute and with `.` and `..` applied, judged
/// from its text alone: a relative `path` is taken from `working`, the
/// session's working folder. Touches no disk.
///
/// # Errors
/// Why the path is refused: empty, longer than [`MAX_PATH_CHARS`], deeper
/// than [`MAX_PATH_DEPTH`], a network or device path, drive- or
/// root-relative, climbing above its root, or relative with no `working`.
pub fn check(working: Option<&Path>, path: &str) -> Result<PathBuf, String> {
    check_shape(path)?;
    let named = Path::new(path);
    let joined = if named.is_absolute() {
        named.to_path_buf()
    } else {
        let Some(working) = working else {
            return Err("the session has no folder".to_owned());
        };
        working.join(named)
    };
    normalize(&joined).ok_or_else(|| format!("{path:?} climbs above its root"))
}

/// Refuse a path by its shape: empty, too long, too deep, naming a network
/// share or a device, or relative to a drive or root.
fn check_shape(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("the path is empty".to_owned());
    }
    if path.chars().count() > MAX_PATH_CHARS {
        return Err(format!(
            "the path is longer than {MAX_PATH_CHARS} characters"
        ));
    }
    if is_network_or_device(path) {
        return Err(format!("{path:?} names a network or device path"));
    }
    let named = Path::new(path);
    if named.components().count() > MAX_PATH_DEPTH {
        return Err(format!(
            "the path is more than {MAX_PATH_DEPTH} components deep"
        ));
    }
    let starts_bare = matches!(named.components().next(), Some(Component::Prefix(_)));
    if !named.is_absolute() && (named.has_root() || starts_bare) {
        return Err(format!(
            "{path:?} is relative to a drive or root, not a folder"
        ));
    }
    Ok(())
}

/// Whether `path` opens with two separators (a UNC share, `\\?\`, `\\.\`)
/// or a Windows prefix other than a drive letter.
fn is_network_or_device(path: &str) -> bool {
    let mut chars = path.chars();
    let two_separators = matches!(
        (chars.next(), chars.next()),
        (Some('\\' | '/'), Some('\\' | '/'))
    );
    let odd_prefix = matches!(
        Path::new(path).components().next(),
        Some(Component::Prefix(prefix)) if !matches!(prefix.kind(), Prefix::Disk(_))
    );
    two_separators || odd_prefix
}

/// `path` with `.` dropped and each `..` taking off the component before it;
/// `None` when a `..` would climb above the root.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    return None;
                }
                out.pop();
            }
            other => out.push(other),
        }
    }
    Some(out)
}

/// `normal`, an absolute disk path, spelled verbatim: `\\?\`, its drive and
/// its names joined with `\`, `.` dropped, so a name with a trailing dot or
/// space, a reserved name such as `CON` or `NUL.txt`, or a stream name such
/// as `f.txt:s` names exactly that entry and is never rewritten by Win32. Any
/// other path (a share, one already verbatim, a relative one, one holding
/// `..`) is given back unchanged.
#[cfg(windows)]
#[must_use]
pub fn verbatim(normal: &Path) -> PathBuf {
    let mut components = normal.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) => drive,
            _ => return normal.to_path_buf(),
        },
        _ => return normal.to_path_buf(),
    };
    if components.next() != Some(Component::RootDir) {
        return normal.to_path_buf();
    }
    let mut text = std::ffi::OsString::from(format!(r"\\?\{}:", char::from(drive)));
    let mut named = false;
    for component in components {
        match component {
            Component::Normal(name) => {
                text.push(r"\");
                text.push(name);
                named = true;
            }
            Component::CurDir => {}
            _ => return normal.to_path_buf(),
        }
    }
    if !named {
        text.push(r"\");
    }
    PathBuf::from(text)
}

/// `normal` itself: only Windows rewrites a name before the file system sees
/// it.
#[cfg(not(windows))]
#[must_use]
pub fn verbatim(normal: &Path) -> PathBuf {
    normal.to_path_buf()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; panic messages aid debugging"
)]
mod tests {
    use super::*;

    /// The working folder a relative row is taken from; never touched.
    fn working() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\work")
        } else {
            PathBuf::from("/work")
        }
    }

    #[test]
    fn check_refuses_each_shape_by_its_reason() {
        let long = "a".repeat(MAX_PATH_CHARS + 1);
        let deep = "a/".repeat(MAX_PATH_DEPTH + 1);
        let mut rows: Vec<(&str, &str)> = vec![
            ("", "the path is empty"),
            (long.as_str(), "longer than"),
            (deep.as_str(), "components deep"),
            (r"\\srv\sh", "network or device"),
            ("//?/C:/x", "network or device"),
        ];
        if cfg!(windows) {
            rows.extend([
                (r"\\?\C:\x", "network or device"),
                (r"\\.\pipe\x", "network or device"),
                (r"\??\C:\x", "relative to a drive or root"),
                ("C:x", "relative to a drive or root"),
                (r"\x", "relative to a drive or root"),
                (r"C:\..\x", "climbs above its root"),
            ]);
        } else {
            rows.push(("/../x", "climbs above its root"));
        }
        let folder = working();
        for (path, why) in rows {
            let err = check(Some(&folder), path).expect_err(path);
            assert!(err.contains(why), "{path:?}: {err}");
        }
    }

    #[test]
    fn check_takes_a_relative_path_from_the_working_folder() {
        let folder = working();
        assert_eq!(
            check(Some(&folder), "logs/./a/../b.log"),
            Ok(folder.join("logs").join("b.log"))
        );
        assert_eq!(
            check(None, "a.log"),
            Err("the session has no folder".to_owned())
        );
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_spells_each_name_literally() {
        let rows = [
            (r"C:\a\b", r"\\?\C:\a\b"),
            (r"C:\a\f.txt.", r"\\?\C:\a\f.txt."),
            (r"C:\a\b ", r"\\?\C:\a\b "),
            (r"C:\a\CON", r"\\?\C:\a\CON"),
            (r"C:\a\NUL.txt", r"\\?\C:\a\NUL.txt"),
            (r"C:\a\f.txt:s", r"\\?\C:\a\f.txt:s"),
            ("C:/a/./b", r"\\?\C:\a\b"),
            (r"C:\", r"\\?\C:\"),
            (r"\\srv\sh\x", r"\\srv\sh\x"),
            (r"\\?\C:\x", r"\\?\C:\x"),
            (r"C:\a\..\b", r"C:\a\..\b"),
            (r"a\b", r"a\b"),
        ];
        for (normal, expected) in rows {
            let spelled = verbatim(Path::new(normal));
            assert_eq!(spelled.as_os_str(), expected, "{normal:?}");
        }
        let spelled = verbatim(Path::new(r"C:\a\CON"));
        assert_eq!(
            spelled.file_name().and_then(|name| name.to_str()),
            Some("CON"),
            "CON stays a component"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn verbatim_leaves_a_path_as_it_is() {
        for normal in ["/a/b", "/a/f.txt.", "/a/CON"] {
            assert_eq!(verbatim(Path::new(normal)), PathBuf::from(normal));
        }
    }
}
