//! The phone's Home: the open hosts' sessions grouped under their repo or
//! workspace, every row in a fixed place. Urgency is a mark, not a
//! position: a container holding a session that needs the owner carries a
//! count and a mark, and its rows say so, but nothing moves.
//!
//! Plain Rust over the sidebar's host groups, so the order and the marks
//! are unit-testable without GPUI; `home_view` draws them.

use crate::needs_you;
use crate::sidebar::{
    Container, ContainerKind, HostGroup, Leaf, LeafKind, LeafState, SidebarModel,
};
use crate::target::HostKey;
use protocol::SessionStatus;

/// How a row's status reads at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    /// The Needs you list shows the session.
    NeedsYou,
    /// The session is starting or working.
    Working,
    /// Everything else: idle, stopped, parked.
    Quiet,
}

/// One session's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HomeRow {
    pub(crate) host: HostKey,
    pub(crate) session_id: String,
    /// The label the sidebar's leaf shows.
    pub(crate) label: String,
    /// The status in a word or two.
    pub(crate) status: &'static str,
    pub(crate) tone: Tone,
}

/// One repo, workspace, shell, folder or Detached group and its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HomeGroup {
    /// The sidebar container's key, unique across hosts.
    pub(crate) key: String,
    pub(crate) name: String,
    /// The group holds sessions whose repo or workspace is gone, and says
    /// so above its rows.
    pub(crate) detached: bool,
    /// How many of its sessions need the owner; the heading's mark shows
    /// while it is above zero.
    pub(crate) needs_you: usize,
    pub(crate) rows: Vec<HomeRow>,
}

/// One open host's groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HomeHost {
    pub(crate) host: HostKey,
    /// The name its header shows.
    pub(crate) label: String,
    pub(crate) groups: Vec<HomeGroup>,
}

/// What Home lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Home {
    /// Each host gets a header only while more than one host is open.
    pub(crate) host_headers: bool,
    pub(crate) hosts: Vec<HomeHost>,
}

impl Home {
    /// Every row, in list order.
    pub(crate) fn rows(&self) -> impl Iterator<Item = &HomeRow> {
        self.hosts
            .iter()
            .flat_map(|host| &host.groups)
            .flat_map(|group| &group.rows)
    }

    /// The label of `host`'s session `session_id`, while Home lists it.
    pub(crate) fn label_of(&self, host: &HostKey, session_id: &str) -> Option<&str> {
        self.rows()
            .find(|row| row.host == *host && row.session_id == session_id)
            .map(|row| row.label.as_str())
    }
}

/// Home over `model`'s host groups in their manual order.
pub(crate) fn from_sidebar(model: &SidebarModel) -> Home {
    home(model.host_groups_in_place())
}

/// Home over `groups`, kept in their order, every host listed whether its
/// connection is open or not, so a dropped connection moves no row. A
/// container with no session row is left out.
pub(crate) fn home(groups: Vec<HostGroup>) -> Home {
    let hosts: Vec<HomeHost> = groups
        .into_iter()
        .map(|group| HomeHost {
            groups: group.containers.iter().filter_map(home_group).collect(),
            host: group.host,
            label: group.label,
        })
        .collect();
    Home {
        host_headers: hosts.len() > 1,
        hosts,
    }
}

/// `container`'s group, or `None` when it lists no session row.
fn home_group(container: &Container) -> Option<HomeGroup> {
    let rows: Vec<HomeRow> = container
        .leaves
        .iter()
        .filter(|leaf| listed(leaf))
        .map(home_row)
        .collect();
    if rows.is_empty() {
        return None;
    }
    Some(HomeGroup {
        key: container.key.clone(),
        name: container.name.clone(),
        detached: container.kind == ContainerKind::Detached,
        needs_you: rows.iter().filter(|row| row.tone == Tone::NeedsYou).count(),
        rows,
    })
}

/// Whether Home lists `leaf`: a session's own row, never a nested subagent
/// or dispatch run.
fn listed(leaf: &Leaf) -> bool {
    leaf.kind == LeafKind::Session && leaf.depth == 0
}

fn home_row(leaf: &Leaf) -> HomeRow {
    HomeRow {
        host: leaf.host.clone(),
        session_id: leaf.id.clone(),
        label: leaf.label.clone(),
        status: status_word(leaf),
        tone: tone(leaf),
    }
}

/// `leaf`'s tone: Needs you exactly when the Needs you list shows it.
fn tone(leaf: &Leaf) -> Tone {
    if needs_you::is_listed(leaf) {
        Tone::NeedsYou
    } else if matches!(
        leaf.status,
        SessionStatus::Spawning | SessionStatus::Working
    ) && matches!(leaf.state, LeafState::Live | LeafState::Orphan)
    {
        Tone::Working
    } else {
        Tone::Quiet
    }
}

/// `leaf`'s status in a word or two.
fn status_word(leaf: &Leaf) -> &'static str {
    if needs_you::is_listed(leaf) {
        return "Needs you";
    }
    match leaf.state {
        LeafState::Inactive { .. } => "Parked",
        LeafState::Abandoned { .. } => "Abandoned",
        LeafState::Live | LeafState::Orphan => status_literal(leaf.status, leaf.background),
    }
}

/// A live session's `status` in a word or two, `background` while idle
/// with its turn's work still running.
fn status_literal(status: SessionStatus, background: bool) -> &'static str {
    match status {
        SessionStatus::Spawning => "Starting",
        SessionStatus::Working => "Working",
        SessionStatus::AwaitingInput => "Needs you",
        SessionStatus::Idle if background => "Background",
        SessionStatus::Idle => "Idle",
        SessionStatus::Stopped => "Stopped",
        SessionStatus::Error => "Error",
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test fixtures")]
mod tests {
    use super::*;
    use crate::connection_picker::SavedHost;
    use crate::target::Target;
    use protocol::{DaemonMessage, RepoEntry, SessionSnapshot};
    use serde_json::json;

    /// The tailnet hosts the fixtures save.
    const BOX: &str = "box.tail1234.ts.net";
    const DEN: &str = "den.tail1234.ts.net";

    fn target(name: &str) -> Target {
        Target::tailnet(name, Some(8787))
    }

    fn key(name: &str) -> HostKey {
        HostKey::of(&target(name))
    }

    fn repo(id: &str) -> RepoEntry {
        serde_json::from_value(json!({ "id": id, "name": id, "path": format!("C:/{id}") }))
            .expect("repo fixture")
    }

    fn session(id: &str, repo: &str, status: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [{
                "repo_id": repo,
                "repo_name": repo,
                "branch": "main",
                "worktree_path": "",
            }],
            "status": status,
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
            "agent": "claude",
        }))
        .expect("session fixture")
    }

    /// `id` in `repo`, waiting on a question its hooks reported.
    fn asking(id: &str, repo: &str) -> SessionSnapshot {
        let mut s = session(id, repo, "awaiting_input");
        s.pending_input = Some(
            serde_json::from_value(json!({
                "kind": "question",
                "questions": [{
                    "question": "keep newest or merge?",
                    "header": "Migration",
                    "options": [],
                    "multi_select": false,
                }],
            }))
            .expect("pending fixture"),
        );
        s
    }

    /// Each of `hosts` saved, This PC hidden as on a phone, and each
    /// listing repos r1 and r2 with `sessions`.
    fn model(hosts: &[&str], sessions: &[SessionSnapshot]) -> SidebarModel {
        let mut model = SidebarModel::default();
        model.set_saved_hosts(
            hosts
                .iter()
                .map(|name| SavedHost::from_target(&target(name)).expect("a tailnet host"))
                .collect(),
        );
        model.hide_this_pc();
        for name in hosts {
            model.apply(
                &key(name),
                &DaemonMessage::Repos {
                    repos: vec![repo("r1"), repo("r2")],
                },
            );
            model.apply(
                &key(name),
                &DaemonMessage::Sessions {
                    sessions: sessions.to_vec(),
                },
            );
        }
        model
    }

    fn working_and_idle() -> Vec<SessionSnapshot> {
        vec![
            session("a", "r1", "working"),
            session("b", "r2", "idle"),
            session("c", "r2", "idle"),
        ]
    }

    /// Each group's name and its rows' ids, in list order.
    fn shape(home: &Home) -> Vec<(String, Vec<String>)> {
        home.hosts
            .iter()
            .flat_map(|host| &host.groups)
            .map(|group| {
                let ids = group
                    .rows
                    .iter()
                    .map(|row| row.session_id.clone())
                    .collect();
                (group.name.clone(), ids)
            })
            .collect()
    }

    fn group<'a>(home: &'a Home, name: &str) -> &'a HomeGroup {
        home.hosts
            .iter()
            .flat_map(|host| &host.groups)
            .find(|group| group.name == name)
            .expect("the group is listed")
    }

    #[test]
    fn rows_keep_their_place_when_a_session_starts_needing_you() {
        let mut model = model(&[BOX], &working_and_idle());
        let before = from_sidebar(&model);
        let second = &before.hosts[0].groups[1];
        let flipped = second.rows[0].session_id.clone();
        let second_name = second.name.clone();
        let repo_id = if second_name == "r1" { "r1" } else { "r2" };

        model.apply(
            &key(BOX),
            &DaemonMessage::SessionUpdated {
                session: asking(&flipped, repo_id),
                request_id: None,
            },
        );
        let after = from_sidebar(&model);

        assert_eq!(shape(&after), shape(&before), "no group or row moved");
        assert_eq!(group(&before, &second_name).needs_you, 0);
        assert_eq!(group(&after, &second_name).needs_you, 1, "only the mark");
        let row = after
            .rows()
            .find(|row| row.session_id == flipped)
            .expect("the row is listed");
        assert_eq!((row.status, row.tone), ("Needs you", Tone::NeedsYou));
    }

    #[test]
    fn a_container_needing_you_carries_the_count_and_mark() {
        let model = model(
            &[BOX],
            &[
                session("a", "r1", "working"),
                asking("b", "r2"),
                asking("c", "r2"),
            ],
        );
        let home = from_sidebar(&model);

        assert_eq!(group(&home, "r2").needs_you, 2);
        assert_eq!(
            group(&home, "r1").needs_you,
            0,
            "a group with none has no mark"
        );
        let a = home.rows().find(|row| row.session_id == "a").expect("a");
        assert_eq!((a.status, a.tone), ("Working", Tone::Working));
    }

    #[test]
    fn one_host_has_no_host_header() {
        let home = from_sidebar(&model(&[BOX], &working_and_idle()));
        assert!(!home.host_headers);
        assert_eq!(home.hosts.len(), 1);
    }

    #[test]
    fn two_hosts_have_host_headers() {
        let home = from_sidebar(&model(&[BOX, DEN], &working_and_idle()));
        assert!(home.host_headers);
        let hosts: Vec<&HostKey> = home.hosts.iter().map(|host| &host.host).collect();
        assert_eq!(hosts, [&key(BOX), &key(DEN)]);

        let mut quiet = model(&[BOX], &working_and_idle());
        let mut saved = quiet.ui_state().saved_hosts.clone();
        saved.push(SavedHost::from_target(&target(DEN)).expect("a tailnet host"));
        quiet.set_saved_hosts(saved);
        let home = from_sidebar(&quiet);
        assert!(home.host_headers, "a saved host that sent no list counts");
        assert!(home.hosts[1].groups.is_empty(), "and lists no row");
    }

    #[test]
    fn subagent_rows_are_not_listed() {
        let mut groups = model(&[BOX], &working_and_idle()).host_groups_in_place();
        let container = &mut groups[0].containers[0];
        let mut nested = container.leaves[0].clone();
        nested.id = "sub".to_owned();
        nested.kind = LeafKind::Subagent;
        nested.depth = 1;
        nested.host_session = Some(container.leaves[0].id.clone());
        container.leaves.insert(1, nested);

        let home = home(groups);
        assert!(
            home.rows().all(|row| row.session_id != "sub"),
            "no nested row is listed"
        );
        assert_eq!(home.rows().count(), 3, "every session's own row is");
    }
}
