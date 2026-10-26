//! Add repo: what a folder picked for a new repo sends, and where the next
//! pick opens; on a remote host, what a typed host path sends, and the
//! string rules a typed host path must pass. Plain Rust, so every rule is
//! unit-tested; the views open the picker or the prompt and forward the
//! answer.
//!
//! A host path's flavour comes from its text, since the host may run
//! another OS than this PC: a drive letter or `\\server` is Windows, a
//! leading `/` is POSIX. Whether the path exists is the daemon's to say.

use std::path::{Path, PathBuf};

use protocol::ClientMessage;

use crate::fetch_link::{split_anchor, unify};
use crate::file_fetch::display_host_path;

/// What a picked folder or a typed host path adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoAdd {
    /// The picked folder, or the host path as typed, trimmed.
    pub path: String,
    /// The repo's name: the folder's name.
    pub name: String,
    /// Where the next pick opens: the picked folder's parent, so it opens
    /// beside this one, or the folder itself when it is a drive root.
    /// `None` for a typed host path, which is never remembered.
    pub last_dir: Option<String>,
}

/// Why a typed host path is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostPathProblem {
    Empty,
    /// An empty field that asks for a file, not a folder.
    EmptyFile,
    /// Not a drive, `\\server` or `/` path.
    NotAbsolute,
    /// A `..` segment, which the daemon would resolve before naming it.
    ParentSegment,
    /// A workspace file path that does not name a `.code-workspace` file.
    NotWorkspaceFile,
}

impl HostPathProblem {
    /// What the field's refusal says, naming `host` (the connection's
    /// short name).
    pub(crate) fn message(self, host: &str) -> String {
        match self {
            Self::Empty => format!("Type a folder path on {host}."),
            Self::EmptyFile => format!("Type a file path on {host}."),
            Self::NotAbsolute => {
                format!("Use a full path on {host}, starting with a drive letter, \\\\server or /.")
            }
            Self::ParentSegment => "Use a path without .. in it.".to_owned(),
            Self::NotWorkspaceFile => "Type the path of a .code-workspace file.".to_owned(),
        }
    }
}

/// A typed host path that passed the string rules.
struct HostPathText {
    /// The text as typed, trimmed.
    text: String,
    /// The last non-empty segment other than `.`, or for a root its anchor
    /// (`D:` for `D:\`, `/` on POSIX).
    name: String,
    /// The text after the last separator, empty when it ends in one.
    last_raw: String,
}

/// Whether `text` passes the host path string rules of
/// [`check_host_path`].
///
/// # Errors
/// The [`HostPathProblem`] that refuses it.
pub(crate) fn check_host_path_rules(text: &str) -> Result<(), HostPathProblem> {
    check_host_path(text).map(|_| ())
}

/// `text` against the string rules: trimmed, not empty, absolute in either
/// flavour, no `..` segment.
fn check_host_path(text: &str) -> Result<HostPathText, HostPathProblem> {
    let text = text.trim();
    if text.is_empty() {
        return Err(HostPathProblem::Empty);
    }
    let unified = unify(text);
    let (anchor, rest) = split_anchor(&unified).ok_or(HostPathProblem::NotAbsolute)?;
    // A bare drive (`C:`) is relative to that drive's current folder, and
    // `\\` alone names no server.
    // A lone leading `\` is relative to the current drive.
    let bare_drive = anchor.ends_with(':') && rest.is_empty();
    let no_server = anchor == "//" && rest.trim_matches('/').is_empty();
    let drive_relative = anchor == "/" && !text.starts_with('/');
    if bare_drive || no_server || drive_relative {
        return Err(HostPathProblem::NotAbsolute);
    }
    // Both Windows anchors (`C:` and `\\`) are two characters; a POSIX
    // name may hold a `\`, so only `/` separates there.
    let (root, after_anchor, separators): (String, String, &[char]) = if anchor == "/" {
        ("/".to_owned(), text[1..].to_owned(), &['/'])
    } else {
        let shown = display_host_path(text);
        let (head, tail) = shown.split_at(2);
        (head.to_owned(), tail.to_owned(), &['\\', '/'])
    };
    let segments: Vec<&str> = after_anchor.split(separators).collect();
    if segments.contains(&"..") {
        return Err(HostPathProblem::ParentSegment);
    }
    let name = segments
        .iter()
        .rev()
        .find(|segment| !segment.is_empty() && **segment != ".")
        .map_or(root, |segment| (*segment).to_owned());
    Ok(HostPathText {
        text: text.to_owned(),
        name,
        last_raw: segments.last().copied().unwrap_or_default().to_owned(),
    })
}

/// A typed `.code-workspace` file on the host: the text, trimmed, when it
/// passes the host path rules and names such a file (any case).
///
/// # Errors
/// The [`HostPathProblem`] that refuses it; an empty field is
/// [`HostPathProblem::NotWorkspaceFile`].
pub(crate) fn host_workspace_file(text: &str) -> Result<String, HostPathProblem> {
    let checked = check_host_path(text).map_err(|problem| match problem {
        HostPathProblem::Empty => HostPathProblem::NotWorkspaceFile,
        other => other,
    })?;
    if checked.last_raw.to_lowercase().ends_with(".code-workspace") {
        Ok(checked.text)
    } else {
        Err(HostPathProblem::NotWorkspaceFile)
    }
}

impl RepoAdd {
    /// The picker's answer as an Add repo; `None` for a cancel, which sends
    /// nothing.
    pub(crate) fn from_pick(picked: Option<PathBuf>) -> Option<Self> {
        let folder = picked?;
        let last_dir = folder
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(&folder)
            .to_string_lossy()
            .into_owned();
        Some(Self {
            path: folder.to_string_lossy().into_owned(),
            name: repo_name(&folder),
            last_dir: Some(last_dir),
        })
    }

    /// A folder path typed for a remote host as an Add repo: the text,
    /// trimmed, named by its last segment, and never remembered.
    ///
    /// # Errors
    /// The [`HostPathProblem`] that refuses it.
    pub(crate) fn from_host_path(text: &str) -> Result<Self, HostPathProblem> {
        let checked = check_host_path(text)?;
        Ok(Self {
            path: checked.text,
            name: checked.name,
            last_dir: None,
        })
    }

    /// The request that registers the folder.
    pub(crate) fn message(&self) -> ClientMessage {
        ClientMessage::AddRepo {
            path: self.path.clone(),
            name: Some(self.name.clone()),
        }
    }
}

/// The repo's name: the folder's last path component, or for a drive root
/// the drive itself (`D:\` is `D:`).
fn repo_name(folder: &Path) -> String {
    if let Some(name) = folder.file_name() {
        return name.to_string_lossy().into_owned();
    }
    let text = folder.to_string_lossy();
    let trimmed = text.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        text.into_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use std::path::{MAIN_SEPARATOR, Path, PathBuf};

    use protocol::ClientMessage;

    use super::{HostPathProblem, RepoAdd, host_workspace_file};

    fn picked(folder: &Path) -> RepoAdd {
        RepoAdd::from_pick(Some(folder.to_path_buf())).expect("a pick adds a repo")
    }

    #[test]
    fn name_is_the_folder_name() {
        let folder = Path::new("C:\\work").join("rustling-tulip");
        let add = picked(&folder);
        assert_eq!(add.name, "rustling-tulip");
        assert!(
            matches!(
                add.message(),
                ClientMessage::AddRepo { path, name: Some(name) }
                    if path == folder.to_string_lossy() && name == "rustling-tulip"
            ),
            "the request carries the folder and its name: {:?}",
            add.message()
        );
        let trailing = PathBuf::from(format!("{}{MAIN_SEPARATOR}", folder.display()));
        assert_eq!(
            picked(&trailing).name,
            "rustling-tulip",
            "a trailing separator"
        );
        if cfg!(windows) {
            assert_eq!(
                picked(Path::new("D:\\")).name,
                "D:",
                "a drive root is its drive"
            );
        } else {
            assert_eq!(picked(Path::new("/")).name, "/", "the root keeps a name");
        }
    }

    #[test]
    fn last_dir_is_the_picked_folders_parent() {
        let parent = Path::new("C:\\work");
        assert_eq!(
            picked(&parent.join("repo")).last_dir.as_deref(),
            Some(&*parent.to_string_lossy())
        );
        if cfg!(windows) {
            assert_eq!(
                picked(Path::new("D:\\")).last_dir.as_deref(),
                Some("D:\\"),
                "a drive root has no parent: it is its own"
            );
        }
    }

    #[test]
    fn a_cancelled_pick_sends_nothing() {
        assert_eq!(RepoAdd::from_pick(None), None);
    }

    fn typed(text: &str) -> RepoAdd {
        RepoAdd::from_host_path(text).expect("a host path adds a repo")
    }

    fn refused(text: &str) -> HostPathProblem {
        RepoAdd::from_host_path(text).expect_err("a refused host path")
    }

    #[test]
    fn host_path_accepts_a_windows_drive_path() {
        let add = typed("  C:\\work\\rustling-tulip  ");
        assert_eq!(add.path, "C:\\work\\rustling-tulip", "trimmed, as typed");
        assert_eq!(add.name, "rustling-tulip");
        assert_eq!(add.last_dir, None, "a host path is never remembered");
        assert!(
            matches!(
                add.message(),
                ClientMessage::AddRepo { path, name: Some(name) }
                    if path == "C:\\work\\rustling-tulip" && name == "rustling-tulip"
            ),
            "the request carries the typed path and its name: {:?}",
            add.message()
        );
        assert_eq!(typed("d:/src/app").name, "app", "forward slashes");
        assert_eq!(
            typed("\\\\?\\C:\\work\\repo").name,
            "repo",
            "a verbatim path"
        );
    }

    #[test]
    fn host_path_accepts_a_unc_path() {
        assert_eq!(typed("\\\\server\\share\\repo").name, "repo");
        assert_eq!(typed("//server/share").name, "share");
        assert_eq!(
            typed("\\\\server\\share\\Repo").path,
            "\\\\server\\share\\Repo"
        );
    }

    #[test]
    fn host_path_accepts_a_posix_path() {
        let add = typed("/home/me/repo");
        assert_eq!(add.path, "/home/me/repo");
        assert_eq!(add.name, "repo");
        assert_eq!(typed("/").name, "/", "the root keeps a name");
        assert_eq!(
            typed("/home/me/a\\b").name,
            "a\\b",
            "a backslash is part of a POSIX name"
        );
    }

    #[test]
    fn host_path_refuses_empty_and_relative() {
        for text in ["", "   "] {
            assert_eq!(refused(text), HostPathProblem::Empty, "{text:?}");
        }
        for text in [
            "repo",
            "work\\repo",
            "~/repo",
            "C:",
            "C:repo",
            ".\\x",
            "\\\\",
        ] {
            assert_eq!(refused(text), HostPathProblem::NotAbsolute, "{text:?}");
        }
        assert_eq!(
            HostPathProblem::Empty.message("box"),
            "Type a folder path on box."
        );
        assert_eq!(
            HostPathProblem::NotAbsolute.message("box"),
            "Use a full path on box, starting with a drive letter, \\\\server or /."
        );
    }

    #[test]
    fn host_path_refuses_dot_dot() {
        for text in [
            "C:\\work\\..\\x",
            "C:/work/..",
            "/home/../x",
            "/home/me/..",
            "\\\\s\\..\\x",
        ] {
            assert_eq!(refused(text), HostPathProblem::ParentSegment, "{text:?}");
        }
        assert_eq!(
            HostPathProblem::ParentSegment.message("box"),
            "Use a path without .. in it."
        );
        assert_eq!(typed("C:\\a..b").name, "a..b", "dots inside a name");
    }

    #[test]
    fn host_repo_name_is_the_last_segment() {
        assert_eq!(typed("C:\\work\\repo\\").name, "repo");
        assert_eq!(typed("C:/work/repo//").name, "repo");
        assert_eq!(typed("C:\\work\\repo\\.").name, "repo", "a . is no name");
        assert_eq!(typed("/home/me/repo/").name, "repo");
        assert_eq!(typed("/home/me/repo//").name, "repo");
        assert_eq!(typed("D:\\").name, "D:", "a drive root is its drive");
        assert_eq!(typed("D:/").name, "D:");
    }

    #[test]
    fn workspace_file_must_end_in_code_workspace() {
        for text in [
            "C:\\ws\\team.code-workspace",
            " C:\\ws\\TEAM.Code-Workspace ",
            "/home/me/w.code-workspace",
            "\\\\server\\share\\w.code-workspace",
        ] {
            assert_eq!(
                host_workspace_file(text).as_deref(),
                Ok(text.trim()),
                "{text:?}"
            );
        }
        for text in [
            "C:\\ws\\team.json",
            "C:\\ws\\team.code-workspace\\",
            "/home/me/.code-workspace/",
            "",
        ] {
            assert_eq!(
                host_workspace_file(text),
                Err(HostPathProblem::NotWorkspaceFile),
                "{text:?}"
            );
        }
        assert_eq!(
            host_workspace_file("team.code-workspace"),
            Err(HostPathProblem::NotAbsolute)
        );
        assert_eq!(
            host_workspace_file("C:\\ws\\..\\w.code-workspace"),
            Err(HostPathProblem::ParentSegment)
        );
        assert_eq!(
            HostPathProblem::NotWorkspaceFile.message("box"),
            "Type the path of a .code-workspace file."
        );
    }
}
