//! Spawns this client asked for, matched to the daemon's reply by request id
//! and placed where the user chose to open them.

use std::collections::HashMap;

use protocol::{CheckoutStrategy, ClientMessage, SessionSnapshot, SpawnRequest, SpawnTarget};

use crate::tabs::{PaneTarget, Placement, TabsModel, collect_panes};

/// Where a spawned session opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenIn {
    /// The tab that was active when the spawn was asked for.
    CurrentTab(String),
    /// A tab the user picked.
    Tab(String),
    /// A tab of its own.
    NewTab,
    /// The pane the spawn was aimed at.
    Pane(PaneAim),
}

/// A pane a spawn is aimed at, as it was when the spawn was aimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneAim {
    pub tab_id: String,
    pub pane_id: String,
    /// What the pane showed: nothing for an empty pane, the stopped session
    /// under the overlay. The reply takes the pane only while it still
    /// shows exactly this.
    pub expected: Option<String>,
    /// A stopped session the new one takes the place of: every pane that
    /// shows it is rebound, then it is discarded, worktree kept.
    pub discard: Option<String>,
}

impl PaneAim {
    /// Where a spawn from a dialog aimed here opens: this pane for the
    /// current-tab choice, else the tab picked, without the discard.
    pub(crate) fn open_in(self, chosen: OpenIn) -> OpenIn {
        match chosen {
            OpenIn::CurrentTab(_) => OpenIn::Pane(self),
            other => other,
        }
    }
}

/// What placing a spawn's reply takes.
#[derive(Debug)]
pub(crate) struct SpawnPlaced {
    /// The requests that put the session in its pane or tab, in order: a
    /// discard of the session it replaces goes last.
    pub messages: Vec<ClientMessage>,
    /// The tab model changed (a tab activated, a pane focused), so the view
    /// must follow it.
    pub relayout: bool,
}

/// A spawn waiting for its reply.
#[derive(Debug)]
struct Pending {
    open_in: OpenIn,
    /// Start order, so the oldest of alike spawns answers an id-less prompt
    /// first.
    seq: u64,
    /// The request as last sent, kept for an in-place spawn only: the
    /// daemon may ask how to switch its dirty tree, and the answer resends it.
    in_place: Option<SpawnRequest>,
    /// Whether an id-less checkout prompt (an older daemon's) has claimed
    /// this spawn, so a second one for the same repo and branch resolves to
    /// another spawn.
    prompted: bool,
}

/// The spawns waiting for their reply, by request id.
#[derive(Debug, Default)]
pub(crate) struct PendingSpawns {
    pending: HashMap<String, Pending>,
    next_seq: u64,
}

impl PendingSpawns {
    /// Records `request` under `request_id` and returns the message to send.
    pub(crate) fn start(
        &mut self,
        mut request: SpawnRequest,
        request_id: String,
        open_in: OpenIn,
    ) -> ClientMessage {
        request.request_id = Some(request_id.clone());
        self.next_seq += 1;
        let pending = Pending {
            open_in,
            seq: self.next_seq,
            in_place: is_in_place(&request).then(|| request.clone()),
            prompted: false,
        };
        self.pending.insert(request_id, pending);
        ClientMessage::SpawnSession(request)
    }

    /// Whether `request_id` is one of this client's spawns.
    #[cfg(test)]
    pub(crate) fn has_request(&self, request_id: &str) -> bool {
        self.pending.contains_key(request_id)
    }

    /// Whether a spawn aimed at pane `pane_id` of `tab_id` waits for its
    /// reply.
    pub(crate) fn aims_at(&self, tab_id: &str, pane_id: &str) -> bool {
        self.pending.values().any(|pending| {
            matches!(&pending.open_in, OpenIn::Pane(aim) if aim.tab_id == tab_id && aim.pane_id == pane_id)
        })
    }

    /// When `request_id` names a pending spawn: where `session` goes. A new
    /// tab is armed so the tab's arrival activates it; a named tab is
    /// activated so the new pane takes its focus, and a filled empty pane is
    /// focused at once. An aimed pane that still shows what it showed takes
    /// the session and the focus, with every other pane showing the session
    /// it replaces, and that session is discarded last. Otherwise the
    /// session goes to the aimed tab (else the active one) and nothing is
    /// discarded.
    pub(crate) fn place(
        &mut self,
        request_id: &str,
        session: &SessionSnapshot,
        tabs: &mut TabsModel,
        sessions: &[SessionSnapshot],
    ) -> Option<SpawnPlaced> {
        let open_in = self.pending.remove(request_id)?.open_in;
        if let OpenIn::Pane(aim) = &open_in
            && pane_shows(tabs, aim)
        {
            return Some(into_pane(aim, session, tabs));
        }
        let placement = match &open_in {
            OpenIn::NewTab => Placement::NewTab,
            OpenIn::CurrentTab(tab_id) | OpenIn::Tab(tab_id) => {
                tabs.activate(tab_id);
                tabs.place_in(tab_id, session, sessions)
            }
            OpenIn::Pane(aim) if tabs.tab(&aim.tab_id).is_some() => {
                tabs.activate(&aim.tab_id);
                tabs.place_in(&aim.tab_id, session, sessions)
            }
            OpenIn::Pane(_) => tabs.place(session, sessions),
        };
        match &placement {
            Placement::NewTab => tabs.arm_create(),
            Placement::Pane {
                tab_id,
                target: PaneTarget::Replace { pane_id },
            } => tabs.focus_pane(tab_id, pane_id),
            Placement::Pane { .. } => {}
        }
        Some(SpawnPlaced {
            messages: vec![placement.message(&session.id)],
            relayout: open_in != OpenIn::NewTab,
        })
    }

    /// Spawn `request_id` ends without a session: a failure reply, or Cancel
    /// on its checkout prompt. Returns whether it was pending.
    pub(crate) fn fail(&mut self, request_id: &str) -> bool {
        self.pending.remove(request_id).is_some()
    }

    /// Forgets every spawn, as a new connection must.
    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }

    /// The pending spawn a `CheckoutConfirmRequired` for `branch` of
    /// `repo_id` declines. A prompt carrying `request_id` names its spawn;
    /// None when that spawn is no longer pending. A prompt without one comes
    /// from an older daemon and falls back to [`Self::claim_checkout`].
    pub(crate) fn resolve_checkout(
        &mut self,
        request_id: Option<&str>,
        repo_id: &str,
        branch: &str,
    ) -> Option<String> {
        match request_id {
            Some(id) => self
                .pending
                .get(id)
                .and_then(|pending| pending.in_place.as_ref())
                .is_some_and(|request| awaits_checkout(request, repo_id, branch))
                .then(|| id.to_owned()),
            None => self.claim_checkout(repo_id, branch),
        }
    }

    /// Fallback for a prompt without a request id: the oldest pending
    /// in-place spawn of `branch` in `repo_id` that has no strategy yet and
    /// that no prompt has claimed. Claims it, so a second prompt for the same
    /// repo and branch resolves to another spawn.
    fn claim_checkout(&mut self, repo_id: &str, branch: &str) -> Option<String> {
        let id = self
            .pending
            .iter()
            .filter(|(_, pending)| {
                !pending.prompted
                    && pending
                        .in_place
                        .as_ref()
                        .is_some_and(|request| awaits_checkout(request, repo_id, branch))
            })
            .min_by_key(|(_, pending)| pending.seq)
            .map(|(id, _)| id.clone())?;
        if let Some(pending) = self.pending.get_mut(&id) {
            pending.prompted = true;
        }
        Some(id)
    }

    /// A waiting prompt's spawn `request_id` asked again, exactly as first
    /// sent. None when it cannot be: it is then forgotten, so nothing waits
    /// for it until a reconnect.
    pub(crate) fn reask(&mut self, request_id: &str) -> Option<ClientMessage> {
        let msg = self.resend(request_id, None);
        if msg.is_none() {
            self.fail(request_id);
        }
        msg
    }

    /// The in-place spawn `request_id` again, under the same request id,
    /// while it still waits for its reply: with `strategy` to answer its
    /// prompt, or without one, exactly as first sent, to ask for a freshly
    /// measured prompt (or the session itself).
    pub(crate) fn resend(
        &mut self,
        request_id: &str,
        strategy: Option<CheckoutStrategy>,
    ) -> Option<ClientMessage> {
        let pending = self.pending.get_mut(request_id)?;
        let request = pending.in_place.as_mut()?;
        let SpawnTarget::Single {
            checkout_strategy, ..
        } = &mut request.target
        else {
            return None;
        };
        *checkout_strategy = strategy;
        pending.prompted = false;
        Some(ClientMessage::SpawnSession(request.clone()))
    }
}

/// Whether the aimed pane is still there and shows exactly what it showed
/// when the spawn was aimed.
fn pane_shows(tabs: &TabsModel, aim: &PaneAim) -> bool {
    tabs.tab(&aim.tab_id)
        .and_then(protocol::TabEntry::grid)
        .is_some_and(|grid| {
            collect_panes(grid)
                .iter()
                .any(|pane| pane.id == aim.pane_id && pane.session == aim.expected.as_deref())
        })
}

/// `session` into the aimed pane, which takes the focus, and into every
/// other pane of any tab that shows the session it replaces; then the
/// discard of that session, sent last so the daemon has rebound the panes
/// before it closes the replaced session's, as
/// [`crate::session_actions::Duplicates::place`] orders it.
fn into_pane(aim: &PaneAim, session: &SessionSnapshot, tabs: &mut TabsModel) -> SpawnPlaced {
    let mut panes = vec![(aim.tab_id.clone(), aim.pane_id.clone())];
    if let Some(discard) = &aim.discard {
        panes.extend(
            tabs.bindings()
                .into_iter()
                .filter(|binding| binding.session_id.as_ref() == Some(discard))
                .filter(|binding| binding.tab_id != aim.tab_id || binding.pane_id != aim.pane_id)
                .map(|binding| (binding.tab_id, binding.pane_id)),
        );
    }
    tabs.focus_pane(&aim.tab_id, &aim.pane_id);
    let mut messages: Vec<ClientMessage> = panes
        .into_iter()
        .map(|(tab_id, pane_id)| {
            Placement::Pane {
                tab_id,
                target: PaneTarget::Replace { pane_id },
            }
            .message(&session.id)
        })
        .collect();
    messages.extend(
        aim.discard
            .as_ref()
            .map(|id| ClientMessage::DiscardSession {
                session_id: id.to_owned(),
                cleanup: Vec::new(),
            }),
    );
    SpawnPlaced {
        messages,
        relayout: true,
    }
}

/// Whether `request` is an in-place spawn of `branch` in `repo` that the
/// daemon may still decline over a dirty tree.
fn awaits_checkout(request: &SpawnRequest, repo: &str, branch: &str) -> bool {
    matches!(
        &request.target,
        SpawnTarget::Single {
            repo_id,
            branch_name,
            use_worktree: false,
            checkout_strategy: None,
            ..
        } if repo_id == repo && branch_name == branch
    )
}

/// A spawn that checks its branch out in the repo's own directory, which
/// the daemon may decline over a dirty tree.
fn is_in_place(request: &SpawnRequest) -> bool {
    matches!(
        request.target,
        SpawnTarget::Single {
            use_worktree: false,
            ..
        }
    )
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert preconditions with expect and panic; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::tabs::tests::{model_with, pane, session, tab, updated, wire};
    use protocol::{AgentOptions, SessionMode, WorktreeReusePolicy};

    fn request(use_worktree: bool) -> SpawnRequest {
        SpawnRequest {
            label: None,
            target: SpawnTarget::Single {
                repo_id: "r1".to_owned(),
                branch_name: "feature".to_owned(),
                base_branch: None,
                use_worktree,
                checkout_strategy: None,
                worktree_reuse: WorktreeReusePolicy::default(),
                existing_worktree: None,
            },
            mode: SessionMode::Interactive,
            initial_prompt: None,
            dangerously_skip_permissions: false,
            agent_options: AgentOptions::Claude {
                permission_mode: None,
            },
            model: None,
            extra_env: Vec::new(),
            prompt_injector: None,
            request_id: None,
        }
    }

    fn sent_request(msg: &ClientMessage) -> &SpawnRequest {
        match msg {
            ClientMessage::SpawnSession(request) => request,
            other => panic!("expected a SpawnSession, got {other:?}"),
        }
    }

    /// The one message a placement sends.
    fn only(placed: &SpawnPlaced) -> &ClientMessage {
        match placed.messages.as_slice() {
            [msg] => msg,
            other => panic!("expected one message, got {other:?}"),
        }
    }

    /// A spawn aimed at a pane showing `stopped` (None for an empty pane),
    /// which it replaces and discards, as the pane's buttons aim one.
    fn aim_at(tab_id: &str, pane_id: &str, stopped: Option<&str>) -> OpenIn {
        OpenIn::Pane(PaneAim {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            expected: stopped.map(str::to_owned),
            discard: stopped.map(str::to_owned),
        })
    }

    fn discard_msg(session_id: &str) -> serde_json::Value {
        wire(&ClientMessage::DiscardSession {
            session_id: session_id.to_owned(),
            cleanup: Vec::new(),
        })
    }

    fn replace_msg(tab_id: &str, pane_id: &str, session_id: &str) -> serde_json::Value {
        wire(&ClientMessage::ReplacePaneSession {
            tab_id: tab_id.to_owned(),
            pane_id: pane_id.to_owned(),
            session_id: Some(session_id.to_owned()),
        })
    }

    #[test]
    fn pane_spawn_replaces_its_pane_and_focuses_it() {
        let grid = protocol::GridNode::Split {
            direction: protocol::SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(pane("a", Some("s1"))),
            second: Box::new(pane("b", None)),
        };
        let mut tabs = model_with(&[tab("t1", &pane("x", None)), tab("t2", &grid)]);
        assert_eq!(tabs.active_id(), Some("t1"));
        tabs.take_focus_request();
        let mut spawns = PendingSpawns::default();
        spawns.start(request(true), "q1".to_owned(), aim_at("t2", "b", None));

        let new = session("new", Some("r1"), None);
        let placed = spawns.place("q1", &new, &mut tabs, &[]).expect("placed");
        assert_eq!(wire(only(&placed)), replace_msg("t2", "b", "new"));
        assert!(placed.relayout);
        assert_eq!(
            tabs.active_id(),
            Some("t2"),
            "the pane's tab comes to the front"
        );
        assert_eq!(tabs.take_focus_request().as_deref(), Some("b"));
        assert!(!spawns.has_request("q1"), "placed once");
    }

    #[test]
    fn pane_spawn_replaces_a_stopped_session_then_discards_it_last() {
        let mut tabs = model_with(&[tab("t1", &pane("a", Some("old")))]);
        let mut spawns = PendingSpawns::default();
        spawns.start(
            request(true),
            "q1".to_owned(),
            aim_at("t1", "a", Some("old")),
        );

        let placed = spawns
            .place("q1", &session("new", None, None), &mut tabs, &[])
            .expect("placed");
        let sent: Vec<serde_json::Value> = placed.messages.iter().map(wire).collect();
        assert_eq!(
            sent,
            [replace_msg("t1", "a", "new"), discard_msg("old")],
            "the pane is rebound before the old session goes, worktree kept"
        );
        assert_eq!(tabs.take_focus_request().as_deref(), Some("a"));
    }

    #[test]
    fn pane_spawn_rebinds_every_pane_showing_the_stopped_session() {
        let grid = protocol::GridNode::Split {
            direction: protocol::SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(pane("c", Some("old"))),
            second: Box::new(pane("d", Some("s2"))),
        };
        let mut tabs = model_with(&[tab("t1", &pane("a", Some("old"))), tab("t2", &grid)]);
        tabs.take_focus_request();
        let mut spawns = PendingSpawns::default();
        spawns.start(
            request(true),
            "q1".to_owned(),
            aim_at("t1", "a", Some("old")),
        );

        let placed = spawns
            .place("q1", &session("new", None, None), &mut tabs, &[])
            .expect("placed");
        let sent: Vec<serde_json::Value> = placed.messages.iter().map(wire).collect();
        assert_eq!(
            sent,
            [
                replace_msg("t1", "a", "new"),
                replace_msg("t2", "c", "new"),
                discard_msg("old"),
            ],
            "every pane of the stopped session is rebound before it goes"
        );
        assert_eq!(tabs.active_id(), Some("t1"), "the aimed pane's tab");
        assert_eq!(tabs.take_focus_request().as_deref(), Some("a"));
    }

    #[test]
    fn pane_spawn_whose_pane_closed_is_placed_in_its_tab_without_discard() {
        let mut tabs = model_with(&[
            tab("t1", &pane("x", None)),
            tab("t2", &pane("a", Some("s1"))),
        ]);
        assert_eq!(tabs.active_id(), Some("t1"));
        let mut spawns = PendingSpawns::default();
        spawns.start(
            request(true),
            "q1".to_owned(),
            aim_at("t2", "gone", Some("old")),
        );

        let placed = spawns
            .place("q1", &session("new", None, None), &mut tabs, &[])
            .expect("placed");
        assert!(
            matches!(only(&placed), ClientMessage::SplitPane { tab_id, pane_id, .. } if tab_id == "t2" && pane_id == "a"),
            "smart placement in the aimed tab, not the active one, and no discard: {:?}",
            placed.messages
        );
        assert!(placed.relayout);
        assert_eq!(
            tabs.active_id(),
            Some("t2"),
            "the aimed tab comes to the front"
        );
    }

    #[test]
    fn pane_spawn_whose_pane_took_another_session_evicts_nothing() {
        let mut tabs = model_with(&[
            tab("t1", &pane("x", None)),
            tab("t2", &pane("b", Some("restarted"))),
        ]);
        let mut spawns = PendingSpawns::default();
        spawns.start(
            request(true),
            "q1".to_owned(),
            aim_at("t2", "b", Some("old")),
        );

        let placed = spawns
            .place("q1", &session("new", None, None), &mut tabs, &[])
            .expect("placed");
        assert!(
            matches!(only(&placed), ClientMessage::SplitPane { tab_id, pane_id, .. } if tab_id == "t2" && pane_id == "b"),
            "the pane keeps what it took; the new session goes beside it: {:?}",
            placed.messages
        );
        assert_eq!(tabs.active_id(), Some("t2"));
    }

    #[test]
    fn empty_pane_spawn_whose_pane_filled_meanwhile_evicts_nothing() {
        let mut tabs = model_with(&[tab("t1", &pane("a", Some("s1")))]);
        let mut spawns = PendingSpawns::default();
        spawns.start(request(true), "q1".to_owned(), aim_at("t1", "a", None));

        let placed = spawns
            .place("q1", &session("new", None, None), &mut tabs, &[])
            .expect("placed");
        assert!(
            matches!(only(&placed), ClientMessage::SplitPane { tab_id, pane_id, .. } if tab_id == "t1" && pane_id == "a"),
            "sent {:?}",
            placed.messages
        );
    }

    #[test]
    fn aims_at_names_the_panes_a_spawn_waits_for() {
        let mut tabs = model_with(&[tab("t1", &pane("a", None))]);
        let mut spawns = PendingSpawns::default();
        spawns.start(request(true), "q0".to_owned(), OpenIn::NewTab);
        assert!(!spawns.aims_at("t1", "a"));

        spawns.start(request(true), "q1".to_owned(), aim_at("t1", "a", None));
        assert!(spawns.aims_at("t1", "a"));
        assert!(!spawns.aims_at("t1", "b"), "another pane");
        assert!(!spawns.aims_at("t2", "a"), "another tab");
        spawns.place("q1", &session("new", None, None), &mut tabs, &[]);
        assert!(!spawns.aims_at("t1", "a"), "placed");

        spawns.start(request(true), "q2".to_owned(), aim_at("t1", "a", None));
        assert!(spawns.fail("q2"));
        assert!(!spawns.aims_at("t1", "a"), "failed");
    }

    #[test]
    fn pane_spawn_whose_tab_closed_falls_back_to_the_active_tab_else_a_new_one() {
        let mut tabs = model_with(&[tab("t1", &pane("a", None))]);
        let mut spawns = PendingSpawns::default();
        spawns.start(
            request(true),
            "q1".to_owned(),
            aim_at("gone", "b", Some("old")),
        );
        let placed = spawns
            .place("q1", &session("new", None, None), &mut tabs, &[])
            .expect("placed");
        assert_eq!(wire(only(&placed)), replace_msg("t1", "a", "new"));

        let mut empty = TabsModel::new(None);
        spawns.start(
            request(true),
            "q2".to_owned(),
            aim_at("gone", "b", Some("old")),
        );
        let placed = spawns
            .place("q2", &session("new", None, None), &mut empty, &[])
            .expect("placed");
        assert!(matches!(only(&placed), ClientMessage::CreateTab { .. }));
    }

    #[test]
    fn start_stamps_the_request_id() {
        let mut spawns = PendingSpawns::default();
        let msg = spawns.start(request(true), "q1".to_owned(), OpenIn::NewTab);
        assert_eq!(sent_request(&msg).request_id.as_deref(), Some("q1"));
        assert!(spawns.has_request("q1"));
    }

    #[test]
    fn spawn_reply_places_in_its_tab() {
        let mut tabs = model_with(&[
            tab("t1", &pane("a", Some("s1"))),
            tab("t2", &pane("b", None)),
        ]);
        assert_eq!(tabs.active_id(), Some("t1"));
        let mut spawns = PendingSpawns::default();
        spawns.start(request(true), "q1".to_owned(), OpenIn::Tab("t2".to_owned()));

        let new = session("new", Some("r1"), None);
        let placed = spawns.place("q1", &new, &mut tabs, &[]).expect("placed");
        assert_eq!(
            wire(only(&placed)),
            wire(&ClientMessage::ReplacePaneSession {
                tab_id: "t2".to_owned(),
                pane_id: "b".to_owned(),
                session_id: Some("new".to_owned()),
            })
        );
        assert!(placed.relayout);
        assert_eq!(tabs.active_id(), Some("t2"), "its tab comes to the front");
        assert_eq!(tabs.take_focus_request().as_deref(), Some("b"));
        assert!(!spawns.has_request("q1"), "placed once");
        assert!(spawns.place("q1", &new, &mut tabs, &[]).is_none());
    }

    #[test]
    fn current_tab_spawn_splits_and_the_new_pane_takes_focus() {
        let mut tabs = model_with(&[tab("t1", &pane("a", Some("s1")))]);
        let mut spawns = PendingSpawns::default();
        spawns.start(
            request(true),
            "q1".to_owned(),
            OpenIn::CurrentTab("t1".to_owned()),
        );
        let new = session("new", None, None);
        let placed = spawns.place("q1", &new, &mut tabs, &[]).expect("placed");
        assert!(
            matches!(only(&placed), ClientMessage::SplitPane { pane_id, .. } if pane_id == "a")
        );
        tabs.take_focus_request();

        let grid = crate::tabs::tests::pane("a", Some("s1"));
        let split = protocol::GridNode::Split {
            direction: protocol::SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(grid),
            second: Box::new(pane("n", Some("new"))),
        };
        assert!(tabs.apply(&updated(&tab("t1", &split))));
        assert_eq!(tabs.take_focus_request().as_deref(), Some("n"));
    }

    #[test]
    fn new_tab_spawn_arms_create() {
        let mut tabs = model_with(&[tab("t1", &pane("a", Some("s1")))]);
        let mut spawns = PendingSpawns::default();
        spawns.start(request(true), "q1".to_owned(), OpenIn::NewTab);
        let new = session("new", None, None);
        let placed = spawns.place("q1", &new, &mut tabs, &[]).expect("placed");
        assert_eq!(
            wire(only(&placed)),
            wire(&ClientMessage::CreateTab {
                name: None,
                initial_session_id: Some("new".to_owned()),
            })
        );
        assert!(!placed.relayout);
        assert!(tabs.apply(&updated(&tab("t9", &pane("n", Some("new"))))));
        assert_eq!(tabs.active_id(), Some("t9"), "the created tab is activated");
    }

    #[test]
    fn spawn_into_a_closed_tab_opens_a_new_one() {
        let mut tabs = model_with(&[tab("t1", &pane("a", Some("s1")))]);
        let mut spawns = PendingSpawns::default();
        spawns.start(
            request(true),
            "q1".to_owned(),
            OpenIn::Tab("gone".to_owned()),
        );
        let placed = spawns
            .place("q1", &session("new", None, None), &mut tabs, &[])
            .expect("placed");
        assert!(matches!(only(&placed), ClientMessage::CreateTab { .. }));
        assert!(tabs.apply(&updated(&tab("t9", &pane("n", Some("new"))))));
        assert_eq!(tabs.active_id(), Some("t9"));
    }

    #[test]
    fn unknown_request_id_is_ignored() {
        let mut tabs = model_with(&[tab("t1", &pane("a", None))]);
        let mut spawns = PendingSpawns::default();
        spawns.start(request(true), "q1".to_owned(), OpenIn::NewTab);
        assert!(
            spawns
                .place("other", &session("new", None, None), &mut tabs, &[])
                .is_none()
        );
        assert!(
            spawns.has_request("q1"),
            "the spawn still waits for its own reply"
        );
        assert!(tabs.apply(&updated(&tab("t9", &pane("n", None)))));
        assert_eq!(
            tabs.active_id(),
            Some("t1"),
            "no new tab was armed for the stranger"
        );
    }

    /// An in-place spawn of `branch` in `repo`.
    fn in_place(repo: &str, branch: &str) -> SpawnRequest {
        let mut request = request(false);
        if let SpawnTarget::Single {
            repo_id,
            branch_name,
            ..
        } = &mut request.target
        {
            repo.clone_into(repo_id);
            branch.clone_into(branch_name);
        }
        request
    }

    fn target_of(msg: &ClientMessage) -> (&str, &str, Option<CheckoutStrategy>) {
        match &sent_request(msg).target {
            SpawnTarget::Single {
                repo_id,
                branch_name,
                checkout_strategy,
                ..
            } => (repo_id, branch_name, *checkout_strategy),
            other => panic!("expected a single-repo spawn, got {other:?}"),
        }
    }

    #[test]
    fn failed_spawn_is_forgotten() {
        let mut tabs = model_with(&[tab("t1", &pane("a", None))]);
        let mut spawns = PendingSpawns::default();
        spawns.start(request(false), "q1".to_owned(), OpenIn::NewTab);
        assert!(spawns.fail("q1"));
        assert!(!spawns.fail("q1"));
        assert!(
            spawns
                .place("q1", &session("new", None, None), &mut tabs, &[])
                .is_none()
        );
        assert_eq!(
            spawns.resolve_checkout(None, "r1", "feature"),
            None,
            "a failed spawn cannot be prompted for"
        );
        assert!(spawns.resend("q1", Some(CheckoutStrategy::Stash)).is_none());
    }

    #[test]
    fn reconnect_clears_pending_spawns() {
        let mut tabs = model_with(&[tab("t1", &pane("a", None))]);
        let mut spawns = PendingSpawns::default();
        spawns.start(request(false), "q1".to_owned(), OpenIn::NewTab);
        spawns.start(request(true), "q2".to_owned(), OpenIn::NewTab);
        spawns.clear();
        assert!(!spawns.has_request("q1"));
        assert!(
            spawns
                .place("q2", &session("new", None, None), &mut tabs, &[])
                .is_none()
        );
        assert_eq!(spawns.resolve_checkout(None, "r1", "feature"), None);
    }

    #[test]
    fn checkout_retry_keeps_request_id() {
        let mut spawns = PendingSpawns::default();
        spawns.start(request(false), "q1".to_owned(), OpenIn::NewTab);
        spawns.start(request(true), "q2".to_owned(), OpenIn::NewTab);

        let id = spawns
            .resolve_checkout(None, "r1", "feature")
            .expect("the in-place spawn");
        assert_eq!(id, "q1", "the worktree spawn is never prompted for");
        let retry = spawns
            .resend(&id, Some(CheckoutStrategy::Stash))
            .expect("a retry");
        let retried = sent_request(&retry);
        assert_eq!(retried.request_id.as_deref(), Some("q1"));
        assert!(matches!(
            retried.target,
            SpawnTarget::Single {
                checkout_strategy: Some(CheckoutStrategy::Stash),
                use_worktree: false,
                ..
            }
        ));
        assert!(spawns.has_request("q1"), "the retry is still on the way");
        assert_eq!(
            spawns.resolve_checkout(None, "r1", "feature"),
            None,
            "a spawn with a strategy is not asked about again"
        );

        assert!(spawns.fail("q1"));
        assert!(!spawns.has_request("q1"), "Cancel forgets the spawn");
        assert!(spawns.has_request("q2"));
        assert!(spawns.resend("q1", Some(CheckoutStrategy::Carry)).is_none());
    }

    #[test]
    fn checkout_prompt_resolves_to_the_spawn_it_names() {
        let mut spawns = PendingSpawns::default();
        spawns.start(in_place("R1", "x"), "a".to_owned(), OpenIn::NewTab);
        spawns.start(in_place("R2", "y"), "b".to_owned(), OpenIn::NewTab);

        let id = spawns
            .resolve_checkout(None, "R1", "x")
            .expect("A is named");
        assert_eq!(id, "a");
        let retry = spawns
            .resend(&id, Some(CheckoutStrategy::Stash))
            .expect("a retry");
        assert_eq!(sent_request(&retry).request_id.as_deref(), Some("a"));
        assert_eq!(
            target_of(&retry),
            ("R1", "x", Some(CheckoutStrategy::Stash)),
            "A's own request, not the latest spawn's"
        );
        assert!(spawns.has_request("b"), "B still waits, untouched");
        assert_eq!(
            spawns.resolve_checkout(None, "R2", "y").as_deref(),
            Some("b")
        );

        spawns.start(in_place("R3", "z"), "c".to_owned(), OpenIn::NewTab);
        spawns.start(in_place("R3", "z"), "d".to_owned(), OpenIn::NewTab);
        assert_eq!(
            spawns.resolve_checkout(None, "R3", "z").as_deref(),
            Some("c"),
            "the oldest of two alike"
        );
        assert!(spawns.fail("c"));
        assert_eq!(
            spawns.resolve_checkout(None, "R3", "z").as_deref(),
            Some("d")
        );
    }

    #[test]
    fn checkout_prompt_without_matching_spawn_resends_nothing() {
        let mut spawns = PendingSpawns::default();
        spawns.start(in_place("R1", "x"), "a".to_owned(), OpenIn::NewTab);
        assert_eq!(spawns.resolve_checkout(None, "R1", "other"), None);
        assert_eq!(spawns.resolve_checkout(None, "R9", "x"), None);
        assert!(
            spawns
                .resend("nope", Some(CheckoutStrategy::Stash))
                .is_none()
        );
        assert!(!spawns.fail("nope"));
        assert!(spawns.has_request("a"), "A is left alone");
    }

    #[test]
    fn resolve_checkout_claims_each_spawn_once() {
        let mut spawns = PendingSpawns::default();
        spawns.start(in_place("R1", "x"), "a".to_owned(), OpenIn::NewTab);
        spawns.start(in_place("R1", "x"), "b".to_owned(), OpenIn::NewTab);

        assert_eq!(
            spawns.resolve_checkout(None, "R1", "x").as_deref(),
            Some("a"),
            "the oldest unclaimed"
        );
        assert_eq!(
            spawns.resolve_checkout(None, "R1", "x").as_deref(),
            Some("b"),
            "the prompt on screen has claimed A"
        );
        assert_eq!(
            spawns.resolve_checkout(None, "R1", "x"),
            None,
            "both are spoken for"
        );
        assert_eq!(
            spawns.resolve_checkout(None, "R9", "z"),
            None,
            "another repo has nothing to claim"
        );
    }

    #[test]
    fn resend_without_strategy_clears_the_claim() {
        let mut spawns = PendingSpawns::default();
        spawns.start(in_place("R1", "x"), "a".to_owned(), OpenIn::NewTab);
        assert_eq!(
            spawns.resolve_checkout(None, "R1", "x").as_deref(),
            Some("a")
        );

        let resent = spawns.resend("a", None).expect("a resend");
        assert_eq!(sent_request(&resent).request_id.as_deref(), Some("a"));
        assert_eq!(
            target_of(&resent),
            ("R1", "x", None),
            "asked again exactly as first sent"
        );
        assert!(spawns.has_request("a"), "still on its way");
        assert_eq!(
            spawns.resolve_checkout(None, "R1", "x").as_deref(),
            Some("a"),
            "the fresh prompt resolves to it again"
        );
        assert!(
            spawns.resend("nope", None).is_none(),
            "a spawn nothing waits for is not resent"
        );
    }

    #[test]
    fn prompt_with_an_id_resolves_to_that_spawn() {
        let mut spawns = PendingSpawns::default();
        spawns.start(in_place("R1", "x"), "a".to_owned(), OpenIn::NewTab);
        spawns.start(in_place("R1", "x"), "b".to_owned(), OpenIn::NewTab);

        assert_eq!(
            spawns.resolve_checkout(Some("b"), "R1", "x").as_deref(),
            Some("b"),
            "the id wins over the oldest of alike spawns"
        );
        assert_eq!(
            spawns.resolve_checkout(Some("b"), "R1", "x").as_deref(),
            Some("b"),
            "an id is not a claim: B's fresh prompt resolves to it again"
        );
        assert_eq!(
            spawns.resolve_checkout(None, "R1", "x").as_deref(),
            Some("a"),
            "the id-less fallback is untouched by it"
        );
        assert_eq!(
            spawns.resolve_checkout(Some("gone"), "R1", "x"),
            None,
            "an id of no pending spawn does not fall back to the branch"
        );
        assert!(spawns.fail("b"));
        assert_eq!(spawns.resolve_checkout(Some("b"), "R1", "x"), None);

        let carry = spawns
            .resend("a", Some(CheckoutStrategy::Carry))
            .expect("a retry");
        assert_eq!(
            target_of(&carry),
            ("R1", "x", Some(CheckoutStrategy::Carry))
        );
        let again = spawns.resend("a", None).expect("a re-ask");
        assert_eq!(
            target_of(&again),
            ("R1", "x", None),
            "one resend takes the strategy off again"
        );
    }

    #[test]
    fn prompt_id_resolves_only_to_an_in_place_spawn_awaiting_a_strategy() {
        let mut spawns = PendingSpawns::default();
        spawns.start(request(true), "wt".to_owned(), OpenIn::NewTab);
        spawns.start(in_place("R1", "x"), "a".to_owned(), OpenIn::NewTab);

        assert_eq!(
            spawns.resolve_checkout(Some("wt"), "r1", "feature"),
            None,
            "a worktree spawn is never declined over a dirty tree"
        );
        assert_eq!(
            spawns.resolve_checkout(Some("a"), "R9", "x"),
            None,
            "the prompt names another repo than the spawn"
        );
        spawns.resend("a", Some(CheckoutStrategy::Stash));
        assert_eq!(
            spawns.resolve_checkout(Some("a"), "R1", "x"),
            None,
            "a spawn that already has a strategy is not asked about"
        );
        spawns.resend("a", None);
        assert_eq!(
            spawns.resolve_checkout(Some("a"), "R1", "x").as_deref(),
            Some("a")
        );
    }

    #[test]
    fn reask_forgets_a_spawn_it_cannot_resend() {
        let mut spawns = PendingSpawns::default();
        spawns.start(request(true), "wt".to_owned(), OpenIn::NewTab);
        spawns.start(in_place("R1", "x"), "a".to_owned(), OpenIn::NewTab);

        let again = spawns.reask("a").expect("an in-place spawn is asked again");
        assert_eq!(sent_request(&again).request_id.as_deref(), Some("a"));
        assert_eq!(target_of(&again), ("R1", "x", None));
        assert!(spawns.has_request("a"), "still on its way");

        assert!(
            spawns.reask("wt").is_none(),
            "a worktree spawn has no prompt"
        );
        assert!(
            !spawns.has_request("wt"),
            "and is forgotten rather than left pending until a reconnect"
        );
        assert!(spawns.reask("gone").is_none());
    }
}
