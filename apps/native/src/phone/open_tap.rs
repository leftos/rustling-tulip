//! What a tap on a phone notification does: the saved host and session it
//! names, and, once that host has listed them, whether the phone opens the
//! session or says why it cannot. Plain Rust over the facts the phone's root
//! reads, so every rule reads and tests without a window.

use crate::target::{HostKey, Target};

/// The host name a tap names, normalised as a saved tailnet host's is: its
/// surrounding space and trailing dots trimmed and its letters lower-cased.
#[must_use]
pub(super) fn normalise(host: &str) -> String {
    host.trim().trim_end_matches('.').to_lowercase()
}

/// The key of the saved host a tap on `host` names: the tap's name
/// normalised. A tap carries a name, not an identity, so a host saved under
/// another spelling never matches and nothing falls back to it.
#[must_use]
pub(super) fn target_key(host: &str) -> HostKey {
    HostKey::of(&Target::tailnet(&normalise(host), None))
}

/// A tap waiting for its host to list the session it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingOpen {
    /// The key of the saved host the tap names.
    pub(super) host: HostKey,
    /// The session the notification was about.
    pub(super) session_id: String,
}

impl PendingOpen {
    /// The host's name as the wire spells it, for the lines that name it.
    #[must_use]
    pub(super) fn host_name(&self) -> &str {
        self.host.wire().unwrap_or_default()
    }
}

/// What the phone's root knows about a pending open's host.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each field is a fact the root reads about the host, not a flag of one state"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Facts {
    /// Whether a saved host has the pending open's key.
    pub(super) saved: bool,
    /// Whether that host is paired with this phone: it holds a device token
    /// and its connection never refused this phone.
    pub(super) paired: bool,
    /// Whether that host is open and has sent its whole session list since
    /// the app last came to the foreground.
    pub(super) loaded: bool,
    /// Whether the loaded list holds the session.
    pub(super) listed: bool,
}

/// What a pending open does now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Next {
    /// Nothing yet: a later root change decides it.
    Wait,
    /// Open the session.
    Open,
    /// Give up, with the line the phone shows.
    Note(String),
}

/// What a tap on `host`'s session does against `facts`: a loaded host's list
/// decides on its own, opening the session or saying it is gone; a host no
/// saved host matches, and a saved host this phone is not paired with, say
/// so; anything else waits for the host's list.
pub(super) fn decide(host: &str, facts: Facts) -> Next {
    if facts.loaded && facts.listed {
        return Next::Open;
    }
    if facts.loaded {
        return Next::Note(format!("That session is no longer on {host}."));
    }
    if !facts.saved {
        return Next::Note(format!("{host} is not saved on this phone."));
    }
    if !facts.paired {
        return Next::Note(format!("{host} is not paired with this phone."));
    }
    Next::Wait
}

#[cfg(test)]
#[expect(
    clippy::panic,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::{Facts, Next, decide, normalise, target_key};
    use crate::target::{HostKey, Target};

    /// A tailnet host's wire name.
    const BOX: &str = "box.tail1234.ts.net";

    /// A saved, paired host open with its whole session list, the session
    /// not listed.
    const LOADED: Facts = Facts {
        saved: true,
        paired: true,
        loaded: true,
        listed: false,
    };

    /// The line a note carries.
    fn noted(next: Next) -> String {
        match next {
            Next::Note(text) => text,
            other => panic!("a note: {other:?}"),
        }
    }

    #[test]
    fn a_listed_session_on_a_loaded_host_opens() {
        let facts = Facts {
            listed: true,
            ..LOADED
        };
        assert_eq!(decide(BOX, facts), Next::Open);
    }

    #[test]
    fn an_unloaded_host_waits() {
        let connecting = Facts {
            loaded: false,
            ..LOADED
        };
        assert_eq!(decide(BOX, connecting), Next::Wait);
    }

    #[test]
    fn a_missing_session_on_a_loaded_host_notes_it() {
        assert_eq!(
            noted(decide(BOX, LOADED)),
            "That session is no longer on box.tail1234.ts.net."
        );
    }

    #[test]
    fn an_unsaved_host_notes_it() {
        let unsaved = Facts {
            saved: false,
            paired: false,
            loaded: false,
            listed: false,
        };
        assert_eq!(
            noted(decide(BOX, unsaved)),
            "box.tail1234.ts.net is not saved on this phone."
        );
    }

    #[test]
    fn a_host_not_paired_notes_it() {
        let unpaired = Facts {
            paired: false,
            loaded: false,
            ..LOADED
        };
        assert_eq!(
            noted(decide(BOX, unpaired)),
            "box.tail1234.ts.net is not paired with this phone."
        );
    }

    #[test]
    fn the_host_name_is_matched_as_saved() {
        let saved = HostKey::of(&Target::tailnet(BOX, Some(8787)));
        assert_eq!(target_key(" Box.Tail1234.TS.Net. "), saved);
        assert_eq!(target_key(BOX), saved);
        assert_ne!(target_key("lab.tail1234.ts.net"), saved);
    }

    #[test]
    fn normalise_trims_the_space_and_dots_and_lowercases() {
        assert_eq!(normalise("  Box.Tail1234.TS.Net..  "), BOX);
        assert_eq!(normalise(BOX), BOX);
    }
}
