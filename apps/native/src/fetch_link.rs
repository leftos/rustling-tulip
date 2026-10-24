//! A terminal path link on a remote host as the files to fetch: each reading
//! of the link's path, relative to a repo or worktree of the pane's session,
//! in the order to try them. Host paths are compared as text, since the host
//! may run another OS than this PC.

use protocol::{RepoEntry, SessionMember, SessionSnapshot};

use crate::file_fetch::FetchRequest;
use crate::links::TerminalLinkCandidate;

/// A folder on the host a fetch may name: a repo or one of its worktrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub repo_id: String,
    pub repo_name: String,
    /// The folder as the host reports it.
    pub path: String,
}

/// The folders a session's link paths may lie in: its members' worktrees in
/// member order, blanks left out; for a session with no members, the
/// registered repo holding `cwd`, the deepest one when repos nest.
#[must_use]
pub fn roots(members: &[SessionMember], cwd: Option<&str>, repos: &[RepoEntry]) -> Vec<Root> {
    if !members.is_empty() {
        return members
            .iter()
            .filter(|member| !member.worktree_path.is_empty())
            .map(|member| Root {
                repo_id: member.repo_id.clone(),
                repo_name: member.repo_name.clone(),
                path: member.worktree_path.clone(),
            })
            .collect();
    }
    let Some(cwd) = cwd.and_then(HostPath::parse) else {
        return Vec::new();
    };
    repos
        .iter()
        .filter_map(|repo| {
            let path = HostPath::parse(&repo.path)?;
            path.holds(&cwd).then_some((path.segments.len(), repo))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, repo)| Root {
            repo_id: repo.id.clone(),
            repo_name: repo.name.clone(),
            path: repo.path.clone(),
        })
        .into_iter()
        .collect()
}

/// The index of the root in `roots` holding `cwd`, the deepest one when
/// roots nest; `None` when no root holds it or there is no `cwd`.
#[must_use]
pub(crate) fn root_holding(roots: &[Root], cwd: Option<&str>) -> Option<usize> {
    let cwd = HostPath::parse(cwd?)?;
    roots
        .iter()
        .enumerate()
        .filter_map(|(index, root)| {
            let path = HostPath::parse(&root.path)?;
            path.holds(&cwd).then_some((path.segments.len(), index))
        })
        .max_by_key(|(depth, index)| (*depth, std::cmp::Reverse(*index)))
        .map(|(_, index)| index)
}

/// What a typed path is against a root on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootStrip {
    /// A relative path, left as typed.
    NotAbsolute,
    /// An absolute path strictly below the root: the rest of it, `/`
    /// separated, with `.` and `..` resolved.
    Inside(String),
    /// An absolute path that is the root itself or not below it.
    Outside,
}

/// `text` against `root_path`: an absolute host path below the root loses
/// the root; see [`RootStrip`].
#[must_use]
pub(crate) fn strip_root(root_path: &str, text: &str) -> RootStrip {
    if split_anchor(&unify(text)).is_none() {
        return RootStrip::NotAbsolute;
    }
    let inside = HostPath::parse(root_path)
        .zip(HostPath::parse(text))
        .and_then(|(root, path)| root.relative(&path));
    match inside {
        Some(rel) => RootStrip::Inside(rel),
        None => RootStrip::Outside,
    }
}

/// The fetches to try for a link's `candidates` (longest first): each
/// candidate in turn, against each of `roots`. A relative candidate is read
/// against `cwd` first, then against each root; `.` and `..` are resolved as
/// text, and a path outside every root gives no reading.
#[must_use]
pub fn readings(
    candidates: &[TerminalLinkCandidate],
    cwd: Option<&str>,
    roots: &[Root],
) -> Vec<FetchRequest> {
    let parsed: Vec<(&Root, HostPath)> = roots
        .iter()
        .filter_map(|root| HostPath::parse(&root.path).map(|path| (root, path)))
        .collect();
    let cwd = cwd.and_then(HostPath::parse);
    let mut found: Vec<FetchRequest> = Vec::new();
    for candidate in candidates {
        for place in places(&candidate.path, cwd.as_ref(), &parsed) {
            for (root, root_path) in &parsed {
                let Some(rel) = root_path.relative(&place) else {
                    continue;
                };
                let request = FetchRequest {
                    repo_id: root.repo_id.clone(),
                    repo_name: root.repo_name.clone(),
                    worktree_path: Some(root.path.clone()),
                    rel_path: rel,
                    line: candidate.line,
                    column: candidate.column,
                    open_when_done: true,
                };
                let seen = found.iter().any(|other| {
                    (&other.repo_id, &other.worktree_path, &other.rel_path)
                        == (&request.repo_id, &request.worktree_path, &request.rel_path)
                });
                if !seen {
                    found.push(request);
                }
            }
        }
    }
    found
}

/// Where the link path `path` may be on the host: itself when absolute,
/// else joined to `cwd`, then to each root.
fn places(path: &str, cwd: Option<&HostPath>, roots: &[(&Root, HostPath)]) -> Vec<HostPath> {
    if split_anchor(&unify(path)).is_some() {
        return HostPath::parse(path).into_iter().collect();
    }
    cwd.into_iter()
        .chain(roots.iter().map(|(_, root)| root))
        .filter_map(|base| base.join(path))
        .collect()
}

/// An absolute host path as text: what it starts from (`C:`, `//` for a
/// network path, `/` on Unix) and its segments, `.` and `..` resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostPath {
    anchor: String,
    /// A Windows path, whose names compare without case.
    windows: bool,
    segments: Vec<String>,
}

impl HostPath {
    /// `text` as an absolute host path, Windows' verbatim prefix stripped
    /// and `\` read as `/`; `None` when it is relative or climbs above its
    /// anchor.
    fn parse(text: &str) -> Option<Self> {
        let unified = unify(text);
        let (anchor, rest) = split_anchor(&unified)?;
        let path = Self {
            anchor: anchor.to_owned(),
            windows: anchor != "/",
            segments: Vec::new(),
        };
        path.join(rest)
    }

    /// This path with the relative path `rel` appended, `.` and `..`
    /// resolved; `None` when `rel` climbs above the anchor.
    fn join(&self, rel: &str) -> Option<Self> {
        let mut segments = self.segments.clone();
        for segment in rel.split(['/', '\\']) {
            match segment {
                "" | "." => {}
                ".." => {
                    segments.pop()?;
                }
                name => segments.push(name.to_owned()),
            }
        }
        Some(Self {
            anchor: self.anchor.clone(),
            windows: self.windows,
            segments,
        })
    }

    fn same_name(&self, a: &str, b: &str) -> bool {
        if self.windows {
            a.to_lowercase() == b.to_lowercase()
        } else {
            a == b
        }
    }

    /// Whether `path` is this folder or lies below it.
    fn holds(&self, path: &Self) -> bool {
        self.windows == path.windows
            && self.same_name(&self.anchor, &path.anchor)
            && path.segments.len() >= self.segments.len()
            && self
                .segments
                .iter()
                .zip(&path.segments)
                .all(|(mine, theirs)| self.same_name(mine, theirs))
    }

    /// `path` relative to this folder, `/` separated; `None` unless it lies
    /// strictly below it.
    fn relative(&self, path: &Self) -> Option<String> {
        if !self.holds(path) {
            return None;
        }
        let below = path.segments.get(self.segments.len()..)?;
        (!below.is_empty()).then(|| below.join("/"))
    }
}

/// `text` without Windows' verbatim prefix, `\` read as `/`.
pub(crate) fn unify(text: &str) -> String {
    crate::file_fetch::display_host_path(text).replace('\\', "/")
}

/// An absolute path's anchor (`//` for a network path, `C:` for a drive,
/// `/` on Unix) and the rest of it; `None` for a relative path.
pub(crate) fn split_anchor(unified: &str) -> Option<(&str, &str)> {
    if let Some(rest) = unified.strip_prefix("//") {
        return Some(("//", rest));
    }
    let mut chars = unified.chars();
    if chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.next() == Some(':') {
        let rest = chars.as_str();
        return (rest.is_empty() || rest.starts_with('/'))
            .then(|| unified.split_at(unified.len() - rest.len()));
    }
    unified.strip_prefix('/').map(|rest| ("/", rest))
}

/// [`readings`] for a link in a pane of `session`.
#[must_use]
pub fn session_readings(
    candidates: &[TerminalLinkCandidate],
    session: &SessionSnapshot,
    repos: &[RepoEntry],
) -> Vec<FetchRequest> {
    let cwd = session.current_cwd.as_deref();
    readings(candidates, cwd, &roots(&session.members, cwd, repos))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use protocol::{RepoEntry, SessionMember};
    use serde_json::json;

    use super::{Root, RootStrip, readings, root_holding, roots, strip_root};
    use crate::file_fetch::FetchRequest;
    use crate::links::TerminalLinkCandidate;

    fn candidate(path: &str, line: Option<u32>, column: Option<u32>) -> TerminalLinkCandidate {
        TerminalLinkCandidate {
            path: path.to_owned(),
            line,
            column,
        }
    }

    fn plain(path: &str) -> TerminalLinkCandidate {
        candidate(path, None, None)
    }

    fn root(repo_id: &str, path: &str) -> Root {
        Root {
            repo_id: repo_id.to_owned(),
            repo_name: format!("{repo_id}-name"),
            path: path.to_owned(),
        }
    }

    fn member(repo_id: &str, worktree_path: &str) -> SessionMember {
        SessionMember {
            repo_id: repo_id.to_owned(),
            repo_name: format!("{repo_id}-name"),
            branch: "main".to_owned(),
            worktree_path: worktree_path.to_owned(),
        }
    }

    fn repo(id: &str, path: &str) -> RepoEntry {
        serde_json::from_value(json!({ "id": id, "name": format!("{id}-name"), "path": path }))
            .expect("a repo entry")
    }

    /// The reading of `rel` under `root`, with no line.
    fn reading(root: &Root, rel: &str) -> FetchRequest {
        FetchRequest {
            repo_id: root.repo_id.clone(),
            repo_name: root.repo_name.clone(),
            worktree_path: Some(root.path.clone()),
            rel_path: rel.to_owned(),
            line: None,
            column: None,
            open_when_done: true,
        }
    }

    /// Each reading as `(repo id, worktree, rel path)`.
    fn places(readings: &[FetchRequest]) -> Vec<(String, String, String)> {
        readings
            .iter()
            .map(|r| {
                (
                    r.repo_id.clone(),
                    r.worktree_path.clone().unwrap_or_default(),
                    r.rel_path.clone(),
                )
            })
            .collect()
    }

    fn place(repo_id: &str, worktree: &str, rel: &str) -> (String, String, String) {
        (repo_id.to_owned(), worktree.to_owned(), rel.to_owned())
    }

    #[test]
    fn absolute_host_path_under_a_member_becomes_a_relative_reading() {
        let x = root("r1", "C:/wt/x");
        let found = readings(
            &[candidate(r"C:\wt\x\src\a.rs", Some(12), Some(3))],
            None,
            std::slice::from_ref(&x),
        );
        let mut expected = reading(&x, "src/a.rs");
        expected.line = Some(12);
        expected.column = Some(3);
        assert_eq!(found, [expected]);
    }

    #[test]
    fn verbatim_and_case_variants_match_the_root() {
        let x = root("r1", "C:/wt/x");
        let share = root("r2", r"\\server\share\repo");
        let all = [x.clone(), share.clone()];
        assert_eq!(
            readings(&[plain(r"\\?\c:\WT\x\a.rs")], None, &all),
            [reading(&x, "a.rs")]
        );
        assert_eq!(
            readings(&[plain(r"\\?\UNC\SERVER\share\Repo\sub\b.rs")], None, &all),
            [reading(&share, "sub/b.rs")]
        );
        assert_eq!(
            readings(&[plain("//server/SHARE/repo/c.rs")], None, &all),
            [reading(&share, "c.rs")]
        );
    }

    #[test]
    fn unix_paths_use_slashes_and_are_case_sensitive() {
        let home = root("r1", "/home/me/repo");
        let all = [home.clone()];
        assert_eq!(
            readings(&[plain("/home/me/repo/src/a.rs")], None, &all),
            [reading(&home, "src/a.rs")]
        );
        assert_eq!(
            readings(&[plain("/Home/me/repo/src/a.rs")], None, &all),
            [],
            "a Unix host path differs by case"
        );
        assert_eq!(
            readings(&[plain("/home/me/repo")], None, &all),
            [],
            "the root itself is not a file in it"
        );
        assert_eq!(
            readings(&[plain("/home/me/repo2/a.rs")], None, &all),
            [],
            "a sibling sharing the root's prefix is not under it"
        );
    }

    #[test]
    fn relative_path_joins_cwd_then_members_in_base_dir_order() {
        let a = root("ra", "C:/wt/a");
        let b = root("rb", "C:/wt/b");
        let found = readings(
            &[candidate("x.rs", Some(4), None)],
            Some(r"C:\wt\b\sub"),
            &[a, b],
        );
        assert_eq!(
            places(&found),
            [
                place("rb", "C:/wt/b", "sub/x.rs"),
                place("ra", "C:/wt/a", "x.rs"),
                place("rb", "C:/wt/b", "x.rs"),
            ]
        );
        assert!(
            found
                .iter()
                .all(|r| r.line == Some(4) && r.column.is_none()),
            "every reading keeps the candidate's line: {found:?}"
        );
    }

    #[test]
    fn dot_and_parent_segments_are_normalised_and_an_escape_yields_no_reading() {
        let x = root("r1", "C:/wt/x");
        let all = [x.clone()];
        let cwd = Some("C:/wt/x");
        assert_eq!(
            readings(&[plain("./a.rs")], cwd, &all),
            [reading(&x, "a.rs")]
        );
        assert_eq!(
            readings(&[plain(r"sub\..\a.rs")], cwd, &all),
            [reading(&x, "a.rs")]
        );
        assert_eq!(
            readings(&[plain("../x/b.rs")], Some("C:/wt/x/sub"), &all),
            [reading(&x, "x/b.rs"), reading(&x, "b.rs")],
            "a parent segment that stays inside the root reads under it, from the cwd then the root"
        );
        assert_eq!(readings(&[plain("../../outside.rs")], cwd, &all), []);
        assert_eq!(readings(&[plain("C:/wt/x/../y/a.rs")], cwd, &all), []);
    }

    #[test]
    fn longest_candidate_comes_first_and_duplicates_are_dropped() {
        let x = root("r1", "C:/wt/x");
        let found = readings(
            &[plain("docs/long name.md"), plain("docs/long")],
            Some("C:/wt/x"),
            &[x],
        );
        assert_eq!(
            places(&found),
            [
                place("r1", "C:/wt/x", "docs/long name.md"),
                place("r1", "C:/wt/x", "docs/long"),
            ],
            "the cwd and the member are one folder, so each candidate reads once"
        );
    }

    #[test]
    fn a_standalone_shell_falls_back_to_the_registered_repo_containing_its_cwd() {
        let repos = [
            repo("outer", "C:/repos"),
            repo("app", r"C:\repos\app"),
            repo("other", "C:/repos/other"),
        ];
        let cwd = Some("C:/repos/app/src");
        let found = roots(&[], cwd, &repos);
        assert_eq!(found, [root("app", r"C:\repos\app")], "the deepest repo");
        assert_eq!(
            places(&readings(&[plain("main.rs")], cwd, &found)),
            [
                place("app", r"C:\repos\app", "src/main.rs"),
                place("app", r"C:\repos\app", "main.rs"),
            ]
        );
        assert_eq!(
            roots(&[], Some("C:/repos/app"), &repos),
            [root("app", r"C:\repos\app")],
            "a cwd at the repo's own folder is in it"
        );
        let members = [
            member("ra", "C:/wt/a"),
            member("rx", ""),
            member("rb", "C:/wt/b"),
        ];
        assert_eq!(
            roots(&members, cwd, &repos),
            [root("ra", "C:/wt/a"), root("rb", "C:/wt/b")],
            "a session with members reads against them, blanks left out"
        );
    }

    #[test]
    fn no_root_gives_no_readings() {
        let repos = [repo("app", "C:/repos/app")];
        assert_eq!(roots(&[], Some("C:/elsewhere"), &repos), []);
        assert_eq!(roots(&[], None, &repos), []);
        assert_eq!(readings(&[plain("C:/repos/app/a.rs")], None, &[]), []);
        assert_eq!(readings(&[plain("a.rs")], Some("C:/repos/app"), &[]), []);
    }

    #[test]
    fn root_holding_picks_the_deepest_root_containing_the_cwd() {
        let all = [
            root("outer", "C:/repos"),
            root("app", r"C:\repos\app"),
            root("other", "C:/repos/other"),
        ];
        assert_eq!(root_holding(&all, Some("c:/REPOS/app/src")), Some(1));
        assert_eq!(root_holding(&all, Some("C:/repos/third")), Some(0));
        assert_eq!(
            root_holding(&all, Some(r"\\?\C:\repos\other")),
            Some(2),
            "a cwd at the root's own folder is in it"
        );
    }

    #[test]
    fn root_holding_is_none_when_no_root_holds_the_cwd() {
        let all = [root("a", "C:/wt/a"), root("home", "/home/me/repo")];
        assert_eq!(root_holding(&all, Some("C:/elsewhere")), None);
        assert_eq!(root_holding(&all, Some("/Home/me/repo")), None);
        assert_eq!(root_holding(&all, Some("relative/dir")), None);
        assert_eq!(root_holding(&all, None), None);
        assert_eq!(root_holding(&[], Some("C:/wt/a")), None);
    }

    #[test]
    fn strip_root_reports_inside_outside_and_relative() {
        assert_eq!(
            strip_root("C:/wt/x", r"\\?\c:\WT\x\src\.\a.rs"),
            RootStrip::Inside("src/a.rs".to_owned())
        );
        assert_eq!(
            strip_root("/home/me/repo", "/home/me/repo/sub/../b.rs"),
            RootStrip::Inside("b.rs".to_owned())
        );
        assert_eq!(strip_root("C:/wt/x", "C:/wt/x"), RootStrip::Outside);
        assert_eq!(strip_root("C:/wt/x", "C:/wt/y/a.rs"), RootStrip::Outside);
        assert_eq!(
            strip_root("C:/wt/x", "C:/wt/x/../y/a.rs"),
            RootStrip::Outside
        );
        assert_eq!(
            strip_root("/home/me/repo", "/Home/me/repo/a.rs"),
            RootStrip::Outside
        );
        assert_eq!(strip_root("C:/wt/x", "src/a.rs"), RootStrip::NotAbsolute);
        assert_eq!(strip_root("C:/wt/x", r"src\a.rs"), RootStrip::NotAbsolute);
    }
}
