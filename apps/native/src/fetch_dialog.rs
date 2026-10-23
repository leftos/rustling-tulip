//! The Fetch file… dialog's state on a remote host: the repo or worktree it
//! fetches from, how the typed path is read, the keyboard focus ring, the
//! fetch's phases and the progress it shows. Plain Rust, so every rule is
//! unit-tested; `fetch_dialog_view` renders it and forwards input, and the
//! fetch goes through [`crate::RootView`]'s file fetch.

use crate::fetch_link::{self, Root, RootStrip};
use crate::file_fetch::{FetchRequest, display_host_path};
use crate::links::split_path_position;
use crate::spawn_form::human_size;

/// A focusable control of the Fetch file… dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchControl {
    Close,
    /// The button choosing the root at this index.
    Scope(usize),
    Path,
    Cancel,
    Submit,
}

impl FetchControl {
    /// The debug selector the control's element carries.
    pub(crate) fn selector(self) -> String {
        match self {
            Self::Close => "fetch-close".to_owned(),
            Self::Scope(index) => format!("fetch-scope-{index}"),
            Self::Path => "fetch-path".to_owned(),
            Self::Cancel => "fetch-cancel".to_owned(),
            Self::Submit => "fetch-submit".to_owned(),
        }
    }
}

/// The path the field shows as an example while it is empty.
pub(crate) const PLACEHOLDER: &str = "src/main.rs:42";

/// What the dialog says when the session has no repo folder to fetch from.
pub(crate) const NO_REPO: &str = "No repo for this session";

/// A typed path read against the chosen root: the file relative to it, and
/// the line and column to open it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchTarget {
    pub rel_path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// Why a typed path cannot be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathProblem {
    Empty,
    ParentSegment,
    /// An absolute path not below the root: the path as typed, and the root
    /// as the host shows it.
    OutsideRoot {
        path: String,
        root: String,
    },
}

impl PathProblem {
    /// What the dialog says under the field.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Empty => "Enter a path.".to_owned(),
            Self::ParentSegment => "A path can't contain \"..\".".to_owned(),
            Self::OutsideRoot { path, root } => {
                format!("{path} is outside {root}. Give a path under it.")
            }
        }
    }
}

/// `text` as a file under `root`: a trailing `:line[:col]` split off, an
/// absolute path below the root made relative, `\` read as `/` and `.`
/// segments dropped.
///
/// # Errors
/// The [`PathProblem`] that refuses it.
pub(crate) fn parse_target(root: &Root, text: &str) -> Result<FetchTarget, PathProblem> {
    let position = split_path_position(text.trim());
    let path = position.path.trim();
    if path.is_empty() {
        return Err(PathProblem::Empty);
    }
    if path.split(['/', '\\']).any(|segment| segment == "..") {
        return Err(PathProblem::ParentSegment);
    }
    let relative = match fetch_link::strip_root(&root.path, path) {
        RootStrip::NotAbsolute => path.to_owned(),
        RootStrip::Inside(rel) => rel,
        RootStrip::Outside => {
            return Err(PathProblem::OutsideRoot {
                path: path.to_owned(),
                root: display_host_path(&root.path),
            });
        }
    };
    let rel_path = relative
        .split(['/', '\\'])
        .filter(|segment| *segment != ".")
        .collect::<Vec<_>>()
        .join("/");
    if rel_path.is_empty() {
        return Err(PathProblem::Empty);
    }
    Ok(FetchTarget {
        rel_path,
        line: position.line,
        column: position.column,
    })
}

/// Where the dialog's fetch is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// The path is being typed.
    Editing,
    /// Fetch was pressed and the fetch is being asked for.
    Starting,
    /// The fetch `id` is in flight.
    Fetching { id: String },
}

/// The Fetch file… dialog's state. The path itself lives in the dialog's
/// text field, and every rule here reads the text it is given.
#[derive(Debug)]
pub(crate) struct FetchForm {
    /// The session's repos and worktrees, read once when the dialog opened.
    roots: Vec<Root>,
    scope: usize,
    focus: FetchControl,
    phase: Phase,
    error: Option<String>,
}

impl FetchForm {
    /// The dialog over `roots`, scoped to the one holding `cwd`, else the
    /// first; the path field starts with the keyboard.
    pub(crate) fn new(roots: Vec<Root>, cwd: Option<&str>) -> Self {
        let scope = fetch_link::root_holding(&roots, cwd).unwrap_or(0);
        Self {
            roots,
            scope,
            focus: FetchControl::Path,
            phase: Phase::Editing,
            error: None,
        }
    }

    pub(crate) fn roots(&self) -> &[Root] {
        &self.roots
    }

    /// The root the path is read against; `None` when the session has none.
    pub(crate) fn scope(&self) -> Option<&Root> {
        self.roots.get(self.scope)
    }

    /// Chooses the root at `index`, when there is one.
    pub(crate) fn select_scope(&mut self, index: usize) {
        if index < self.roots.len() {
            self.scope = index;
        }
    }

    /// Left (`forward` false) or Right on the scope row: the previous or
    /// next root, wrapping, which takes the focus with it.
    pub(crate) fn step_scope(&mut self, forward: bool) {
        let len = self.roots.len();
        if len == 0 {
            return;
        }
        self.scope = if forward {
            (self.scope + 1) % len
        } else {
            (self.scope + len - 1) % len
        };
        self.focus = FetchControl::Scope(self.scope);
    }

    /// Whether Fetch can go with the field holding `text`.
    pub(crate) fn can_submit(&self, text: &str) -> bool {
        self.phase == Phase::Editing
            && self
                .scope()
                .is_some_and(|root| parse_target(root, text).is_ok())
    }

    /// The focusable controls, in the order they are drawn: the scope row
    /// only for several roots, Fetch only while it can go.
    pub(crate) fn controls(&self, text: &str) -> Vec<FetchControl> {
        let mut ring = vec![FetchControl::Close];
        if self.roots.len() > 1 {
            ring.extend((0..self.roots.len()).map(FetchControl::Scope));
        }
        ring.push(FetchControl::Path);
        ring.push(FetchControl::Cancel);
        if self.can_submit(text) {
            ring.push(FetchControl::Submit);
        }
        ring
    }

    /// The focused control; one that is gone gives way to the first.
    pub(crate) fn focused(&self, text: &str) -> FetchControl {
        if self.controls(text).contains(&self.focus) {
            self.focus
        } else {
            FetchControl::Close
        }
    }

    pub(crate) fn set_focus(&mut self, control: FetchControl) {
        self.focus = control;
    }

    /// Tab (`forward`) or Shift+Tab: the next or previous control, wrapping.
    pub(crate) fn move_focus(&mut self, forward: bool, text: &str) {
        let ring = self.controls(text);
        let at = ring
            .iter()
            .position(|c| *c == self.focused(text))
            .unwrap_or(0);
        let len = ring.len();
        let next = if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        if let Some(control) = ring.into_iter().nth(next) {
            self.focus = control;
        }
    }

    /// Fetch: the request for `text` under the chosen root, once, which
    /// starts the fetch. A path that cannot be fetched says why under the
    /// field instead; `None` then, with no root, and while a fetch is on.
    pub(crate) fn submit(&mut self, text: &str) -> Option<FetchRequest> {
        if self.phase != Phase::Editing {
            return None;
        }
        let root = self.scope()?;
        let target = match parse_target(root, text) {
            Ok(target) => target,
            Err(problem) => {
                self.error = Some(problem.message());
                return None;
            }
        };
        let request = FetchRequest {
            repo_id: root.repo_id.clone(),
            repo_name: root.repo_name.clone(),
            worktree_path: Some(root.path.clone()),
            rel_path: target.rel_path,
            line: target.line,
            column: target.column,
            open_when_done: true,
        };
        self.phase = Phase::Starting;
        self.error = None;
        // Fetch leaves the ring while the fetch runs; the keyboard goes to
        // the field, where Enter starts nothing, not to ✕, which cancels.
        self.focus = FetchControl::Path;
        Some(request)
    }

    /// The fetch asked for went out as `id`.
    pub(crate) fn started(&mut self, id: String) {
        if self.phase == Phase::Starting {
            self.phase = Phase::Fetching { id };
        }
    }

    /// The fetch could not start or failed: back to editing, `message`
    /// under the field and the keyboard in it.
    pub(crate) fn fail(&mut self, message: String) {
        self.phase = Phase::Editing;
        self.error = Some(message);
        self.focus = FetchControl::Path;
    }

    /// The path was edited, which clears the error under it.
    pub(crate) fn edited(&mut self) {
        self.error = None;
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether a fetch is starting or in flight.
    pub(crate) fn is_busy(&self) -> bool {
        self.phase != Phase::Editing
    }

    /// The fetch in flight.
    pub(crate) fn fetch_id(&self) -> Option<&str> {
        match &self.phase {
            Phase::Fetching { id } => Some(id),
            Phase::Editing | Phase::Starting => None,
        }
    }

    /// The line naming the root, for no root or one; several are a row of
    /// buttons instead.
    pub(crate) fn caption(&self) -> Option<String> {
        match self.roots.as_slice() {
            [] => Some(NO_REPO.to_owned()),
            [root] => Some(root.repo_name.clone()),
            _ => None,
        }
    }

    /// What the field's text is relative to.
    pub(crate) fn hint(&self) -> Option<String> {
        self.scope().map(|root| {
            format!(
                "Relative to {}. Add :line to open at a line.",
                root.repo_name
            )
        })
    }
}

/// How far the fetch is: `received` bytes of `size`, or that the host has
/// not answered before the size is known.
pub(crate) fn progress_text(received: u64, size: Option<u64>, host: &str) -> String {
    match size {
        Some(size) => format!("{} of {}", human_size(received), human_size(size)),
        None => format!("Waiting for {host}…"),
    }
}

/// The share of the progress bar filled: none before the size is known,
/// all of it for an empty file, never more than all.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "a bar's width needs no byte-exact share"
)]
pub(crate) fn progress_fraction(received: u64, size: Option<u64>) -> f32 {
    match size {
        None => 0.0,
        Some(0) => 1.0,
        Some(size) => (received as f64 / size as f64).min(1.0) as f32,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::{
        FetchControl, FetchForm, NO_REPO, PathProblem, parse_target, progress_fraction,
        progress_text,
    };
    use crate::fetch_link::Root;

    fn root(repo_id: &str, path: &str) -> Root {
        Root {
            repo_id: repo_id.to_owned(),
            repo_name: format!("{repo_id}-name"),
            path: path.to_owned(),
        }
    }

    fn parsed(root_path: &str, text: &str) -> (String, Option<u32>, Option<u32>) {
        let target = parse_target(&root("r1", root_path), text).expect("a fetchable path");
        (target.rel_path, target.line, target.column)
    }

    fn problem(root_path: &str, text: &str) -> PathProblem {
        parse_target(&root("r1", root_path), text).expect_err("a refused path")
    }

    fn two_roots() -> Vec<Root> {
        vec![root("ra", "C:/wt/a"), root("rb", "C:/wt/b")]
    }

    #[test]
    fn a_relative_path_is_kept_and_a_line_is_split_off() {
        assert_eq!(
            parsed("C:/wt/x", " src/main.rs:42 "),
            ("src/main.rs".to_owned(), Some(42), None)
        );
        assert_eq!(
            parsed("C:/wt/x", "README.md"),
            ("README.md".to_owned(), None, None)
        );
    }

    #[test]
    fn a_line_and_column_are_both_split_off() {
        assert_eq!(
            parsed("C:/wt/x", "a.rs:7:3"),
            ("a.rs".to_owned(), Some(7), Some(3))
        );
    }

    #[test]
    fn a_zero_line_is_not_a_line() {
        assert_eq!(
            parsed("C:/wt/x", "a.rs:0"),
            ("a.rs:0".to_owned(), None, None)
        );
    }

    #[test]
    fn an_absolute_path_under_the_root_has_the_root_stripped() {
        assert_eq!(
            parsed("C:/wt/x", r"C:\wt\x\src\a.rs:5"),
            ("src/a.rs".to_owned(), Some(5), None)
        );
        assert_eq!(
            parsed(r"\\?\C:\wt\x", r"c:\WT\X\src\a.rs"),
            ("src/a.rs".to_owned(), None, None),
            "drive paths compare without case and without the verbatim prefix"
        );
    }

    #[test]
    fn an_absolute_unix_path_is_case_sensitive() {
        assert_eq!(
            parsed("/home/me/repo", "/home/me/repo/src/a.rs"),
            ("src/a.rs".to_owned(), None, None)
        );
        assert!(matches!(
            problem("/home/me/repo", "/Home/me/repo/src/a.rs"),
            PathProblem::OutsideRoot { .. }
        ));
    }

    #[test]
    fn an_absolute_path_outside_the_root_names_the_root() {
        let refused = problem(r"\\?\C:\wt\x", r"C:\other\a.rs:3");
        assert_eq!(
            refused.message(),
            r"C:\other\a.rs is outside C:\wt\x. Give a path under it."
        );
    }

    #[test]
    fn the_root_itself_is_not_a_file() {
        assert!(matches!(
            problem("C:/wt/x", "C:/wt/x"),
            PathProblem::OutsideRoot { .. }
        ));
    }

    #[test]
    fn an_empty_or_blank_path_is_refused() {
        for text in ["", "   ", ":42", ".", "./"] {
            assert_eq!(problem("C:/wt/x", text), PathProblem::Empty, "{text:?}");
        }
        assert_eq!(PathProblem::Empty.message(), "Enter a path.");
    }

    #[test]
    fn a_parent_segment_is_refused_even_inside_the_text() {
        for text in ["../a.rs", r"src\..\a.rs", "src/..", "C:/wt/x/sub/../a.rs"] {
            assert_eq!(
                problem("C:/wt/x", text),
                PathProblem::ParentSegment,
                "{text:?}"
            );
        }
        assert_eq!(
            PathProblem::ParentSegment.message(),
            "A path can't contain \"..\"."
        );
        assert_eq!(
            parsed("C:/wt/x", "a..b.rs"),
            ("a..b.rs".to_owned(), None, None),
            "dots inside a name are not a segment"
        );
    }

    #[test]
    fn dot_segments_are_dropped() {
        assert_eq!(
            parsed("C:/wt/x", r"./src\.\a.rs:9"),
            ("src/a.rs".to_owned(), Some(9), None)
        );
    }

    #[test]
    fn can_submit_needs_a_scope_and_a_valid_path() {
        let form = FetchForm::new(vec![root("r1", "C:/wt/x")], None);
        assert!(form.can_submit("src/a.rs"));
        assert!(!form.can_submit(""));
        assert!(!form.can_submit("../a.rs"));
        let none = FetchForm::new(Vec::new(), None);
        assert!(!none.can_submit("src/a.rs"));
    }

    #[test]
    fn no_roots_means_no_scope_and_a_caption_saying_so() {
        let mut form = FetchForm::new(Vec::new(), Some("C:/wt/x"));
        assert_eq!(form.scope(), None);
        assert_eq!(form.caption().as_deref(), Some(NO_REPO));
        assert_eq!(form.hint(), None);
        assert_eq!(form.submit("a.rs"), None);
        assert_eq!(form.error(), None, "the caption already says why");
    }

    #[test]
    fn the_default_scope_is_the_member_holding_the_cwd() {
        let form = FetchForm::new(two_roots(), Some("C:/wt/b/src"));
        assert_eq!(form.scope().map(|r| r.repo_id.as_str()), Some("rb"));
        assert_eq!(form.caption(), None, "several roots are buttons");
        assert_eq!(
            form.hint().as_deref(),
            Some("Relative to rb-name. Add :line to open at a line.")
        );
    }

    #[test]
    fn the_default_scope_falls_back_to_the_first_member() {
        let form = FetchForm::new(two_roots(), Some("C:/elsewhere"));
        assert_eq!(form.scope().map(|r| r.repo_id.as_str()), Some("ra"));
        let single = FetchForm::new(vec![root("r1", "C:/wt/x")], None);
        assert_eq!(single.caption().as_deref(), Some("r1-name"));
    }

    #[test]
    fn the_scope_row_is_in_the_focus_ring_only_for_several_members() {
        let single = FetchForm::new(vec![root("r1", "C:/wt/x")], None);
        assert_eq!(
            single.controls("a.rs"),
            [
                FetchControl::Close,
                FetchControl::Path,
                FetchControl::Cancel,
                FetchControl::Submit
            ]
        );
        let mut several = FetchForm::new(two_roots(), None);
        assert_eq!(
            several.controls(""),
            [
                FetchControl::Close,
                FetchControl::Scope(0),
                FetchControl::Scope(1),
                FetchControl::Path,
                FetchControl::Cancel
            ]
        );
        several.step_scope(true);
        assert_eq!(several.scope().map(|r| r.repo_id.as_str()), Some("rb"));
        assert_eq!(several.focused(""), FetchControl::Scope(1));
        several.step_scope(true);
        assert_eq!(
            several.scope().map(|r| r.repo_id.as_str()),
            Some("ra"),
            "Right wraps"
        );
        several.select_scope(5);
        assert_eq!(several.scope().map(|r| r.repo_id.as_str()), Some("ra"));
        assert_eq!(FetchControl::Scope(1).selector(), "fetch-scope-1");
    }

    #[test]
    fn the_focus_ring_wraps_and_skips_submit_while_invalid() {
        let mut form = FetchForm::new(vec![root("r1", "C:/wt/x")], None);
        assert_eq!(form.focused(""), FetchControl::Path);
        form.move_focus(true, "");
        assert_eq!(form.focused(""), FetchControl::Cancel);
        form.move_focus(true, "");
        assert_eq!(form.focused(""), FetchControl::Close, "no Submit to reach");
        form.move_focus(false, "");
        assert_eq!(form.focused(""), FetchControl::Cancel);
        form.move_focus(true, "a.rs");
        assert_eq!(form.focused("a.rs"), FetchControl::Submit);
        assert_eq!(
            form.focused(""),
            FetchControl::Close,
            "a Submit that went away gives way to the first control"
        );
    }

    #[test]
    fn submit_builds_the_request_for_the_chosen_scope() {
        let mut form = FetchForm::new(two_roots(), None);
        form.select_scope(1);
        let request = form.submit("src/a.rs:4:2").expect("a request");
        assert_eq!(request.repo_id, "rb");
        assert_eq!(request.repo_name, "rb-name");
        assert_eq!(request.worktree_path.as_deref(), Some("C:/wt/b"));
        assert_eq!(request.rel_path, "src/a.rs");
        assert_eq!((request.line, request.column), (Some(4), Some(2)));
        assert!(request.open_when_done);
        assert!(form.is_busy());
    }

    #[test]
    fn submit_leaves_the_focus_in_the_path_field() {
        let mut form = FetchForm::new(vec![root("r1", "C:/wt/x")], None);
        form.set_focus(FetchControl::Submit);
        assert_eq!(form.focused("a.rs"), FetchControl::Submit);
        assert!(form.submit("a.rs").is_some());
        assert_eq!(
            form.focused("a.rs"),
            FetchControl::Path,
            "not the ✕, where the next Enter would cancel the fetch"
        );
    }

    #[test]
    fn a_second_submit_is_ignored_while_starting() {
        let mut form = FetchForm::new(vec![root("r1", "C:/wt/x")], None);
        assert!(form.submit("a.rs").is_some());
        assert_eq!(form.submit("a.rs"), None);
        assert!(!form.can_submit("a.rs"));
        form.started("fetch-1".to_owned());
        assert_eq!(form.fetch_id(), Some("fetch-1"));
        assert_eq!(form.submit("a.rs"), None, "nor while fetching");
    }

    #[test]
    fn a_failure_reopens_editing_with_the_error_and_an_edit_clears_it() {
        let mut form = FetchForm::new(vec![root("r1", "C:/wt/x")], None);
        assert_eq!(form.submit("../a.rs"), None);
        assert_eq!(form.error(), Some("A path can't contain \"..\"."));
        form.edited();
        assert_eq!(form.error(), None);
        assert!(form.submit("a.rs").is_some());
        form.started("fetch-1".to_owned());
        form.set_focus(FetchControl::Cancel);
        form.fail("path \"a.rs\" does not exist".to_owned());
        assert!(!form.is_busy());
        assert_eq!(form.fetch_id(), None);
        assert_eq!(form.error(), Some("path \"a.rs\" does not exist"));
        assert_eq!(form.focused("a.rs"), FetchControl::Path);
        assert!(form.submit("a.rs").is_some(), "Fetch goes again");
        assert_eq!(form.error(), None);
    }

    #[test]
    fn progress_text_reads_bytes_of_size() {
        assert_eq!(
            progress_text(12_897_485, Some(42_048_717), "box"),
            "12.3 MB of 40.1 MB"
        );
        assert_eq!(progress_text(4, Some(10), "box"), "4 B of 10 B");
    }

    #[test]
    fn progress_before_the_size_is_known_says_waiting_for_the_host() {
        assert_eq!(
            progress_text(0, None, "box.tail1234.ts.net"),
            "Waiting for box.tail1234.ts.net…"
        );
    }

    #[test]
    fn progress_fraction_is_clamped_and_zero_without_a_size() {
        assert!(progress_fraction(5, None).abs() < f32::EPSILON);
        assert!((progress_fraction(4, Some(10)) - 0.4).abs() < 1e-6);
        assert!((progress_fraction(20, Some(10)) - 1.0).abs() < f32::EPSILON);
        assert!((progress_fraction(0, Some(0)) - 1.0).abs() < f32::EPSILON);
    }
}
