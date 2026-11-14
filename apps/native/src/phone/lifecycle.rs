//! What the phone does with its saved hosts when it comes back to the
//! foreground: plain Rust over resolved facts, so the rule can be read and
//! tested without a window.

use crate::connection::State;
use crate::target::{HostKey, Target};

/// What returning to the foreground does for one saved host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Redial {
    /// Redial the held connection now (`NetCommand::Restart`).
    Restart(HostKey),
    /// Open a connection the window does not hold.
    Connect(Target),
}

/// One saved host as the rule sees it.
pub(crate) struct SavedHostNow<'a> {
    /// The host the saved record names.
    pub(crate) target: Target,
    /// Whether the window connects to this host at all.
    pub(crate) enabled: bool,
    /// The held connection's state; `None` when the window holds none.
    pub(crate) state: Option<&'a State>,
    /// The host's connection was dropped as another host's duplicate.
    pub(crate) duplicate: bool,
}

/// What becoming visible (`true`) or hidden does, host by host in saved
/// order: a hidden window sends nothing, and a visible one redials each
/// enabled host whose connection closed or failed and connects each enabled
/// host the window holds no connection to.
pub(crate) fn on_visibility(visible: bool, hosts: &[SavedHostNow<'_>]) -> Vec<Redial> {
    if !visible {
        return Vec::new();
    }
    hosts.iter().filter_map(redial).collect()
}

/// What returning to the foreground does for one saved host: nothing when it
/// is switched off or was dropped as another host's duplicate, a connect when
/// the window holds no connection to it, a restart when its held connection
/// closed or failed, and otherwise nothing.
fn redial(host: &SavedHostNow<'_>) -> Option<Redial> {
    if !host.enabled || host.duplicate {
        return None;
    }
    match host.state {
        None => Some(Redial::Connect(host.target.clone())),
        Some(State::Closed { .. } | State::Error { .. }) => {
            Some(Redial::Restart(HostKey::of(&host.target)))
        }
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Redial, SavedHostNow, on_visibility};
    use crate::connection::State;
    use crate::target::{HostKey, Target};

    /// A tailnet host's `MagicDNS` name.
    const BOX: &str = "box.tail1234.ts.net";
    /// A second tailnet host's `MagicDNS` name.
    const DEN: &str = "den.tail1234.ts.net";

    /// The target of a tailnet host saved on port 8787.
    fn tailnet(name: &str) -> Target {
        Target::tailnet(name, Some(8787))
    }

    /// One enabled saved host, not a duplicate, in `state`.
    fn host<'a>(name: &str, state: Option<&'a State>) -> SavedHostNow<'a> {
        SavedHostNow {
            target: tailnet(name),
            enabled: true,
            state,
            duplicate: false,
        }
    }

    /// The key the window holds a host by.
    fn key(name: &str) -> HostKey {
        HostKey::of(&tailnet(name))
    }

    /// A first attempt, not yet made.
    fn init() -> State {
        State::Init
    }

    /// An attempt under way.
    fn connecting() -> State {
        State::Connecting
    }

    /// A live connection.
    fn open() -> State {
        State::Open {
            negotiated: 24,
            features: Vec::new(),
            origin: None,
            build_version: None,
        }
    }

    /// A socket that closed, its reconnect pending.
    fn closed() -> State {
        State::Closed {
            reason: "suspended".to_owned(),
        }
    }

    /// A failed attempt, its reconnect pending.
    fn error() -> State {
        State::Error {
            reason: "the daemon did not start".to_owned(),
        }
    }

    /// A refused token.
    fn auth_failed() -> State {
        State::AuthFailed {
            reason: "bad token".to_owned(),
        }
    }

    /// A host that has not approved this phone.
    fn not_approved() -> State {
        State::NotApproved {
            reason: "not approved".to_owned(),
            had_token: false,
        }
    }

    #[test]
    fn hidden_sends_nothing() {
        let states = [
            None,
            Some(init()),
            Some(connecting()),
            Some(open()),
            Some(closed()),
            Some(error()),
            Some(auth_failed()),
            Some(not_approved()),
        ];
        for state in states {
            let hosts = [host(BOX, state.as_ref())];
            assert_eq!(
                on_visibility(false, &hosts),
                Vec::new(),
                "a hidden window acts on no host"
            );
        }
    }

    #[test]
    fn an_attempt_due_is_left_alone() {
        let state = init();
        let hosts = [host(BOX, Some(&state))];
        assert_eq!(on_visibility(true, &hosts), Vec::new());
    }

    #[test]
    fn an_attempt_under_way_is_left_alone() {
        let state = connecting();
        let hosts = [host(BOX, Some(&state))];
        assert_eq!(on_visibility(true, &hosts), Vec::new());
    }

    #[test]
    fn an_open_host_is_left_alone() {
        let state = open();
        let hosts = [host(BOX, Some(&state))];
        assert_eq!(on_visibility(true, &hosts), Vec::new());
    }

    #[test]
    fn a_closed_host_redials() {
        let state = closed();
        let hosts = [host(BOX, Some(&state))];
        assert_eq!(on_visibility(true, &hosts), vec![Redial::Restart(key(BOX))]);
    }

    #[test]
    fn a_failed_host_redials() {
        let state = error();
        let hosts = [host(BOX, Some(&state))];
        assert_eq!(on_visibility(true, &hosts), vec![Redial::Restart(key(BOX))]);
    }

    #[test]
    fn a_refused_token_is_left_alone() {
        let state = auth_failed();
        let hosts = [host(BOX, Some(&state))];
        assert_eq!(on_visibility(true, &hosts), Vec::new());
    }

    #[test]
    fn a_pairing_connection_is_left_alone() {
        let state = not_approved();
        let hosts = [host(BOX, Some(&state))];
        assert_eq!(on_visibility(true, &hosts), Vec::new());
    }

    #[test]
    fn a_host_not_held_connects() {
        let hosts = [host(BOX, None)];
        assert_eq!(
            on_visibility(true, &hosts),
            vec![Redial::Connect(tailnet(BOX))]
        );
    }

    #[test]
    fn a_duplicate_host_stays_dropped() {
        let hosts = [SavedHostNow {
            target: tailnet(BOX),
            enabled: true,
            state: None,
            duplicate: true,
        }];
        assert_eq!(on_visibility(true, &hosts), Vec::new());
    }

    #[test]
    fn a_switched_off_host_gets_nothing() {
        let state = closed();
        for state in [None, Some(&state)] {
            let hosts = [SavedHostNow {
                target: tailnet(BOX),
                enabled: false,
                state,
                duplicate: false,
            }];
            assert_eq!(on_visibility(true, &hosts), Vec::new());
        }
    }

    #[test]
    fn only_the_dropped_hosts_redial_in_saved_order() {
        let (state, dropped) = (open(), closed());
        let hosts = [
            host(BOX, Some(&state)),
            host(DEN, Some(&dropped)),
            host("lab.tail1234.ts.net", None),
        ];
        assert_eq!(
            on_visibility(true, &hosts),
            vec![
                Redial::Restart(key(DEN)),
                Redial::Connect(tailnet("lab.tail1234.ts.net")),
            ]
        );
    }
}
