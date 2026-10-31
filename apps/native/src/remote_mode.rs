//! What the client may do on the daemon it connects to. A remote host's
//! files are not on this PC, so nothing opens, reveals or picks them here,
//! and its daemon's process is not this client's to stop, restart or read
//! the files of. Views ask [`Capabilities`] rather than the target.

use crate::target::Target;

/// Why a local-file action is disabled on a remote host: its tooltip, and
/// the toast when one is refused.
pub const LOCAL_FILES_REASON: &str = "Not available on a remote host: this opens files on this PC";

/// What the sidebar's empty-registry note says on the local daemon.
const SIDEBAR_NO_REPOS_NOTE: &str = "No repos or workspaces yet.";

/// What the spawn dialog's note says while the local daemon has no repo.
const SPAWN_NO_REPOS_NOTE: &str = "No repos yet.";

/// What the sidebar's empty-registry note reads on `target`: the local
/// daemon's wording, or one naming the remote host its registry is empty on.
#[must_use]
pub fn sidebar_no_repos_note(target: &Target) -> String {
    match target.short_name() {
        Some(host) => format!("No repos or workspaces on {host} yet."),
        None => SIDEBAR_NO_REPOS_NOTE.to_owned(),
    }
}

/// What the spawn dialog's note reads on `target`, as
/// [`sidebar_no_repos_note`] does.
#[must_use]
pub fn spawn_no_repos_note(target: &Target) -> String {
    match target.short_name() {
        Some(host) => format!("No repos on {host} yet."),
        None => SPAWN_NO_REPOS_NOTE.to_owned(),
    }
}

/// What the client may do on one connection target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    local_files: bool,
    daemon_management: bool,
}

impl Capabilities {
    /// A remote host's: it opens no file on this PC and manages no daemon,
    /// whether or not the window holds a connection to it.
    #[must_use]
    pub const fn remote() -> Self {
        Self {
            local_files: false,
            daemon_management: false,
        }
    }

    /// The capabilities on `target`: everything on the local daemon, none
    /// of these on a remote host.
    #[must_use]
    pub fn for_target(target: &Target) -> Self {
        let local = target.is_local();
        Self {
            local_files: local,
            daemon_management: local,
        }
    }

    /// Whether the client may open, reveal or pick the daemon's files on
    /// this PC: Reveal, Open in Explorer and VS Code, Add repo and the
    /// Browse buttons. A path link on a remote host fetches its file
    /// instead.
    #[must_use]
    pub fn opens_local_files(self) -> bool {
        self.local_files
    }

    /// Why a local-file action is disabled, or `None` when it is offered.
    #[must_use]
    pub fn local_files_refusal(self) -> Option<&'static str> {
        (!self.local_files).then_some(LOCAL_FILES_REASON)
    }

    /// Whether the client manages the daemon's process and files: Stop,
    /// the `daemon.json` handshake row, `daemon.log`, and a window close
    /// that may shut the daemon down.
    #[must_use]
    pub fn manages_daemon(self) -> bool {
        self.daemon_management
    }
}

#[cfg(test)]
mod tests {
    use super::{Capabilities, LOCAL_FILES_REASON, sidebar_no_repos_note, spawn_no_repos_note};
    use crate::target::Target;

    fn local() -> Capabilities {
        Capabilities::for_target(&Target::Local)
    }

    fn remote() -> Capabilities {
        Capabilities::for_target(&Target::tailnet("box.tail1234.ts.net", None))
    }

    #[test]
    fn local_files_open_only_on_the_local_daemon() {
        assert!(local().opens_local_files());
        assert_eq!(local().local_files_refusal(), None);
        assert!(!remote().opens_local_files());
        assert_eq!(remote().local_files_refusal(), Some(LOCAL_FILES_REASON));
        assert_eq!(
            LOCAL_FILES_REASON,
            "Not available on a remote host: this opens files on this PC"
        );
    }

    #[test]
    #[expect(clippy::expect_used, reason = "the spec's host must parse")]
    fn ssh_has_neither_local_files_nor_daemon_management() {
        let ssh = Target::parse_ssh("me@devbox", None).expect("an SSH host");
        let caps = Capabilities::for_target(&ssh);
        assert!(!caps.opens_local_files());
        assert_eq!(caps.local_files_refusal(), Some(LOCAL_FILES_REASON));
        assert!(!caps.manages_daemon());
        assert_eq!(spawn_no_repos_note(&ssh), "No repos on devbox yet.");
        assert_eq!(
            sidebar_no_repos_note(&ssh),
            "No repos or workspaces on devbox yet."
        );
    }

    #[test]
    fn daemon_is_managed_only_on_the_local_daemon() {
        assert!(local().manages_daemon());
        assert!(!remote().manages_daemon());
    }

    #[test]
    fn local_notes_keep_their_wording() {
        assert_eq!(
            sidebar_no_repos_note(&Target::Local),
            "No repos or workspaces yet."
        );
        assert_eq!(spawn_no_repos_note(&Target::Local), "No repos yet.");
    }

    #[test]
    fn remote_notes_name_the_host() {
        let target = Target::tailnet("box.tail1234.ts.net", None);
        assert_eq!(
            sidebar_no_repos_note(&target),
            "No repos or workspaces on box yet."
        );
        assert_eq!(spawn_no_repos_note(&target), "No repos on box yet.");
    }
}
