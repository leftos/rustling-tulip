//! The terminal's fonts: the families the client bundles, the font settings
//! a pane renders with, and how a requested family resolves against the
//! fonts the text system knows.

use std::borrow::Cow;

use gpui::{App, SharedString, TextSystem};
use protocol::{RepoEntry, SessionSnapshot, WorkspaceEntry};
use serde::{Deserialize, Serialize};

/// The family a pane renders in when the settings name none.
pub const DEFAULT_FAMILY: &str = "Geist Mono";
pub const DEFAULT_SIZE: f32 = 13.0;
pub const MIN_SIZE: f32 = 8.0;
pub const MAX_SIZE: f32 = 32.0;

/// The families the client ships, as their fonts name them.
pub const BUNDLED_FAMILIES: [&str; 4] =
    ["Geist Mono", "Fira Code", "JetBrains Mono", "Cascadia Code"];

/// Tried in order when the requested family is not installed.
const FALLBACK_CHAIN: [&str; 4] = ["Geist Mono", "Cascadia Mono", "Consolas", "Courier New"];

/// A bundled face and the style it is.
struct Face {
    data: &'static [u8],
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the tests check each face's style against it")
    )]
    italic: bool,
}

const fn upright(data: &'static [u8]) -> Face {
    Face {
        data,
        italic: false,
    }
}

const fn italic(data: &'static [u8]) -> Face {
    Face { data, italic: true }
}

/// The bundled faces: Regular and Bold of each family, plus Italic and Bold
/// Italic where the family has them (Fira Code has none).
const BUNDLED_FILES: [Face; 14] = [
    upright(include_bytes!(
        "../assets/fonts/GeistMono/GeistMono-Regular.ttf"
    )),
    upright(include_bytes!(
        "../assets/fonts/GeistMono/GeistMono-Bold.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/GeistMono/GeistMono-Italic.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/GeistMono/GeistMono-BoldItalic.ttf"
    )),
    upright(include_bytes!(
        "../assets/fonts/FiraCode/FiraCode-Regular.ttf"
    )),
    upright(include_bytes!("../assets/fonts/FiraCode/FiraCode-Bold.ttf")),
    upright(include_bytes!(
        "../assets/fonts/JetBrainsMono/JetBrainsMono-Regular.ttf"
    )),
    upright(include_bytes!(
        "../assets/fonts/JetBrainsMono/JetBrainsMono-Bold.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/JetBrainsMono/JetBrainsMono-Italic.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/JetBrainsMono/JetBrainsMono-BoldItalic.ttf"
    )),
    upright(include_bytes!(
        "../assets/fonts/CascadiaCode/CascadiaCode-Regular.ttf"
    )),
    upright(include_bytes!(
        "../assets/fonts/CascadiaCode/CascadiaCode-Bold.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/CascadiaCode/CascadiaCode-Italic.ttf"
    )),
    italic(include_bytes!(
        "../assets/fonts/CascadiaCode/CascadiaCode-BoldItalic.ttf"
    )),
];

/// How a terminal pane draws its text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontSettings {
    /// The family asked for; `None` is [`DEFAULT_FAMILY`].
    pub family: Option<String>,
    /// The size in pixels, a whole number from [`MIN_SIZE`] to [`MAX_SIZE`].
    pub size: f32,
    /// Draws normal text at the bold weight; bold cells stay bold.
    pub bold: bool,
}

impl Default for FontSettings {
    fn default() -> Self {
        Self {
            family: None,
            size: DEFAULT_SIZE,
            bold: false,
        }
    }
}

impl FontSettings {
    /// These settings with the size clamped and rounded.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.size = clamp_size(self.size);
        self
    }
}

/// `size` rounded to a whole number of pixels and clamped to
/// [`MIN_SIZE`]..=[`MAX_SIZE`]; a size that is not a number is the default.
#[must_use]
pub fn clamp_size(size: f32) -> f32 {
    if size.is_nan() {
        return DEFAULT_SIZE;
    }
    size.round().clamp(MIN_SIZE, MAX_SIZE)
}

/// The size `size` moves to one step of `delta`, or `None` when the clamp
/// already holds it there: a shortcut at a limit does nothing.
#[must_use]
pub fn stepped(size: f32, delta: f32) -> Option<f32> {
    let size = clamp_size(size);
    let next = clamp_size(size + delta);
    ((next - size).abs() > f32::EPSILON).then_some(next)
}

/// The terminal font size `session`'s container sets: its workspace's when it
/// belongs to one, else its first member's repo's.
#[must_use]
pub fn container_size(
    session: &SessionSnapshot,
    repos: &[RepoEntry],
    workspaces: &[WorkspaceEntry],
) -> Option<u16> {
    if let Some(id) = session.workspace_id.as_deref() {
        return workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .and_then(|workspace| workspace.appearance.terminal_font_size);
    }
    let id = session.members.first()?.repo_id.as_str();
    repos
        .iter()
        .find(|repo| repo.id == id)
        .and_then(|repo| repo.appearance.terminal_font_size)
}

/// The size a pane draws at: its tab's override, else its session's size,
/// else its container's, else the app's. The winner is clamped.
#[must_use]
pub fn resolve_size(
    tab: Option<f32>,
    session: Option<u16>,
    container: Option<u16>,
    app: f32,
) -> f32 {
    let level = tab
        .or_else(|| session.map(f32::from))
        .or_else(|| container.map(f32::from));
    clamp_size(level.unwrap_or(app))
}

/// The family to render `requested` (or [`DEFAULT_FAMILY`]) with: itself
/// when `available` has it, else the first of the fallback chain that it
/// has. With nothing to check against, the family is returned unchecked.
#[must_use]
pub fn resolve_family(requested: Option<&str>, available: &[SharedString]) -> SharedString {
    let wanted = requested.unwrap_or(DEFAULT_FAMILY);
    let find = |name: &str| {
        available
            .iter()
            .find(|family| family.eq_ignore_ascii_case(name))
            .cloned()
    };
    if available.is_empty() {
        return SharedString::from(wanted.to_owned());
    }
    find(wanted)
        .or_else(|| FALLBACK_CHAIN.iter().find_map(|name| find(name)))
        .unwrap_or_else(|| SharedString::from(wanted.to_owned()))
}

/// The line height for a font's `ascent` and `descent` (either sign) at
/// `size`: their sum rounded up to whole pixels, or `size × 1.2` when the
/// font gave no usable metrics.
#[must_use]
pub fn line_height(ascent: f32, descent: f32, size: f32) -> f32 {
    let natural = ascent + descent.abs();
    if natural.is_finite() && natural > 0.0 {
        natural.ceil()
    } else {
        (size * 1.2).ceil()
    }
}

/// Registers the bundled fonts with the text system; a failure is logged
/// and the terminal falls back to installed families.
pub fn register_bundled(cx: &App) {
    let fonts = BUNDLED_FILES
        .iter()
        .map(|face| Cow::Borrowed(face.data))
        .collect();
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        tracing::error!("registering the bundled terminal fonts: {err:#}");
    }
}

/// Every family the text system can render.
#[must_use]
pub fn available_families(text: &TextSystem) -> Vec<SharedString> {
    text.all_font_names()
        .into_iter()
        .map(SharedString::from)
        .collect()
}

/// The installed families that are not bundled, sorted, for a font picker.
#[must_use]
pub fn system_families(cx: &App) -> Vec<SharedString> {
    let mut families: Vec<SharedString> = available_families(cx.text_system())
        .into_iter()
        .filter(|family| !BUNDLED_FAMILIES.contains(&family.as_ref()))
        .collect();
    families.sort();
    families.dedup();
    families
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::sidebar::UiState;
    use serde_json::json;

    fn names(list: &[&'static str]) -> Vec<SharedString> {
        list.iter().copied().map(SharedString::new_static).collect()
    }

    /// A single session whose members are `repos`, in workspace `workspace`.
    fn session_with(workspace: Option<&str>, repos: &[&str]) -> SessionSnapshot {
        let members: Vec<serde_json::Value> = repos
            .iter()
            .map(|id| {
                json!({
                    "repo_id": id,
                    "repo_name": id,
                    "branch": "main",
                    "worktree_path": "",
                })
            })
            .collect();
        serde_json::from_value(json!({
            "id": "s1",
            "label": "s1",
            "kind": "single",
            "members": members,
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
            "workspace_id": workspace,
        }))
        .expect("session fixture")
    }

    fn repo_with(id: &str, size: u16) -> RepoEntry {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "path": "C:/repos/r",
            "appearance": { "terminal_font_size": size },
        }))
        .expect("repo fixture")
    }

    fn workspace_with(id: &str, size: u16) -> WorkspaceEntry {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "member_repo_ids": ["r1"],
            "appearance": { "terminal_font_size": size },
        }))
        .expect("workspace fixture")
    }

    #[test]
    fn resolution_prefers_tab_then_session_then_container_then_app() {
        assert!((resolve_size(Some(20.0), Some(16), Some(15), 13.0) - 20.0).abs() < f32::EPSILON);
        assert!((resolve_size(None, Some(16), Some(15), 13.0) - 16.0).abs() < f32::EPSILON);
        assert!((resolve_size(None, None, Some(15), 13.0) - 15.0).abs() < f32::EPSILON);
        assert!((resolve_size(None, None, None, 18.0) - 18.0).abs() < f32::EPSILON);
        assert!(
            (resolve_size(None, None, None, DEFAULT_SIZE) - DEFAULT_SIZE).abs() < f32::EPSILON,
            "the app default is the built-in 13"
        );
    }

    #[test]
    fn resolution_clamps_the_winning_level() {
        assert!((resolve_size(Some(99.0), None, None, 13.0) - MAX_SIZE).abs() < f32::EPSILON);
        assert!((resolve_size(None, Some(1), Some(9), 13.0) - MIN_SIZE).abs() < f32::EPSILON);
        assert!(
            (resolve_size(None, None, None, f32::NAN) - DEFAULT_SIZE).abs() < f32::EPSILON,
            "an app size that is not a number is the default"
        );
    }

    #[test]
    fn a_workspace_container_wins_over_the_first_members_repo() {
        let repos = [repo_with("r1", 20)];
        let workspaces = [workspace_with("w1", 16)];
        assert_eq!(
            container_size(&session_with(Some("w1"), &["r1"]), &repos, &workspaces),
            Some(16),
            "the session's workspace is its container"
        );
        assert_eq!(
            container_size(&session_with(None, &["r1"]), &repos, &workspaces),
            Some(20),
            "without a workspace, its first member's repo is"
        );
        assert_eq!(
            container_size(&session_with(None, &["r1", "r2"]), &repos, &workspaces),
            Some(20),
            "the first member's repo, not any member's"
        );
        assert_eq!(
            container_size(&session_with(Some("w9"), &["r1"]), &repos, &workspaces),
            None,
            "a container the registry does not hold sets nothing"
        );
    }

    #[test]
    fn stepping_stops_at_the_clamp_limits() {
        assert_eq!(stepped(MIN_SIZE, -1.0), None);
        assert_eq!(stepped(MAX_SIZE, 1.0), None);
        assert_eq!(stepped(MIN_SIZE, 1.0), Some(9.0));
        assert_eq!(stepped(MAX_SIZE, -1.0), Some(31.0));
        assert_eq!(stepped(40.0, 1.0), None, "clamped first, then stepped");
    }

    #[test]
    fn sizes_clamp_and_round() {
        assert!((clamp_size(13.4) - 13.0).abs() < f32::EPSILON);
        assert!((clamp_size(13.5) - 14.0).abs() < f32::EPSILON);
        assert!((clamp_size(2.0) - MIN_SIZE).abs() < f32::EPSILON);
        assert!((clamp_size(7.6) - MIN_SIZE).abs() < f32::EPSILON);
        assert!((clamp_size(40.0) - MAX_SIZE).abs() < f32::EPSILON);
        assert!((clamp_size(f32::INFINITY) - MAX_SIZE).abs() < f32::EPSILON);
        assert!((clamp_size(f32::NAN) - DEFAULT_SIZE).abs() < f32::EPSILON);
        let settings = FontSettings {
            size: 100.0,
            ..FontSettings::default()
        };
        assert!((settings.normalized().size - MAX_SIZE).abs() < f32::EPSILON);
    }

    #[test]
    fn requested_family_is_used_when_available() {
        let available = names(&["Consolas", "Fira Code", "Geist Mono"]);
        assert_eq!(resolve_family(Some("Fira Code"), &available), "Fira Code");
        assert_eq!(resolve_family(None, &available), "Geist Mono");
    }

    #[test]
    fn missing_family_falls_back_to_geist_then_cascadia() {
        let with_geist = names(&["Cascadia Mono", "Consolas", "Geist Mono"]);
        assert_eq!(resolve_family(Some("Nope"), &with_geist), "Geist Mono");
        let without_geist = names(&["Arial", "Cascadia Mono", "Consolas"]);
        assert_eq!(
            resolve_family(Some("Nope"), &without_geist),
            "Cascadia Mono"
        );
        assert_eq!(resolve_family(None, &without_geist), "Cascadia Mono");
    }

    #[test]
    fn empty_list_returns_the_request_unchecked() {
        assert_eq!(resolve_family(Some("Nope"), &[]), "Nope");
        assert_eq!(resolve_family(None, &[]), "Geist Mono");
    }

    #[test]
    fn ui_state_round_trips_the_terminal_font() {
        let state = UiState {
            terminal_font: FontSettings {
                family: Some("Fira Code".to_owned()),
                size: 16.0,
                bold: true,
            },
            ..UiState::default()
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let back: UiState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);

        let older: UiState = serde_json::from_str(
            r#"{ "sidebar_width": 300.0, "sidebar_collapsed": true, "collapsed_containers": [] }"#,
        )
        .expect("a file from before the terminal font was saved");
        assert_eq!(older.terminal_font, FontSettings::default());
        assert!(older.sidebar_collapsed);

        let partial: UiState = serde_json::from_str(r#"{ "terminal_font": { "size": 20.0 } }"#)
            .expect("a font with only its size");
        assert_eq!(partial.terminal_font.family, None);
        assert!(!partial.terminal_font.bold);
    }

    #[test]
    fn ui_state_round_trips_the_tab_font_sizes() {
        let mut state = UiState::default();
        state.tab_font_sizes.insert("t1".to_owned(), 20.0);
        state.tab_font_sizes.insert("t2".to_owned(), 9.0);
        let json = serde_json::to_string(&state).expect("serialize");
        let back: UiState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);

        let partial: UiState = serde_json::from_str(r#"{ "tab_font_sizes": { "t9": 15.0 } }"#)
            .expect("a file holding only tab sizes");
        assert_eq!(
            partial,
            UiState {
                tab_font_sizes: [("t9".to_owned(), 15.0)].into(),
                ..UiState::default()
            },
            "only the tab sizes were saved; everything else is the default"
        );
    }

    #[test]
    fn bundled_fonts_are_truetype_or_opentype() {
        for face in BUNDLED_FILES {
            let tag = face.data.get(..4).expect("a font header");
            assert!(
                tag == [0, 1, 0, 0] || tag == b"OTTO" || tag == b"true",
                "not a TrueType/OpenType file: {tag:?}"
            );
        }
    }

    fn be_u16(data: &[u8], at: usize) -> u16 {
        let bytes = data.get(at..at + 2).expect("in bounds");
        u16::from_be_bytes([bytes[0], bytes[1]])
    }

    fn be_u32(data: &[u8], at: usize) -> usize {
        let bytes = data.get(at..at + 4).expect("in bounds");
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
    }

    /// Whether the face's `OS/2` table sets the ITALIC bit of `fsSelection`.
    fn os2_italic(data: &[u8]) -> bool {
        let tables = usize::from(be_u16(data, 4));
        let os2 = (0..tables)
            .map(|i| 12 + i * 16)
            .find(|&record| data.get(record..record + 4) == Some(b"OS/2".as_slice()))
            .map(|record| be_u32(data, record + 8))
            .expect("an OS/2 table");
        be_u16(data, os2 + 62) & 1 == 1
    }

    #[test]
    fn bundled_faces_carry_their_italic_style() {
        for (i, face) in BUNDLED_FILES.iter().enumerate() {
            assert_eq!(os2_italic(face.data), face.italic, "face {i}");
        }
    }
}
