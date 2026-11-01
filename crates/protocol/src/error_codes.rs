//! The catalogue of [`crate::DaemonMessage::Error`] codes: a stable,
//! machine-readable `code` beside the human `message`. A client branches on
//! the code, never on the message text, and keeps a code it does not know as
//! an opaque string.

/// A frame that is not valid JSON.
pub const MALFORMED_MESSAGE: &str = "malformed_message";
/// A request whose type is known but whose fields or values the daemon refuses.
pub const INVALID_REQUEST: &str = "invalid_request";
/// A message type, or a request, this daemon does not support.
pub const UNSUPPORTED_MESSAGE: &str = "unsupported_message";
/// A handler failed in a way no narrower code describes.
pub const INTERNAL: &str = "internal";
/// A request that only a loopback (local) connection may make.
pub const LOCAL_ONLY: &str = "local_only";
/// A device approval refused because the device is blocked.
pub const DEVICE_BLOCKED: &str = "device_blocked";
/// The session, job or other target the request names does not exist.
pub const NOT_FOUND: &str = "not_found";
/// A request for a setting or feature the daemon has retired.
pub const GONE: &str = "gone";
/// The session has no screen to read.
pub const NO_SCREEN: &str = "no_screen";
/// A tab-layout request (create, merge, move, split, close, swap, reorder,
/// extract) was refused.
pub const TAB_REFUSED: &str = "tab_refused";
/// Opening a diff, viewer or dashboard tab failed.
pub const OPEN_FAILED: &str = "open_failed";
/// A git read (status, stashes, commits, remote URL, branch-name suggestion)
/// failed.
pub const GIT_FAILED: &str = "git_failed";
/// A spawn preview or a session duplicate was refused.
pub const SPAWN_REFUSED: &str = "spawn_refused";
/// An agent setting or launch-command change was refused for a reason no
/// finer settings code names.
pub const SETTINGS_REFUSED: &str = "settings_refused";
/// A launch command's Program was refused.
pub const LAUNCH_PROGRAM_REFUSED: &str = "launch_program_refused";
/// A launch command's extra arguments were refused.
pub const LAUNCH_ARGS_REFUSED: &str = "launch_args_refused";
/// The worktrees root could not be created or used.
pub const WORKTREES_ROOT_REFUSED: &str = "worktrees_root_refused";
/// A summarizer settings change or test summary was refused.
pub const SUMMARIZER_REFUSED: &str = "summarizer_refused";
/// Resuming a session failed.
pub const RESUME_FAILED: &str = "resume_failed";
/// A repo's or workspace's preset list could not be read: an unknown repo or
/// workspace, or a `presets.json` that cannot be read or parsed.
pub const PRESETS_FAILED: &str = "presets_failed";
/// Another writer is typing into the session; retry.
pub const BUSY: &str = "busy";
/// The terminal's state does not take a chat message now.
pub const CHAT_REFUSED: &str = "chat_refused";
/// A per-connection limit was reached, such as a fifth file follow
/// ([`crate::ClientMessage::TailFile`]) on one connection; stop one first.
pub const LIMIT_REACHED: &str = "limit_reached";

/// Every code in the catalogue.
pub const ALL: &[&str] = &[
    MALFORMED_MESSAGE,
    INVALID_REQUEST,
    UNSUPPORTED_MESSAGE,
    INTERNAL,
    LOCAL_ONLY,
    DEVICE_BLOCKED,
    NOT_FOUND,
    GONE,
    NO_SCREEN,
    TAB_REFUSED,
    OPEN_FAILED,
    GIT_FAILED,
    SPAWN_REFUSED,
    SETTINGS_REFUSED,
    LAUNCH_PROGRAM_REFUSED,
    LAUNCH_ARGS_REFUSED,
    WORKTREES_ROOT_REFUSED,
    SUMMARIZER_REFUSED,
    RESUME_FAILED,
    PRESETS_FAILED,
    BUSY,
    CHAT_REFUSED,
    LIMIT_REACHED,
];

#[cfg(test)]
mod tests {
    use super::{ALL, LIMIT_REACHED};

    #[test]
    fn the_catalogue_lists_limit_reached() {
        assert!(ALL.contains(&LIMIT_REACHED), "{ALL:?}");
    }

    fn is_snake_case(code: &str) -> bool {
        !code.is_empty()
            && !code.starts_with('_')
            && !code.ends_with('_')
            && !code.contains("__")
            && code.chars().all(|c| c.is_ascii_lowercase() || c == '_')
    }

    #[test]
    fn every_code_is_unique_snake_case() {
        for code in ALL {
            assert!(is_snake_case(code), "{code} is not snake_case");
        }
        let mut sorted = ALL.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ALL.len(), "a code appears twice: {ALL:?}");
    }
}
