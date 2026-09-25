//! The delete-worktree confirm: what a discard does to each member's branch,
//! which buttons the dialog offers, and the messages its answer sends.
//! Mirrors the Tauri app's `DeleteWorktreeDialog.tsx` and `utils/branchFate.ts`.

use std::time::{Duration, Instant};

use protocol::{
    BranchCleanup, BranchFate, CleanupAction, ClientMessage, MemberBranchFate, MergeEvidence,
    SessionSnapshot, SessionStatus, UntouchedReason,
};

/// How long the dialog waits for the daemon's `DiscardPreview` before it
/// falls back to the explicit keep / delete choice with no branch
/// information.
pub(crate) const PREVIEW_TIMEOUT: Duration = Duration::from_secs(10);

/// Which button set the dialog offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscardChoice {
    /// Every branch the daemon would touch already landed, so one "delete
    /// worktree and branch" button is safe.
    DeleteAll,
    /// A branch holds work that landed nowhere the daemon checked, or has a
    /// fate this build cannot read, so the user picks keep or delete.
    /// `lost_commits` is what the delete destroys, `None` when any
    /// contributing count is unknown.
    Choose { lost_commits: Option<u32> },
    /// No branch can be deleted; only the worktree goes.
    WorktreeOnly,
}

/// What the dialog shows: the wait for the preview, or a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialogMode {
    Loading,
    Ready(DiscardChoice),
}

/// A button of the dialog's footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialogButton {
    Cancel,
    /// "Delete worktree, keep branch".
    KeepBranch,
    /// "Delete worktree and branch", with the commits it loses when asked.
    DeleteBranch,
    /// "Delete worktree", when no branch can be deleted.
    WorktreeOnly,
}

/// One member repo as the dialog lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberRow {
    pub(crate) repo: String,
    pub(crate) branch: String,
    pub(crate) fate: String,
}

/// The open dialog for one session: the preview once it arrives, whether
/// it timed out, and the focused button.
#[derive(Debug)]
pub(crate) struct DeleteWorktreeConfirm {
    session_id: String,
    members: Option<Vec<MemberBranchFate>>,
    timed_out: bool,
    deadline: Instant,
    focused: DialogButton,
}

/// One sentence on what a discard does to a member's branch.
pub(crate) fn describe_member_fate(fate: &BranchFate) -> String {
    match fate {
        BranchFate::WillDelete { into, via } => match via {
            MergeEvidence::PatchEquivalent => {
                format!("already in {into} (cherry-picked or rebased)")
            }
            MergeEvidence::Ancestry => format!("already merged into {into}"),
            MergeEvidence::Unknown => format!("already in {into}"),
        },
        BranchFate::KeptByDefault {
            unique_commits,
            checked_against,
        } => describe_kept(*unique_commits, checked_against),
        BranchFate::Untouched { reason } => describe_untouched(*reason).to_owned(),
        BranchFate::Unknown => "unknown state; left alone unless you choose delete".to_owned(),
    }
}

fn describe_kept(unique_commits: Option<u32>, checked_against: &[String]) -> String {
    let Some(count) = unique_commits else {
        return "couldn't determine whether its commits landed".to_owned();
    };
    let commits = commit_count(count);
    if checked_against.is_empty() {
        format!("{commits} not found anywhere the daemon checked")
    } else {
        format!("{commits} not in {}", checked_against.join(", "))
    }
}

fn commit_count(count: u32) -> String {
    if count == 1 {
        "1 commit".to_owned()
    } else {
        format!("{count} commits")
    }
}

fn describe_untouched(reason: UntouchedReason) -> &'static str {
    match reason {
        UntouchedReason::ExternalWorktree => "not managed by rustling-tulip; left alone",
        UntouchedReason::CheckedOutElsewhere => "checked out in another worktree; left alone",
        UntouchedReason::BranchMissing => "branch no longer exists",
        UntouchedReason::Unknown => "left alone",
    }
}

/// Whether a member forces the explicit keep / delete choice: unlanded
/// work, or a fate this build cannot read and so treats as "might hold
/// work".
fn needs_explicit_choice(fate: &BranchFate) -> bool {
    matches!(fate, BranchFate::KeptByDefault { .. } | BranchFate::Unknown)
}

/// Commits a branch delete would destroy across every member, or `None`
/// when any member's contribution cannot be counted.
fn lost_commit_count(members: &[MemberBranchFate]) -> Option<u32> {
    let mut total: u32 = 0;
    for member in members {
        match &member.fate {
            BranchFate::Unknown => return None,
            BranchFate::KeptByDefault { unique_commits, .. } => {
                total = total.saturating_add((*unique_commits)?);
            }
            BranchFate::WillDelete { .. } | BranchFate::Untouched { .. } => {}
        }
    }
    Some(total)
}

/// Reduces every member's fate to the one decision the dialog asks for. An
/// empty member list means nothing is deletable, as all-untouched does.
pub(crate) fn discard_choice(members: &[MemberBranchFate]) -> DiscardChoice {
    if members.iter().any(|m| needs_explicit_choice(&m.fate)) {
        return DiscardChoice::Choose {
            lost_commits: lost_commit_count(members),
        };
    }
    if members
        .iter()
        .any(|m| matches!(m.fate, BranchFate::WillDelete { .. }))
    {
        return DiscardChoice::DeleteAll;
    }
    DiscardChoice::WorktreeOnly
}

/// What confirming sends: `StopSession` while the session is still live,
/// then `DiscardSession` removing every member's worktree with `branch` for
/// its branch. The daemon's discard closes the panes showing it, as it does
/// for the Tauri app's menu and overlay deletes.
pub(crate) fn confirm_messages(
    session: &SessionSnapshot,
    branch: BranchCleanup,
) -> Vec<ClientMessage> {
    let mut messages = Vec::new();
    if !matches!(
        session.status,
        SessionStatus::Stopped | SessionStatus::Error
    ) {
        messages.push(ClientMessage::StopSession {
            session_id: session.id.clone(),
            cleanup: Vec::new(),
        });
    }
    messages.push(ClientMessage::DiscardSession {
        session_id: session.id.clone(),
        cleanup: session
            .members
            .iter()
            .map(|member| CleanupAction {
                repo_id: member.repo_id.clone(),
                remove_worktree: true,
                branch,
            })
            .collect(),
    });
    messages
}

impl DialogButton {
    /// The button's debug selector, the Tauri dialog's test id.
    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "delete-worktree-cancel",
            Self::KeepBranch => "delete-worktree-keep-branch",
            Self::DeleteBranch => "delete-worktree-and-branch",
            Self::WorktreeOnly => "delete-worktree-only",
        }
    }

    /// What the button answers for every member's branch; `None` cancels.
    pub(crate) fn branch(self) -> Option<BranchCleanup> {
        match self {
            Self::Cancel => None,
            Self::KeepBranch | Self::WorktreeOnly => Some(BranchCleanup::Keep),
            Self::DeleteBranch => Some(BranchCleanup::Delete),
        }
    }

    pub(crate) fn is_danger(self) -> bool {
        matches!(self, Self::DeleteBranch | Self::WorktreeOnly)
    }
}

impl DeleteWorktreeConfirm {
    /// A dialog for `session_id` opened at `now`, waiting for its preview.
    pub(crate) fn new(session_id: &str, now: Instant) -> Self {
        Self {
            session_id: session_id.to_owned(),
            members: None,
            timed_out: false,
            deadline: now + PREVIEW_TIMEOUT,
            focused: DialogButton::Cancel,
        }
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The request that asks the daemon for the preview.
    pub(crate) fn request(&self) -> ClientMessage {
        ClientMessage::PreviewDiscard {
            session_id: self.session_id.clone(),
        }
    }

    /// When the wait for the preview gives up; `None` once it is over.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.members.is_none().then_some(self.deadline)
    }

    /// The daemon's preview for `session_id`; returns whether it was this
    /// dialog's. A preview after the fallback replaces it.
    pub(crate) fn on_preview(&mut self, session_id: &str, members: &[MemberBranchFate]) -> bool {
        if session_id != self.session_id {
            return false;
        }
        let before = self.buttons();
        self.members = Some(members.to_vec());
        self.timed_out = false;
        self.refocus_if_changed(&before);
        true
    }

    /// Back to the safe button when the footer changed under the focus, as
    /// the Tauri dialog refocuses only on a mode change; a focus the user
    /// placed on buttons that are still there stays.
    fn refocus_if_changed(&mut self, before: &[DialogButton]) {
        let now = self.buttons();
        if now != before || !now.contains(&self.focused) {
            self.focused = self.safe_button();
        }
    }

    /// The clock at `now`: past the deadline with no preview, the dialog
    /// falls back. Returns whether it did.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        if self.members.is_some() || now < self.deadline {
            return false;
        }
        let before = self.buttons();
        self.members = Some(Vec::new());
        self.timed_out = true;
        self.refocus_if_changed(&before);
        true
    }

    pub(crate) fn mode(&self) -> DialogMode {
        if self.timed_out {
            return DialogMode::Ready(DiscardChoice::Choose { lost_commits: None });
        }
        match &self.members {
            None => DialogMode::Loading,
            Some(members) => DialogMode::Ready(discard_choice(members)),
        }
    }

    /// The muted line above the members: still checking, or gave up.
    pub(crate) fn status_note(&self) -> Option<&'static str> {
        if self.timed_out {
            Some("Couldn't determine branch state.")
        } else if self.members.is_none() {
            Some("Checking branch state…")
        } else {
            None
        }
    }

    pub(crate) fn member_rows(&self) -> Vec<MemberRow> {
        self.members
            .iter()
            .flatten()
            .map(|member| MemberRow {
                repo: member.repo_name.clone(),
                branch: member.branch.clone(),
                fate: describe_member_fate(&member.fate),
            })
            .collect()
    }

    /// The footer's buttons, Cancel first, as the Tauri footer orders them.
    pub(crate) fn buttons(&self) -> Vec<DialogButton> {
        match self.mode() {
            DialogMode::Loading => vec![DialogButton::Cancel],
            DialogMode::Ready(DiscardChoice::WorktreeOnly) => {
                vec![DialogButton::Cancel, DialogButton::WorktreeOnly]
            }
            DialogMode::Ready(DiscardChoice::DeleteAll) => {
                vec![DialogButton::Cancel, DialogButton::DeleteBranch]
            }
            DialogMode::Ready(DiscardChoice::Choose { .. }) => vec![
                DialogButton::Cancel,
                DialogButton::KeepBranch,
                DialogButton::DeleteBranch,
            ],
        }
    }

    pub(crate) fn label(&self, button: DialogButton) -> String {
        match (button, self.mode()) {
            (DialogButton::Cancel, _) => "Cancel".to_owned(),
            (DialogButton::KeepBranch, _) => "Delete worktree, keep branch".to_owned(),
            (DialogButton::WorktreeOnly, _) => "Delete worktree".to_owned(),
            (
                DialogButton::DeleteBranch,
                DialogMode::Ready(DiscardChoice::Choose { lost_commits }),
            ) => match lost_commits {
                None => "Delete worktree and branch (commit count unknown)".to_owned(),
                Some(count) => {
                    format!("Delete worktree and branch (loses {})", commit_count(count))
                }
            },
            (DialogButton::DeleteBranch, _) => "Delete worktree and branch".to_owned(),
        }
    }

    pub(crate) fn focused(&self) -> DialogButton {
        self.focused
    }

    /// Moves the focus to the next button (or the previous one), wrapping
    /// around the footer.
    pub(crate) fn move_focus(&mut self, forward: bool) {
        let buttons = self.buttons();
        let Some(at) = buttons.iter().position(|b| *b == self.focused) else {
            self.focused = self.safe_button();
            return;
        };
        let count = buttons.len();
        let next = if forward {
            (at + 1) % count
        } else {
            (at + count - 1) % count
        };
        self.focused = buttons[next];
    }

    /// The button a stray Enter may press: keep the branch when there is a
    /// choice, else Cancel.
    fn safe_button(&self) -> DialogButton {
        match self.mode() {
            DialogMode::Ready(DiscardChoice::Choose { .. }) => DialogButton::KeepBranch,
            DialogMode::Loading | DialogMode::Ready(_) => DialogButton::Cancel,
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests build fixtures with expect; failure messages aid debugging"
)]
mod tests {
    use std::time::{Duration, Instant};

    use protocol::{
        BranchCleanup, BranchFate, CleanupAction, ClientMessage, MemberBranchFate, MergeEvidence,
        SessionSnapshot, UntouchedReason,
    };
    use serde_json::{Value, json};

    use super::{
        DeleteWorktreeConfirm, DialogButton, DialogMode, DiscardChoice, MemberRow,
        confirm_messages, describe_member_fate, discard_choice,
    };

    fn member(fate: BranchFate, repo_name: &str) -> MemberBranchFate {
        MemberBranchFate {
            repo_id: format!("{repo_name}-id"),
            repo_name: repo_name.to_owned(),
            branch: "wt/brave-otter".to_owned(),
            fate,
        }
    }

    /// A fate carrying a tag or value the daemon could grow that this build
    /// has never seen, decoded the way the wire decodes it.
    fn unknown_fate(raw: Value) -> BranchFate {
        serde_json::from_value(raw).expect("an unknown fate still decodes")
    }

    fn will_delete(into: &str, via: MergeEvidence) -> BranchFate {
        BranchFate::WillDelete {
            into: into.to_owned(),
            via,
        }
    }

    fn kept(unique_commits: Option<u32>, checked_against: &[&str]) -> BranchFate {
        BranchFate::KeptByDefault {
            unique_commits,
            checked_against: checked_against.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn untouched(reason: UntouchedReason) -> BranchFate {
        BranchFate::Untouched { reason }
    }

    fn choose(lost_commits: Option<u32>) -> DiscardChoice {
        DiscardChoice::Choose { lost_commits }
    }

    #[test]
    fn fate_names_the_target_for_an_ancestry_merged_branch() {
        assert_eq!(
            describe_member_fate(&will_delete("origin/main", MergeEvidence::Ancestry)),
            "already merged into origin/main"
        );
    }

    #[test]
    fn fate_flags_a_patch_equivalent_land_as_cherry_picked_or_rebased() {
        assert_eq!(
            describe_member_fate(&will_delete("origin/main", MergeEvidence::PatchEquivalent)),
            "already in origin/main (cherry-picked or rebased)"
        );
    }

    #[test]
    fn fate_falls_back_to_a_bare_already_in_for_an_unknown_merge_evidence() {
        let fate = unknown_fate(
            json!({ "kind": "will_delete", "into": "origin/main", "via": "bisected" }),
        );
        assert_eq!(describe_member_fate(&fate), "already in origin/main");
    }

    #[test]
    fn fate_lists_every_ref_a_kept_branch_was_measured_against() {
        assert_eq!(
            describe_member_fate(&kept(Some(3), &["origin/main", "main"])),
            "3 commits not in origin/main, main"
        );
    }

    #[test]
    fn fate_uses_the_singular_for_a_single_unique_commit() {
        assert_eq!(
            describe_member_fate(&kept(Some(1), &["main"])),
            "1 commit not in main"
        );
    }

    #[test]
    fn fate_says_anywhere_the_daemon_checked_when_no_target_resolved() {
        assert_eq!(
            describe_member_fate(&kept(Some(2), &[])),
            "2 commits not found anywhere the daemon checked"
        );
    }

    #[test]
    fn fate_admits_ignorance_when_the_commit_count_is_unknown() {
        assert_eq!(
            describe_member_fate(&kept(None, &["main"])),
            "couldn't determine whether its commits landed"
        );
    }

    #[test]
    fn fate_explains_each_untouched_reason() {
        assert_eq!(
            describe_member_fate(&untouched(UntouchedReason::ExternalWorktree)),
            "not managed by rustling-tulip; left alone"
        );
        assert_eq!(
            describe_member_fate(&untouched(UntouchedReason::CheckedOutElsewhere)),
            "checked out in another worktree; left alone"
        );
        assert_eq!(
            describe_member_fate(&untouched(UntouchedReason::BranchMissing)),
            "branch no longer exists"
        );
    }

    #[test]
    fn fate_falls_back_to_left_alone_for_an_unknown_untouched_reason() {
        let fate = unknown_fate(json!({ "kind": "untouched", "reason": "locked_by_hook" }));
        assert_eq!(describe_member_fate(&fate), "left alone");
    }

    #[test]
    fn fate_describes_an_unrecognized_kind_without_claiming_an_outcome() {
        let fate = unknown_fate(json!({ "kind": "will_archive" }));
        assert_eq!(
            describe_member_fate(&fate),
            "unknown state; left alone unless you choose delete"
        );
    }

    #[test]
    fn choice_offers_the_single_delete_button_when_every_branch_already_landed() {
        assert_eq!(
            discard_choice(&[
                member(will_delete("main", MergeEvidence::Ancestry), "repo1"),
                member(will_delete("main", MergeEvidence::PatchEquivalent), "repo2"),
            ]),
            DiscardChoice::DeleteAll
        );
    }

    #[test]
    fn choice_asks_when_a_branch_holds_unlanded_work() {
        assert_eq!(
            discard_choice(&[member(kept(Some(4), &["main"]), "repo1")]),
            choose(Some(4))
        );
    }

    #[test]
    fn choice_removes_the_worktree_only_when_no_branch_is_deletable() {
        assert_eq!(
            discard_choice(&[
                member(untouched(UntouchedReason::ExternalWorktree), "repo1"),
                member(untouched(UntouchedReason::BranchMissing), "repo2"),
            ]),
            DiscardChoice::WorktreeOnly
        );
    }

    #[test]
    fn choice_treats_an_empty_member_list_as_worktree_only() {
        assert_eq!(discard_choice(&[]), DiscardChoice::WorktreeOnly);
    }

    #[test]
    fn choice_keeps_delete_all_when_untouched_members_ride_along() {
        assert_eq!(
            discard_choice(&[
                member(will_delete("main", MergeEvidence::Ancestry), "repo1"),
                member(untouched(UntouchedReason::CheckedOutElsewhere), "repo2"),
            ]),
            DiscardChoice::DeleteAll
        );
    }

    #[test]
    fn choice_one_kept_member_outvotes_any_number_of_merged_ones() {
        assert_eq!(
            discard_choice(&[
                member(will_delete("main", MergeEvidence::Ancestry), "repo1"),
                member(kept(Some(2), &["main"]), "repo2"),
            ]),
            choose(Some(2))
        );
    }

    #[test]
    fn choice_sums_unique_commits_across_kept_members() {
        assert_eq!(
            discard_choice(&[
                member(kept(Some(2), &["main"]), "repo1"),
                member(kept(Some(5), &["main"]), "repo2"),
            ]),
            choose(Some(7))
        );
    }

    #[test]
    fn choice_propagates_unknown_when_one_kept_members_count_is_unknown() {
        assert_eq!(
            discard_choice(&[
                member(kept(Some(2), &["main"]), "repo1"),
                member(kept(None, &["main"]), "repo2"),
            ]),
            choose(None)
        );
    }

    #[test]
    fn choice_an_unknown_fate_kind_forces_the_choice_with_an_unknown_count() {
        let fate = unknown_fate(json!({ "kind": "will_archive" }));
        assert_eq!(discard_choice(&[member(fate, "repo1")]), choose(None));
    }

    #[test]
    fn choice_an_unknown_kind_hides_an_otherwise_countable_total() {
        let fate = unknown_fate(json!({ "kind": "will_archive" }));
        assert_eq!(
            discard_choice(&[
                member(kept(Some(3), &["main"]), "repo1"),
                member(fate, "repo2"),
            ]),
            choose(None)
        );
    }

    fn labels(dialog: &DeleteWorktreeConfirm) -> Vec<(&'static str, String)> {
        dialog
            .buttons()
            .into_iter()
            .map(|b| (b.selector(), dialog.label(b)))
            .collect()
    }

    fn owned(pairs: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
        pairs.iter().map(|(s, l)| (*s, (*l).to_owned())).collect()
    }

    #[test]
    fn dialog_waits_for_the_preview_then_falls_back_after_ten_seconds() {
        let t0 = Instant::now();
        let mut dialog = DeleteWorktreeConfirm::new("s1", t0);
        assert!(matches!(
            dialog.request(),
            ClientMessage::PreviewDiscard { session_id } if session_id == "s1"
        ));
        assert_eq!(dialog.mode(), DialogMode::Loading);
        assert_eq!(dialog.status_note(), Some("Checking branch state…"));
        assert_eq!(
            labels(&dialog),
            owned(&[("delete-worktree-cancel", "Cancel")])
        );
        assert_eq!(dialog.deadline(), Some(t0 + Duration::from_secs(10)));

        assert!(!dialog.tick(t0 + Duration::from_millis(9_999)));
        assert_eq!(dialog.mode(), DialogMode::Loading);
        assert!(dialog.tick(t0 + Duration::from_secs(10)));
        assert_eq!(dialog.mode(), DialogMode::Ready(choose(None)));
        assert_eq!(
            dialog.status_note(),
            Some("Couldn't determine branch state.")
        );
        assert!(dialog.member_rows().is_empty());
        assert_eq!(dialog.deadline(), None);
        assert_eq!(dialog.focused(), DialogButton::KeepBranch);
        assert_eq!(
            labels(&dialog),
            owned(&[
                ("delete-worktree-cancel", "Cancel"),
                (
                    "delete-worktree-keep-branch",
                    "Delete worktree, keep branch"
                ),
                (
                    "delete-worktree-and-branch",
                    "Delete worktree and branch (commit count unknown)"
                ),
            ])
        );
        assert!(
            !dialog.tick(t0 + Duration::from_secs(20)),
            "falls back once"
        );
    }

    #[test]
    fn dialog_late_preview_replaces_the_fallback_and_others_are_ignored() {
        let t0 = Instant::now();
        let mut dialog = DeleteWorktreeConfirm::new("s1", t0);
        let landed = [member(
            will_delete("main", MergeEvidence::Ancestry),
            "repo1",
        )];
        assert!(
            !dialog.on_preview("s2", &landed),
            "another session's preview"
        );
        assert_eq!(dialog.mode(), DialogMode::Loading);
        dialog.tick(t0 + Duration::from_secs(10));

        assert!(dialog.on_preview("s1", &landed));
        assert_eq!(dialog.mode(), DialogMode::Ready(DiscardChoice::DeleteAll));
        assert_eq!(dialog.status_note(), None);
        assert_eq!(
            dialog.member_rows(),
            [MemberRow {
                repo: "repo1".to_owned(),
                branch: "wt/brave-otter".to_owned(),
                fate: "already merged into main".to_owned(),
            }]
        );
        assert_eq!(dialog.focused(), DialogButton::Cancel);
        assert!(!dialog.tick(t0 + Duration::from_secs(30)));
    }

    #[test]
    fn dialog_buttons_and_labels_follow_the_mode() {
        let t0 = Instant::now();
        let mut dialog = DeleteWorktreeConfirm::new("s1", t0);
        dialog.on_preview(
            "s1",
            &[member(
                will_delete("main", MergeEvidence::Ancestry),
                "repo1",
            )],
        );
        assert_eq!(
            labels(&dialog),
            owned(&[
                ("delete-worktree-cancel", "Cancel"),
                ("delete-worktree-and-branch", "Delete worktree and branch"),
            ])
        );
        dialog.on_preview(
            "s1",
            &[member(untouched(UntouchedReason::BranchMissing), "repo1")],
        );
        assert_eq!(
            labels(&dialog),
            owned(&[
                ("delete-worktree-cancel", "Cancel"),
                ("delete-worktree-only", "Delete worktree"),
            ])
        );
        dialog.on_preview("s1", &[member(kept(Some(1), &["main"]), "repo1")]);
        assert_eq!(
            dialog.label(DialogButton::DeleteBranch),
            "Delete worktree and branch (loses 1 commit)"
        );
        dialog.on_preview("s1", &[member(kept(Some(3), &["main"]), "repo1")]);
        assert_eq!(
            dialog.label(DialogButton::DeleteBranch),
            "Delete worktree and branch (loses 3 commits)"
        );
    }

    #[test]
    fn dialog_focuses_the_safe_button_and_tab_cycles_the_footer() {
        let mut dialog = DeleteWorktreeConfirm::new("s1", Instant::now());
        assert_eq!(dialog.focused(), DialogButton::Cancel);
        dialog.move_focus(true);
        assert_eq!(dialog.focused(), DialogButton::Cancel, "one button");

        dialog.on_preview("s1", &[member(kept(Some(2), &["main"]), "repo1")]);
        assert_eq!(dialog.focused(), DialogButton::KeepBranch);
        dialog.move_focus(true);
        assert_eq!(dialog.focused(), DialogButton::DeleteBranch);
        dialog.move_focus(true);
        assert_eq!(dialog.focused(), DialogButton::Cancel, "wraps around");
        dialog.move_focus(false);
        assert_eq!(dialog.focused(), DialogButton::DeleteBranch);

        dialog.on_preview(
            "s1",
            &[member(
                will_delete("main", MergeEvidence::Ancestry),
                "repo1",
            )],
        );
        assert_eq!(
            dialog.focused(),
            DialogButton::Cancel,
            "a new mode refocuses"
        );
    }

    #[test]
    fn dialog_late_preview_keeps_the_focus_the_user_placed() {
        let t0 = Instant::now();
        let mut dialog = DeleteWorktreeConfirm::new("s1", t0);
        dialog.tick(t0 + Duration::from_secs(10));
        assert_eq!(dialog.focused(), DialogButton::KeepBranch);
        dialog.move_focus(false);
        assert_eq!(dialog.focused(), DialogButton::Cancel);

        dialog.on_preview("s1", &[member(kept(Some(2), &["main"]), "repo1")]);
        assert_eq!(
            dialog.focused(),
            DialogButton::Cancel,
            "the same buttons: the focus stays where the user put it"
        );
        dialog.move_focus(true);
        dialog.move_focus(true);
        assert_eq!(dialog.focused(), DialogButton::DeleteBranch);
        dialog.on_preview(
            "s1",
            &[member(
                will_delete("main", MergeEvidence::Ancestry),
                "repo1",
            )],
        );
        assert_eq!(
            dialog.focused(),
            DialogButton::Cancel,
            "a new button set goes back to the safe button"
        );
    }

    #[test]
    fn dialog_tick_before_the_deadline_leaves_it_pending() {
        let t0 = Instant::now();
        let mut dialog = DeleteWorktreeConfirm::new("s1", t0);
        let early = t0 + Duration::from_secs(4);
        assert!(!dialog.tick(early));
        assert_eq!(dialog.mode(), DialogMode::Loading);
        let deadline = dialog.deadline().expect("still waiting");
        assert_eq!(
            deadline.saturating_duration_since(early),
            Duration::from_secs(6)
        );
    }

    #[test]
    fn dialog_answers_map_to_branch_cleanup() {
        assert_eq!(DialogButton::Cancel.branch(), None);
        assert_eq!(DialogButton::KeepBranch.branch(), Some(BranchCleanup::Keep));
        assert_eq!(
            DialogButton::WorktreeOnly.branch(),
            Some(BranchCleanup::Keep)
        );
        assert_eq!(
            DialogButton::DeleteBranch.branch(),
            Some(BranchCleanup::Delete)
        );
        assert!(DialogButton::DeleteBranch.is_danger() && DialogButton::WorktreeOnly.is_danger());
        assert!(!DialogButton::Cancel.is_danger() && !DialogButton::KeepBranch.is_danger());
    }

    fn session(status: &str) -> SessionSnapshot {
        serde_json::from_value(json!({
            "id": "s1",
            "label": "repo:main",
            "kind": "workspace",
            "members": [
                { "repo_id": "r1", "repo_name": "r1", "branch": "wt/a", "worktree_path": "C:/wt/a/r1" },
                { "repo_id": "r2", "repo_name": "r2", "branch": "wt/a", "worktree_path": "C:/wt/a/r2" },
            ],
            "status": status,
            "mode": "interactive",
            "started_at": "2026-01-01T00:00:00Z",
            "exit_code": null,
            "metrics": { "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0, "last_activity_at": null },
            "recent_actions": [],
            "agent": "claude",
            "has_per_session_worktree": true,
        }))
        .expect("session fixture")
    }

    fn removing(branch: BranchCleanup) -> Vec<CleanupAction> {
        ["r1", "r2"]
            .into_iter()
            .map(|repo_id| CleanupAction {
                repo_id: repo_id.to_owned(),
                remove_worktree: true,
                branch,
            })
            .collect()
    }

    #[test]
    fn confirm_stops_a_live_session_then_discards() {
        let sent = confirm_messages(&session("working"), BranchCleanup::Delete);
        let expected = removing(BranchCleanup::Delete);
        assert!(
            matches!(sent.as_slice(), [
                ClientMessage::StopSession { session_id: stopped, cleanup: stop_cleanup },
                ClientMessage::DiscardSession { session_id: discarded, cleanup },
            ] if stopped == "s1" && stop_cleanup.is_empty()
                && discarded == "s1" && *cleanup == expected),
            "no ClosePane: the daemon's discard closes the panes: {sent:?}"
        );
    }

    #[test]
    fn confirm_skips_the_stop_for_a_stopped_session_and_sends_every_members_branch() {
        for status in ["stopped", "error"] {
            let sent = confirm_messages(&session(status), BranchCleanup::Keep);
            let expected = removing(BranchCleanup::Keep);
            assert!(
                matches!(sent.as_slice(), [
                    ClientMessage::DiscardSession { session_id, cleanup },
                ] if session_id == "s1" && *cleanup == expected),
                "{status}: {sent:?}"
            );
        }
    }
}
