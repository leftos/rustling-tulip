//! The orders the phone's Home can list its sessions in ([`PhoneOrder`]),
//! each applied inside every host, the hosts keeping their order: Manual
//! leaves Home as it is; A–Z sorts the groups and their rows by name;
//! Desktop tabs mirrors the tabs of the desktop's own layout on the host, as
//! its last `HostLayouts` reply had them.
//!
//! With the "Needs you first" switch on, the order runs first and each host's
//! sessions waiting on the owner are then gathered into one group at its top.
//!
//! Plain Rust over [`Home`], so each order is unit-testable without GPUI.

use std::collections::{HashMap, HashSet};

use protocol::{GridNode, HostLayout, TabEntry};

use super::home::{Home, HomeGroup, HomeHost, HomeRow, Tone};
use crate::sidebar::PhoneOrder;
use crate::target::HostKey;

/// The heading of the group Needs you first opens a host with.
pub(crate) const NEEDS_YOU_GROUP: &str = "Needs you";
/// Every Home row's tab name, by its host's wire key (`None` for This PC)
/// and its id.
type TabGroups = HashMap<(Option<String>, String), Option<String>>;
/// The heading of Desktop tabs' last group: the sessions in no tab.
pub(crate) const NOT_IN_A_TAB: &str = "Not in a tab";
/// What a host says under its header in Desktop tabs while its desktop's
/// own layout is unknown.
pub(crate) const NO_LAYOUT_NOTE: &str = "No desktop layout on this host";

/// The desktop's own layout on each host, from the host's last
/// `HostLayouts` reply, and the request out to each host for it.
#[derive(Debug, Default)]
pub(crate) struct PhoneLayouts {
    /// The request id of the `ListHostLayouts` out to each host.
    out: HashMap<HostKey, String>,
    /// Each host's own layout, while its last reply named one.
    own: HashMap<HostKey, HostLayout>,
}

impl PhoneLayouts {
    /// A `ListHostLayouts` under `request_id` went to `host`; only its reply
    /// is kept.
    pub(crate) fn asked(&mut self, host: &HostKey, request_id: String) {
        self.out.insert(host.clone(), request_id);
    }

    /// `host`'s reply under `request_id`: when it answers the request out to
    /// that host, its own layout ([`own_layout`]) replaces the one kept.
    /// Returns whether it was taken.
    pub(crate) fn on_reply(
        &mut self,
        host: &HostKey,
        host_name: Option<&str>,
        layouts: &[HostLayout],
        request_id: Option<&str>,
    ) -> bool {
        if request_id.is_none() || self.out.get(host).map(String::as_str) != request_id {
            return false;
        }
        self.out.remove(host);
        match own_layout(host_name, layouts) {
            Some(layout) => {
                self.own.insert(host.clone(), layout.clone());
            }
            None => {
                self.own.remove(host);
            }
        }
        true
    }

    /// Forgets `host`'s layout and its request out; returns whether there
    /// was anything to forget.
    pub(crate) fn forget(&mut self, host: &HostKey) -> bool {
        let asked = self.out.remove(host).is_some();
        self.own.remove(host).is_some() || asked
    }

    /// `host`'s own layout, while its last reply named one.
    pub(crate) fn own(&self, host: &HostKey) -> Option<&HostLayout> {
        self.own.get(host)
    }
}

/// The host's own layout among `layouts`: the one named `host_name` (both
/// names known), else the only one, else none.
pub(crate) fn own_layout<'a>(
    host_name: Option<&str>,
    layouts: &'a [HostLayout],
) -> Option<&'a HostLayout> {
    let named = host_name.and_then(|name| {
        layouts
            .iter()
            .find(|layout| layout.name.as_deref() == Some(name))
    });
    match (named, layouts) {
        (Some(layout), _) => Some(layout),
        (None, [only]) => Some(only),
        (None, _) => None,
    }
}

/// `home` in `order`, each host's own layout read from `layouts`, its
/// sessions waiting on the owner gathered first while `needs_you_first`.
pub(crate) fn apply(
    home: Home,
    order: PhoneOrder,
    needs_you_first: bool,
    layouts: &PhoneLayouts,
) -> Home {
    if order == PhoneOrder::Manual && !needs_you_first {
        return home;
    }
    let tab_groups = tab_groups(&home, order);
    Home {
        host_headers: home.host_headers,
        hosts: home
            .hosts
            .into_iter()
            .map(|host| order_host(host, order, needs_you_first, layouts, &tab_groups))
            .collect(),
    }
}

/// Every listed row's tab name, by its host's wire key and id, under the
/// Desktop tabs order; none under the others.
fn tab_groups(home: &Home, order: PhoneOrder) -> TabGroups {
    if order != PhoneOrder::DesktopTabs {
        return TabGroups::new();
    }
    home.hosts
        .iter()
        .flat_map(|host| &host.groups)
        .flat_map(|group| &group.rows)
        .map(|row| {
            let key = (row.host.wire().map(str::to_owned), row.session_id.clone());
            (key, row.tab_group.clone())
        })
        .collect()
}

/// `host`'s groups in `order`, then its waiting sessions gathered first
/// while `needs_you_first`.
fn order_host(
    mut host: HomeHost,
    order: PhoneOrder,
    needs_you_first: bool,
    layouts: &PhoneLayouts,
    tab_groups: &TabGroups,
) -> HomeHost {
    let groups = std::mem::take(&mut host.groups);
    let ordered = match order {
        PhoneOrder::Manual => groups,
        PhoneOrder::Alphabetical => alphabetical(groups),
        PhoneOrder::DesktopTabs => {
            if let Some(layout) = layouts.own(&host.host) {
                desktop_tabs(&host.host, groups, layout, tab_groups)
            } else {
                host.note = Some(NO_LAYOUT_NOTE);
                groups
            }
        }
    };
    host.groups = if needs_you_first {
        gather_needs_you(&host.host, ordered)
    } else {
        ordered
    };
    host
}

/// The key a host's own groups are tagged with.
fn host_tag(host: &HostKey) -> &str {
    host.wire().unwrap_or("local")
}

/// A group of `rows` under `name`, its Needs you count taken from them, its
/// rows naming the repo or workspace they sit under while `show_container`.
fn group(key: String, name: String, rows: Vec<HomeRow>, show_container: bool) -> HomeGroup {
    HomeGroup {
        key,
        name,
        detached: false,
        show_container,
        needs_you: rows.iter().filter(|row| row.tone == Tone::NeedsYou).count(),
        rows,
    }
}

/// The groups by name and each group's rows by label, both
/// case-insensitive; a Detached group stays last.
fn alphabetical(mut groups: Vec<HomeGroup>) -> Vec<HomeGroup> {
    for group in &mut groups {
        group
            .rows
            .sort_by_cached_key(|row| row.label.to_lowercase());
    }
    groups.sort_by_cached_key(|group| (group.detached, group.name.to_lowercase()));
    groups
}

/// A "Needs you" group of every waiting session, in the order the groups
/// now sit, then the groups without them; a group left with no row goes.
/// With no session waiting, the groups as they are.
fn gather_needs_you(host: &HostKey, groups: Vec<HomeGroup>) -> Vec<HomeGroup> {
    let waiting: Vec<HomeRow> = groups
        .iter()
        .flat_map(|group| &group.rows)
        .filter(|row| row.tone == Tone::NeedsYou)
        .cloned()
        .collect();
    if waiting.is_empty() {
        return groups;
    }
    let key = format!("needs-you-{}", host_tag(host));
    let rest = groups.into_iter().filter_map(|mut group| {
        group.rows.retain(|row| row.tone != Tone::NeedsYou);
        group.needs_you = 0;
        (!group.rows.is_empty()).then_some(group)
    });
    std::iter::once(group(key, NEEDS_YOU_GROUP.to_owned(), waiting, false))
        .chain(rest)
        .collect()
}

/// One group per grid tab of `layout` holding a session of `host`, in tab
/// order, its rows in pane order (a session listed once, at its first
/// pane), then "Not in a tab" with the rest in manual order.
fn desktop_tabs(
    host: &HostKey,
    groups: Vec<HomeGroup>,
    layout: &HostLayout,
    tab_groups: &TabGroups,
) -> Vec<HomeGroup> {
    let tag = host_tag(host);
    let rows: Vec<HomeRow> = groups.into_iter().flat_map(|group| group.rows).collect();
    let own = host.wire();
    let mut placed: HashSet<&str> = HashSet::new();
    let mut tabs = Vec::new();
    for (at, tab) in layout.tabs.iter().enumerate() {
        let Some(grid) = tab.grid() else {
            continue;
        };
        let mut panes = Vec::new();
        pane_sessions(grid, &mut panes);
        let tab_rows: Vec<HomeRow> = panes
            .iter()
            .filter(|(named, _)| named.is_none() || *named == own)
            .filter_map(|(_, id)| rows.iter().find(|row| row.session_id == *id))
            .filter(|row| placed.insert(row.session_id.as_str()))
            .cloned()
            .collect();
        if !tab_rows.is_empty() {
            tabs.push(group(
                format!("tab-{tag}-{}", tab.id),
                tab_name(tab, at, host, tab_groups),
                tab_rows,
                true,
            ));
        }
    }
    let rest: Vec<HomeRow> = rows
        .iter()
        .filter(|row| !placed.contains(row.session_id.as_str()))
        .cloned()
        .collect();
    if !rest.is_empty() {
        tabs.push(group(
            format!("no-tab-{tag}"),
            NOT_IN_A_TAB.to_owned(),
            rest,
            true,
        ));
    }
    tabs
}

/// `tab`'s heading: the name the desktop gives it, its own name or, for a tab
/// nobody named, the repos and workspaces of its sessions in pane order
/// ([`crate::tab_name::display_name`]). A pane naming another host the phone
/// lists reads that host's row; a pane naming a host it does not list is
/// left out. "Tab {n}" while that names nothing, `n` counting from 1 in tab
/// order.
fn tab_name(tab: &TabEntry, at: usize, host: &HostKey, names: &TabGroups) -> String {
    let mut panes = Vec::new();
    if let Some(grid) = tab.grid() {
        pane_sessions(grid, &mut panes);
    }
    let groups = panes.iter().filter_map(|(named, id)| {
        let wire = (*named)
            .map(str::to_owned)
            .or_else(|| host.wire().map(str::to_owned));
        names.get(&(wire, (*id).to_owned())).map(Option::as_deref)
    });
    let name = crate::tab_name::display_name(&tab.name, groups);
    if name.trim().is_empty() {
        format!("Tab {}", at + 1)
    } else {
        name
    }
}

/// The sessions `node`'s panes show, in pane order: a split's first side,
/// then its second, each with the host key its pane names, `None` for the
/// daemon's own.
fn pane_sessions<'a>(node: &'a GridNode, panes: &mut Vec<(Option<&'a str>, &'a str)>) {
    match node {
        GridNode::Pane {
            session_id: Some(id),
            host,
            ..
        } => panes.push((host.as_deref(), id)),
        GridNode::Pane { .. } => {}
        GridNode::Split { first, second, .. } => {
            pane_sessions(first, panes);
            pane_sessions(second, panes);
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::needless_pass_by_value,
    reason = "test fixtures, built from JSON values they take whole"
)]
mod tests {
    use super::*;
    use crate::connection_picker::SavedHost;
    use crate::sidebar::SidebarModel;
    use crate::target::Target;
    use protocol::{DaemonMessage, RepoEntry, SessionSnapshot};
    use serde_json::{Value, json};

    /// A phone's model with one saved tailnet host `host` and This PC hidden,
    /// listing `repos` and `sessions`.
    fn sidebar(host: &str, repos: Vec<RepoEntry>, sessions: Vec<SessionSnapshot>) -> SidebarModel {
        let target = Target::tailnet(host, Some(8787));
        let mut model = SidebarModel::default();
        model.set_saved_hosts(vec![
            SavedHost::from_target(&target).expect("a tailnet host"),
        ]);
        model.hide_this_pc();
        let key = HostKey::of(&target);
        model.apply(&key, &DaemonMessage::Repos { repos });
        model.apply(&key, &DaemonMessage::Sessions { sessions });
        model
    }

    /// The key of the tailnet host `host`.
    fn key(host: &str) -> HostKey {
        HostKey::of(&Target::tailnet(host, Some(8787)))
    }

    fn repo_entry(id: &str, path: &str) -> RepoEntry {
        serde_json::from_value(json!({ "id": id, "name": id, "path": path })).expect("repo fixture")
    }

    /// The base of a session fixture of `id`.
    fn base_session(id: &str) -> Value {
        json!({
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
            "agent": "claude",
        })
    }

    /// A session of `id` working in `repo`.
    fn in_repo_session(id: &str, repo: &str) -> SessionSnapshot {
        let mut value = base_session(id);
        value["members"] = json!([{
            "repo_id": repo,
            "repo_name": repo,
            "branch": "main",
            "worktree_path": "",
        }]);
        serde_json::from_value(value).expect("session fixture")
    }

    /// A standalone shell of `id`, filed under no repo.
    fn shell_session(id: &str) -> SessionSnapshot {
        let mut value = base_session(id);
        value["mode"] = json!("plain_shell");
        value["kind"] = json!("standalone");
        serde_json::from_value(value).expect("shell fixture")
    }

    /// A shell of `id` working in the folder `cwd`, which is no repo.
    fn dir_session(id: &str, cwd: &str) -> SessionSnapshot {
        let mut value = base_session(id);
        value["mode"] = json!("plain_shell");
        value["current_cwd"] = json!(cwd);
        serde_json::from_value(value).expect("folder shell fixture")
    }

    fn host() -> HostKey {
        HostKey::Local
    }

    fn row(id: &str, label: &str, tone: Tone) -> HomeRow {
        HomeRow {
            host: host(),
            session_id: id.to_owned(),
            label: label.to_owned(),
            status: "Idle",
            tone,
            container: None,
            tab_group: None,
        }
    }

    fn quiet(id: &str, label: &str) -> HomeRow {
        row(id, label, Tone::Quiet)
    }

    fn container(name: &str, detached: bool, mut rows: Vec<HomeRow>) -> HomeGroup {
        let listed_under = (!detached).then(|| name.to_owned());
        for row in &mut rows {
            row.container = listed_under.clone();
            row.tab_group = listed_under.clone();
        }
        HomeGroup {
            key: format!("repo-{name}"),
            name: name.to_owned(),
            detached,
            show_container: false,
            needs_you: rows.iter().filter(|row| row.tone == Tone::NeedsYou).count(),
            rows,
        }
    }

    fn home(groups: Vec<HomeGroup>) -> Home {
        Home {
            host_headers: false,
            hosts: vec![HomeHost {
                host: host(),
                label: "box".to_owned(),
                note: None,
                groups,
            }],
        }
    }

    /// Two repos and Detached, in the sidebar's manual order.
    fn manual() -> Home {
        home(vec![
            container(
                "web",
                false,
                vec![quiet("w2", "zeta"), row("w1", "Alpha", Tone::NeedsYou)],
            ),
            container(
                "api",
                false,
                vec![quiet("a1", "beta"), quiet("a2", "Gamma")],
            ),
            container("Detached", true, vec![row("d1", "delta", Tone::NeedsYou)]),
        ])
    }

    /// Each group's name and its rows' ids, in list order.
    fn shape(home: &Home) -> Vec<(String, Vec<String>)> {
        home.hosts[0]
            .groups
            .iter()
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

    fn names(pairs: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        pairs
            .iter()
            .map(|(name, ids)| {
                let ids = ids.iter().map(|id| (*id).to_owned()).collect();
                ((*name).to_owned(), ids)
            })
            .collect()
    }

    fn pane(id: &str, session: Option<&str>, host: Option<&str>) -> Value {
        json!({ "kind": "pane", "pane_id": id, "session_id": session, "host": host })
    }

    fn split(first: Value, second: Value) -> Value {
        json!({ "kind": "split", "direction": "horizontal", "ratio": 0.5, "first": first, "second": second })
    }

    fn tab(id: &str, name: &str, content: Value) -> Value {
        json!({ "id": id, "name": name, "content": content, "created_at": "2026-01-01T00:00:00Z" })
    }

    fn grid(node: Value) -> Value {
        json!({ "kind": "grid", "grid": node })
    }

    fn layout(name: Option<&str>, tabs: Vec<Value>) -> HostLayout {
        serde_json::from_value(json!({ "client_id": "c1", "name": name, "tabs": tabs }))
            .expect("a layout fixture")
    }

    /// The store holding `layout` as the host's reply.
    fn layouts_with(layout: HostLayout) -> PhoneLayouts {
        layouts_for(&host(), layout)
    }

    /// The store holding `layout` as `host`'s reply.
    fn layouts_for(host: &HostKey, layout: HostLayout) -> PhoneLayouts {
        let mut layouts = PhoneLayouts::default();
        layouts.asked(host, "q1".to_owned());
        assert!(layouts.on_reply(host, None, &[layout], Some("q1")));
        layouts
    }

    /// `value` as the tab it describes.
    fn tab_entry(id: &str, name: &str, content: Value) -> TabEntry {
        serde_json::from_value(tab(id, name, content)).expect("a tab fixture")
    }

    #[test]
    fn manual_leaves_home_as_it_is() {
        let ordered = apply(
            manual(),
            PhoneOrder::Manual,
            false,
            &PhoneLayouts::default(),
        );
        assert_eq!(ordered, manual());
    }

    #[test]
    fn a_to_z_sorts_groups_and_rows_case_insensitively_with_detached_last() {
        let ordered = apply(
            manual(),
            PhoneOrder::Alphabetical,
            false,
            &PhoneLayouts::default(),
        );
        assert_eq!(
            shape(&ordered),
            names(&[
                ("api", &["a1", "a2"]),
                ("web", &["w1", "w2"]),
                ("Detached", &["d1"]),
            ])
        );
        assert!(
            ordered.hosts[0].groups[2].detached,
            "Detached keeps its banner"
        );
        assert_eq!(ordered.hosts[0].note, None);
    }

    #[test]
    fn needs_you_first_gathers_the_waiting_sessions_into_one_group() {
        let ordered = apply(manual(), PhoneOrder::Manual, true, &PhoneLayouts::default());
        assert_eq!(
            shape(&ordered),
            names(&[
                ("Needs you", &["w1", "d1"]),
                ("web", &["w2"]),
                ("api", &["a1", "a2"]),
            ]),
            "the waiting sessions leave their groups; Detached, emptied, goes"
        );
        let groups = &ordered.hosts[0].groups;
        assert_eq!(groups[0].needs_you, 2, "the group's mark counts them");
        assert_eq!(groups[1].needs_you, 0, "no group below carries a mark");
    }

    #[test]
    fn needs_you_first_adds_no_group_while_no_session_waits() {
        let calm = home(vec![
            container("web", false, vec![quiet("w1", "a")]),
            container("api", false, vec![quiet("a1", "b")]),
        ]);
        let ordered = apply(
            calm.clone(),
            PhoneOrder::Manual,
            true,
            &PhoneLayouts::default(),
        );
        assert_eq!(ordered, calm);
    }

    #[test]
    fn the_needs_you_switch_gathers_after_a_to_z() {
        let ordered = apply(
            manual(),
            PhoneOrder::Alphabetical,
            true,
            &PhoneLayouts::default(),
        );
        assert_eq!(
            shape(&ordered),
            names(&[
                ("Needs you", &["w1", "d1"]),
                ("api", &["a1", "a2"]),
                ("web", &["w2"]),
            ]),
            "the order runs first: web's w1 before Detached's d1; Detached, emptied, goes"
        );
        let groups = &ordered.hosts[0].groups;
        assert_eq!(groups[0].needs_you, 2, "the group's mark counts them");
    }

    #[test]
    fn the_needs_you_switch_gathers_after_desktop_tabs() {
        let own = layout(
            Some("box"),
            vec![
                tab(
                    "t1",
                    "Build",
                    grid(split(
                        pane("p1", Some("a2"), None),
                        pane("p2", Some("w2"), None),
                    )),
                ),
                tab("t2", "", grid(pane("p3", Some("d1"), None))),
            ],
        );
        let ordered = apply(manual(), PhoneOrder::DesktopTabs, true, &layouts_with(own));
        assert_eq!(
            shape(&ordered),
            names(&[
                ("Needs you", &["d1", "w1"]),
                ("Build", &["a2", "w2"]),
                ("Not in a tab", &["a1"]),
            ]),
            "the tabs order first; Tab 2, emptied, goes"
        );
    }

    #[test]
    fn the_needs_you_switch_off_leaves_the_order_alone() {
        let ordered = apply(
            manual(),
            PhoneOrder::Alphabetical,
            false,
            &PhoneLayouts::default(),
        );
        assert_eq!(
            shape(&ordered),
            names(&[
                ("api", &["a1", "a2"]),
                ("web", &["w1", "w2"]),
                ("Detached", &["d1"]),
            ])
        );
    }

    #[test]
    fn desktop_tabs_lists_each_tab_in_order_with_its_sessions_in_pane_order() {
        let own = layout(
            Some("box"),
            vec![
                tab(
                    "t1",
                    "Build",
                    grid(split(
                        pane("p1", Some("a2"), None),
                        pane("p2", Some("w2"), None),
                    )),
                ),
                tab("t2", "", grid(pane("p3", Some("d1"), None))),
            ],
        );
        let ordered = apply(manual(), PhoneOrder::DesktopTabs, false, &layouts_with(own));
        assert_eq!(
            shape(&ordered),
            names(&[
                ("Build", &["a2", "w2"]),
                ("Tab 2", &["d1"]),
                ("Not in a tab", &["w1", "a1"]),
            ])
        );
        let groups = &ordered.hosts[0].groups;
        assert_eq!(
            groups[1].needs_you, 1,
            "a tab's mark counts its waiting sessions"
        );
        assert_eq!(groups[2].needs_you, 1);
        assert_eq!(ordered.hosts[0].note, None);
    }

    #[test]
    fn an_unnamed_tab_takes_the_automatic_name_and_rows_carry_their_container() {
        let own = layout(
            None,
            vec![
                tab(
                    "t1",
                    "Tab 2",
                    grid(split(
                        pane("p1", Some("a1"), None),
                        pane("p2", Some("w1"), None),
                    )),
                ),
                tab("t2", "", grid(pane("p3", Some("d1"), None))),
            ],
        );
        let mut source = manual();
        source.hosts[0].groups[2].rows[0].tab_group = Some("delta".to_owned());
        let ordered = apply(source, PhoneOrder::DesktopTabs, false, &layouts_with(own));
        assert_eq!(
            shape(&ordered),
            names(&[
                ("api, web", &["a1", "w1"]),
                ("delta", &["d1"]),
                ("Not in a tab", &["w2", "a2"]),
            ]),
            "a tab nobody named reads its panes' names as the desktop pill does, a Detached session by its repo"
        );
        let groups = &ordered.hosts[0].groups;
        assert!(groups[0].show_container, "a tab's rows name their repo");
        let containers: Vec<Option<&str>> = groups[0]
            .rows
            .iter()
            .map(|row| row.container.as_deref())
            .collect();
        assert_eq!(containers, [Some("api"), Some("web")]);
        assert_eq!(
            groups[1].rows[0].container, None,
            "a Detached session names no container"
        );
        assert_eq!(
            groups[1].rows[0].tab_group.as_deref(),
            Some("delta"),
            "while the tab names its repo"
        );
    }

    #[test]
    fn an_unnamed_tab_names_itself_as_the_desktop_pill_does() {
        let model = sidebar(
            "box.tail1234.ts.net",
            vec![repo_entry("r1", "C:/r1")],
            vec![
                shell_session("sh"),
                dir_session("dir", "E:/elsewhere"),
                in_repo_session("gone", "unregistered"),
            ],
        );
        let host = key("box.tail1234.ts.net");
        let named = Some("box.tail1234.ts.net");
        let own = tab_entry(
            "t1",
            "",
            grid(split(
                pane("p1", Some("sh"), named),
                split(
                    pane("p2", Some("dir"), named),
                    pane("p3", Some("gone"), named),
                ),
            )),
        );
        let home = crate::phone::home::from_sidebar(&model);
        let layout = HostLayout {
            client_id: "c1".to_owned(),
            name: None,
            tabs: vec![own.clone()],
        };
        let ordered = apply(
            home,
            PhoneOrder::DesktopTabs,
            false,
            &layouts_for(&host, layout),
        );
        assert_eq!(
            ordered.hosts[0].groups[0].name,
            model.tab_display_name(&own),
            "the phone's heading is the desktop's pill"
        );
        assert_eq!(
            ordered.hosts[0].groups[0].name, "unregistered",
            "a standalone shell and an unregistered folder name nothing; a Detached repo names its repo"
        );
    }

    #[test]
    fn an_unnamed_tab_names_a_pane_from_another_known_host() {
        let den = HostKey::Remote("den".to_owned());
        let mut lab = container("lab", false, vec![quiet("d9", "delta")]);
        lab.rows[0].host = den.clone();
        let mut source = manual();
        source.hosts.push(HomeHost {
            host: den,
            label: "den".to_owned(),
            note: None,
            groups: vec![lab],
        });
        let own = layout(
            None,
            vec![tab(
                "t1",
                "",
                grid(split(
                    pane("p1", Some("a1"), None),
                    pane("p2", Some("d9"), Some("den")),
                )),
            )],
        );
        let ordered = apply(source, PhoneOrder::DesktopTabs, false, &layouts_with(own));
        assert_eq!(
            shape(&ordered),
            names(&[
                ("api, lab", &["a1"]),
                ("Not in a tab", &["w2", "w1", "a2", "d1"]),
            ]),
            "a pane of another host the phone lists names that host's repo"
        );
    }

    #[test]
    fn a_named_tab_keeps_its_name() {
        let own = layout(
            None,
            vec![
                tab("t1", "Build", grid(pane("p1", Some("a1"), None))),
                tab("t2", "Tab 2 of 3", grid(pane("p2", Some("w1"), None))),
            ],
        );
        let ordered = apply(manual(), PhoneOrder::DesktopTabs, false, &layouts_with(own));
        assert_eq!(
            shape(&ordered),
            names(&[
                ("Build", &["a1"]),
                ("Tab 2 of 3", &["w1"]),
                ("Not in a tab", &["w2", "a2", "d1"]),
            ]),
            "a real name stays, even one that only reads like a default"
        );
    }

    #[test]
    fn desktop_tabs_groups_show_containers_and_the_needs_you_gather_does_not() {
        let own = layout(
            None,
            vec![tab(
                "t1",
                "Build",
                grid(split(
                    pane("p1", Some("a2"), None),
                    pane("p2", Some("w2"), None),
                )),
            )],
        );
        let ordered = apply(manual(), PhoneOrder::DesktopTabs, true, &layouts_with(own));
        assert_eq!(
            shape(&ordered),
            names(&[
                ("Needs you", &["w1", "d1"]),
                ("Build", &["a2", "w2"]),
                ("Not in a tab", &["a1"]),
            ])
        );
        let groups = &ordered.hosts[0].groups;
        assert!(
            !groups[0].show_container,
            "the gather's heading names nothing"
        );
        assert!(groups[1].show_container, "a tab's heading is the tab's");
        assert!(groups[2].show_container, "so is Not in a tab's");
        assert_eq!(
            groups[0].rows[0].container.as_deref(),
            Some("web"),
            "a gathered row still names the repo it left"
        );
        let model = sidebar(
            "box.tail1234.ts.net",
            vec![repo_entry("r1", "C:/r1")],
            vec![in_repo_session("a1", "r1")],
        );
        let manual_home = crate::phone::home::from_sidebar(&model);
        assert!(
            !manual_home.hosts[0].groups[0].show_container,
            "Manual's heading already names the repo"
        );
    }

    #[test]
    fn a_session_shown_twice_is_listed_once_at_its_first_pane() {
        let own = layout(
            None,
            vec![
                tab(
                    "t1",
                    "One",
                    grid(split(
                        pane("p1", Some("a1"), None),
                        pane("p2", Some("a1"), None),
                    )),
                ),
                tab(
                    "t2",
                    "Two",
                    grid(split(
                        pane("p3", Some("a1"), None),
                        pane("p4", Some("w1"), None),
                    )),
                ),
            ],
        );
        let ordered = apply(manual(), PhoneOrder::DesktopTabs, false, &layouts_with(own));
        assert_eq!(
            shape(&ordered),
            names(&[
                ("One", &["a1"]),
                ("Two", &["w1"]),
                ("Not in a tab", &["w2", "a2", "d1"]),
            ])
        );
    }

    #[test]
    fn a_non_grid_tab_and_a_tab_with_no_session_of_the_host_are_left_out() {
        let own = layout(
            None,
            vec![
                tab("t1", "Dash", json!({ "kind": "dashboard" })),
                tab("t2", "Empty", grid(pane("p1", None, None))),
                tab("t3", "Gone", grid(pane("p2", Some("nope"), None))),
                tab("t4", "Work", grid(pane("p3", Some("w1"), None))),
            ],
        );
        let ordered = apply(manual(), PhoneOrder::DesktopTabs, false, &layouts_with(own));
        assert_eq!(
            shape(&ordered),
            names(&[
                ("Work", &["w1"]),
                ("Not in a tab", &["w2", "a1", "a2", "d1"]),
            ])
        );
    }

    #[test]
    fn a_pane_of_another_host_is_skipped() {
        let own = layout(
            None,
            vec![tab(
                "t1",
                "Mixed",
                grid(split(
                    pane("p1", Some("a1"), Some("den")),
                    pane("p2", Some("a2"), None),
                )),
            )],
        );
        let ordered = apply(manual(), PhoneOrder::DesktopTabs, false, &layouts_with(own));
        assert_eq!(
            shape(&ordered),
            names(&[
                ("Mixed", &["a2"]),
                ("Not in a tab", &["w2", "w1", "a1", "d1"]),
            ]),
            "the other host's a1 is not this host's"
        );
    }

    #[test]
    fn with_every_session_in_a_tab_there_is_no_not_in_a_tab_group() {
        let calm = home(vec![container("web", false, vec![quiet("w1", "a")])]);
        let own = layout(
            None,
            vec![tab("t1", "Work", grid(pane("p1", Some("w1"), None)))],
        );
        let ordered = apply(calm, PhoneOrder::DesktopTabs, false, &layouts_with(own));
        assert_eq!(shape(&ordered), names(&[("Work", &["w1"])]));
    }

    #[test]
    fn no_layout_keeps_the_manual_order_with_the_note() {
        let ordered = apply(
            manual(),
            PhoneOrder::DesktopTabs,
            false,
            &PhoneLayouts::default(),
        );
        assert_eq!(shape(&ordered), shape(&manual()));
        assert_eq!(ordered.hosts[0].note, Some(NO_LAYOUT_NOTE));
        assert_eq!(NO_LAYOUT_NOTE, "No desktop layout on this host");
    }

    #[test]
    fn the_own_layout_is_named_after_the_host_else_the_only_one_else_none() {
        let desk = layout(Some("desk"), Vec::new());
        let laptop = layout(Some("laptop"), Vec::new());
        let unnamed = layout(None, Vec::new());
        let both = [laptop.clone(), desk.clone()];

        assert_eq!(
            own_layout(Some("desk"), &both),
            Some(&desk),
            "named after the host"
        );
        assert_eq!(
            own_layout(Some("box"), &both),
            None,
            "several, none named so"
        );
        assert_eq!(own_layout(None, &both), None, "no host name, several");
        assert_eq!(
            own_layout(Some("box"), std::slice::from_ref(&laptop)),
            Some(&laptop),
            "the only one"
        );
        assert_eq!(
            own_layout(None, std::slice::from_ref(&unnamed)),
            Some(&unnamed)
        );
        assert_eq!(own_layout(Some("box"), &[]), None, "none");
    }

    #[test]
    fn only_the_reply_to_the_request_out_is_kept_until_the_host_closes() {
        let own = layout(
            Some("box"),
            vec![tab("t1", "Work", grid(pane("p1", Some("w1"), None)))],
        );
        let mut layouts = PhoneLayouts::default();
        assert!(
            !layouts.on_reply(&host(), Some("box"), std::slice::from_ref(&own), Some("q1")),
            "nothing asked"
        );
        layouts.asked(&host(), "q2".to_owned());
        assert!(!layouts.on_reply(&host(), Some("box"), std::slice::from_ref(&own), Some("q1")));
        assert!(!layouts.on_reply(&host(), Some("box"), std::slice::from_ref(&own), None));
        assert_eq!(layouts.own(&host()), None);
        assert!(layouts.on_reply(&host(), Some("box"), std::slice::from_ref(&own), Some("q2")));
        assert_eq!(layouts.own(&host()), Some(&own));

        assert!(layouts.forget(&host()));
        assert_eq!(layouts.own(&host()), None, "a closed host's layout goes");
        assert!(!layouts.forget(&host()), "nothing left");
    }
}
