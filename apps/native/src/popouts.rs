//! The pop-out windows' rules: what each window holds, which further pop-outs
//! would fight it for the same content, what to close when its content goes,
//! and what its title reads. Plain data with unit tests; `popout_view.rs`
//! draws it.

use protocol::{AgentDescriptor, SessionSnapshot, TabEntry};

use crate::sidebar::display_label;
use crate::tabs::{Pane, TabsModel, collect_panes};

/// What one pop-out window holds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PopoutKey {
    /// One pane of a tab, named by its tab and its pane.
    Pane { tab_id: String, pane_id: String },
    /// A whole tab.
    Tab { tab_id: String },
    /// A session that has no pane of its own.
    Session { session_id: String },
}

/// A pop-out window's size in logical pixels.
pub(crate) const POPOUT_SIZE: (u16, u16) = (1100, 720);

/// How small a pop-out window may be dragged.
pub(crate) const POPOUT_MIN_SIZE: (u16, u16) = (700, 400);

/// How far right and down each further pop-out opens from the one before it.
pub(crate) const POPOUT_CASCADE: u16 = 32;

/// Why Pop out is dimmed for a session no pane shows.
pub(crate) const POP_OUT_NO_PANE_TIP: &str = "Show it in a pane to pop it out";

/// Why Pop out is dimmed for a session an open pop-out already shows.
pub(crate) const POP_OUT_ALREADY_TIP: &str = "Already in a pop-out";

/// What [`Popouts::open`] did with the pop-out it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenOutcome {
    /// The pop-out is new, and is now open.
    Opened,
    /// This pop-out already has a window: focus it.
    Existing(PopoutKey),
    /// An open pop-out already shows content this one would share: focus it.
    Overlaps(PopoutKey),
}

/// The open pop-outs, in the order they were opened.
#[derive(Debug, Clone, Default)]
pub(crate) struct Popouts {
    keys: Vec<PopoutKey>,
}

impl Popouts {
    /// Opens `key`, or reports the open pop-out that already shows what it
    /// would: the same key first, then one whose content overlaps. A pane
    /// key whose pane is not in its tab opens nothing and reports itself as
    /// already open, as a pop-out with nothing to show.
    pub(crate) fn open(&mut self, key: PopoutKey, tabs: &[TabEntry]) -> OpenOutcome {
        if self.is_open(&key) {
            return OpenOutcome::Existing(key);
        }
        if let PopoutKey::Pane { tab_id, pane_id } = &key
            && !pane_exists(tab_id, pane_id, tabs)
        {
            return OpenOutcome::Existing(key);
        }
        if let Some(open) = self.overlapping(&key, tabs) {
            return OpenOutcome::Overlaps(open);
        }
        self.keys.push(key);
        OpenOutcome::Opened
    }

    /// Closes `key`; whether it was open.
    pub(crate) fn close(&mut self, key: &PopoutKey) -> bool {
        let Some(index) = self.keys.iter().position(|open| open == key) else {
            return false;
        };
        self.keys.remove(index);
        true
    }

    /// The open pop-outs, in the order they were opened.
    pub(crate) fn keys(&self) -> &[PopoutKey] {
        &self.keys
    }

    /// Whether `key` has a window.
    pub(crate) fn is_open(&self, key: &PopoutKey) -> bool {
        self.keys.contains(key)
    }

    /// `key`'s place among the open pop-outs, which sets its cascade offset.
    pub(crate) fn cascade_index(&self, key: &PopoutKey) -> Option<usize> {
        self.keys.iter().position(|open| open == key)
    }

    /// The pane pop-out showing pane `pane_id`, whichever tab its key names.
    pub(crate) fn popped_key(&self, pane_id: &str) -> Option<&PopoutKey> {
        self.keys
            .iter()
            .find(|key| matches!(key, PopoutKey::Pane { pane_id: id, .. } if id == pane_id))
    }

    /// The pane pop-out showing session `session_id`, if any.
    pub(crate) fn showing_session(
        &self,
        session_id: &str,
        tabs: &[TabEntry],
    ) -> Option<&PopoutKey> {
        self.keys.iter().find(|key| {
            matches!(key, PopoutKey::Pane { .. }) && sessions_of(key, tabs).contains(&session_id)
        })
    }

    /// The panes open in pane pop-outs, by pane id.
    pub(crate) fn popped_panes(&self) -> Vec<&str> {
        self.keys
            .iter()
            .filter_map(|key| match key {
                PopoutKey::Pane { pane_id, .. } => Some(pane_id.as_str()),
                PopoutKey::Tab { .. } | PopoutKey::Session { .. } => None,
            })
            .collect()
    }

    /// Re-keys each pane pop-out whose pane has left its key's tab for
    /// another tab, since the daemon keeps a moved pane's id; returns each
    /// move as `(old key, new key)`. A pane gone from every tab keeps its
    /// key, for [`Popouts::prune`] to close. Nothing moves before the first
    /// tab list arrives.
    pub(crate) fn follow_moves(&mut self, tabs: &TabsModel) -> Vec<(PopoutKey, PopoutKey)> {
        if !tabs.is_loaded() {
            return Vec::new();
        }
        let mut moved = Vec::new();
        for key in &mut self.keys {
            let Some(new_tab) = moved_to(key, tabs.tabs()) else {
                continue;
            };
            let old = key.clone();
            if let PopoutKey::Pane { tab_id, .. } = key {
                *tab_id = new_tab;
            }
            moved.push((old, key.clone()));
        }
        moved
    }

    /// Closes and returns the pop-outs whose content has gone, in the order
    /// they were opened. Nothing closes before the first tab list arrives.
    pub(crate) fn prune(&mut self, tabs: &TabsModel) -> Vec<PopoutKey> {
        if !tabs.is_loaded() {
            return Vec::new();
        }
        let gone: Vec<PopoutKey> = self
            .keys
            .iter()
            .filter(|key| content_gone(key, tabs.tabs()))
            .cloned()
            .collect();
        for key in &gone {
            self.close(key);
        }
        gone
    }

    /// The open pop-out that already shows what `key` would, as
    /// [`Popouts::open`] would report it, without opening anything.
    pub(crate) fn blocking(&self, key: &PopoutKey, tabs: &[TabEntry]) -> Option<PopoutKey> {
        if self.is_open(key) {
            return Some(key.clone());
        }
        self.overlapping(key, tabs)
    }

    /// The first open pop-out `key` would share content with, if any.
    fn overlapping(&self, key: &PopoutKey, tabs: &[TabEntry]) -> Option<PopoutKey> {
        let wanted = sessions_of(key, tabs);
        self.keys
            .iter()
            .find(|open| {
                same_tab(open, key)
                    || sessions_of(open, tabs)
                        .iter()
                        .any(|session| wanted.contains(session))
            })
            .cloned()
    }
}

/// Whether the session `key` names is still in `sessions`; true for a pane or
/// a tab, whose own content decides.
#[must_use]
pub(crate) fn session_found(key: &PopoutKey, sessions: &[SessionSnapshot]) -> bool {
    match key {
        PopoutKey::Session { session_id } => sessions.iter().any(|s| s.id == session_id.as_str()),
        PopoutKey::Pane { .. } | PopoutKey::Tab { .. } => true,
    }
}

/// A pop-out window's title: the session's display label, the tab's name, or
/// the not-found text once the daemon has dropped the session.
#[must_use]
pub(crate) fn title(
    key: &PopoutKey,
    tabs: &[TabEntry],
    sessions: &[SessionSnapshot],
    agents: &[AgentDescriptor],
) -> String {
    match key {
        PopoutKey::Session { session_id } => sessions
            .iter()
            .find(|s| s.id == session_id.as_str())
            .map_or_else(
                || "Session not found".to_owned(),
                |s| display_label(s, agents),
            ),
        PopoutKey::Pane { tab_id, pane_id } => bound_session(tab_id, pane_id, tabs)
            .and_then(|id| sessions.iter().find(|s| s.id == id))
            .map_or_else(|| "Empty pane".to_owned(), |s| display_label(s, agents)),
        PopoutKey::Tab { tab_id } => tabs
            .iter()
            .find(|t| t.id == tab_id.as_str())
            .map_or_else(|| "Tab not found".to_owned(), |t| t.name.clone()),
    }
}

/// The pane Pop out pops for `session_id`, as `(tab id, pane id)`: the
/// pane showing it in the active tab, else the first pane showing it in tab
/// order; `None` when no pane shows it.
#[must_use]
pub(crate) fn pane_to_pop(
    session_id: &str,
    tabs: &[TabEntry],
    active_tab_id: Option<&str>,
) -> Option<(String, String)> {
    let shown_in = |tab: &TabEntry| {
        let pane = collect_panes(tab.grid()?)
            .into_iter()
            .find(|pane| pane.session == Some(session_id))?;
        Some((tab.id.clone(), pane.id.to_owned()))
    };
    active_tab_id
        .and_then(|active| tabs.iter().find(|tab| tab.id == active))
        .and_then(shown_in)
        .or_else(|| tabs.iter().find_map(shown_in))
}

/// Whether the content `key` shows has gone from `tabs`. A pane pop-out goes
/// only with its pane: a session removed before the tab update that
/// rebinds the pane leaves it open. A session pop-out never goes: it
/// outlives its session's stop, and shows "not found" once the daemon drops
/// it.
fn content_gone(key: &PopoutKey, tabs: &[TabEntry]) -> bool {
    match key {
        PopoutKey::Pane { pane_id, .. } => pane_in_tabs(pane_id, tabs).is_none(),
        PopoutKey::Tab { tab_id } => !tabs.iter().any(|t| t.id == tab_id.as_str()),
        PopoutKey::Session { .. } => false,
    }
}

/// The tab a pane pop-out's pane now lives in, when it has left its key's
/// tab for another one; `None` while it is still there or is in no tab.
fn moved_to(key: &PopoutKey, tabs: &[TabEntry]) -> Option<String> {
    let PopoutKey::Pane { tab_id, pane_id } = key else {
        return None;
    };
    if pane_exists(tab_id, pane_id, tabs) {
        return None;
    }
    tabs.iter()
        .find(|tab| {
            tab.grid()
                .is_some_and(|grid| collect_panes(grid).iter().any(|pane| pane.id == pane_id))
        })
        .map(|tab| tab.id.clone())
}

/// Pane `pane_id` in whichever tab holds it: the first in tab order while a
/// move has it in two tabs for one fold.
pub(crate) fn pane_in_tabs<'a>(pane_id: &str, tabs: &'a [TabEntry]) -> Option<Pane<'a>> {
    tabs.iter()
        .filter_map(TabEntry::grid)
        .flat_map(collect_panes)
        .find(|pane| pane.id == pane_id)
}

/// Whether `tab_id`'s grid still holds `pane_id`, bound or empty.
fn pane_exists(tab_id: &str, pane_id: &str, tabs: &[TabEntry]) -> bool {
    tabs.iter()
        .find(|t| t.id == tab_id)
        .and_then(TabEntry::grid)
        .is_some_and(|grid| collect_panes(grid).iter().any(|pane| pane.id == pane_id))
}

/// The session bound to `pane_id` in `tab_id`'s grid: none when the tab or the
/// pane has gone, or the pane is empty.
fn bound_session<'a>(tab_id: &str, pane_id: &str, tabs: &'a [TabEntry]) -> Option<&'a str> {
    collect_panes(tabs.iter().find(|t| t.id == tab_id)?.grid()?)
        .into_iter()
        .find(|pane| pane.id == pane_id)?
        .session
}

/// The sessions `key` shows: its own, a pane's one, a tab's every bound one.
fn sessions_of<'a>(key: &'a PopoutKey, tabs: &'a [TabEntry]) -> Vec<&'a str> {
    match key {
        PopoutKey::Pane { tab_id, pane_id } => {
            bound_session(tab_id, pane_id, tabs).into_iter().collect()
        }
        PopoutKey::Tab { tab_id } => tabs
            .iter()
            .find(|t| t.id == tab_id.as_str())
            .and_then(TabEntry::grid)
            .map(|grid| {
                collect_panes(grid)
                    .into_iter()
                    .filter_map(|pane| pane.session)
                    .collect()
            })
            .unwrap_or_default(),
        PopoutKey::Session { session_id } => vec![session_id.as_str()],
    }
}

/// Whether one key names a tab and the other a pane of it, in either order.
fn same_tab(one: &PopoutKey, other: &PopoutKey) -> bool {
    match (one, other) {
        (PopoutKey::Tab { tab_id: a }, PopoutKey::Pane { tab_id: b, .. })
        | (PopoutKey::Pane { tab_id: a, .. }, PopoutKey::Tab { tab_id: b }) => a == b,
        _ => false,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert their fixtures with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::tabs::tests::{model_with, pane, split, tab};
    use protocol::SplitDirection;
    use serde_json::json;

    fn session(id: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [],
            "status": "idle",
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
        }))
        .expect("session fixture")
    }

    fn session_key(id: &str) -> PopoutKey {
        PopoutKey::Session {
            session_id: id.to_owned(),
        }
    }

    fn tab_key(id: &str) -> PopoutKey {
        PopoutKey::Tab {
            tab_id: id.to_owned(),
        }
    }

    fn pane_key(tab_id: &str, pane_id: &str) -> PopoutKey {
        PopoutKey::Pane {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
        }
    }

    /// A tab holding two panes, `p1` on session `s1` and `p2` on `s2`.
    fn two_panes() -> Vec<TabEntry> {
        vec![tab(
            "t1",
            &split(
                SplitDirection::Horizontal,
                0.5,
                pane("p1", Some("s1")),
                pane("p2", Some("s2")),
            ),
        )]
    }

    #[test]
    fn opening_twice_focuses_the_first() {
        let tabs = vec![tab("t1", &pane("p1", Some("s1")))];
        let mut popouts = Popouts::default();
        let key = session_key("s1");

        assert_eq!(popouts.open(key.clone(), &tabs), OpenOutcome::Opened);
        assert_eq!(
            popouts.open(key.clone(), &tabs),
            OpenOutcome::Existing(key.clone()),
            "the second open focuses the first window"
        );
        assert_eq!(popouts.keys(), [key].as_slice(), "one window, not two");
    }

    #[test]
    fn opening_a_pane_of_a_popped_tab_overlaps() {
        let tabs = vec![tab("t1", &pane("p1", Some("s1")))];
        let mut popouts = Popouts::default();
        popouts.open(tab_key("t1"), &tabs);

        assert_eq!(
            popouts.open(pane_key("t1", "p1"), &tabs),
            OpenOutcome::Overlaps(tab_key("t1"))
        );
        assert_eq!(popouts.keys().len(), 1, "nothing was added");
    }

    #[test]
    fn opening_a_tab_holding_a_popped_pane_overlaps() {
        let tabs = vec![tab("t1", &pane("p1", Some("s1")))];
        let mut popouts = Popouts::default();
        popouts.open(pane_key("t1", "p1"), &tabs);

        assert_eq!(
            popouts.open(tab_key("t1"), &tabs),
            OpenOutcome::Overlaps(pane_key("t1", "p1"))
        );
        assert_eq!(popouts.keys().len(), 1, "nothing was added");
    }

    #[test]
    fn a_session_popped_as_a_pane_overlaps_its_session_popout() {
        let tabs = vec![tab("t1", &pane("p1", Some("s1")))];

        let mut session_first = Popouts::default();
        session_first.open(session_key("s1"), &tabs);
        assert_eq!(
            session_first.open(pane_key("t1", "p1"), &tabs),
            OpenOutcome::Overlaps(session_key("s1"))
        );

        let mut pane_first = Popouts::default();
        pane_first.open(pane_key("t1", "p1"), &tabs);
        assert_eq!(
            pane_first.open(session_key("s1"), &tabs),
            OpenOutcome::Overlaps(pane_key("t1", "p1"))
        );
    }

    #[test]
    fn empty_panes_never_overlap_by_session() {
        let tabs = vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", None)),
        ];
        let mut popouts = Popouts::default();
        popouts.open(session_key("s1"), &tabs);

        assert_eq!(
            popouts.open(pane_key("t2", "p2"), &tabs),
            OpenOutcome::Opened,
            "an empty pane shows no session, so it shares none"
        );
        assert_eq!(popouts.keys().len(), 2);
    }

    #[test]
    fn pane_popout_closes_when_its_pane_goes() {
        let mut popouts = Popouts::default();
        popouts.open(pane_key("t1", "p1"), &two_panes());

        let closed = popouts.prune(&model_with(&[tab("t1", &pane("p2", Some("s2")))]));

        assert_eq!(closed, vec![pane_key("t1", "p1")]);
        assert!(!popouts.is_open(&pane_key("t1", "p1")));
    }

    #[test]
    fn pane_popout_closes_when_its_tab_goes() {
        let mut popouts = Popouts::default();
        popouts.open(pane_key("t1", "p1"), &two_panes());

        let closed = popouts.prune(&model_with(&[tab("t2", &pane("p2", Some("s2")))]));

        assert_eq!(closed, vec![pane_key("t1", "p1")]);
        assert_eq!(popouts.keys(), [].as_slice(), "the registry is empty again");
    }

    #[test]
    fn pane_popout_outlives_its_session_while_the_pane_stays() {
        let mut popouts = Popouts::default();
        popouts.open(pane_key("t1", "p1"), &two_panes());

        // The session's removal can come before the tab update that
        // rebinds its pane to a resumed or restarted session.
        let closed = popouts.prune(&model_with(&two_panes()));

        assert!(closed.is_empty(), "only a pane in no tab closes");
        assert!(popouts.is_open(&pane_key("t1", "p1")));
    }

    #[test]
    fn follow_moves_rekeys_a_pane_moved_to_another_tab() {
        let mut popouts = Popouts::default();
        popouts.open(pane_key("t1", "p1"), &two_panes());
        let moved = model_with(&[
            tab("t1", &pane("p2", Some("s2"))),
            tab("t2", &pane("p1", Some("s1"))),
        ]);

        assert_eq!(
            popouts.follow_moves(&moved),
            vec![(pane_key("t1", "p1"), pane_key("t2", "p1"))]
        );
        assert_eq!(popouts.keys(), [pane_key("t2", "p1")].as_slice());
        assert!(
            popouts.prune(&moved).is_empty(),
            "the re-keyed pop-out still has its content"
        );
        assert!(
            popouts.follow_moves(&moved).is_empty(),
            "a pane already under its tab does not move again"
        );
    }

    #[test]
    fn follow_moves_leaves_a_gone_pane_for_prune() {
        let mut popouts = Popouts::default();
        popouts.open(pane_key("t1", "p1"), &two_panes());
        let gone = model_with(&[tab("t1", &pane("p2", Some("s2")))]);

        assert_eq!(popouts.follow_moves(&gone), Vec::new());
        assert_eq!(
            popouts.keys(),
            [pane_key("t1", "p1")].as_slice(),
            "a pane in no tab keeps its key"
        );
        assert_eq!(popouts.prune(&gone), vec![pane_key("t1", "p1")]);
        assert!(
            Popouts::default()
                .follow_moves(&TabsModel::new(None))
                .is_empty(),
            "nothing moves before the first tab list"
        );
    }

    #[test]
    fn prune_keeps_a_pane_still_in_some_tab() {
        let mut popouts = Popouts::default();
        popouts.open(pane_key("t1", "p1"), &two_panes());
        let moved = model_with(&[
            tab("t1", &pane("p2", Some("s2"))),
            tab("t2", &pane("p1", Some("s1"))),
        ]);

        let closed = popouts.prune(&moved);

        assert!(
            closed.is_empty(),
            "a pane in another tab is still open, re-keyed or not"
        );
        assert!(popouts.is_open(&pane_key("t1", "p1")));
    }

    #[test]
    fn open_refuses_a_pane_not_in_its_tab() {
        let tabs = two_panes();
        let mut popouts = Popouts::default();

        for stale in [pane_key("t2", "p1"), pane_key("t1", "p9")] {
            assert_ne!(
                popouts.open(stale.clone(), &tabs),
                OpenOutcome::Opened,
                "{stale:?} names no pane of its tab"
            );
        }
        assert!(popouts.keys().is_empty(), "nothing opened");
        assert_eq!(
            popouts.open(pane_key("t1", "p1"), &tabs),
            OpenOutcome::Opened
        );
    }

    #[test]
    fn empty_pane_popout_stays_while_the_pane_exists() {
        let tabs = vec![tab("t2", &pane("p2", None))];
        let mut popouts = Popouts::default();
        popouts.open(pane_key("t2", "p2"), &tabs);

        let closed = popouts.prune(&model_with(&tabs));

        assert!(closed.is_empty(), "an empty pane that still exists stays");
        assert!(popouts.is_open(&pane_key("t2", "p2")));
    }

    #[test]
    fn tab_popout_closes_with_its_tab() {
        let tabs = vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s2"))),
        ];
        let mut popouts = Popouts::default();
        popouts.open(tab_key("t1"), &tabs);

        let closed = popouts.prune(&model_with(&[tab("t2", &pane("p2", Some("s2")))]));

        assert_eq!(closed, vec![tab_key("t1")]);
        assert_eq!(popouts.keys(), [].as_slice());
    }

    #[test]
    fn session_popout_outlives_a_stop() {
        let tabs = vec![tab("t1", &pane("p1", Some("s1")))];
        let mut popouts = Popouts::default();
        popouts.open(session_key("s1"), &tabs);
        let mut stopped = session("s1");
        stopped.status = protocol::SessionStatus::Stopped;

        let closed = popouts.prune(&model_with(&tabs));

        assert!(closed.is_empty(), "a stopped session is still listed");
        assert!(popouts.is_open(&session_key("s1")));
        assert!(session_found(&session_key("s1"), &[stopped]));
    }

    #[test]
    fn session_popout_not_found_once_removed() {
        let tabs = vec![tab("t1", &pane("p1", Some("s1")))];
        let mut popouts = Popouts::default();
        popouts.open(session_key("s1"), &tabs);

        let closed = popouts.prune(&model_with(&tabs));

        assert!(closed.is_empty(), "a session pop-out is never pruned");
        assert!(popouts.is_open(&session_key("s1")), "the window stays open");
        assert!(!session_found(&session_key("s1"), &[]));
        assert_eq!(
            title(&session_key("s1"), &tabs, &[], &[]),
            "Session not found"
        );
    }

    #[test]
    fn prune_closes_nothing_before_tabs_load() {
        let tabs = vec![tab("t1", &pane("p1", Some("s1")))];
        let mut popouts = Popouts::default();
        popouts.open(pane_key("t1", "p1"), &tabs);

        let closed = popouts.prune(&TabsModel::new(None));

        assert!(
            closed.is_empty(),
            "an empty list before the first Tabs is no news"
        );
        assert!(popouts.is_open(&pane_key("t1", "p1")));
    }

    #[test]
    fn cascade_index_follows_open_order() {
        let tabs = vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab("t2", &pane("p2", Some("s2"))),
        ];
        let mut popouts = Popouts::default();
        for key in [pane_key("t1", "p1"), tab_key("t2"), session_key("s9")] {
            assert_eq!(popouts.open(key, &tabs), OpenOutcome::Opened);
        }

        assert_eq!(popouts.cascade_index(&pane_key("t1", "p1")), Some(0));
        assert_eq!(popouts.cascade_index(&tab_key("t2")), Some(1));
        assert_eq!(popouts.cascade_index(&session_key("s9")), Some(2));
        assert_eq!(popouts.cascade_index(&session_key("gone")), None);
    }

    fn ids(tab_id: &str, pane_id: &str) -> (String, String) {
        (tab_id.to_owned(), pane_id.to_owned())
    }

    /// `s1` shown in `t1`'s `p1` and in `t2`'s `p2`, beside `s2` in `p3`.
    fn shown_twice() -> Vec<TabEntry> {
        vec![
            tab("t1", &pane("p1", Some("s1"))),
            tab(
                "t2",
                &split(
                    SplitDirection::Horizontal,
                    0.5,
                    pane("p3", Some("s2")),
                    pane("p2", Some("s1")),
                ),
            ),
        ]
    }

    #[test]
    fn pane_to_pop_prefers_the_active_tab() {
        assert_eq!(
            pane_to_pop("s1", &shown_twice(), Some("t2")),
            Some(ids("t2", "p2")),
            "the pane in the tab on screen"
        );
        assert_eq!(
            pane_to_pop("s2", &shown_twice(), Some("t2")),
            Some(ids("t2", "p3"))
        );
    }

    #[test]
    fn pane_to_pop_falls_back_to_the_first_pane() {
        let mut tabs = shown_twice();
        tabs.push(tab("t3", &pane("p4", None)));
        assert_eq!(
            pane_to_pop("s1", &tabs, Some("t3")),
            Some(ids("t1", "p1")),
            "the active tab does not show it: the first pane in tab order"
        );
        assert_eq!(
            pane_to_pop("s1", &tabs, None),
            Some(ids("t1", "p1")),
            "no active tab"
        );
        assert_eq!(pane_to_pop("s2", &tabs, Some("t1")), Some(ids("t2", "p3")));
    }

    #[test]
    fn pane_to_pop_is_none_without_a_pane() {
        let tabs = vec![tab("t1", &pane("p1", None))];
        assert_eq!(pane_to_pop("s1", &tabs, Some("t1")), None);
        assert_eq!(pane_to_pop("s1", &[], None), None, "no tabs at all");
    }

    #[test]
    fn titles_name_the_session_and_the_tab() {
        let mut named = tab("t1", &pane("p1", Some("s1")));
        named.name = "Build".to_owned();
        let tabs = vec![named];
        let sessions = vec![session("s1")];

        assert_eq!(
            title(&tab_key("t1"), &tabs, &sessions, &[]),
            "Build",
            "a tab pop-out is titled by its tab"
        );
        assert_eq!(
            title(&pane_key("t1", "p1"), &tabs, &sessions, &[]),
            "s1",
            "a pane pop-out is titled by the session bound to it"
        );
        assert_eq!(
            title(&session_key("s1"), &tabs, &sessions, &[]),
            "s1",
            "a session pop-out is titled by its display label"
        );
    }
}
