//! Add repo: what a folder picked for a new repo sends, and where the next
//! pick opens. Plain Rust, so every rule is unit-tested; the views open the
//! picker and forward its answer.

use std::path::{Path, PathBuf};

use protocol::ClientMessage;

/// What a picked folder adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoAdd {
    /// The picked folder.
    pub path: String,
    /// The repo's name: the folder's name.
    pub name: String,
    /// Where the next pick opens: the picked folder's parent, so it opens
    /// beside this one, or the folder itself when it is a drive root.
    pub last_dir: String,
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
            last_dir,
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

    use super::RepoAdd;

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
            picked(&parent.join("repo")).last_dir,
            parent.to_string_lossy()
        );
        if cfg!(windows) {
            assert_eq!(
                picked(Path::new("D:\\")).last_dir,
                "D:\\",
                "a drive root has no parent: it is its own"
            );
        }
    }

    #[test]
    fn a_cancelled_pick_sends_nothing() {
        assert_eq!(RepoAdd::from_pick(None), None);
    }
}
