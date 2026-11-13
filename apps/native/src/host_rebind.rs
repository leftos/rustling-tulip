//! The host rebind dialog as plain data: the host's active sessions in their
//! order, the layout picked for them (an automatic one, or one of the host's
//! own layouts to follow), and the unbind and bind messages it sends.

use protocol::{
    ClientMessage, HostArrangement, HostLayout, SessionRole, SessionSnapshot, SessionStatus,
};

use crate::layout_picker::LayoutPicker;
use crate::target::HostKey;

/// What a layout with no name reads.
pub(crate) const UNNAMED_LAYOUT: &str = "Unnamed layout";
/// The picker's "Max sessions per tab" when the dialog opens.
const DEFAULT_MAX_PER_TAB: usize = 4;

/// How the rebound sessions are laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RebindChoice {
    /// The picker's layout and maximum per tab.
    Auto,
    /// The host's layout made by the client with this id.
    Follow(String),
}

/// The open rebind dialog of one host.
#[derive(Debug, Clone)]
pub(crate) struct HostRebind {
    host: HostKey,
    host_name: Option<String>,
    /// The host's active sessions, oldest first.
    session_ids: Vec<String>,
    picker: LayoutPicker,
    choice: RebindChoice,
    /// The host's own name as its daemon reported it with its layouts.
    own_layout_name: Option<String>,
    layouts: Vec<HostLayout>,
}

impl HostRebind {
    /// The dialog for `host`, named `host_name`, over its `sessions`: the
    /// active ones, on the Auto grid with at most four per tab.
    pub(crate) fn new(
        host: HostKey,
        host_name: Option<String>,
        sessions: &[SessionSnapshot],
    ) -> Self {
        let session_ids = active_session_ids(sessions);
        let picker = LayoutPicker::new(session_ids.len(), Some(DEFAULT_MAX_PER_TAB));
        Self {
            host,
            host_name,
            session_ids,
            picker,
            choice: RebindChoice::Auto,
            own_layout_name: None,
            layouts: Vec::new(),
        }
    }

    pub(crate) fn host(&self) -> &HostKey {
        &self.host
    }

    pub(crate) fn host_name(&self) -> Option<&str> {
        self.host_name.as_deref()
    }

    /// The host's active sessions, in the order they are bound.
    pub(crate) fn session_ids(&self) -> &[String] {
        &self.session_ids
    }

    pub(crate) fn picker(&self) -> &LayoutPicker {
        &self.picker
    }

    pub(crate) fn picker_mut(&mut self) -> &mut LayoutPicker {
        &mut self.picker
    }

    pub(crate) fn choice(&self) -> &RebindChoice {
        &self.choice
    }

    /// Rebuilds the session list from the host's `sessions`, keeping the
    /// picker's mode, its grid shape while still offered (else Auto), its max
    /// per tab, and the choice.
    pub(crate) fn refresh(&mut self, sessions: &[SessionSnapshot]) {
        self.session_ids = active_session_ids(sessions);
        let old = &self.picker;
        let mut picker = LayoutPicker::new(self.session_ids.len(), old.max_per_tab());
        picker.set_mode(old.mode());
        if picker.shape_cols().any(|cols| cols == old.cols()) {
            picker.set_shape(old.cols());
        }
        self.picker = picker;
    }

    /// Keeps the host's answer: its own name and its clients' layouts. The
    /// choice stays as it is; the layout named after the host is offered
    /// first.
    pub(crate) fn set_layouts(&mut self, host_name: Option<String>, layouts: Vec<HostLayout>) {
        self.own_layout_name = host_name;
        self.layouts = layouts;
    }

    /// The layouts to follow as `(client_id, label)`: the host's own first,
    /// then by client id, each labelled by its name.
    pub(crate) fn follow_options(&self) -> Vec<(String, String)> {
        let mut layouts: Vec<&HostLayout> = self.layouts.iter().collect();
        layouts
            .sort_by(|a, b| (!self.is_own(a), &a.client_id).cmp(&(!self.is_own(b), &b.client_id)));
        layouts
            .into_iter()
            .map(|layout| {
                let label = layout
                    .name
                    .clone()
                    .unwrap_or_else(|| UNNAMED_LAYOUT.to_owned());
                (layout.client_id.clone(), label)
            })
            .collect()
    }

    /// Whether `layout` is the host's own: named after the host, both names
    /// known.
    fn is_own(&self, layout: &HostLayout) -> bool {
        layout.name.is_some() && layout.name == self.own_layout_name
    }

    /// Follows the layout of the client with `client_id`.
    pub(crate) fn choose_follow(&mut self, client_id: String) {
        self.choice = RebindChoice::Follow(client_id);
    }

    /// Lays the sessions out with the picker.
    pub(crate) fn choose_auto(&mut self) {
        self.choice = RebindChoice::Auto;
    }

    /// What unbinds every session of the host, sent to This PC.
    pub(crate) fn unbind_message(&self, request_id: String) -> ClientMessage {
        ClientMessage::UnbindHostPanes {
            host: self.host.wire().map(str::to_owned),
            request_id: Some(request_id),
        }
    }

    /// What unbinds every session of the host and binds its active ones
    /// again as chosen, with `ids` the unbind's and the bind's request ids;
    /// `aspect` is the pane area's width over its height.
    pub(crate) fn rebind_messages(&self, aspect: f32, ids: (String, String)) -> Vec<ClientMessage> {
        let (unbind_id, bind_id) = ids;
        let mut out = vec![self.unbind_message(unbind_id)];
        if !self.session_ids.is_empty() {
            out.push(ClientMessage::BindHostSessions {
                host: self.host.wire().map(str::to_owned),
                session_ids: self.session_ids.clone(),
                arrangement: self.arrangement(aspect),
                request_id: Some(bind_id),
            });
        }
        out
    }

    /// The chosen layout to follow while the host still lists it, else the
    /// picker's.
    fn arrangement(&self, aspect: f32) -> HostArrangement {
        if let RebindChoice::Follow(client_id) = &self.choice
            && let Some(layout) = self
                .layouts
                .iter()
                .find(|layout| &layout.client_id == client_id)
        {
            return HostArrangement::Follow {
                tabs: layout.tabs.clone(),
            };
        }
        let picked = self.picker.arrangement(aspect);
        HostArrangement::Auto {
            layout: picked.layout,
            max_per_tab: picked
                .max_per_tab
                .map(|max| u32::try_from(max).unwrap_or(u32::MAX)),
        }
    }
}

/// The ids of `sessions` that are active, oldest first, ties by id.
fn active_session_ids(sessions: &[SessionSnapshot]) -> Vec<String> {
    let mut active: Vec<&SessionSnapshot> = sessions.iter().filter(|s| is_active(s)).collect();
    active.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    active
        .into_iter()
        .map(|session| session.id.clone())
        .collect()
}

/// Whether `session` is active: running or starting, not parked or
/// abandoned, and one of the user's own rather than an account login.
fn is_active(session: &SessionSnapshot) -> bool {
    !matches!(
        session.status,
        SessionStatus::Stopped | SessionStatus::Error
    ) && !session.is_inactive
        && !session.is_abandoned
        && session.role != SessionRole::AccountLogin
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "test fixtures parse fixed JSON and timestamps"
)]
mod tests {
    use protocol::{RearrangeLayout, TabContent, TabEntry};
    use serde_json::json;

    use super::*;
    use crate::layout_picker::Mode;

    const ASPECT: f32 = 16.0 / 10.0;

    /// A running session `id` started at minute `minute`.
    fn session(id: &str, minute: u32) -> SessionSnapshot {
        serde_json::from_value(json!({
            "id": id,
            "label": id,
            "kind": "single",
            "members": [],
            "status": "idle",
            "mode": "interactive",
            "started_at": format!("2026-01-01T00:{minute:02}:00Z"),
            "exit_code": null,
            "metrics": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cost_usd": 0.0,
                "last_activity_at": null,
            },
            "recent_actions": [],
            "agent": null,
        }))
        .expect("session fixture")
    }

    fn running(count: usize) -> Vec<SessionSnapshot> {
        (0..count)
            .map(|n| session(&format!("s{n}"), u32::try_from(n).unwrap_or(0)))
            .collect()
    }

    fn tab(id: &str) -> TabEntry {
        TabEntry {
            id: id.to_owned(),
            name: id.to_owned(),
            content: TabContent::Dashboard,
            created_at: "2026-01-01T00:00:00Z".parse().expect("timestamp"),
            picked_layout: None,
        }
    }

    fn layout(client_id: &str, name: Option<&str>) -> HostLayout {
        HostLayout {
            client_id: client_id.to_owned(),
            name: name.map(str::to_owned),
            tabs: vec![tab(&format!("{client_id}-tab"))],
        }
    }

    fn ids() -> (String, String) {
        ("r-unbind".to_owned(), "r-bind".to_owned())
    }

    fn bound_arrangement(msgs: &[ClientMessage]) -> Option<&HostArrangement> {
        msgs.iter().find_map(|msg| match msg {
            ClientMessage::BindHostSessions { arrangement, .. } => Some(arrangement),
            _ => None,
        })
    }

    fn remote() -> HostKey {
        HostKey::Remote("box".to_owned())
    }

    #[test]
    fn active_sessions_leave_out_stopped_parked_abandoned_and_logins() {
        let mut stopped = session("stopped", 1);
        stopped.status = SessionStatus::Stopped;
        let mut errored = session("errored", 2);
        errored.status = SessionStatus::Error;
        let mut parked = session("parked", 3);
        parked.is_inactive = true;
        let mut abandoned = session("abandoned", 4);
        abandoned.is_abandoned = true;
        let mut login = session("login", 5);
        login.role = SessionRole::AccountLogin;
        let sessions = vec![
            stopped,
            session("a", 6),
            errored,
            parked,
            abandoned,
            login,
            session("b", 7),
        ];
        let rebind = HostRebind::new(HostKey::Local, None, &sessions);
        assert_eq!(rebind.session_ids(), ["a", "b"]);
    }

    #[test]
    fn active_sessions_are_ordered_by_start() {
        let sessions = vec![session("c", 5), session("b", 1), session("a", 5)];
        let rebind = HostRebind::new(HostKey::Local, None, &sessions);
        assert_eq!(
            rebind.session_ids(),
            ["b", "a", "c"],
            "by start time, ties by id"
        );
    }

    #[test]
    fn unbind_names_the_host_or_this_pc() {
        let local = HostRebind::new(HostKey::Local, None, &[]);
        assert!(
            matches!(
                local.unbind_message("r1".to_owned()),
                ClientMessage::UnbindHostPanes { host: None, request_id: Some(ref id) } if id == "r1"
            ),
            "This PC is no host on the wire"
        );
        let remote = HostRebind::new(remote(), Some("box".to_owned()), &[]);
        assert!(
            matches!(
                remote.unbind_message("r2".to_owned()),
                ClientMessage::UnbindHostPanes { host: Some(ref host), request_id: Some(ref id) }
                    if host == "box" && id == "r2"
            ),
            "a remote host by its key"
        );
    }

    #[test]
    fn rebind_sends_unbind_then_an_auto_bind() {
        let sessions = running(5);
        let mut rebind = HostRebind::new(remote(), None, &sessions);
        rebind.picker_mut().max_down();
        rebind.picker_mut().max_down();
        let sent = rebind.rebind_messages(ASPECT, ids());
        let expected_ids: Vec<String> = (0..5).map(|n| format!("s{n}")).collect();
        let expected = HostArrangement::Auto {
            layout: RearrangeLayout::Grid {
                cols: 0,
                aspect: Some(ASPECT),
            },
            max_per_tab: Some(2),
        };
        assert!(
            matches!(
                sent.as_slice(),
                [
                    ClientMessage::UnbindHostPanes { host: Some(unbind_host), request_id: Some(unbind_id) },
                    ClientMessage::BindHostSessions { host: Some(bind_host), session_ids, arrangement, request_id: Some(bind_id) },
                ] if unbind_host == "box" && unbind_id == "r-unbind" && bind_host == "box"
                    && *session_ids == expected_ids && *arrangement == expected && bind_id == "r-bind"
            ),
            "{sent:?}"
        );
    }

    #[test]
    fn side_by_side_and_all_in_one_tab() {
        let sessions = running(3);
        let mut rebind = HostRebind::new(HostKey::Local, None, &sessions);
        rebind.picker_mut().set_mode(Mode::SideBySide);
        for _ in 0..4 {
            rebind.picker_mut().max_down();
        }
        assert_eq!(rebind.picker().max_per_tab(), None);
        let sent = rebind.rebind_messages(ASPECT, ids());
        assert_eq!(
            bound_arrangement(&sent),
            Some(&HostArrangement::Auto {
                layout: RearrangeLayout::Horizontal,
                max_per_tab: None,
            })
        );
    }

    #[test]
    fn follow_sends_the_chosen_layouts_tabs() {
        let sessions = running(2);
        let mut rebind = HostRebind::new(remote(), None, &sessions);
        rebind.set_layouts(
            Some("box".to_owned()),
            vec![layout("c1", Some("lab")), layout("c2", Some("box"))],
        );
        assert_eq!(*rebind.choice(), RebindChoice::Auto, "nothing preselected");
        rebind.choose_follow("c2".to_owned());
        let sent = rebind.rebind_messages(ASPECT, ids());
        assert_eq!(
            bound_arrangement(&sent),
            Some(&HostArrangement::Follow {
                tabs: vec![tab("c2-tab")],
            })
        );
    }

    #[test]
    fn the_hosts_own_layout_is_offered_first() {
        let mut rebind = HostRebind::new(remote(), Some("box".to_owned()), &[]);
        rebind.set_layouts(
            Some("box".to_owned()),
            vec![layout("c1", Some("lab")), layout("c2", Some("box"))],
        );
        assert_eq!(
            rebind.follow_options(),
            vec![
                ("c2".to_owned(), "box".to_owned()),
                ("c1".to_owned(), "lab".to_owned()),
            ]
        );
    }

    #[test]
    fn an_unnamed_layout_reads_unnamed() {
        let mut rebind = HostRebind::new(remote(), None, &[]);
        rebind.set_layouts(None, vec![layout("c2", Some("lab")), layout("c1", None)]);
        assert_eq!(
            rebind.follow_options(),
            vec![
                ("c1".to_owned(), UNNAMED_LAYOUT.to_owned()),
                ("c2".to_owned(), "lab".to_owned()),
            ],
            "a nameless layout is never the host's own: the rest by client id"
        );
        assert_eq!(UNNAMED_LAYOUT, "Unnamed layout");
    }

    #[test]
    fn a_vanished_follow_choice_falls_back_to_auto() {
        let sessions = running(2);
        let mut rebind = HostRebind::new(remote(), None, &sessions);
        rebind.set_layouts(Some("box".to_owned()), vec![layout("c1", Some("box"))]);
        rebind.choose_follow("c9".to_owned());
        let sent = rebind.rebind_messages(ASPECT, ids());
        assert!(
            matches!(bound_arrangement(&sent), Some(HostArrangement::Auto { .. })),
            "{sent:?}"
        );
    }

    #[test]
    fn refresh_takes_a_new_session_and_drops_a_stopped_one() {
        let mut sessions = running(3);
        let mut rebind = HostRebind::new(remote(), None, &sessions);
        if let Some(stopped) = sessions.get_mut(1) {
            stopped.status = SessionStatus::Stopped;
        }
        sessions.push(session("s9", 30));
        rebind.refresh(&sessions);
        assert_eq!(rebind.session_ids(), ["s0", "s2", "s9"]);
        assert_eq!(rebind.picker().count(), 3, "the picker counts the new list");
    }

    #[test]
    fn refresh_keeps_the_mode_max_and_follow_choice() {
        let mut rebind = HostRebind::new(remote(), None, &running(5));
        rebind.set_layouts(Some("box".to_owned()), vec![layout("c1", Some("box"))]);
        rebind.picker_mut().set_mode(Mode::Stacked);
        rebind.picker_mut().max_up();
        rebind.choose_follow("c1".to_owned());
        rebind.refresh(&running(6));
        assert_eq!(rebind.session_ids().len(), 6);
        assert_eq!(rebind.picker().mode(), Mode::Stacked);
        assert_eq!(rebind.picker().max_per_tab(), Some(5));
        assert_eq!(*rebind.choice(), RebindChoice::Follow("c1".to_owned()));
        assert_eq!(
            rebind.follow_options(),
            vec![("c1".to_owned(), "box".to_owned())],
            "the host's layouts stay"
        );
    }

    #[test]
    fn refresh_falls_back_to_auto_when_the_shape_is_no_longer_offered() {
        let mut rebind = HostRebind::new(remote(), None, &running(5));
        rebind.picker_mut().set_shape(4);
        rebind.refresh(&running(5));
        assert_eq!(rebind.picker().cols(), 4, "a shape still offered stays");
        rebind.refresh(&running(3));
        assert_eq!(
            rebind.picker().cols(),
            0,
            "three sessions offer no 4-column grid"
        );
    }

    #[test]
    fn no_active_sessions_sends_only_the_unbind() {
        let mut sessions = running(2);
        for session in &mut sessions {
            session.status = SessionStatus::Stopped;
        }
        let rebind = HostRebind::new(remote(), None, &sessions);
        let sent = rebind.rebind_messages(ASPECT, ids());
        assert!(
            matches!(
                sent.as_slice(),
                [ClientMessage::UnbindHostPanes { request_id: Some(id), .. }] if id == "r-unbind"
            ),
            "{sent:?}"
        );
    }
}
