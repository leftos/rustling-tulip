//! The screen dump: one session's rendered grid and how its screen rules read
//! it, for the `screen` subcommand. Host-only: `host_only_refusal` refuses it
//! on a tailnet connection before it reaches [`handle`].

use super::Hub;
use super::connection::ConnCtx;
use crate::screen::rules::{Explanation, RuleTrace, ScreenState, Verdict, rules_for};
use crate::sync::lock;
use agent_api::ScreenRegion;
use protocol::{ClientMessage, DaemonMessage, ScreenExplanation, ScreenRuleTrace};
use tracing::warn;

/// Handle the screen messages `dispatch` routes here.
pub(super) fn handle(hub: &Hub, msg: ClientMessage, ctx: &ConnCtx<'_>) {
    match msg {
        ClientMessage::DumpScreen {
            session_id,
            request_id,
        } => {
            let _ = ctx.out_tx.send(dump(hub, session_id, request_id));
        }
        other => warn!(?other, "screen::handle: routed a message it does not own"),
    }
}

/// `session_id`'s screen dump, or the `Error` carrying `request_id` that says
/// why there is none.
fn dump(hub: &Hub, session_id: String, request_id: Option<String>) -> DaemonMessage {
    let Some(record) = hub.sessions.get(&session_id) else {
        return DaemonMessage::Error {
            message: format!("unknown session: {session_id}"),
            request_id,
        };
    };
    let (screen, agent, wire_agent) = {
        let record = lock(&record);
        (
            record.screen.clone(),
            record.agent.clone(),
            record.wire_agent(),
        )
    };
    let Some(screen) = screen else {
        return DaemonMessage::Error {
            message: format!("session {session_id} has no screen"),
            request_id,
        };
    };
    let rules = rules_for(&agent);
    let screen = lock(&screen);
    let (cols, rows) = screen.size();
    DaemonMessage::ScreenDump {
        session_id,
        request_id,
        cols,
        rows,
        grid: screen.rows(),
        title: screen.title(),
        agent: wire_agent,
        explanation: wire_explanation(&rules.explain(&screen)),
    }
}

/// The trace in its wire form.
fn wire_explanation(explanation: &Explanation) -> ScreenExplanation {
    ScreenExplanation {
        verdict: verdict_text(explanation.winner.verdict).to_owned(),
        winner: explanation.winner.rule.as_deref().map(str::to_owned),
        from_title: explanation.winner.from_title,
        rules: explanation.rules.iter().map(wire_trace).collect(),
        rejected: explanation
            .rejected
            .iter()
            .map(ToString::to_string)
            .collect(),
    }
}

fn wire_trace(trace: &RuleTrace) -> ScreenRuleTrace {
    ScreenRuleTrace {
        name: trace.name.to_string(),
        priority: trace.priority,
        region: region_text(trace.region),
        verdict: verdict_text(trace.verdict).to_owned(),
        matched: trace.matched,
        vetoed_by: trace
            .vetoed_by
            .map(|index| u32::try_from(index).unwrap_or(u32::MAX)),
        preview: trace.preview.clone(),
    }
}

fn verdict_text(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::State(ScreenState::Working) => "working",
        Verdict::State(ScreenState::AwaitingInput) => "awaiting_input",
        Verdict::State(ScreenState::Idle) => "idle",
        Verdict::Keep => "keep",
        Verdict::Background => "background",
        Verdict::NoMatch => "no_match",
    }
}

fn region_text(region: ScreenRegion) -> String {
    match region {
        ScreenRegion::BottomNonEmpty { rows } => format!("bottom_non_empty({rows})"),
        ScreenRegion::PromptBoxBody => "prompt_box_body".to_owned(),
        ScreenRegion::AbovePromptBox { rows } => format!("above_prompt_box({rows})"),
        ScreenRegion::Title => "title".to_owned(),
        ScreenRegion::AfterLastRule => "after_last_rule".to_owned(),
        ScreenRegion::Unknown => "unknown".to_owned(),
    }
}

#[cfg(test)]
#[expect(
    clippy::panic,
    clippy::expect_used,
    reason = "a test asserts its preconditions and a wrong reply fails it loudly"
)]
mod tests {
    use super::*;
    use crate::screen::{Screen, SharedScreen};
    use crate::server::connection::AuthKind;
    use crate::server::test_support::{
        dispatch_one, dispatch_one_as, idle_record, test_hub, test_peer,
    };
    use regex::Regex;
    use std::sync::{Arc, LazyLock, Mutex};

    /// The CSI sequences a fixture capture carries, for measuring a row's
    /// visible width without them.
    static CSI: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("the CSI pattern compiles"));

    fn ask(session_id: &str) -> ClientMessage {
        ClientMessage::DumpScreen {
            session_id: session_id.to_owned(),
            request_id: Some("q1".to_owned()),
        }
    }

    /// A fixture capture as a screen: its lines are the rows, in a grid wide
    /// enough for the widest of them.
    fn screen_from(text: &str) -> Screen {
        let lines: Vec<&str> = text.lines().collect();
        let widest = lines
            .iter()
            .map(|line| CSI.replace_all(line, "").chars().count())
            .max()
            .unwrap_or(0);
        let columns = u16::try_from(widest + 4).expect("a capture fits a grid");
        let rows = u16::try_from(lines.len() + 2).expect("a capture fits a grid");
        let mut screen = Screen::new(columns, rows);
        screen.feed(lines.join("\r\n").as_bytes());
        screen
    }

    #[tokio::test]
    async fn dump_of_an_unknown_session_is_refused_with_its_request_id() {
        let (hub, _scratch) = test_hub("screen-unknown");
        let reply = dispatch_one(&hub, ask("nope")).await;
        let DaemonMessage::Error {
            message,
            request_id,
        } = reply
        else {
            panic!("not an error: {reply:?}");
        };
        assert_eq!(message, "unknown session: nope");
        assert_eq!(request_id.as_deref(), Some("q1"));
    }

    #[tokio::test]
    async fn dump_of_a_session_without_a_screen_is_refused() {
        let (hub, _scratch) = test_hub("screen-none");
        hub.sessions.insert(idle_record("s1"));
        let reply = dispatch_one(&hub, ask("s1")).await;
        let DaemonMessage::Error {
            message,
            request_id,
        } = reply
        else {
            panic!("not an error: {reply:?}");
        };
        assert_eq!(message, "session s1 has no screen");
        assert_eq!(request_id.as_deref(), Some("q1"));
    }

    #[tokio::test]
    async fn dump_of_a_fed_screen_carries_its_rows_and_the_rules_reading() {
        let (hub, _scratch) = test_hub("screen-fed");
        let screen = screen_from(include_str!("../screen/fixtures/claude--fresh-idle.txt"));
        let rules = rules_for(&agent_registry::default_agent());
        let read = rules.read(&screen);
        assert_eq!(
            read.verdict,
            Verdict::State(ScreenState::Idle),
            "the fed screen reads Idle, so a rule wins"
        );
        let (cols, rows) = screen.size();
        let fed_rows = screen.rows();
        let screen: SharedScreen = Arc::new(Mutex::new(screen));
        let mut record = idle_record("s1");
        record.screen = Some(screen);
        hub.sessions.insert(record);

        let reply = dispatch_one(&hub, ask("s1")).await;
        let DaemonMessage::ScreenDump {
            session_id,
            request_id,
            cols: dumped_cols,
            rows: dumped_rows,
            grid,
            agent,
            explanation,
            ..
        } = reply
        else {
            panic!("not a dump: {reply:?}");
        };
        assert_eq!(session_id, "s1");
        assert_eq!(request_id.as_deref(), Some("q1"));
        assert_eq!((dumped_cols, dumped_rows), (cols, rows));
        assert_eq!(grid, fed_rows);
        assert_eq!(agent, Some(agent_registry::default_agent()));
        assert_eq!(explanation.winner.as_deref(), read.rule.as_deref());
        assert_eq!(explanation.verdict, verdict_text(read.verdict));
        assert_eq!(explanation.from_title, read.from_title);
        assert_ne!(explanation.verdict, "no_match", "a rule wins");
        assert!(!explanation.rules.is_empty(), "every rule is traced");
        assert_eq!(explanation.rejected, Vec::<String>::new());
    }

    #[tokio::test]
    async fn dump_from_a_tailnet_connection_is_local_only() {
        let (hub, _scratch) = test_hub("screen-tailnet");
        hub.sessions.insert(idle_record("s1"));
        let tailnet = AuthKind::Tailnet(test_peer());
        let reply = dispatch_one_as(&hub, ask("s1"), &tailnet).await;
        assert!(
            matches!(&reply, DaemonMessage::Error { message, .. } if message == "screen dump is local-only"),
            "{reply:?}"
        );
    }
}
