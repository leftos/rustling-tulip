//! The diff tabs' state, without the UI: the `OpenDiffTab` requests this
//! client sent and the tab one of them waits to activate, and per tab its
//! snapshot fetch, the texts it holds, the build generation and the
//! live-refresh flag. [`crate::diff_tab_view`] renders it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{ClientMessage, SnapshotUnavailable};

use crate::source_control::ScKey;

/// The toast an `OpenDiffTab` the daemon refused raises.
pub const OPEN_FAILED_TITLE: &str = "Couldn't open the diff";
/// How long a tab the daemon named waits for its `TabUpdated` before the
/// activation is dropped.
pub const PENDING_ACTIVATE_TTL: Duration = Duration::from_secs(4);
/// The body while the snapshot or its first diff is on its way.
pub const LOADING_TEXT: &str = "loading…";
/// The body of a diff with no changes.
pub const EMPTY_TEXT: &str = "No changes";
/// Under [`EMPTY_TEXT`] when only the line endings differ.
pub const LINE_ENDINGS_NOTE: &str = "Only line endings differ";
/// Under [`EMPTY_TEXT`] when only the final newline differs.
pub const FINAL_NEWLINE_NOTE: &str = "Only the final newline differs";
/// The body when a side is binary.
pub const BINARY_TEXT: &str = "Binary file, not shown";
/// The body when the daemon gives a reason this build does not know.
pub const UNKNOWN_UNAVAILABLE_TEXT: &str = "Cannot display this file";
/// The whitespace toggle's label.
pub const WHITESPACE_LABEL: &str = "include whitespace";
/// The whitespace toggle's tooltip.
pub const WHITESPACE_TIP: &str = "When unchecked, whitespace-only changes are hidden from the diff";
/// The syntax highlighting toggle's label.
pub const HIGHLIGHT_LABEL: &str = "highlight";
/// The syntax highlighting toggle's tooltip.
pub const HIGHLIGHT_TIP: &str = "Colour code by its language";
/// What the body says before the reason a snapshot read failed.
const ERROR_PREFIX: &str = "Could not load diff: ";
const MIB: u64 = 1024 * 1024;

/// The ids of the diff requests, and the `OpenDiffTab`s out.
#[derive(Debug, Default)]
pub(crate) struct DiffOpens {
    next_open: u64,
    next_snapshot: u64,
    /// The `OpenDiffTab` ids the daemon has not answered.
    sent: HashSet<String>,
    /// A tab the daemon named before its `TabUpdated` arrived, and when.
    pending: Option<(String, Instant)>,
}

impl DiffOpens {
    /// A fresh `OpenDiffTab` id, recorded as sent.
    pub(crate) fn open_id(&mut self) -> String {
        self.next_open += 1;
        let id = format!("diff-open-{}", self.next_open);
        self.sent.insert(id.clone());
        id
    }

    /// A fresh `GetFileSnapshot` id.
    pub(crate) fn snapshot_id(&mut self) -> String {
        self.next_snapshot += 1;
        format!("diff-snap-{}", self.next_snapshot)
    }

    /// `DiffTabOpened`: the tab to activate now, when this client asked for
    /// it and the tab list already holds it (`known`). A tab the list does
    /// not hold yet waits for its `TabUpdated`; another client's open is
    /// ignored.
    pub(crate) fn opened(
        &mut self,
        id: &str,
        tab_id: &str,
        known: bool,
        now: Instant,
    ) -> Option<String> {
        if !self.sent.remove(id) {
            return None;
        }
        if known {
            self.pending = None;
            return Some(tab_id.to_owned());
        }
        self.pending = Some((tab_id.to_owned(), now));
        None
    }

    /// The waiting tab, once `known` says the tab list holds it; one that
    /// waited [`PENDING_ACTIVATE_TTL`] is dropped instead.
    pub(crate) fn take_ready(
        &mut self,
        known: impl Fn(&str) -> bool,
        now: Instant,
    ) -> Option<String> {
        let (tab_id, at) = self.pending.as_ref()?;
        if now.saturating_duration_since(*at) >= PENDING_ACTIVATE_TTL {
            self.pending = None;
            return None;
        }
        if !known(tab_id) {
            return None;
        }
        self.pending.take().map(|(tab_id, _)| tab_id)
    }

    /// Whether `id` is an `OpenDiffTab` of this client, which is forgotten.
    pub(crate) fn failed(&mut self, id: &str) -> bool {
        self.sent.remove(id)
    }

    /// A new connection: no answer to an old request will come.
    pub(crate) fn reset(&mut self) {
        self.sent.clear();
        self.pending = None;
    }
}

/// What a diff tab compares: its tab's key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffTarget {
    pub repo_id: String,
    pub path: String,
    /// `None` for the worktree against the index, `"HEAD"` for the index
    /// against HEAD, a sha for that commit against its parent.
    pub against: Option<String>,
    pub worktree_path: Option<String>,
}

impl DiffTarget {
    /// The source-control section whose statuses refresh the tab.
    fn key(&self) -> ScKey {
        ScKey {
            repo_id: self.repo_id.clone(),
            worktree: self.worktree_path.clone(),
        }
    }

    /// Whether the sides can change under the tab: a commit's cannot.
    fn is_live(&self) -> bool {
        self.against
            .as_deref()
            .is_none_or(|against| against == "HEAD")
    }
}

/// A `FileSnapshot` reply's content.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub old: String,
    pub new: String,
    pub language: String,
    pub unavailable: Option<SnapshotUnavailable>,
}

/// What a tab holds from its last landed reply.
#[derive(Debug, Clone)]
enum Content {
    Loading,
    Failed(String),
    Unavailable(SnapshotUnavailable),
    Texts { old: Arc<str>, new: Arc<str> },
}

/// What the last accepted build found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Built {
    pub changes: usize,
    pub line_endings: bool,
    pub final_newline: bool,
}

/// What a landed reply asks of the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Landed {
    /// New texts to build a diff from.
    pub build: bool,
    /// A refresh came in while the fetch was out: fetch again.
    pub refetch: bool,
}

/// What a diff tab's body shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffTabBody {
    /// A line of text in place of the diff, and a muted line under it.
    Text {
        text: String,
        note: Option<&'static str>,
    },
    /// The side-by-side diff.
    Diff,
}

/// A diff tab's header, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffTabHeader {
    pub path: String,
    /// `worktree vs index`, `index vs HEAD` or `@ <short sha>`.
    pub mode: String,
    /// The full sha, for a commit's diff.
    pub mode_tip: Option<String>,
    pub include_whitespace: bool,
    /// Whether the diff is coloured by its language.
    pub highlight: bool,
    /// `…`,`no changes`, `1 change` or `N changes`; empty when the body
    /// is an error or a file that is not shown.
    pub count: String,
    /// Whether the change buttons take clicks.
    pub nav_enabled: bool,
}

/// One diff tab's fetch and build state.
#[derive(Debug)]
pub(crate) struct DiffTabState {
    target: DiffTarget,
    content: Content,
    /// The snapshot's language, kept for syntax highlighting.
    language: Option<String>,
    /// The `GetFileSnapshot` out, by id.
    fetching: Option<String>,
    /// A refresh arrived while a fetch was out.
    dirty: bool,
    /// Numbers the builds; only the latest one's result is taken.
    generation: u64,
    built: Option<Built>,
}

impl DiffTabState {
    pub(crate) fn new(target: DiffTarget) -> Self {
        Self {
            target,
            content: Content::Loading,
            language: None,
            fetching: None,
            dirty: false,
            generation: 0,
            built: None,
        }
    }

    /// The snapshot's language hint, once one landed.
    pub(crate) fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    /// The (old, new) texts of the last snapshot, while the tab holds them.
    pub(crate) fn texts(&self) -> Option<(Arc<str>, Arc<str>)> {
        match &self.content {
            Content::Texts { old, new } => Some((old.clone(), new.clone())),
            Content::Loading | Content::Failed(_) | Content::Unavailable(_) => None,
        }
    }

    /// Whether `id` is the fetch out.
    pub(crate) fn awaits(&self, id: &str) -> bool {
        self.fetching.as_deref() == Some(id)
    }

    /// Records `id` as the fetch out and returns its request.
    pub(crate) fn start_fetch(&mut self, id: String) -> ClientMessage {
        self.fetching = Some(id.clone());
        self.dirty = false;
        ClientMessage::GetFileSnapshot {
            id,
            repo_id: self.target.repo_id.clone(),
            path: self.target.path.clone(),
            against: self.target.against.clone(),
            worktree_path: self.target.worktree_path.clone(),
        }
    }

    /// A status for `key` arrived: whether to fetch now. A commit's tab and
    /// another section's status ask nothing; one arriving while a fetch is
    /// out is held for when that fetch lands.
    pub(crate) fn on_repo_status(&mut self, key: &ScKey) -> bool {
        if !self.target.is_live() || self.target.key() != *key {
            return false;
        }
        if self.fetching.is_some() {
            self.dirty = true;
            return false;
        }
        true
    }

    /// The fetch `id` answered with `snapshot`; `None` for a stale id.
    pub(crate) fn on_snapshot(&mut self, id: &str, snapshot: Snapshot) -> Option<Landed> {
        self.land(id)?;
        self.language = Some(snapshot.language);
        let build = if let Some(reason) = snapshot.unavailable {
            self.replace_content(Content::Unavailable(reason));
            false
        } else {
            self.content = Content::Texts {
                old: snapshot.old.into(),
                new: snapshot.new.into(),
            };
            true
        };
        Some(Landed {
            build,
            refetch: std::mem::take(&mut self.dirty),
        })
    }

    /// The fetch `id` failed with `error`; `None` for a stale id.
    pub(crate) fn on_snapshot_error(&mut self, id: &str, error: String) -> Option<Landed> {
        self.land(id)?;
        self.replace_content(Content::Failed(error));
        Some(Landed {
            build: false,
            refetch: std::mem::take(&mut self.dirty),
        })
    }

    /// Takes the fetch `id` as landed, when it is the one out.
    fn land(&mut self, id: &str) -> Option<()> {
        if self.fetching.as_deref() != Some(id) {
            return None;
        }
        self.fetching = None;
        Some(())
    }

    /// Content with no diff: a build on its way is discarded and the old
    /// diff goes.
    fn replace_content(&mut self, content: Content) {
        self.content = content;
        self.generation += 1;
        self.built = None;
    }

    /// The texts for a new build and its generation; any build before it
    /// is discarded when it lands. `None` without texts.
    pub(crate) fn build_input(&mut self) -> Option<(Arc<str>, Arc<str>, u64)> {
        let Content::Texts { old, new } = &self.content else {
            return None;
        };
        let texts = (old.clone(), new.clone());
        self.generation += 1;
        Some((texts.0, texts.1, self.generation))
    }

    /// A build of `generation` finished with `built`: whether it is the
    /// latest, so the view takes it.
    pub(crate) fn accept_build(&mut self, generation: u64, built: Built) -> bool {
        if generation != self.generation {
            return false;
        }
        self.built = Some(built);
        true
    }

    /// A new connection: a fetch out will not be answered. Returns whether
    /// to fetch again: when one was out, or when the sides can have changed.
    pub(crate) fn reset_connection(&mut self) -> bool {
        let was_fetching = self.fetching.take().is_some();
        self.dirty = false;
        was_fetching || self.target.is_live()
    }

    pub(crate) fn body(&self) -> DiffTabBody {
        let text = |text: String| DiffTabBody::Text { text, note: None };
        match &self.content {
            Content::Loading => text(LOADING_TEXT.to_owned()),
            Content::Failed(error) => text(format!("{ERROR_PREFIX}{error}")),
            Content::Unavailable(reason) => text(unavailable_text(reason)),
            Content::Texts { .. } => match self.built {
                None => text(LOADING_TEXT.to_owned()),
                Some(built) if built.changes == 0 => DiffTabBody::Text {
                    text: EMPTY_TEXT.to_owned(),
                    note: empty_note(built),
                },
                Some(_) => DiffTabBody::Diff,
            },
        }
    }

    pub(crate) fn header(&self, include_whitespace: bool, highlight: bool) -> DiffTabHeader {
        let (mode, mode_tip) = mode_label(self.target.against.as_deref());
        let count = match self.content {
            Content::Failed(_) | Content::Unavailable(_) => String::new(),
            Content::Loading | Content::Texts { .. } => {
                count_text(self.built.map(|built| built.changes))
            }
        };
        DiffTabHeader {
            path: self.target.path.clone(),
            mode,
            mode_tip,
            include_whitespace,
            highlight,
            count,
            nav_enabled: self.built.is_some_and(|built| built.changes > 0),
        }
    }
}

/// The header's mode for `against`, and the tooltip a commit's carries.
pub(crate) fn mode_label(against: Option<&str>) -> (String, Option<String>) {
    match against {
        None => ("worktree vs index".to_owned(), None),
        Some("HEAD") => ("index vs HEAD".to_owned(), None),
        Some(sha) => {
            let short: String = sha.chars().take(7).collect();
            (format!("@ {short}"), Some(sha.to_owned()))
        }
    }
}

/// The header's change count; `None` while no diff is built.
pub(crate) fn count_text(changes: Option<usize>) -> String {
    match changes {
        None => "…".to_owned(),
        Some(0) => "no changes".to_owned(),
        Some(1) => "1 change".to_owned(),
        Some(n) => format!("{n} changes"),
    }
}

/// The body for a file the daemon would not ship.
pub(crate) fn unavailable_text(reason: &SnapshotUnavailable) -> String {
    match reason {
        SnapshotUnavailable::Binary => BINARY_TEXT.to_owned(),
        SnapshotUnavailable::TooLarge { bytes, limit } => format!(
            "File too large to diff ({} MiB, limit {} MiB)",
            mib_one_decimal(*bytes),
            mib_limit(*limit)
        ),
        SnapshotUnavailable::Unknown => UNKNOWN_UNAVAILABLE_TEXT.to_owned(),
    }
}

/// `bytes` in MiB with one decimal.
fn mib_one_decimal(bytes: u64) -> String {
    let tenths = (u128::from(bytes) * 10 + u128::from(MIB) / 2) / u128::from(MIB);
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// A limit in MiB: whole when it is whole, else with one decimal.
fn mib_limit(bytes: u64) -> String {
    if bytes.is_multiple_of(MIB) {
        (bytes / MIB).to_string()
    } else {
        mib_one_decimal(bytes)
    }
}

/// The note under an empty diff: the line endings first, then the final
/// newline.
fn empty_note(built: Built) -> Option<&'static str> {
    if built.line_endings {
        Some(LINE_ENDINGS_NOTE)
    } else if built.final_newline {
        Some(FINAL_NEWLINE_NOTE)
    } else {
        None
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::sidebar::UiState;

    fn target(against: Option<&str>) -> DiffTarget {
        DiffTarget {
            repo_id: "r1".to_owned(),
            path: "src/a.rs".to_owned(),
            against: against.map(str::to_owned),
            worktree_path: Some("C:/wt/r1".to_owned()),
        }
    }

    fn key(worktree: Option<&str>) -> ScKey {
        ScKey {
            repo_id: "r1".to_owned(),
            worktree: worktree.map(str::to_owned),
        }
    }

    fn texts(old: &str, new: &str) -> Snapshot {
        Snapshot {
            old: old.to_owned(),
            new: new.to_owned(),
            language: "rust".to_owned(),
            unavailable: None,
        }
    }

    fn built(changes: usize, line_endings: bool, final_newline: bool) -> Built {
        Built {
            changes,
            line_endings,
            final_newline,
        }
    }

    fn text(body: &DiffTabBody) -> (&str, Option<&'static str>) {
        match body {
            DiffTabBody::Text { text, note } => (text, *note),
            DiffTabBody::Diff => ("<diff>", None),
        }
    }

    #[test]
    fn an_open_answered_after_its_tab_arrived_activates_at_once() {
        let mut opens = DiffOpens::default();
        let now = Instant::now();
        let id = opens.open_id();
        assert_eq!(id, "diff-open-1");
        assert_eq!(opens.opened(&id, "t9", true, now).as_deref(), Some("t9"));
        assert_eq!(opens.take_ready(|_| true, now), None, "nothing waits");
        assert!(!opens.failed(&id), "the answered id is forgotten");
    }

    #[test]
    fn an_open_answered_before_its_tab_arrived_waits_for_it() {
        let mut opens = DiffOpens::default();
        let now = Instant::now();
        let id = opens.open_id();
        assert_eq!(opens.opened(&id, "t9", false, now), None);
        assert_eq!(
            opens.take_ready(|tab| tab == "t1", now),
            None,
            "t9 not listed yet"
        );
        assert_eq!(
            opens.take_ready(|tab| tab == "t9", now).as_deref(),
            Some("t9")
        );
        assert_eq!(opens.take_ready(|_| true, now), None, "activated once");
    }

    #[test]
    fn another_clients_open_is_ignored() {
        let mut opens = DiffOpens::default();
        let now = Instant::now();
        assert_eq!(opens.opened("elsewhere-1", "t9", true, now), None);
        assert_eq!(opens.opened("elsewhere-2", "t9", false, now), None);
        assert_eq!(opens.take_ready(|_| true, now), None);
    }

    #[test]
    fn a_waiting_activation_is_dropped_after_four_seconds_or_on_reset() {
        let mut opens = DiffOpens::default();
        let now = Instant::now();
        let id = opens.open_id();
        opens.opened(&id, "t9", false, now);
        let almost = now + PENDING_ACTIVATE_TTL.saturating_sub(Duration::from_millis(1));
        assert_eq!(opens.take_ready(|_| false, almost), None);
        assert_eq!(
            opens.take_ready(|_| false, now + PENDING_ACTIVATE_TTL),
            None
        );
        assert_eq!(
            opens.take_ready(|_| true, now + PENDING_ACTIVATE_TTL),
            None,
            "dropped, so the late tab stays where it is"
        );

        let id = opens.open_id();
        opens.opened(&id, "t8", false, now);
        opens.reset();
        assert_eq!(opens.take_ready(|_| true, now), None);
    }

    #[test]
    fn a_failed_open_is_known_once() {
        let mut opens = DiffOpens::default();
        let id = opens.open_id();
        assert!(opens.failed(&id));
        assert!(!opens.failed(&id));
        assert!(!opens.failed("diff-open-99"));
    }

    #[test]
    fn snapshot_ids_count_up() {
        let mut opens = DiffOpens::default();
        assert_eq!(opens.snapshot_id(), "diff-snap-1");
        assert_eq!(opens.snapshot_id(), "diff-snap-2");
    }

    #[test]
    fn a_stale_snapshot_id_is_ignored() {
        let mut state = DiffTabState::new(target(None));
        state.start_fetch("diff-snap-1".to_owned());
        state.start_fetch("diff-snap-2".to_owned());
        assert_eq!(state.on_snapshot("diff-snap-1", texts("a", "b")), None);
        assert_eq!(
            state.on_snapshot_error("diff-snap-1", "gone".to_owned()),
            None
        );
        assert_eq!(text(&state.body()).0, LOADING_TEXT);
        let landed = state.on_snapshot("diff-snap-2", texts("a", "b"));
        assert_eq!(
            landed,
            Some(Landed {
                build: true,
                refetch: false
            })
        );
        assert_eq!(state.language(), Some("rust"));
        assert_eq!(state.on_snapshot("diff-snap-2", texts("a", "b")), None);
    }

    #[test]
    fn the_mode_label_names_the_sides() {
        assert_eq!(mode_label(None), ("worktree vs index".to_owned(), None));
        assert_eq!(mode_label(Some("HEAD")), ("index vs HEAD".to_owned(), None));
        let sha = "0a1b2c3d4e5f60718293a4b5c6d7e8f901234567";
        assert_eq!(
            mode_label(Some(sha)),
            ("@ 0a1b2c3".to_owned(), Some(sha.to_owned()))
        );
    }

    #[test]
    fn the_count_text() {
        assert_eq!(count_text(None), "…");
        assert_eq!(count_text(Some(0)), "no changes");
        assert_eq!(count_text(Some(1)), "1 change");
        assert_eq!(count_text(Some(12)), "12 changes");
    }

    #[test]
    fn the_unavailable_texts() {
        assert_eq!(
            unavailable_text(&SnapshotUnavailable::Binary),
            "Binary file, not shown"
        );
        assert_eq!(
            unavailable_text(&SnapshotUnavailable::TooLarge {
                bytes: 3 * MIB + MIB / 2,
                limit: 2 * MIB,
            }),
            "File too large to diff (3.5 MiB, limit 2 MiB)"
        );
        assert_eq!(
            unavailable_text(&SnapshotUnavailable::TooLarge {
                bytes: 2 * MIB + 1,
                limit: MIB + MIB / 2,
            }),
            "File too large to diff (2.0 MiB, limit 1.5 MiB)"
        );
        assert_eq!(
            unavailable_text(&SnapshotUnavailable::Unknown),
            UNKNOWN_UNAVAILABLE_TEXT
        );
    }

    #[test]
    fn an_empty_diff_notes_what_alone_differs() {
        let mut state = DiffTabState::new(target(None));
        state.start_fetch("s".to_owned());
        state.on_snapshot("s", texts("a\n", "a\r\n"));
        let (_, _, generation) = state.build_input().expect("texts to build");
        assert_eq!(text(&state.body()).0, LOADING_TEXT, "not built yet");
        assert_eq!(state.header(true, true).count, "…");
        assert!(state.accept_build(generation, built(0, true, false)));
        assert_eq!(text(&state.body()), (EMPTY_TEXT, Some(LINE_ENDINGS_NOTE)));
        assert_eq!(state.header(true, true).count, "no changes");
        assert!(!state.header(true, true).nav_enabled);
        assert!(state.accept_build(generation, built(0, false, true)));
        assert_eq!(text(&state.body()), (EMPTY_TEXT, Some(FINAL_NEWLINE_NOTE)));
        assert!(state.accept_build(generation, built(0, false, false)));
        assert_eq!(text(&state.body()), (EMPTY_TEXT, None));
        assert!(state.accept_build(generation, built(2, false, false)));
        assert_eq!(state.body(), DiffTabBody::Diff);
        assert!(state.header(true, true).nav_enabled);
    }

    #[test]
    fn an_older_build_is_discarded() {
        let mut state = DiffTabState::new(target(None));
        state.start_fetch("s".to_owned());
        state.on_snapshot("s", texts("a", "b"));
        let (_, _, first) = state.build_input().expect("texts");
        let (_, _, second) = state.build_input().expect("texts");
        assert!(!state.accept_build(first, built(1, false, false)));
        assert!(state.accept_build(second, built(1, false, false)));
    }

    #[test]
    fn two_refreshes_during_a_fetch_make_one_refetch() {
        let mut state = DiffTabState::new(target(None));
        state.start_fetch("s1".to_owned());
        assert!(!state.on_repo_status(&key(Some("C:/wt/r1"))));
        assert!(!state.on_repo_status(&key(Some("C:/wt/r1"))));
        let landed = state.on_snapshot("s1", texts("a", "b"));
        assert_eq!(landed.map(|l| l.refetch), Some(true), "one refetch");
        state.start_fetch("s2".to_owned());
        let landed = state.on_snapshot("s2", texts("a", "c"));
        assert_eq!(landed.map(|l| l.refetch), Some(false), "and only one");
        assert!(
            state.on_repo_status(&key(Some("C:/wt/r1"))),
            "idle: at once"
        );
        assert!(!state.on_repo_status(&key(None)), "another tree's status");
    }

    #[test]
    fn a_commits_tab_ignores_statuses() {
        let mut state = DiffTabState::new(target(Some("0a1b2c3d4e5f")));
        assert!(!state.on_repo_status(&key(Some("C:/wt/r1"))));
        state.start_fetch("s1".to_owned());
        assert!(!state.on_repo_status(&key(Some("C:/wt/r1"))));
        let landed = state.on_snapshot("s1", texts("a", "b"));
        assert_eq!(landed.map(|l| l.refetch), Some(false));
        assert!(!state.reset_connection(), "a landed commit diff stays");

        let mut staged = DiffTabState::new(target(Some("HEAD")));
        assert!(staged.on_repo_status(&key(Some("C:/wt/r1"))));
    }

    #[test]
    fn a_failure_or_a_file_not_shown_drops_the_old_diff() {
        let mut state = DiffTabState::new(target(None));
        state.start_fetch("s1".to_owned());
        state.on_snapshot("s1", texts("a", "b"));
        let (_, _, generation) = state.build_input().expect("texts");
        state.accept_build(generation, built(1, false, false));
        state.start_fetch("s2".to_owned());
        state.on_snapshot_error("s2", "no such path".to_owned());
        assert_eq!(text(&state.body()).0, "Could not load diff: no such path");
        assert_eq!(state.header(true, true).count, "");
        assert!(!state.accept_build(generation, built(1, false, false)));

        state.start_fetch("s3".to_owned());
        let mut binary = texts("", "");
        binary.unavailable = Some(SnapshotUnavailable::Binary);
        let landed = state.on_snapshot("s3", binary);
        assert_eq!(landed.map(|l| l.build), Some(false));
        assert_eq!(text(&state.body()).0, BINARY_TEXT);
        assert!(state.build_input().is_none());
    }

    #[test]
    fn a_reconnect_refetches_a_fetch_out_or_a_live_tab() {
        let mut live = DiffTabState::new(target(None));
        assert!(live.reset_connection());
        let mut commit = DiffTabState::new(target(Some("0a1b2c3")));
        commit.start_fetch("s1".to_owned());
        assert!(commit.reset_connection(), "its fetch will not be answered");
        assert_eq!(commit.on_snapshot("s1", texts("a", "b")), None);
    }

    #[test]
    fn a_layout_without_the_whitespace_setting_includes_whitespace() {
        let old: UiState = serde_json::from_str(r#"{"sidebar_width": 200.0}"#).expect("old layout");
        assert!(old.diff_include_whitespace);
        let saved: UiState =
            serde_json::from_str(r#"{"diff_include_whitespace": false}"#).expect("layout");
        assert!(!saved.diff_include_whitespace);
        assert!(UiState::default().diff_include_whitespace);
    }

    #[test]
    fn an_empty_layout_includes_whitespace_and_highlights() {
        let empty: UiState = serde_json::from_str("{}").expect("an empty layout");
        assert!(empty.diff_include_whitespace);
        assert!(empty.diff_highlight);
    }
}
