//! Where a worktree spawn would fork from, asked of the daemon before the
//! spawn: which request the dialog's fields call for, the debounce before a
//! repo's is sent, which reply belongs to the fields on show, the "reuse or
//! recreate" choice a collision offers, and the texts the dialog shows for
//! a preview.

use std::time::{Duration, Instant};

use protocol::{ClientMessage, MemberSpawnPreview, WorktreeReusePolicy};

/// How long a repo's preview waits after the last change to its fields.
pub(crate) const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(250);

pub(crate) const REUSE_NOTE: &str =
    "(keeps its existing fork point; the base branch is not applied)";
pub(crate) const RECREATE_LABEL: &str = "Recreate from the base branch";

/// The fields a preview answers for: the target, the trimmed branch and
/// the trimmed base (`None` when empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreviewKey {
    Repo {
        repo_id: String,
        branch: String,
        base: Option<String>,
    },
    Workspace {
        workspace_id: String,
        branch: String,
        base: Option<String>,
    },
}

impl PreviewKey {
    fn request(&self, request_id: String) -> ClientMessage {
        match self {
            Self::Repo {
                repo_id,
                branch,
                base,
            } => ClientMessage::PreviewSpawn {
                repo_id: repo_id.clone(),
                branch_name: branch.clone(),
                base_branch: base.clone(),
                use_worktree: true,
                request_id: Some(request_id),
            },
            Self::Workspace {
                workspace_id,
                branch,
                base,
            } => ClientMessage::PreviewWorkspaceSpawn {
                workspace_id: workspace_id.clone(),
                branch_name: branch.clone(),
                base_branch: base.clone(),
                request_id: Some(request_id),
            },
        }
    }
}

/// What to do with a worktree or branch already at the spawn's place.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ReuseChoice {
    #[default]
    Reuse,
    Recreate,
}

impl ReuseChoice {
    pub(crate) const ALL: [Self; 2] = [Self::Reuse, Self::Recreate];

    fn policy(self) -> WorktreeReusePolicy {
        match self {
            Self::Reuse => WorktreeReusePolicy::Reuse,
            Self::Recreate => WorktreeReusePolicy::RecreateFromBase,
        }
    }
}

/// The preview for the fields on show.
#[derive(Debug, Default)]
pub(crate) struct Preview {
    /// What the fields call for; `None` when they call for no preview.
    key: Option<PreviewKey>,
    /// When a repo's request goes out, while it waits.
    due: Option<Instant>,
    /// The id of the latest request for `key`, sent after its last change
    /// and not yet answered; only its reply is taken.
    pending: Option<String>,
    /// Each member's answer, one for a repo.
    members: Vec<MemberSpawnPreview>,
    reuse: ReuseChoice,
}

impl Preview {
    /// The fields now call for `key`. A change drops the preview on show,
    /// forgets any request in flight, resets the choice to Reuse, and for a
    /// repo waits out the debounce from `now` before asking again.
    pub(crate) fn follow(&mut self, key: Option<PreviewKey>, now: Instant) {
        if key == self.key {
            return;
        }
        self.due = matches!(key, Some(PreviewKey::Repo { .. })).then(|| now + PREVIEW_DEBOUNCE);
        self.key = key;
        self.pending = None;
        self.members.clear();
        self.reuse = ReuseChoice::Reuse;
    }

    /// When the waiting repo request goes out.
    pub(crate) fn due(&self) -> Option<Instant> {
        self.due
    }

    /// The repo request, once its debounce ran out at `now`.
    pub(crate) fn take_due(&mut self, now: Instant) -> Option<ClientMessage> {
        if self.due.is_none_or(|due| now < due) {
            return None;
        }
        self.due = None;
        self.send()
    }

    fn send(&mut self) -> Option<ClientMessage> {
        let request_id = crate::new_request_id();
        let msg = self.key.as_ref()?.request(request_id.clone());
        self.pending = Some(request_id);
        Some(msg)
    }

    /// Whether a workspace's Preview button can ask: its branch is set.
    pub(crate) fn can_request(&self) -> bool {
        matches!(self.key, Some(PreviewKey::Workspace { .. }))
    }

    /// The workspace's Preview button.
    pub(crate) fn request(&mut self) -> Option<ClientMessage> {
        if self.can_request() {
            self.send()
        } else {
            None
        }
    }

    /// A repo's reply to request `request_id`; returns whether it is the one
    /// the fields wait for. An id-less reply, from a daemon that echoes none,
    /// is taken when it names the repo and branch on show.
    pub(crate) fn on_repo_reply(
        &mut self,
        repo_id: &str,
        branch_name: &str,
        preview: &MemberSpawnPreview,
        request_id: Option<&str>,
    ) -> bool {
        let names_key = matches!(&self.key, Some(PreviewKey::Repo { repo_id: id, branch, .. })
            if id == repo_id && branch == branch_name);
        self.take_reply(request_id, names_key, std::slice::from_ref(preview))
    }

    /// A workspace's reply to request `request_id`; returns whether it is
    /// the one the fields wait for. An id-less reply is taken when it names
    /// the workspace and branch on show.
    pub(crate) fn on_workspace_reply(
        &mut self,
        workspace_id: &str,
        branch_name: &str,
        per_member: &[MemberSpawnPreview],
        request_id: Option<&str>,
    ) -> bool {
        let names_key = matches!(&self.key, Some(PreviewKey::Workspace { workspace_id: id, branch, .. })
            if id == workspace_id && branch == branch_name);
        self.take_reply(request_id, names_key, per_member)
    }

    fn take_reply(
        &mut self,
        request_id: Option<&str>,
        names_key: bool,
        members: &[MemberSpawnPreview],
    ) -> bool {
        let wanted = match request_id {
            Some(id) => self.pending.as_deref() == Some(id),
            None => self.pending.is_some() && names_key,
        };
        if !wanted {
            return false;
        }
        self.pending = None;
        self.members = members.to_vec();
        true
    }

    /// A daemon `Error` for request `request_id`; returns whether it failed
    /// the request the fields wait for, which is then no longer waited for.
    pub(crate) fn on_error(&mut self, request_id: &str) -> bool {
        if self.pending.as_deref() != Some(request_id) {
            return false;
        }
        self.pending = None;
        true
    }

    /// Each member's answer on show; empty while there is none.
    pub(crate) fn members(&self) -> &[MemberSpawnPreview] {
        &self.members
    }

    /// The first member whose worktree or branch is already there.
    pub(crate) fn collision(&self) -> Option<&MemberSpawnPreview> {
        self.members
            .iter()
            .find(|m| m.worktree_exists || m.branch_exists)
    }

    pub(crate) fn reuse(&self) -> ReuseChoice {
        self.reuse
    }

    pub(crate) fn choose(&mut self, choice: ReuseChoice) {
        self.reuse = choice;
    }

    /// What the spawn asks for: the choice while a collision shows, else
    /// Reuse.
    pub(crate) fn policy(&self) -> WorktreeReusePolicy {
        if self.collision().is_some() {
            self.reuse.policy()
        } else {
            WorktreeReusePolicy::Reuse
        }
    }
}

fn plural(count: u32) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// The warning for a base that trails its remote counterpart; `None` when
/// it is level, unknown, or has no remote counterpart.
pub(crate) fn staleness_notice(preview: &MemberSpawnPreview) -> Option<String> {
    let behind = preview.base_behind_remote.unwrap_or(0);
    let remote = preview.base_remote_ref.as_deref()?;
    if behind == 0 {
        return None;
    }
    let base = preview.effective_base.as_deref().unwrap_or_default();
    Some(format!(
        "{base} is {behind} commit{} behind {remote}. Branching from it forks from that older point.",
        plural(behind)
    ))
}

/// What the collision notice says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CollisionNotice {
    pub headline: String,
    /// The Reuse choice's label; its note is [`REUSE_NOTE`].
    pub reuse_label: &'static str,
    /// The note after [`RECREATE_LABEL`].
    pub recreate_note: &'static str,
    /// Recreating throws away uncommitted work: the note is a danger.
    pub danger: bool,
}

/// The notice for a worktree already at the spawn's path, or a branch left
/// without its worktree; `None` for neither.
pub(crate) fn collision_notice(preview: &MemberSpawnPreview) -> Option<CollisionNotice> {
    let branch_only = !preview.worktree_exists && preview.branch_exists;
    if !preview.worktree_exists && !branch_only {
        return None;
    }
    let (behind, head) = if branch_only {
        (
            preview.existing_branch_behind_base,
            &preview.existing_branch_head,
        )
    } else {
        (
            preview.existing_worktree_behind_base,
            &preview.existing_worktree_head,
        )
    };
    let what = if branch_only {
        "A branch with this name already exists (its worktree was deleted)"
    } else {
        "A worktree already exists at this path"
    };
    let at = head
        .as_deref()
        .filter(|head| !head.is_empty())
        .map_or_else(String::new, |head| format!(" at {head}"));
    let behind = behind.unwrap_or(0);
    let lag = if behind > 0 {
        let base = preview
            .resolved_base_ref
            .as_deref()
            .unwrap_or("the base branch");
        format!(", {behind} commit{} behind {base}", plural(behind))
    } else {
        String::new()
    };
    let headline = format!("{what}{at}{lag}.");
    let danger = !branch_only && preview.existing_worktree_dirty;
    let recreate_note = if danger {
        "(discards uncommitted changes in that worktree)"
    } else if branch_only {
        "(deletes the old branch and forks fresh)"
    } else {
        "(deletes and re-adds the worktree)"
    };
    Some(CollisionNotice {
        headline,
        reuse_label: if branch_only {
            "Attach it as-is"
        } else {
            "Reuse it as-is"
        },
        recreate_note,
        danger,
    })
}

/// How a badge in the workspace preview table is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Ok,
    Warn,
}

/// One member's row of the workspace preview table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberRow {
    pub repo: String,
    pub branch: String,
    pub badges: Vec<(String, Tone)>,
    pub path: String,
}

/// `member`'s row for a spawn on `branch`; a new branch with no base named
/// forks from `default_base`.
pub(crate) fn member_row(
    member: &MemberSpawnPreview,
    branch: &str,
    default_base: &str,
) -> MemberRow {
    let mut badges = vec![if member.branch_exists {
        ("reuse".to_owned(), Tone::Ok)
    } else {
        let base = member.effective_base.as_deref().unwrap_or(default_base);
        (format!("new from {base}"), Tone::Warn)
    }];
    if member.worktree_exists {
        let behind = member
            .existing_worktree_behind_base
            .filter(|behind| *behind > 0)
            .map_or_else(String::new, |behind| format!(" ({behind} behind)"));
        badges.push((format!("worktree exists{behind}"), Tone::Warn));
    }
    if let Some(behind) = member.base_behind_remote.filter(|behind| *behind > 0) {
        let remote = member.base_remote_ref.as_deref().unwrap_or_default();
        badges.push((format!("base {behind} behind {remote}"), Tone::Warn));
    }
    MemberRow {
        repo: member.repo_name.clone(),
        branch: branch.to_owned(),
        badges,
        path: member.worktree_path.clone(),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::time::{Duration, Instant};

    use protocol::{ClientMessage, MemberSpawnPreview, WorktreeReusePolicy};

    use super::{
        PREVIEW_DEBOUNCE, Preview, PreviewKey, ReuseChoice, Tone, collision_notice, member_row,
        staleness_notice,
    };

    fn member(repo_id: &str) -> MemberSpawnPreview {
        MemberSpawnPreview {
            repo_id: repo_id.to_owned(),
            repo_name: repo_id.to_uppercase(),
            branch_exists: false,
            effective_base: Some("main".to_owned()),
            worktree_path: format!("C:/wt/{repo_id}"),
            resolved_base_ref: Some("origin/main".to_owned()),
            base_remote_ref: Some("origin/main".to_owned()),
            base_behind_remote: None,
            worktree_exists: false,
            existing_worktree_head: None,
            existing_worktree_dirty: false,
            existing_worktree_behind_base: None,
            existing_branch_head: None,
            existing_branch_behind_base: None,
        }
    }

    fn repo_key(branch: &str, base: Option<&str>) -> PreviewKey {
        PreviewKey::Repo {
            repo_id: "r1".to_owned(),
            branch: branch.to_owned(),
            base: base.map(str::to_owned),
        }
    }

    #[test]
    fn repo_request_waits_out_the_debounce_from_the_last_change() {
        let start = Instant::now();
        let mut preview = Preview::default();
        preview.follow(Some(repo_key("a", None)), start);
        let later = start + Duration::from_millis(200);
        preview.follow(Some(repo_key("ab", None)), later);
        assert!(
            preview.take_due(start + PREVIEW_DEBOUNCE).is_none(),
            "restarted"
        );
        assert!(matches!(
            preview.take_due(later + PREVIEW_DEBOUNCE),
            Some(ClientMessage::PreviewSpawn { branch_name, base_branch: None, use_worktree: true, .. })
                if branch_name == "ab"
        ));
        assert!(
            preview.take_due(later + PREVIEW_DEBOUNCE).is_none(),
            "sent once"
        );
    }

    #[test]
    fn no_key_sends_nothing() {
        let now = Instant::now();
        let mut preview = Preview::default();
        preview.follow(None, now);
        assert_eq!(preview.due(), None);
        assert!(preview.take_due(now + PREVIEW_DEBOUNCE).is_none());
        assert!(preview.request().is_none(), "no workspace, no button");
    }

    #[test]
    fn reply_taken_only_for_the_request_in_flight() {
        let now = Instant::now();
        let mut preview = Preview::default();
        preview.follow(Some(repo_key("b", Some("main"))), now);
        assert!(
            !preview.on_repo_reply("r1", "b", &member("r1"), None),
            "nothing asked yet"
        );
        preview.take_due(now + PREVIEW_DEBOUNCE);
        preview.follow(Some(repo_key("b", Some("dev"))), now + PREVIEW_DEBOUNCE);
        assert!(
            !preview.on_repo_reply("r1", "b", &member("r1"), None),
            "the old base's reply"
        );
        preview.take_due(now + PREVIEW_DEBOUNCE * 2);
        assert!(!preview.on_repo_reply("r1", "other", &member("r1"), None));
        assert!(!preview.on_repo_reply("r0", "b", &member("r1"), None));
        assert!(preview.on_repo_reply("r1", "b", &member("r1"), None));
        assert_eq!(preview.members().len(), 1);
        assert!(
            !preview.on_repo_reply("r1", "b", &member("r1"), None),
            "one reply"
        );
    }

    #[test]
    fn change_clears_the_preview_and_resets_the_choice() {
        let now = Instant::now();
        let mut preview = Preview::default();
        preview.follow(Some(repo_key("b", None)), now);
        preview.take_due(now + PREVIEW_DEBOUNCE);
        let mut collides = member("r1");
        collides.worktree_exists = true;
        assert!(preview.on_repo_reply("r1", "b", &collides, None));
        preview.choose(ReuseChoice::Recreate);
        assert_eq!(preview.policy(), WorktreeReusePolicy::RecreateFromBase);
        preview.follow(Some(repo_key("b", None)), now);
        assert_eq!(preview.reuse(), ReuseChoice::Recreate, "no change, kept");
        preview.follow(Some(repo_key("c", None)), now);
        assert!(preview.members().is_empty());
        assert_eq!(preview.reuse(), ReuseChoice::Reuse);
        assert_eq!(preview.policy(), WorktreeReusePolicy::Reuse);
    }

    #[test]
    fn workspace_request_goes_on_the_button_only() {
        let now = Instant::now();
        let mut preview = Preview::default();
        let key = PreviewKey::Workspace {
            workspace_id: "w1".to_owned(),
            branch: "b".to_owned(),
            base: None,
        };
        preview.follow(Some(key), now);
        assert_eq!(preview.due(), None, "no debounce");
        assert!(preview.can_request());
        assert!(matches!(
            preview.request(),
            Some(ClientMessage::PreviewWorkspaceSpawn { workspace_id, .. }) if workspace_id == "w1"
        ));
        assert!(!preview.on_workspace_reply("w1", "c", &[member("r1")], None));
        assert!(preview.on_workspace_reply("w1", "b", &[member("r1"), member("r2")], None));
        assert_eq!(preview.members().len(), 2);
    }

    #[test]
    fn staleness_needs_a_remote_and_a_lag() {
        let mut preview = member("r1");
        assert_eq!(staleness_notice(&preview), None);
        preview.base_behind_remote = Some(1);
        assert_eq!(
            staleness_notice(&preview).as_deref(),
            Some(
                "main is 1 commit behind origin/main. Branching from it forks from that older point."
            )
        );
        preview.base_behind_remote = Some(3);
        assert!(staleness_notice(&preview).is_some_and(|text| text.contains("3 commits behind")));
        preview.base_remote_ref = None;
        assert_eq!(staleness_notice(&preview), None);
    }

    #[test]
    fn collision_texts_for_a_worktree_and_a_leftover_branch() {
        let mut preview = member("r1");
        assert_eq!(collision_notice(&preview), None);
        preview.worktree_exists = true;
        preview.existing_worktree_head = Some("abc1234".to_owned());
        preview.existing_worktree_behind_base = Some(2);
        let notice = collision_notice(&preview).expect("a worktree collides");
        assert_eq!(
            notice.headline,
            "A worktree already exists at this path at abc1234, 2 commits behind origin/main."
        );
        assert_eq!(notice.reuse_label, "Reuse it as-is");
        assert_eq!(notice.recreate_note, "(deletes and re-adds the worktree)");
        assert!(!notice.danger);
        preview.existing_worktree_dirty = true;
        let notice = collision_notice(&preview).expect("a worktree collides");
        assert!(notice.danger);
        assert_eq!(
            notice.recreate_note,
            "(discards uncommitted changes in that worktree)"
        );

        let mut leftover = member("r1");
        leftover.branch_exists = true;
        leftover.existing_worktree_dirty = true;
        leftover.resolved_base_ref = None;
        leftover.existing_branch_behind_base = Some(1);
        let notice = collision_notice(&leftover).expect("a branch collides");
        assert_eq!(
            notice.headline,
            "A branch with this name already exists (its worktree was deleted), 1 commit behind the base branch."
        );
        assert_eq!(notice.reuse_label, "Attach it as-is");
        assert_eq!(
            notice.recreate_note,
            "(deletes the old branch and forks fresh)"
        );
        assert!(!notice.danger, "no worktree, nothing to discard");
    }

    #[test]
    fn member_rows_follow_the_tauri_badges() {
        let mut fresh = member("r1");
        fresh.effective_base = None;
        fresh.base_behind_remote = Some(4);
        let row = member_row(&fresh, "wt/b", "origin/dev");
        assert_eq!(row.repo, "R1");
        assert_eq!(row.branch, "wt/b");
        assert_eq!(row.path, "C:/wt/r1");
        assert_eq!(
            row.badges,
            [
                ("new from origin/dev".to_owned(), Tone::Warn),
                ("base 4 behind origin/main".to_owned(), Tone::Warn)
            ]
        );
        let mut reused = member("r2");
        reused.branch_exists = true;
        reused.worktree_exists = true;
        reused.existing_worktree_behind_base = Some(0);
        assert_eq!(
            member_row(&reused, "wt/b", "main").badges,
            [
                ("reuse".to_owned(), Tone::Ok),
                ("worktree exists".to_owned(), Tone::Warn)
            ]
        );
        reused.existing_worktree_behind_base = Some(5);
        assert_eq!(
            member_row(&reused, "wt/b", "main").badges[1].0,
            "worktree exists (5 behind)"
        );
    }
}
