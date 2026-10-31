//! Spawn dialog specs across hosts: a remote host's repo opens the dialog
//! on that host, with that host's agents, and its spawn goes to that host.

use crate::support;

use gpui::{TestAppContext, px};
use protocol::{AgentId, ClientMessage, DaemonMessage, SpawnRequest, SpawnTarget, SuggestTarget};
use rustling_tulip_native::target::Target;
use support::{Fixture, Harness, TestDir, repo, session};

/// The tailnet host the specs connect beside This PC.
const REMOTE: &str = "box.tail1234.ts.net";

/// The one agent the remote host lists, which This PC does not.
const BOX_AGENT: &str = "box-agent";

fn remote() -> Target {
    Target::tailnet(REMOTE, None)
}

/// A window on This PC holding `local_repos`, with the tailnet host
/// connected beside it, listing only [`BOX_AGENT`] and holding
/// `remote_repos`; nothing sent is left unread on either host.
fn two_hosts<'a>(
    cx: &'a mut TestAppContext,
    dir: &TestDir,
    local_repos: &[&str],
    remote_repos: &[&str],
) -> Harness<'a> {
    let mut fixture = Fixture::single(session("s1").build());
    fixture.repos = local_repos
        .iter()
        .map(|id| repo(id, &format!("C:/{id}")))
        .collect();
    let mut h = Harness::with(cx, dir, &fixture);
    let mut agent = agent_registry::descriptors()
        .into_iter()
        .next()
        .expect("the registry lists an agent");
    agent.id = AgentId(BOX_AGENT.to_owned());
    let mut host = h.host(&remote());
    host.connect();
    host.send(DaemonMessage::Agents {
        agents: vec![agent],
    });
    host.send(DaemonMessage::Sessions {
        sessions: Vec::new(),
    });
    host.send(DaemonMessage::Repos {
        repos: remote_repos
            .iter()
            .map(|id| repo(id, &format!("D:/{id}")))
            .collect(),
    });
    host.sent();
    h.sent();
    h
}

fn spawns(sent: &[ClientMessage]) -> Vec<SpawnRequest> {
    sent.iter()
        .filter_map(|msg| match msg {
            ClientMessage::SpawnSession(request) => Some(request.clone()),
            _ => None,
        })
        .collect()
}

/// The repo id and agent of the one spawn in `sent`.
fn the_spawn(sent: &[ClientMessage]) -> (String, Option<String>) {
    let requests = spawns(sent);
    assert_eq!(
        requests.len(),
        1,
        "expected one SpawnSession, sent {requests:?}"
    );
    let request = requests.into_iter().next().expect("one spawn");
    let repo_id = match &request.target {
        SpawnTarget::Single { repo_id, .. } => Some(repo_id.clone()),
        SpawnTarget::Workspace { .. } | SpawnTarget::Standalone { .. } => None,
    }
    .expect("a repo spawn");
    (repo_id, request.agent.map(|agent| agent.agent.0))
}

/// The remote host suggests `name` for its repo `repo_id`, as it answers
/// the dialog's own request.
fn remote_suggests(h: &mut Harness<'_>, repo_id: &str, name: &str) {
    h.host(&remote()).send(DaemonMessage::BranchNameSuggestion {
        target: SuggestTarget::Repo {
            repo_id: repo_id.to_owned(),
        },
        name: name.to_owned(),
        request_id: None,
    });
}

fn selected_targets(h: &mut Harness<'_>) -> Vec<String> {
    h.root(|root, _| root.spawn_dialog_selected())
        .into_iter()
        .filter(|selector| selector.starts_with("spawn-target-"))
        .collect()
}

#[gpui::test]
fn spawn_from_a_remote_containers_menu_goes_to_that_host_with_its_agent(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &["r1"], &["r2"]);
    h.right_click_on(&format!("container-{REMOTE}::repo:r2"));
    assert!(
        h.root(|root, _| root.container_menu_open()),
        "the menu opened"
    );
    h.click_on("container-menu-spawn");

    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "the dialog opened on the remote repo, not refused"
    );
    assert_eq!(
        selected_targets(&mut h),
        [format!("spawn-target-repo-r2@{REMOTE}")],
        "held on the remote host's r2"
    );
    let asked = h.host(&remote()).sent();
    assert!(
        asked
            .iter()
            .any(|msg| matches!(msg, ClientMessage::ListBranches { repo_id } if repo_id == "r2")),
        "the branches are asked of the remote host: {asked:?}"
    );
    remote_suggests(&mut h, "r2", "feat-box");
    h.click_on("spawn-submit");

    assert_eq!(
        the_spawn(&h.host(&remote()).sent()),
        ("r2".to_owned(), Some(BOX_AGENT.to_owned())),
        "the remote host spawns, with the agent it lists"
    );
    assert!(spawns(&h.sent()).is_empty(), "nothing spawns on This PC");
}

#[gpui::test]
fn with_repos_on_the_remote_host_alone_the_dialog_opens_on_them(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &[], &["r9"]);
    assert!(h.root(|root, _| root.has_repos()), "a remote repo counts");

    h.click_on("sidebar-add-session");

    assert!(
        h.root(|root, _| root.spawn_dialog_open()),
        "+ Session is enabled and opens the dialog"
    );
    assert!(
        !h.root(|root, _| root.spawn_dialog_no_repos()),
        "the form, not the no-repos state"
    );
    assert!(
        !(h.in_model("spawn-no-repos") && h.bounds("spawn-no-repos").origin.x >= px(0.0)),
        "no no-repos layer is drawn"
    );
    assert_eq!(
        selected_targets(&mut h),
        [format!("spawn-target-repo-r9@{REMOTE}")],
        "on the remote host's repo"
    );
}

#[gpui::test]
fn picking_the_remote_target_of_a_shared_repo_id_spawns_on_that_host(cx: &mut TestAppContext) {
    let dir = TestDir::new();
    let mut h = two_hosts(cx, &dir, &["r1"], &["r1"]);
    h.click_on("sidebar-add-session");
    assert_eq!(
        selected_targets(&mut h),
        ["spawn-target-repo-r1".to_owned()],
        "This PC's r1 first"
    );
    h.sent();

    h.click_on(&format!("spawn-target-repo-r1@{REMOTE}"));
    assert_eq!(
        selected_targets(&mut h),
        [format!("spawn-target-repo-r1@{REMOTE}")]
    );
    remote_suggests(&mut h, "r1", "feat-box");
    h.click_on("spawn-submit");

    assert_eq!(
        the_spawn(&h.host(&remote()).sent()),
        ("r1".to_owned(), Some(BOX_AGENT.to_owned())),
        "the remote host's r1 spawns there"
    );
    assert!(spawns(&h.sent()).is_empty(), "This PC's r1 is left alone");
}
