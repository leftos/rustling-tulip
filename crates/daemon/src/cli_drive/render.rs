//! What the session-driving verbs print: plain text and `--json`. Nothing
//! here touches a socket.

use super::waiting::Line;
use protocol::{SessionSnapshot, SessionStatus};
use serde_json::json;

/// What a session is shown as: the user's name for it, else the title its
/// program set, else its canonical label.
fn title(session: &SessionSnapshot) -> &str {
    session
        .user_label
        .as_deref()
        .or(session.terminal_title.as_deref())
        .unwrap_or(&session.label)
}

/// The name `status` carries on the wire.
pub fn status_name(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Spawning => "spawning",
        SessionStatus::Idle => "idle",
        SessionStatus::Working => "working",
        SessionStatus::AwaitingInput => "awaiting_input",
        SessionStatus::Stopped => "stopped",
        SessionStatus::Error => "error",
    }
}

/// Whether the session's agent is still running in its shell. A shell where
/// an agent was started keeps its `agent` after the agent ends: `program_name`
/// is that agent's tag while it runs and the shell's own name once the shell
/// is handed back. A session whose `program_name` was never recorded (a
/// sidecar from a daemon that pre-dates the field), or whose agent has no
/// registered plugin to name its tag, counts as up.
fn agent_up(session: &SessionSnapshot) -> bool {
    let Some(agent) = &session.agent else {
        return false;
    };
    let Some(plugin) = agent_registry::plugin(agent) else {
        return true;
    };
    let program = session
        .program_name
        .as_deref()
        .filter(|name| !name.is_empty());
    program.is_none_or(|program| program == plugin.descriptor().tag.as_str())
}

/// One tab-separated line per session: id, status, `bg` or `-`, agent id or
/// `shell` (also for a session whose agent has ended in its shell), title,
/// repo, branch. A tab or a line break inside a title, which a program sets
/// freely, turns into a space so a line stays one row.
pub fn sessions_text(sessions: &[SessionSnapshot]) -> String {
    let mut text = String::new();
    for session in sessions {
        let member = session.members.first();
        let agent = if agent_up(session) {
            session.agent.as_ref().map(|agent| agent.0.as_str())
        } else {
            None
        };
        let fields = [
            session.id.as_str(),
            status_name(session.status),
            if session.background { "bg" } else { "-" },
            agent.unwrap_or("shell"),
            &title(session).replace(['\t', '\r', '\n'], " "),
            member.map_or("-", |member| &member.repo_name),
            member.map_or("-", |member| &member.branch),
        ];
        text.push_str(&fields.join("\t"));
        text.push('\n');
    }
    text
}

/// The sessions as a JSON array, one line.
pub fn sessions_json(sessions: &[SessionSnapshot]) -> String {
    let rows: Vec<serde_json::Value> = sessions
        .iter()
        .map(|session| {
            let member = session.members.first();
            let agent = if agent_up(session) {
                session.agent.as_ref().map(|agent| agent.0.as_str())
            } else {
                None
            };
            json!({
                "id": session.id,
                "label": title(session),
                "status": status_name(session.status),
                "status_since": session.status_since,
                "background": session.background,
                "agent": agent,
                "repo": member.map(|member| &member.repo_name),
                "branch": member.map(|member| &member.branch),
            })
        })
        .collect();
    format!("{}\n", serde_json::Value::Array(rows))
}

/// A screen's rows, one per line.
pub fn screen_text(grid: &[String]) -> String {
    let mut text = grid.join("\n");
    text.push('\n');
    text
}

/// A screen as one line of JSON.
pub fn screen_json(
    id: &str,
    (cols, rows): (u16, u16),
    title: Option<&str>,
    grid: &[String],
) -> String {
    let screen = json!({ "id": id, "cols": cols, "rows": rows, "title": title, "lines": grid });
    format!("{screen}\n")
}

/// Scrollback text, ending in one line break.
pub fn scrollback_text(text: &str) -> String {
    let mut text = text.to_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Scrollback as one line of JSON.
pub fn scrollback_json(id: &str, truncated: bool, text: &str) -> String {
    let scrollback = json!({ "id": id, "truncated": truncated, "text": text });
    format!("{scrollback}\n")
}

/// A spawned session as one line of JSON.
pub fn spawned_json(session: &SessionSnapshot) -> String {
    let spawned = json!({
        "id": session.id,
        "label": title(session),
        "status": status_name(session.status),
    });
    format!("{spawned}\n")
}

/// The session a verb acted on, as one line of JSON.
pub fn id_json(id: &str) -> String {
    format!("{}\n", json!({ "id": id }))
}

/// The status a wait reached, as one line of JSON.
pub fn status_json(id: &str, status: SessionStatus) -> String {
    format!("{}\n", json!({ "id": id, "status": status_name(status) }))
}

/// The screen row a wait matched, as one line of JSON.
pub fn line_json(id: &str, line: &str) -> String {
    format!("{}\n", json!({ "id": id, "line": line }))
}

/// One `watch` line: when the status began, or `-` when the daemon never set
/// it, the status, `bg` or `-`, and the status's sequence number.
pub fn watch_text(line: &Line) -> String {
    let since = line
        .status_since
        .map_or_else(|| "-".to_owned(), |since| since.to_rfc3339());
    let status = status_name(line.status);
    let background = if line.background { "bg" } else { "-" };
    format!("{since} {status} {background} seq={}\n", line.status_seq)
}

/// One `watch` line as one line of JSON.
pub fn watch_json(line: &Line) -> String {
    let change = json!({
        "status_since": line.status_since,
        "status": status_name(line.status),
        "background": line.background,
        "status_seq": line.status_seq,
    });
    format!("{change}\n")
}

/// A failure as one line of JSON.
pub fn error_json(code: &str, message: &str) -> String {
    let error = json!({ "error": { "code": code, "message": message } });
    format!("{error}\n")
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::history::test_support::record;
    use protocol::{SessionMember, SessionMode};

    fn session() -> SessionSnapshot {
        let mut session = record("s1", SessionMode::Interactive).snapshot();
        session.label = "canonical".to_owned();
        session.agent = None;
        session
    }

    fn with_member(mut session: SessionSnapshot) -> SessionSnapshot {
        session.members = vec![SessionMember {
            repo_id: "r1".to_owned(),
            repo_name: "tulip".to_owned(),
            branch: "main".to_owned(),
            worktree_path: "C:\\tulip".to_owned(),
        }];
        session
    }

    #[test]
    fn sessions_lines_prefer_the_user_label() {
        let mut named = with_member(session());
        named.user_label = Some("mine".to_owned());
        named.terminal_title = Some("program".to_owned());
        named.background = true;
        let mut titled = session();
        titled.terminal_title = Some("pro\tgram".to_owned());
        let plain = session();
        assert_eq!(
            sessions_text(&[named, titled, plain]),
            "s1\tidle\tbg\tshell\tmine\ttulip\tmain\n\
             s1\tidle\t-\tshell\tpro gram\t-\t-\n\
             s1\tidle\t-\tshell\tcanonical\t-\t-\n"
        );
    }

    #[test]
    fn a_session_without_members_prints_dashes() {
        let text = sessions_text(&[session()]);
        assert!(text.ends_with("\tcanonical\t-\t-\n"), "{text:?}");
    }

    #[test]
    fn sessions_json_carries_its_fields() {
        let mut session = with_member(session());
        session.status = SessionStatus::AwaitingInput;
        session.agent = Some(protocol::AgentId("some-agent".to_owned()));
        let since = session.status_since;
        let text = sessions_json(&[session]);
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(
            value,
            json!([{
                "id": "s1",
                "label": "canonical",
                "status": "awaiting_input",
                "status_since": since,
                "background": false,
                "agent": "some-agent",
                "repo": "tulip",
                "branch": "main",
            }])
        );
    }

    /// The tag the default agent's leaf shows.
    fn agent_tag() -> String {
        agent_registry::plugin(&agent_registry::default_agent())
            .expect("the default agent is registered")
            .descriptor()
            .tag
            .clone()
    }

    /// A session bound to the default agent, whose `program_name` is
    /// `program`.
    fn with_agent(program: Option<&str>) -> SessionSnapshot {
        let mut session = with_member(session());
        session.agent = Some(agent_registry::default_agent());
        session.program_name = program.map(str::to_owned);
        session
    }

    #[test]
    fn sessions_text_names_a_shell_once_its_agent_ended() {
        let ended = with_agent(Some("pwsh"));
        assert_eq!(
            sessions_text(&[ended]),
            "s1\tidle\t-\tshell\tcanonical\ttulip\tmain\n"
        );
    }

    #[test]
    fn sessions_json_has_no_agent_once_it_ended() {
        let ended = with_agent(Some("pwsh"));
        let value: serde_json::Value =
            serde_json::from_str(&sessions_json(&[ended])).expect("valid JSON");
        assert_eq!(value[0]["agent"], serde_json::Value::Null);
    }

    #[test]
    fn sessions_still_name_a_running_agent_in_a_shell() {
        let tag = agent_tag();
        let id = agent_registry::default_agent().0;
        assert_eq!(
            sessions_text(&[with_agent(Some(&tag))]),
            format!("s1\tidle\t-\t{id}\tcanonical\ttulip\tmain\n")
        );
        let value: serde_json::Value =
            serde_json::from_str(&sessions_json(&[with_agent(Some(&tag))])).expect("valid JSON");
        assert_eq!(value[0]["agent"], json!(id));
    }

    #[test]
    fn a_missing_program_name_counts_as_up() {
        let value: serde_json::Value =
            serde_json::from_str(&sessions_json(&[with_agent(None)])).expect("valid JSON");
        assert_eq!(value[0]["agent"], json!(agent_registry::default_agent().0));
    }

    #[test]
    fn an_error_as_json_has_code_and_message() {
        assert_eq!(
            error_json("not_found", "unknown session: s1"),
            "{\"error\":{\"code\":\"not_found\",\"message\":\"unknown session: s1\"}}\n"
        );
    }
}
