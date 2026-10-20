//! What the client may do on the daemon it connects to. A remote host's
//! files are not on this PC, so nothing opens, reveals or picks them here,
//! and its daemon's process is not this client's to stop, restart or read
//! the files of. Views ask [`Capabilities`] rather than the target.

use crate::target::Target;

/// Why a local-file action is disabled on a remote host: its tooltip, and
/// the toast when one is refused.
pub const LOCAL_FILES_REASON: &str = "Not available on a remote host: this opens files on this PC";

/// The toast a Ctrl+click on a path link shows on a remote host.
pub const PATH_LINK_REASON: &str = "Paths on a remote host can't be opened from this PC yet";

/// What the client may do on one connection target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    local_files: bool,
    daemon_management: bool,
}

impl Capabilities {
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
    /// this PC: Reveal, Open in Explorer and VS Code, path links, Add repo
    /// and the Browse buttons.
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
    use super::{Capabilities, LOCAL_FILES_REASON};
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
    fn daemon_is_managed_only_on_the_local_daemon() {
        assert!(local().manages_daemon());
        assert!(!remote().manages_daemon());
    }
}
