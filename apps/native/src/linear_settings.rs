//! What each host's daemon last said about its Linear settings, for the
//! Settings modal's Linear tab: its status, the Test it ran for this
//! window, and the refusal of the key this window last sent. Plain Rust,
//! fed from the root view's message folds.

use std::collections::HashMap;

use protocol::{DaemonMessage, LinearKeyState};

use crate::dashboard_rows::DASH;
use crate::target::HostKey;

/// One daemon's `LinearStatus`, as it last sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinearStatus {
    pub(crate) enabled: bool,
    pub(crate) key: LinearKeyState,
    pub(crate) workspace_name: Option<String>,
    pub(crate) team_keys: Vec<String>,
    pub(crate) last_error: Option<String>,
    /// Whether the key in use is the one saved in the host's secret store.
    pub(crate) key_saved: bool,
}

/// Where the Test this window asked a host for stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TestOutcome {
    /// Sent; no answer yet.
    Waiting,
    /// The key reached `workspace`, whose teams are `team_keys`.
    Reply {
        workspace: String,
        team_keys: Vec<String>,
    },
    /// The daemon answered or refused the Test with this error.
    Failed(String),
}

/// The Test last sent to a host and where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TestRun {
    request_id: String,
    outcome: TestOutcome,
}

/// One host's Linear settings, as the Linear tab reads them.
#[derive(Debug, Default)]
struct HostLinear {
    status: Option<LinearStatus>,
    test: Option<TestRun>,
    /// The request id of the newest key save or clear, until it is refused.
    key_request: Option<String>,
    /// The daemon's refusal of the last key save or clear.
    key_error: Option<String>,
}

/// Every host's Linear settings state.
#[derive(Debug, Default)]
pub(crate) struct LinearSettingsState {
    hosts: HashMap<HostKey, HostLinear>,
}

impl LinearSettingsState {
    /// Folds what `msg` from `host` says about its Linear settings: a
    /// status replaces the old one, a Test reply lands when it answers the
    /// Test last sent, and a `Welcome` forgets everything the host's old
    /// connection said. Returns whether `msg` changed anything.
    pub(crate) fn fold(&mut self, host: &HostKey, msg: &DaemonMessage) -> bool {
        match msg {
            DaemonMessage::LinearStatus {
                enabled,
                key,
                workspace_name,
                team_keys,
                last_error,
                key_saved,
            } => {
                self.hosts.entry(host.clone()).or_default().status = Some(LinearStatus {
                    enabled: *enabled,
                    key: *key,
                    workspace_name: workspace_name.clone(),
                    team_keys: team_keys.clone(),
                    last_error: last_error.clone(),
                    key_saved: *key_saved,
                });
                true
            }
            DaemonMessage::LinearTest {
                workspace_name,
                team_keys,
                error,
                request_id,
            } => {
                let outcome = match error {
                    Some(error) => TestOutcome::Failed(error.clone()),
                    None => TestOutcome::Reply {
                        workspace: workspace_name.clone().unwrap_or_else(|| DASH.to_owned()),
                        team_keys: team_keys.clone(),
                    },
                };
                self.settle_test(host, request_id.as_deref(), outcome)
            }
            DaemonMessage::Welcome { .. } => self.hosts.remove(host).is_some(),
            _ => false,
        }
    }

    /// A daemon `Error` from `host` answering `request_id`: the Test it
    /// answers fails with `message`, or the key send it answers shows
    /// `message` under the key field. Returns whether it answered either.
    pub(crate) fn refused(&mut self, host: &HostKey, request_id: &str, message: &str) -> bool {
        if self.settle_test(
            host,
            Some(request_id),
            TestOutcome::Failed(message.to_owned()),
        ) {
            return true;
        }
        let Some(state) = self.hosts.get_mut(host) else {
            return false;
        };
        if state.key_request.as_deref() != Some(request_id) {
            return false;
        }
        state.key_request = None;
        state.key_error = Some(message.to_owned());
        true
    }

    /// The Test `request_id` was sent to `host`; an earlier one's answer
    /// no longer lands.
    pub(crate) fn start_test(&mut self, host: &HostKey, request_id: String) {
        self.hosts.entry(host.clone()).or_default().test = Some(TestRun {
            request_id,
            outcome: TestOutcome::Waiting,
        });
    }

    /// The key save or clear `request_id` was sent to `host`; the last
    /// refusal goes, and an older send's refusal no longer lands.
    pub(crate) fn sent_key(&mut self, host: &HostKey, request_id: String) {
        let state = self.hosts.entry(host.clone()).or_default();
        state.key_request = Some(request_id);
        state.key_error = None;
    }

    /// `host`'s Linear status, once its daemon sent one.
    pub(crate) fn status(&self, host: &HostKey) -> Option<&LinearStatus> {
        self.hosts.get(host)?.status.as_ref()
    }

    /// Whether `host`'s daemon has Linear on, once it said.
    pub(crate) fn enabled(&self, host: &HostKey) -> Option<bool> {
        self.status(host).map(|status| status.enabled)
    }

    /// Where the Test last sent to `host` stands.
    pub(crate) fn test(&self, host: &HostKey) -> Option<&TestOutcome> {
        Some(&self.hosts.get(host)?.test.as_ref()?.outcome)
    }

    /// The daemon's refusal of the last key send to `host`.
    pub(crate) fn key_error(&self, host: &HostKey) -> Option<&str> {
        self.hosts.get(host)?.key_error.as_deref()
    }

    /// Puts `outcome` on `host`'s Test when `request_id` names it.
    fn settle_test(
        &mut self,
        host: &HostKey,
        request_id: Option<&str>,
        outcome: TestOutcome,
    ) -> bool {
        let Some(run) = self
            .hosts
            .get_mut(host)
            .and_then(|state| state.test.as_mut())
            .filter(|run| Some(run.request_id.as_str()) == request_id)
        else {
            return false;
        };
        run.outcome = outcome;
        true
    }
}

/// The words the Linear tab shows for the key's state; a refused key says
/// whether it is the saved one (`key_saved`) or `LINEAR_API_KEY`.
pub(crate) const fn key_state_words(state: LinearKeyState, key_saved: bool) -> &'static str {
    match state {
        LinearKeyState::Missing => "No key",
        LinearKeyState::Stored => "Key saved",
        LinearKeyState::Environment => "Using LINEAR_API_KEY",
        LinearKeyState::Rejected if key_saved => "Linear refused the saved key",
        LinearKeyState::Rejected => "Linear refused LINEAR_API_KEY",
        LinearKeyState::Unreachable => "The secure store is out of reach on this host",
        LinearKeyState::Unknown => "Unknown",
    }
}

/// The Test result line's text.
pub(crate) fn test_text(outcome: &TestOutcome) -> String {
    match outcome {
        TestOutcome::Waiting => "Testing…".to_owned(),
        TestOutcome::Failed(message) => message.clone(),
        TestOutcome::Reply {
            workspace,
            team_keys,
        } if team_keys.is_empty() => format!("Reached {workspace}"),
        TestOutcome::Reply {
            workspace,
            team_keys,
        } => format!("Reached {workspace} · teams {}", team_keys.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use protocol::{DaemonMessage, LinearKeyState};

    use super::{LinearSettingsState, TestOutcome, key_state_words, test_text};
    use crate::target::HostKey;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn status(enabled: bool) -> DaemonMessage {
        DaemonMessage::LinearStatus {
            enabled,
            key: LinearKeyState::Stored,
            workspace_name: Some("Leftos".to_owned()),
            team_keys: strings(&["RT", "YAAT"]),
            last_error: None,
            key_saved: true,
        }
    }

    fn reply(
        request_id: Option<&str>,
        workspace: Option<&str>,
        error: Option<&str>,
    ) -> DaemonMessage {
        DaemonMessage::LinearTest {
            workspace_name: workspace.map(str::to_owned),
            team_keys: strings(&["RT"]),
            error: error.map(str::to_owned),
            request_id: request_id.map(str::to_owned),
        }
    }

    fn welcome() -> DaemonMessage {
        DaemonMessage::Welcome {
            protocol_version: 1,
            supported_versions: vec![1],
            features: Vec::new(),
            daemon_id: None,
            origin: None,
            build_version: None,
        }
    }

    #[test]
    fn key_state_words_name_every_state() {
        for (state, saved, words) in [
            (LinearKeyState::Missing, false, "No key"),
            (LinearKeyState::Stored, true, "Key saved"),
            (LinearKeyState::Environment, false, "Using LINEAR_API_KEY"),
            (
                LinearKeyState::Rejected,
                true,
                "Linear refused the saved key",
            ),
            (
                LinearKeyState::Rejected,
                false,
                "Linear refused LINEAR_API_KEY",
            ),
            (
                LinearKeyState::Unreachable,
                false,
                "The secure store is out of reach on this host",
            ),
            (LinearKeyState::Unknown, false, "Unknown"),
        ] {
            assert_eq!(
                key_state_words(state, saved),
                words,
                "{state:?}, saved {saved}"
            );
        }
    }

    #[test]
    fn test_text_reads_waiting_success_and_failure() {
        assert_eq!(test_text(&TestOutcome::Waiting), "Testing…");
        let reached = |teams: &[&str]| TestOutcome::Reply {
            workspace: "Leftos".to_owned(),
            team_keys: strings(teams),
        };
        assert_eq!(
            test_text(&reached(&["RT", "YAAT"])),
            "Reached Leftos · teams RT, YAAT"
        );
        assert_eq!(test_text(&reached(&[])), "Reached Leftos", "no teams");
        assert_eq!(
            test_text(&TestOutcome::Failed("No Linear API key is set.".to_owned())),
            "No Linear API key is set."
        );
    }

    #[test]
    fn a_welcome_forgets_the_hosts_status_and_only_its() {
        let mut state = LinearSettingsState::default();
        let local = HostKey::Local;
        let other = HostKey::Remote("box".to_owned());
        assert_eq!(state.enabled(&local), None, "nothing before a status");
        assert!(state.fold(&local, &status(false)));
        assert!(state.fold(&other, &status(true)));
        assert_eq!(state.enabled(&local), Some(false));
        assert_eq!(
            state.status(&local).map(|s| s.team_keys.clone()),
            Some(strings(&["RT", "YAAT"]))
        );
        state.start_test(&local, "t1".to_owned());
        assert!(state.fold(&local, &welcome()));
        assert!(
            state.status(&local).is_none(),
            "a Welcome forgets the status"
        );
        assert!(state.test(&local).is_none(), "and the Test");
        assert_eq!(state.enabled(&other), Some(true), "another host's stays");
        assert!(!state.fold(&local, &welcome()), "nothing left to forget");
    }

    #[test]
    fn a_test_reply_lands_only_on_the_test_last_sent() {
        let mut state = LinearSettingsState::default();
        let host = HostKey::Local;
        assert!(
            !state.fold(&host, &reply(Some("t0"), Some("Leftos"), None)),
            "no Test was sent"
        );
        state.start_test(&host, "t1".to_owned());
        state.start_test(&host, "t2".to_owned());
        assert_eq!(state.test(&host), Some(&TestOutcome::Waiting));
        assert!(
            !state.fold(&host, &reply(Some("t1"), Some("Leftos"), None)),
            "a stale Test's reply is ignored"
        );
        assert!(!state.fold(&host, &reply(None, Some("Leftos"), None)));
        assert_eq!(state.test(&host), Some(&TestOutcome::Waiting));
        assert!(state.fold(&host, &reply(Some("t2"), Some("Leftos"), None)));
        assert_eq!(
            state.test(&host),
            Some(&TestOutcome::Reply {
                workspace: "Leftos".to_owned(),
                team_keys: strings(&["RT"]),
            })
        );
        state.start_test(&host, "t3".to_owned());
        assert!(state.fold(
            &host,
            &reply(Some("t3"), None, Some("Linear refused the key."))
        ));
        assert_eq!(
            state.test(&host),
            Some(&TestOutcome::Failed("Linear refused the key.".to_owned()))
        );
    }

    #[test]
    fn only_the_last_key_send_takes_a_refusal() {
        let mut state = LinearSettingsState::default();
        let host = HostKey::Local;
        assert!(!state.refused(&host, "k0", "unasked"), "no key was sent");
        state.sent_key(&host, "k1".to_owned());
        state.sent_key(&host, "k2".to_owned());
        assert!(
            !state.refused(&host, "k1", "old"),
            "a newer send replaced it"
        );
        assert_eq!(state.key_error(&host), None);
        assert!(state.refused(&host, "k2", "The key is blank."));
        assert_eq!(state.key_error(&host), Some("The key is blank."));
        state.sent_key(&host, "k3".to_owned());
        assert_eq!(state.key_error(&host), None, "a new send clears it");
        state.start_test(&host, "t1".to_owned());
        assert!(
            state.refused(&host, "t1", "offline"),
            "a refused Test fails"
        );
        assert_eq!(
            state.test(&host),
            Some(&TestOutcome::Failed("offline".to_owned()))
        );
    }
}
