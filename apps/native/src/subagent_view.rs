//! A pane that follows a session's or a dispatch run's subagent, read-only:
//! a header row of the subagent's last snapshot (its label, `SA · type ·
//! model` and its state), a note once its session or run has ended or it is
//! no longer available, over the transcript body a dispatch run's pane draws.
//! The rules are plain Rust, so each is unit-tested; `RootView` keeps the
//! state and draws it.

use std::collections::{HashMap, HashSet};

use gpui::{Context, Div, Pixels, div, prelude::*, px};
use protocol::{AgentState, DaemonMessage, DispatchRunSnapshot, SessionSnapshot, SubagentSnapshot};

use crate::driver_id::DriverId as _;
use crate::fonts::{ui_px_at, ui_text_size};
use crate::sidebar::SidebarModel;
use crate::subagents;
use crate::target::HostKey;
use crate::transcript_view::{TranscriptKey, transcript_lines};
use crate::ui_theme::{UiTheme, theme};
use crate::{RootView, UI_TEXT_SIZE};

/// The note of a subagent whose host session has ended.
pub(crate) const ENDED_NOTE: &str = "Its session has ended";
/// The note of a subagent whose dispatch run has ended.
pub(crate) const RUN_ENDED_NOTE: &str = "Its run has ended";
/// The note of a subagent whose follow was refused with nothing held of it.
pub(crate) const UNAVAILABLE_NOTE: &str = "This subagent is no longer available";

/// The word a subagent's header shows for its `state`, followed by
/// ` · {end_status}` when it has one.
pub(crate) fn state_word(state: AgentState, end_status: Option<&str>) -> String {
    let word = match state {
        AgentState::Running => "running",
        AgentState::Queued => "queued",
        AgentState::Finished => "finished",
        AgentState::Failed => "failed",
        AgentState::Unknown => "unknown",
    };
    match end_status {
        Some(status) => format!("{word} · {status}"),
        None => word.to_owned(),
    }
}

/// What a subagent pane's header row draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentHeader {
    pub title: String,
    pub chip: String,
    pub state: String,
}

/// The header row of `sub`: its label, `SA · {agent type} · {model}` with
/// a missing part and its separator left out, and its state word.
pub(crate) fn subagent_header(sub: &SubagentSnapshot) -> SubagentHeader {
    let chip = match subagents::subline(sub) {
        Some(line) => format!("SA · {line}"),
        None => "SA".to_owned(),
    };
    SubagentHeader {
        title: subagents::label(sub),
        chip,
        state: state_word(sub.state, sub.end_status.as_deref()),
    }
}

/// Subagent `agent_id` of `session`, when `session` is `session_id`.
fn subagent_of<'a>(
    session: &'a SessionSnapshot,
    session_id: &str,
    agent_id: &str,
) -> Option<&'a SubagentSnapshot> {
    if session.id != session_id {
        return None;
    }
    session
        .subagents
        .iter()
        .find(|sub| sub.agent_id == agent_id)
}

/// The note an ended transcript of `key` reads.
pub(crate) fn ended_note(key: &TranscriptKey) -> &'static str {
    match key {
        TranscriptKey::RunSubagent { .. } => RUN_ENDED_NOTE,
        TranscriptKey::Run(_) | TranscriptKey::Subagent { .. } => ENDED_NOTE,
    }
}

/// Subagent `agent_id` of `run`, when the run lists it.
fn subagent_in_run<'a>(
    run: &'a DispatchRunSnapshot,
    agent_id: &str,
) -> Option<&'a SubagentSnapshot> {
    run.subagents.iter().find(|sub| sub.agent_id == agent_id)
}

/// Subagent `agent_id` of session `session_id` as `msg` lists it.
fn listed_in_session<'a>(
    msg: &'a DaemonMessage,
    session_id: &str,
    agent_id: &str,
) -> Option<&'a SubagentSnapshot> {
    match msg {
        DaemonMessage::Sessions { sessions } => sessions
            .iter()
            .find_map(|session| subagent_of(session, session_id, agent_id)),
        DaemonMessage::SessionUpdated { session, .. } => subagent_of(session, session_id, agent_id),
        _ => None,
    }
}

/// Subagent `agent_id` of dispatch run `run_id` as `msg` lists it.
fn listed_in_run<'a>(
    msg: &'a DaemonMessage,
    run_id: &str,
    agent_id: &str,
) -> Option<&'a SubagentSnapshot> {
    let run = match msg {
        DaemonMessage::DispatchRuns { runs } => runs.iter().find(|run| run.run_id == run_id),
        DaemonMessage::DispatchRunUpdated { run } if run.run_id == run_id => Some(run),
        _ => None,
    };
    run.and_then(|run| subagent_in_run(run, agent_id))
}

/// Subagent `key` as `msg` lists it.
fn listed_in<'a>(msg: &'a DaemonMessage, key: &TranscriptKey) -> Option<&'a SubagentSnapshot> {
    match key {
        TranscriptKey::Run(_) => None,
        TranscriptKey::Subagent {
            session_id,
            agent_id,
        } => listed_in_session(msg, session_id, agent_id),
        TranscriptKey::RunSubagent { run_id, agent_id } => listed_in_run(msg, run_id, agent_id),
    }
}

/// Subagent `key` of `host` as `sidebar` lists it now.
fn live_subagent<'a>(
    sidebar: &'a SidebarModel,
    host: &HostKey,
    key: &TranscriptKey,
) -> Option<&'a SubagentSnapshot> {
    match key {
        TranscriptKey::Run(_) => None,
        TranscriptKey::Subagent {
            session_id,
            agent_id,
        } => subagent_of(sidebar.session_on(host, session_id)?, session_id, agent_id),
        TranscriptKey::RunSubagent { run_id, agent_id } => {
            subagent_in_run(sidebar.run_on(host, run_id)?, agent_id)
        }
    }
}

/// The kept last snapshots of followed subagents, by host and transcript
/// key: a session's subagent or a dispatch run's.
#[derive(Debug, Default)]
pub(crate) struct KeptSubagents(HashMap<SubagentKey, SubagentSnapshot>);

impl KeptSubagents {
    /// Before `msg` from `host` folds: keeps each `followed` subagent's
    /// snapshot from `msg` when it lists it, else the one already kept,
    /// else the one `current` finds. Nothing kept is ever dropped here.
    pub(crate) fn keep_from(
        &mut self,
        host: &HostKey,
        followed: &[TranscriptKey],
        msg: &DaemonMessage,
        current: impl Fn(&TranscriptKey) -> Option<SubagentSnapshot>,
    ) {
        for key in followed {
            let kept = (host.clone(), key.clone());
            if let Some(sub) = listed_in(msg, key) {
                self.0.insert(kept, sub.clone());
            } else if !self.0.contains_key(&kept)
                && let Some(sub) = current(key)
            {
                self.0.insert(kept, sub);
            }
        }
    }

    /// The kept snapshot of `key` of `host`.
    pub(crate) fn get(&self, host: &HostKey, key: &TranscriptKey) -> Option<&SubagentSnapshot> {
        self.0.get(&(host.clone(), key.clone()))
    }

    /// Keeps only the subagents a pane still follows, by host and key in
    /// `followed`. A kept session subagent no pane follows moves first to a
    /// followed key of the same host and agent id that has no snapshot of
    /// its own, as when a Resume moves a pane to the host's new session; a
    /// dispatch run's subagent never moves. What is left unfollowed is
    /// dropped.
    pub(crate) fn prune(&mut self, followed: &HashSet<SubagentKey>) {
        let unfollowed: Vec<SubagentKey> = self
            .0
            .keys()
            .filter(|key| !followed.contains(*key))
            .cloned()
            .collect();
        for old in unfollowed {
            if let Some(new) = self.moved_to(&old, followed)
                && let Some(snapshot) = self.0.remove(&old)
            {
                self.0.insert(new, snapshot);
            }
        }
        self.0.retain(|key, _| followed.contains(key));
    }

    /// The followed key `old` moved to: a session subagent's, of the same
    /// host and agent id, in another session, with no snapshot kept of its
    /// own.
    fn moved_to(&self, old: &SubagentKey, followed: &HashSet<SubagentKey>) -> Option<SubagentKey> {
        let (host, old_key) = old;
        let TranscriptKey::Subagent {
            session_id,
            agent_id,
        } = old_key
        else {
            return None;
        };
        followed
            .iter()
            .find(|(on, key)| {
                on == host
                    && matches!(
                        key,
                        TranscriptKey::Subagent { session_id: session, agent_id: agent }
                            if agent == agent_id && session != session_id
                    )
                    && !self.0.contains_key(&(on.clone(), key.clone()))
            })
            .cloned()
    }
}

/// A kept subagent's host and transcript key.
type SubagentKey = (HostKey, TranscriptKey);

/// The header row of pane `pane_id`, its text `size`.
fn header_row(pane_id: &str, header: SubagentHeader, size: Pixels, t: &UiTheme) -> Div {
    let part = |text: String, colour: u32| {
        div()
            .flex_none()
            .whitespace_nowrap()
            .text_color(gpui::rgb(colour))
            .child(text)
    };
    let title = div()
        .min_w(px(0.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_color(gpui::rgb(t.text))
        .child(header.title);
    div()
        .driver_id(|| format!("subagent-header-{pane_id}"))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .py(px(4.0))
        .text_size(size)
        .child(title)
        .child(part(header.chip, t.subtle))
        .child(part(header.state, t.muted))
}

impl RootView {
    /// The transcript key of each subagent of `host` a pane follows, a
    /// session's or a dispatch run's.
    fn followed_subagents_on(&self, host: &HostKey) -> Vec<TranscriptKey> {
        self.followed_transcripts()
            .into_iter()
            .filter(|f| f.host == *host)
            .filter_map(|f| match f.key {
                TranscriptKey::Run(_) => None,
                key => Some(key),
            })
            .collect()
    }

    /// Keeps the snapshot of each followed subagent of `host` that `msg`
    /// lists, before the sidebar folds it; one it leaves out keeps the
    /// snapshot already kept, else the sidebar's current one.
    pub(crate) fn keep_followed_subagents(&mut self, host: &HostKey, msg: &DaemonMessage) {
        if !matches!(
            msg,
            DaemonMessage::Sessions { .. }
                | DaemonMessage::SessionUpdated { .. }
                | DaemonMessage::SessionRemoved { .. }
                | DaemonMessage::DispatchRuns { .. }
                | DaemonMessage::DispatchRunUpdated { .. }
                | DaemonMessage::DispatchRunRemoved { .. }
        ) {
            return;
        }
        let followed = self.followed_subagents_on(host);
        if followed.is_empty() {
            return;
        }
        let sidebar = &self.sidebar;
        self.kept_subagents.keep_from(host, &followed, msg, |key| {
            live_subagent(sidebar, host, key).cloned()
        });
    }

    /// Keeps only the kept snapshots of subagents a pane still follows.
    pub(crate) fn prune_kept_subagents(&mut self) {
        let followed: HashSet<SubagentKey> = self
            .followed_transcripts()
            .into_iter()
            .filter_map(|f| match f.key {
                TranscriptKey::Run(_) => None,
                key => Some((f.host, key)),
            })
            .collect();
        self.kept_subagents.prune(&followed);
    }

    /// The snapshot subagent `key` of `host` draws its header from: the
    /// live one, else the kept one.
    fn subagent_snapshot(&self, host: &HostKey, key: &TranscriptKey) -> Option<&SubagentSnapshot> {
        live_subagent(&self.sidebar, host, key).or_else(|| self.kept_subagents.get(host, key))
    }

    /// A refused follow of subagent `key`: unavailable when nothing is held
    /// of it, neither a transcript nor a snapshot.
    pub(crate) fn subagent_refused(&mut self, key: (HostKey, TranscriptKey)) {
        let held =
            self.transcripts.contains_key(&key) || self.subagent_snapshot(&key.0, &key.1).is_some();
        if !held {
            self.transcripts_unavailable.insert(key);
        }
    }

    /// The note subagent `key`'s pane draws under its header, if any.
    fn subagent_note(&self, key: &(HostKey, TranscriptKey)) -> Option<&'static str> {
        if self.transcripts.get(key).is_some_and(|held| held.ended) {
            Some(ended_note(&key.1))
        } else if self.transcripts_unavailable.contains(key) {
            Some(UNAVAILABLE_NOTE)
        } else {
            None
        }
    }

    /// The body of pane `pane_id`, which follows subagent `key` of `host`:
    /// its header row, a note, then its transcript.
    pub(crate) fn subagent_body(
        &self,
        pane_id: &str,
        host: &HostKey,
        key: &TranscriptKey,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let size = ui_px_at(ui_text_size(cx), UI_TEXT_SIZE);
        let held = (host.clone(), key.clone());
        let header = self
            .subagent_snapshot(host, key)
            .map(|sub| header_row(pane_id, subagent_header(sub), size, &t));
        let note = self.subagent_note(&held).map(|text| {
            div()
                .driver_id(|| format!("subagent-note-{pane_id}"))
                .px(px(8.0))
                .py(px(4.0))
                .text_size(size)
                .text_color(gpui::rgb(t.muted))
                .child(text)
        });
        let rows = match self.transcripts.get(&held) {
            Some(transcript) => {
                let line_ids = format!("subagent-line-{pane_id}");
                Self::transcript_rows(&line_ids, &held, transcript, cx)
            }
            None => Vec::new(),
        };
        let log = self.log_column(pane_id, format!("subagent-log-{pane_id}"), rows, cx);
        div()
            .driver_id(|| format!("pane-follow-{pane_id}"))
            .flex()
            .flex_col()
            .flex_1()
            .size_full()
            .min_h(px(0.0))
            .children(header)
            .children(note)
            .child(log)
    }

    /// The host and subagent pane `pane_id` follows, a session's or a
    /// dispatch run's.
    fn pane_subagent(&self, pane_id: &str) -> Option<(HostKey, TranscriptKey)> {
        self.followed_transcripts()
            .into_iter()
            .find(|f| f.pane_id == pane_id && !matches!(f.key, TranscriptKey::Run(_)))
            .map(|f| (f.host, f.key))
    }

    /// The title, chip and state pane `pane_id`'s header row draws for the
    /// subagent it follows; `None` when nothing is known of the subagent.
    #[must_use]
    pub fn subagent_view_header(&self, pane_id: &str) -> Option<(String, String, String)> {
        let (host, key) = self.pane_subagent(pane_id)?;
        let header = subagent_header(self.subagent_snapshot(&host, &key)?);
        Some((header.title, header.chip, header.state))
    }

    /// The transcript lines pane `pane_id` draws for the subagent it
    /// follows, oldest first; none until a transcript is held.
    #[must_use]
    pub fn subagent_view_lines(&self, pane_id: &str) -> Vec<String> {
        self.pane_subagent(pane_id)
            .and_then(|key| self.transcripts.get(&key))
            .map(|transcript| {
                transcript_lines(transcript)
                    .into_iter()
                    .map(|line| line.text)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The note pane `pane_id` draws under its header for the subagent it
    /// follows, if any.
    #[must_use]
    pub fn subagent_view_note(&self, pane_id: &str) -> Option<String> {
        let key = self.pane_subagent(pane_id)?;
        self.subagent_note(&key).map(str::to_owned)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;

    use protocol::{AgentState, DaemonMessage, SessionSnapshot, SubagentSnapshot};

    fn sub(id: &str, state: AgentState) -> SubagentSnapshot {
        SubagentSnapshot {
            agent_id: id.to_owned(),
            depth: 1,
            description: Some(format!("task {id}")),
            state,
            ..SubagentSnapshot::default()
        }
    }

    fn session_with(id: &str, subagents: Vec<SubagentSnapshot>) -> SessionSnapshot {
        let mut session: SessionSnapshot = serde_json::from_value(serde_json::json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [],
            "status": "idle",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cost_usd": 0.0,
                "last_activity_at": null,
            },
            "recent_actions": [],
            "agent": "fake",
        }))
        .expect("session fixture");
        session.subagents = subagents;
        session
    }

    #[test]
    fn header_line_leaves_out_missing_type_and_model() {
        let mut s = sub("a1", AgentState::Running);
        s.agent_type = Some("explore".to_owned());
        s.model = Some("model-x".to_owned());
        let header = subagent_header(&s);
        assert_eq!(header.title, "task a1");
        assert_eq!(header.chip, "SA · explore · model-x");
        assert_eq!(header.state, "running");

        s.model = None;
        assert_eq!(subagent_header(&s).chip, "SA · explore");
        s.agent_type = None;
        s.model = Some("model-x".to_owned());
        assert_eq!(subagent_header(&s).chip, "SA · model-x");
        s.model = None;
        assert_eq!(subagent_header(&s).chip, "SA");
    }

    #[test]
    fn state_word_maps_each_state_and_appends_the_end_status() {
        for (state, word) in [
            (AgentState::Running, "running"),
            (AgentState::Queued, "queued"),
            (AgentState::Finished, "finished"),
            (AgentState::Failed, "failed"),
            (AgentState::Unknown, "unknown"),
        ] {
            assert_eq!(state_word(state, None), word, "{state:?}");
            assert_eq!(
                state_word(state, Some("stopped by the user")),
                format!("{word} · stopped by the user"),
                "{state:?}"
            );
        }
    }

    fn subagent_key(session_id: &str, agent_id: &str) -> TranscriptKey {
        TranscriptKey::Subagent {
            session_id: session_id.to_owned(),
            agent_id: agent_id.to_owned(),
        }
    }

    fn run_key(run_id: &str, agent_id: &str) -> TranscriptKey {
        TranscriptKey::RunSubagent {
            run_id: run_id.to_owned(),
            agent_id: agent_id.to_owned(),
        }
    }

    /// Dispatch run `run_id` listing `subagents`.
    fn run_with(run_id: &str, subagents: &[SubagentSnapshot]) -> DispatchRunSnapshot {
        serde_json::from_value(serde_json::json!({
            "run_id": run_id,
            "title": format!("title {run_id}"),
            "backend": "backend-x",
            "worktree": "C:\\wt",
            "run_dir": "C:\\runs",
            "started_at": "2026-01-01T00:00:00Z",
            "status": "working",
            "subagents": subagents,
        }))
        .expect("run fixture")
    }

    #[test]
    fn kept_snapshot_refreshes_from_the_message_and_survives_a_drop() {
        let host = HostKey::Local;
        let a1 = subagent_key("s1", "a1");
        let followed = vec![a1.clone()];
        let running = sub("a1", AgentState::Running);
        let finished = sub("a1", AgentState::Finished);
        let mut kept = KeptSubagents::default();

        // Not in the message: the sidebar's current copy is taken.
        kept.keep_from(
            &host,
            &followed,
            &DaemonMessage::SessionRemoved {
                session_id: "other".to_owned(),
            },
            |_| Some(running.clone()),
        );
        assert_eq!(kept.get(&host, &a1), Some(&running));

        // In the message: refreshed from it.
        let updated = DaemonMessage::SessionUpdated {
            session: session_with("s1", vec![finished.clone()]),
            request_id: None,
        };
        kept.keep_from(&host, &followed, &updated, |_| Some(running.clone()));
        assert_eq!(kept.get(&host, &a1), Some(&finished));

        // Dropped from the list, then the session removed: the copy stays.
        let dropped = DaemonMessage::Sessions {
            sessions: vec![session_with("s1", Vec::new())],
        };
        kept.keep_from(&host, &followed, &dropped, |_| None);
        let removed = DaemonMessage::SessionRemoved {
            session_id: "s1".to_owned(),
        };
        kept.keep_from(&host, &followed, &removed, |_| None);
        assert_eq!(kept.get(&host, &a1), Some(&finished));

        // Another host's copy is its own.
        assert_eq!(kept.get(&HostKey::Remote("box".to_owned()), &a1), None);

        // No pane follows it: dropped.
        kept.prune(&HashSet::new());
        assert_eq!(kept.get(&host, &a1), None);
    }

    #[test]
    fn a_moved_follow_keeps_its_snapshot() {
        let host = HostKey::Local;
        let remote = HostKey::Remote("box".to_owned());
        let a1 = sub("a1", AgentState::Finished);
        let own = sub("a1", AgentState::Running);
        let mut kept = KeptSubagents::default();
        let followed = vec![subagent_key("s1", "a1")];
        let none = DaemonMessage::SessionRemoved {
            session_id: "other".to_owned(),
        };
        kept.keep_from(&host, &followed, &none, |_| Some(a1.clone()));
        kept.keep_from(&remote, &followed, &none, |_| Some(a1.clone()));
        let s3 = vec![subagent_key("s3", "a1")];
        kept.keep_from(&remote, &s3, &none, |_| Some(own.clone()));

        let key = |host: &HostKey, session: &str| (host.clone(), subagent_key(session, "a1"));
        kept.prune(&HashSet::from([key(&host, "s2"), key(&remote, "s3")]));

        assert_eq!(
            kept.get(&host, &subagent_key("s2", "a1")),
            Some(&a1),
            "moved to the new session"
        );
        assert_eq!(
            kept.get(&host, &subagent_key("s1", "a1")),
            None,
            "the old key is gone"
        );
        assert_eq!(
            kept.get(&remote, &subagent_key("s3", "a1")),
            Some(&own),
            "a followed key's own snapshot is never replaced"
        );
        assert_eq!(
            kept.get(&remote, &subagent_key("s1", "a1")),
            None,
            "left unfollowed, dropped"
        );
    }

    #[test]
    fn kept_run_subagent_snapshot_comes_from_its_runs_update() {
        let host = HostKey::Local;
        let r1a = run_key("d1", "r1a");
        let followed = vec![r1a.clone()];
        let running = sub("r1a", AgentState::Running);
        let finished = sub("r1a", AgentState::Finished);
        let mut kept = KeptSubagents::default();

        // Not in the message: the sidebar's current copy is taken.
        kept.keep_from(
            &host,
            &followed,
            &DaemonMessage::DispatchRunRemoved {
                run_id: "other".to_owned(),
            },
            |_| Some(running.clone()),
        );
        assert_eq!(kept.get(&host, &r1a), Some(&running));

        // The run's update lists it: refreshed from it.
        kept.keep_from(
            &host,
            &followed,
            &DaemonMessage::DispatchRunUpdated {
                run: run_with("d1", std::slice::from_ref(&finished)),
            },
            |_| Some(running.clone()),
        );
        assert_eq!(kept.get(&host, &r1a), Some(&finished));

        // Another run's update is not this run's.
        kept.keep_from(
            &host,
            &followed,
            &DaemonMessage::DispatchRunUpdated {
                run: run_with("d2", &[sub("r1a", AgentState::Failed)]),
            },
            |_| None,
        );
        assert_eq!(kept.get(&host, &r1a), Some(&finished));

        // The whole list lists it: refreshed from it, too.
        kept.keep_from(
            &host,
            &followed,
            &DaemonMessage::DispatchRuns {
                runs: vec![run_with("d1", &[sub("r1a", AgentState::Queued)])],
            },
            |_| None,
        );
        assert_eq!(
            kept.get(&host, &r1a).map(|sub| sub.state),
            Some(AgentState::Queued)
        );
    }

    #[test]
    fn a_run_removal_keeps_the_kept_run_subagent() {
        let host = HostKey::Local;
        let r1a = run_key("d1", "r1a");
        let followed = vec![r1a.clone()];
        let finished = sub("r1a", AgentState::Finished);
        let mut kept = KeptSubagents::default();
        kept.keep_from(
            &host,
            &followed,
            &DaemonMessage::DispatchRunUpdated {
                run: run_with("d1", std::slice::from_ref(&finished)),
            },
            |_| None,
        );

        kept.keep_from(
            &host,
            &followed,
            &DaemonMessage::DispatchRunRemoved {
                run_id: "d1".to_owned(),
            },
            |_| None,
        );

        assert_eq!(
            kept.get(&host, &r1a),
            Some(&finished),
            "a removed run keeps its subagent's last snapshot"
        );
    }

    #[test]
    fn a_run_subagent_key_never_moves_on_prune() {
        let host = HostKey::Local;
        let a1 = sub("a1", AgentState::Finished);
        let mut kept = KeptSubagents::default();
        let none = DaemonMessage::SessionRemoved {
            session_id: "other".to_owned(),
        };
        kept.keep_from(&host, &[run_key("d1", "a1")], &none, |_| Some(a1.clone()));

        kept.prune(&HashSet::from([(host.clone(), subagent_key("s2", "a1"))]));

        assert_eq!(
            kept.get(&host, &run_key("d1", "a1")),
            None,
            "left unfollowed, dropped rather than moved onto a session's key"
        );
        assert_eq!(
            kept.get(&host, &subagent_key("s2", "a1")),
            None,
            "the session key gains nothing from a run's subagent"
        );

        kept.keep_from(&host, &[run_key("d1", "a1")], &none, |_| Some(a1.clone()));
        kept.prune(&HashSet::from([(host.clone(), run_key("d1", "a1"))]));
        assert_eq!(
            kept.get(&host, &run_key("d1", "a1")),
            Some(&a1),
            "a followed run key keeps its own snapshot"
        );
    }

    #[test]
    fn an_ended_run_reads_its_run_has_ended() {
        assert_eq!(RUN_ENDED_NOTE, "Its run has ended");
        assert_eq!(ended_note(&run_key("d1", "r1a")), RUN_ENDED_NOTE);
        assert_eq!(ended_note(&subagent_key("s1", "a1")), ENDED_NOTE);
    }
}
