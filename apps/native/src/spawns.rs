//! Spawns this client asked for, matched to the daemon's reply by request id
//! and placed where the user chose to open them.

use std::collections::HashMap;

use protocol::{CheckoutStrategy, ClientMessage, SessionSnapshot, SpawnRequest, SpawnTarget};

use crate::tabs::{PaneTarget, Placement, TabsModel};

/// Where a spawned session opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenIn {
    /// The tab that was active when the spawn was asked for.
    CurrentTab(String),
    /// A tab the user picked.
    Tab(String),
    /// A tab of its own.
    NewTab,
}

/// What placing a spawn's reply takes.
#[derive(Debug)]
pub(crate) struct SpawnPlaced {
    /// The request that puts the session in its pane or tab.
    pub message: ClientMessage,
    /// The tab model changed (a tab activated, a pane focused), so the view
    /// must follow it.
    pub relayout: bool,
}

/// A spawn waiting for its reply.
#[derive(Debug)]
struct Pending {
    open_in: OpenIn,
    /// Start order, so the oldest of alike spawns answers a prompt first.
    seq: u64,
    /// The request as last sent, kept for an in-place spawn only: the
    /// daemon may ask how to switch its dirty tree, and the answer resends it.
    in_place: Option<SpawnRequest>,
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
        };
        self.pending.insert(request_id, pending);
        ClientMessage::SpawnSession(request)
    }

    /// Whether `request_id` is one of this client's spawns.
    #[cfg(test)]
    pub(crate) fn has_request(&self, request_id: &str) -> bool {
        self.pending.contains_key(request_id)
    }

    /// When `request_id` names a pending spawn: where `session` goes. A new
    /// tab is armed so the tab's arrival activates it; a named tab is
    /// activated so the new pane takes its focus, and a filled empty pane is
    /// focused at once.
    pub(crate) fn place(
        &mut self,
        request_id: &str,
        session: &SessionSnapshot,
        tabs: &mut TabsModel,
        sessions: &[SessionSnapshot],
    ) -> Option<SpawnPlaced> {
        let open_in = self.pending.remove(request_id)?.open_in;
        let placement = match &open_in {
            OpenIn::NewTab => Placement::NewTab,
            OpenIn::CurrentTab(tab_id) | OpenIn::Tab(tab_id) => {
                tabs.activate(tab_id);
                tabs.place_in(tab_id, session, sessions)
            }
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
            message: placement.message(&session.id),
            relayout: open_in != OpenIn::NewTab,
        })
    }

    /// A failure reply for `request_id`; returns whether it was pending.
    pub(crate) fn fail(&mut self, request_id: &str) -> bool {
        self.pending.remove(request_id).is_some()
    }

    /// Forgets every spawn, as a new connection must.
    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }

    /// The spawn a `CheckoutConfirmRequired` for `branch` of `repo_id` asks
    /// about: the oldest pending in-place spawn of that branch and repo that
    /// has no strategy yet. The daemon's prompt copies both from the request
    /// and carries no request id.
    pub(crate) fn resolve_checkout(&self, repo_id: &str, branch: &str) -> Option<String> {
        self.pending
            .iter()
            .filter(|(_, pending)| {
                pending
                    .in_place
                    .as_ref()
                    .is_some_and(|request| awaits_checkout(request, repo_id, branch))
            })
            .min_by_key(|(_, pending)| pending.seq)
            .map(|(id, _)| id.clone())
    }

    /// The in-place spawn `request_id` again, with `strategy` and the same
    /// request id, while it still waits for its reply.
    pub(crate) fn retry_checkout(
        &mut self,
        request_id: &str,
        strategy: CheckoutStrategy,
    ) -> Option<ClientMessage> {
        let request = self.pending.get_mut(request_id)?.in_place.as_mut()?;
        let SpawnTarget::Single {
            checkout_strategy, ..
        } = &mut request.target
        else {
            return None;
        };
        *checkout_strategy = Some(strategy);
        Some(ClientMessage::SpawnSession(request.clone()))
    }

    /// Cancel on the checkout prompt: spawn `request_id` is dropped; returns
    /// whether it was still pending.
    pub(crate) fn cancel_checkout(&mut self, request_id: &str) -> bool {
        self.pending.remove(request_id).is_some()
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
            wire(&placed.message),
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
            matches!(placed.message, ClientMessage::SplitPane { ref pane_id, .. } if pane_id == "a")
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
            wire(&placed.message),
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
        assert!(matches!(placed.message, ClientMessage::CreateTab { .. }));
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
            spawns.resolve_checkout("r1", "feature"),
            None,
            "a failed spawn cannot be prompted for"
        );
        assert!(
            spawns
                .retry_checkout("q1", CheckoutStrategy::Stash)
                .is_none()
        );
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
        assert_eq!(spawns.resolve_checkout("r1", "feature"), None);
    }

    #[test]
    fn checkout_retry_keeps_request_id() {
        let mut spawns = PendingSpawns::default();
        spawns.start(request(false), "q1".to_owned(), OpenIn::NewTab);
        spawns.start(request(true), "q2".to_owned(), OpenIn::NewTab);

        let id = spawns
            .resolve_checkout("r1", "feature")
            .expect("the in-place spawn");
        assert_eq!(id, "q1", "the worktree spawn is never prompted for");
        let retry = spawns
            .retry_checkout(&id, CheckoutStrategy::Stash)
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
            spawns.resolve_checkout("r1", "feature"),
            None,
            "a spawn with a strategy is not asked about again"
        );

        assert!(spawns.cancel_checkout("q1"));
        assert!(!spawns.has_request("q1"), "Cancel forgets the spawn");
        assert!(spawns.has_request("q2"));
        assert!(
            spawns
                .retry_checkout("q1", CheckoutStrategy::Carry)
                .is_none()
        );
    }

    #[test]
    fn checkout_prompt_resolves_to_the_spawn_it_names() {
        let mut spawns = PendingSpawns::default();
        spawns.start(in_place("R1", "x"), "a".to_owned(), OpenIn::NewTab);
        spawns.start(in_place("R2", "y"), "b".to_owned(), OpenIn::NewTab);

        let id = spawns.resolve_checkout("R1", "x").expect("A is named");
        assert_eq!(id, "a");
        let retry = spawns
            .retry_checkout(&id, CheckoutStrategy::Stash)
            .expect("a retry");
        assert_eq!(sent_request(&retry).request_id.as_deref(), Some("a"));
        assert_eq!(
            target_of(&retry),
            ("R1", "x", Some(CheckoutStrategy::Stash)),
            "A's own request, not the latest spawn's"
        );
        assert!(spawns.has_request("b"), "B still waits, untouched");
        assert_eq!(spawns.resolve_checkout("R2", "y").as_deref(), Some("b"));

        spawns.start(in_place("R3", "z"), "c".to_owned(), OpenIn::NewTab);
        spawns.start(in_place("R3", "z"), "d".to_owned(), OpenIn::NewTab);
        assert_eq!(
            spawns.resolve_checkout("R3", "z").as_deref(),
            Some("c"),
            "the oldest of two alike"
        );
        assert!(spawns.cancel_checkout("c"));
        assert_eq!(spawns.resolve_checkout("R3", "z").as_deref(), Some("d"));
    }

    #[test]
    fn checkout_prompt_without_matching_spawn_resends_nothing() {
        let mut spawns = PendingSpawns::default();
        spawns.start(in_place("R1", "x"), "a".to_owned(), OpenIn::NewTab);
        assert_eq!(spawns.resolve_checkout("R1", "other"), None);
        assert_eq!(spawns.resolve_checkout("R9", "x"), None);
        assert!(
            spawns
                .retry_checkout("nope", CheckoutStrategy::Stash)
                .is_none()
        );
        assert!(!spawns.cancel_checkout("nope"));
        assert!(spawns.has_request("a"), "A is left alone");
    }
}
