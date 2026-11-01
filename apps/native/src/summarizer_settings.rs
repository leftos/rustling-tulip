//! What each host's daemon last said about its Dashboard summarizer, for
//! the Settings modal's Dashboard tab: its full status, the Test it ran for
//! this window, and the cap change it refused. Plain Rust, fed from the
//! root view's message folds.

use std::collections::HashMap;

use protocol::{
    DaemonMessage, KeyState, SummarizerKey, SummarizerProvider, SummarizerSettings,
    SummaryTestResult,
};

use crate::target::HostKey;

/// One daemon's `SummarizerStatus`, as it last sent it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SummarizerStatus {
    pub(crate) settings: SummarizerSettings,
    pub(crate) providers: Vec<SummarizerProvider>,
    pub(crate) keys: Vec<SummarizerKey>,
    pub(crate) spend_today_usd: f64,
    pub(crate) cap_reached: bool,
}

impl SummarizerStatus {
    /// The provider the settings choose, when the daemon offers it.
    pub(crate) fn provider(&self) -> Option<&SummarizerProvider> {
        self.providers
            .iter()
            .find(|provider| provider.id == self.settings.provider)
    }

    /// The state of the chosen provider's key, when the daemon reports one.
    pub(crate) fn key_state(&self) -> Option<KeyState> {
        self.keys
            .iter()
            .find(|key| key.provider == self.settings.provider)
            .map(|key| key.state)
    }
}

/// Where the Test this window asked a host for stands.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TestOutcome {
    /// Sent; no answer yet.
    Waiting,
    /// The daemon's sample summary.
    Reply {
        result: SummaryTestResult,
        latency_ms: u64,
        cost_usd: f64,
    },
    /// The daemon refused the Test with this message.
    Failed(String),
}

/// The Test last sent to a host and where it stands.
#[derive(Debug, Clone, PartialEq)]
struct TestRun {
    request_id: String,
    outcome: TestOutcome,
}

/// One host's summarizer, as the Dashboard tab reads it.
#[derive(Debug, Default)]
struct HostSummarizer {
    status: Option<SummarizerStatus>,
    test: Option<TestRun>,
    /// The settings this window last sent, until the daemon's next status:
    /// the next change builds on them, so quick changes keep each other.
    last_sent: Option<SummarizerSettings>,
    /// The request id of the newest cap change, until it is refused.
    cap_request: Option<String>,
    /// The daemon's refusal of the last cap change.
    cap_error: Option<String>,
}

/// Every host's summarizer state.
#[derive(Debug, Default)]
pub(crate) struct SummarizerSettingsState {
    hosts: HashMap<HostKey, HostSummarizer>,
}

impl SummarizerSettingsState {
    /// Folds what `msg` from `host` says about its summarizer: a status
    /// replaces the old one and the settings last sent, a Test reply lands
    /// when it answers the Test last sent, and a `Welcome` forgets
    /// everything the host's old connection said. Returns whether `msg`
    /// changed anything.
    pub(crate) fn fold(&mut self, host: &HostKey, msg: &DaemonMessage) -> bool {
        match msg {
            DaemonMessage::SummarizerStatus {
                settings,
                providers,
                keys,
                spend_today_usd,
                cap_reached,
            } => {
                let state = self.hosts.entry(host.clone()).or_default();
                state.status = Some(SummarizerStatus {
                    settings: settings.clone(),
                    providers: providers.clone(),
                    keys: keys.clone(),
                    spend_today_usd: *spend_today_usd,
                    cap_reached: *cap_reached,
                });
                state.last_sent = None;
                true
            }
            DaemonMessage::SummaryTest {
                result,
                latency_ms,
                cost_usd,
                request_id,
            } => self.settle_test(
                host,
                request_id.as_deref(),
                TestOutcome::Reply {
                    result: result.clone(),
                    latency_ms: *latency_ms,
                    cost_usd: *cost_usd,
                },
            ),
            DaemonMessage::Welcome { .. } => self.hosts.remove(host).is_some(),
            _ => false,
        }
    }

    /// A daemon `Error` from `host` answering `request_id`: the Test it
    /// answers fails with `message`, or the cap change it answers shows
    /// `message` under the cap. Returns whether it answered either.
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
        if state.cap_request.as_deref() != Some(request_id) {
            return false;
        }
        state.cap_request = None;
        state.cap_error = Some(message.to_owned());
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

    /// The cap change `request_id` was sent to `host`; the last refusal's
    /// message goes, and an older cap change's refusal no longer lands.
    pub(crate) fn sent_cap(&mut self, host: &HostKey, request_id: String) {
        let state = self.hosts.entry(host.clone()).or_default();
        state.cap_request = Some(request_id);
        state.cap_error = None;
    }

    /// `settings` were sent to `host`; the next change builds on them
    /// until its daemon sends a status.
    pub(crate) fn sent_settings(&mut self, host: &HostKey, settings: SummarizerSettings) {
        self.hosts.entry(host.clone()).or_default().last_sent = Some(settings);
    }

    /// The settings the next change to `host`'s summarizer builds on: the
    /// ones last sent, else its daemon's.
    pub(crate) fn next_settings(&self, host: &HostKey) -> Option<&SummarizerSettings> {
        let state = self.hosts.get(host)?;
        state
            .last_sent
            .as_ref()
            .or_else(|| state.status.as_ref().map(|status| &status.settings))
    }

    /// `host`'s summarizer status, once its daemon sent one.
    pub(crate) fn status(&self, host: &HostKey) -> Option<&SummarizerStatus> {
        self.hosts.get(host)?.status.as_ref()
    }

    /// Where the Test last sent to `host` stands.
    pub(crate) fn test(&self, host: &HostKey) -> Option<&TestOutcome> {
        Some(&self.hosts.get(host)?.test.as_ref()?.outcome)
    }

    /// The daemon's refusal of the last cap change sent to `host`.
    pub(crate) fn cap_error(&self, host: &HostKey) -> Option<&str> {
        self.hosts.get(host)?.cap_error.as_deref()
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

/// The cap `text` names: a number of dollars, zero or more, a leading `$`
/// allowed; `-0` reads as 0.
pub(crate) fn parse_cap(text: &str) -> Option<f64> {
    let text = text.trim();
    let number = text.strip_prefix('$').unwrap_or(text).trim();
    number
        .parse::<f64>()
        .ok()
        .filter(|cap| cap.is_finite() && *cap >= 0.0)
        .map(f64::abs)
}

#[cfg(test)]
mod tests {
    use protocol::{
        DaemonMessage, KeyState, SummarizerKey, SummarizerKeySource, SummarizerSettings,
        SummarySource, SummaryTestResult,
    };

    use super::{SummarizerSettingsState, TestOutcome, parse_cap};

    #[test]
    fn only_the_newest_cap_change_takes_a_refusal() {
        let mut state = SummarizerSettingsState::default();
        let host = HostKey::Local;
        state.sent_cap(&host, "c1".to_owned());
        state.sent_cap(&host, "c2".to_owned());
        assert!(
            !state.refused(&host, "c1", "old refusal"),
            "a newer cap change replaced it"
        );
        assert_eq!(state.cap_error(&host), None, "so its refusal shows nothing");
        assert!(state.refused(&host, "c2", "new refusal"));
        assert_eq!(state.cap_error(&host), Some("new refusal"));
        assert!(
            !state.refused(&host, "c2", "again"),
            "a cap change takes one refusal"
        );
    }

    #[test]
    fn parse_cap_reads_dollars_zero_or_more() {
        let cap = |text: &str| parse_cap(text).map(|cap| format!("{cap:.2}"));
        assert_eq!(cap("-0").as_deref(), Some("0.00"), "minus zero is zero");
        assert_eq!(cap("$-0.0").as_deref(), Some("0.00"));
        assert_eq!(cap(" $2.5 ").as_deref(), Some("2.50"));
        assert_eq!(cap("0").as_deref(), Some("0.00"));
        assert_eq!(cap("-0.01"), None, "below zero");
        assert_eq!(cap("inf"), None);
        assert_eq!(cap("NaN"), None);
        assert_eq!(cap(""), None);
    }
    use crate::target::HostKey;

    fn status(provider: &str, spend: f64) -> DaemonMessage {
        DaemonMessage::SummarizerStatus {
            settings: SummarizerSettings {
                enabled: true,
                provider: provider.to_owned(),
                key_source: SummarizerKeySource::Stored,
                daily_cap_usd: 1.0,
            },
            providers: Vec::new(),
            keys: vec![SummarizerKey {
                provider: provider.to_owned(),
                state: KeyState::Stored,
            }],
            spend_today_usd: spend,
            cap_reached: false,
        }
    }

    fn reply(request_id: Option<&str>, headline: &str) -> DaemonMessage {
        DaemonMessage::SummaryTest {
            result: SummaryTestResult {
                kind: None,
                headline: headline.to_owned(),
                notification: "n".to_owned(),
                source: SummarySource::Model,
            },
            latency_ms: 12,
            cost_usd: 0.001,
            request_id: request_id.map(str::to_owned),
        }
    }

    fn headline(state: &SummarizerSettingsState, host: &HostKey) -> Option<String> {
        match state.test(host)? {
            TestOutcome::Reply { result, .. } => Some(result.headline.clone()),
            TestOutcome::Waiting | TestOutcome::Failed(_) => None,
        }
    }

    #[test]
    fn status_replaces_the_old_one() {
        let mut state = SummarizerSettingsState::default();
        let local = HostKey::Local;
        let other = HostKey::Remote("box".to_owned());
        assert!(state.status(&local).is_none(), "nothing before a status");

        assert!(state.fold(&local, &status("p1", 0.1)));
        assert!(state.fold(&local, &status("p2", 0.4)));
        let shown = state
            .status(&local)
            .map(|s| (s.settings.provider.clone(), s.spend_today_usd));
        assert_eq!(
            shown,
            Some(("p2".to_owned(), 0.4)),
            "the newest status wins"
        );
        assert!(state.status(&other).is_none(), "another host's stays apart");

        state.fold(&other, &status("p1", 0.0));
        let welcome = DaemonMessage::Welcome {
            protocol_version: 1,
            supported_versions: vec![1],
            features: Vec::new(),
            daemon_id: None,
        };
        assert!(state.fold(&local, &welcome));
        assert!(
            state.status(&local).is_none(),
            "a Welcome forgets the host's status"
        );
        assert!(state.status(&other).is_some(), "and only that host's");
    }

    #[test]
    fn a_test_reply_is_matched_by_request_id() {
        let mut state = SummarizerSettingsState::default();
        let host = HostKey::Local;
        assert!(
            !state.fold(&host, &reply(Some("t0"), "unasked")),
            "no Test was sent"
        );

        state.start_test(&host, "t1".to_owned());
        assert_eq!(state.test(&host), Some(&TestOutcome::Waiting));
        assert!(!state.fold(&host, &reply(Some("other"), "stray")));
        assert!(!state.fold(&host, &reply(None, "unrouted")));
        assert_eq!(
            state.test(&host),
            Some(&TestOutcome::Waiting),
            "a stray reply lands nowhere"
        );

        assert!(state.fold(&host, &reply(Some("t1"), "mine")));
        assert_eq!(headline(&state, &host).as_deref(), Some("mine"));

        state.start_test(&host, "t2".to_owned());
        assert!(
            !state.refused(&host, "t1", "late"),
            "an older Test's answer no longer lands"
        );
        assert!(state.refused(&host, "t2", "Daily cap reached."));
        assert_eq!(
            state.test(&host),
            Some(&TestOutcome::Failed("Daily cap reached.".to_owned()))
        );

        state.sent_cap(&host, "c1".to_owned());
        assert!(state.refused(&host, "c1", "bad cap"));
        assert_eq!(state.cap_error(&host), Some("bad cap"));
        state.sent_cap(&host, "c2".to_owned());
        assert_eq!(
            state.cap_error(&host),
            None,
            "a new cap change clears the refusal"
        );
    }
}
