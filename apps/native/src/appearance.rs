//! A session's appearance: its accent, its terminal background and frame
//! colours and its terminal font, each resolved from the session's own
//! overrides, then its container's (its workspace, else its first member's
//! repo), then the app's, then the built-in value, with the level that set
//! it. The tab's font size sits on top of all of them. Mirrors the Tauri
//! client's `appearance.ts`. What may be stored is the protocol's
//! [`AppearanceOverrides::normalized`], which the daemon applies too.

#![expect(clippy::unreadable_literal, reason = "hex colors read as #rrggbb")]

use std::collections::HashMap;

use alacritty_terminal::vte::ansi::Rgb;
use protocol::{AppearanceError, AppearanceOverrides, RepoEntry, SessionSnapshot, WorkspaceEntry};
use serde::{Deserialize, Serialize};

use crate::fonts::{self, FontSettings};
use crate::theme;

/// The accent a session has when no level sets one.
pub const BUILTIN_ACCENT: u32 = 0x5b9bff;
/// The terminal background a session has when no level sets one: the
/// theme's default.
pub const BUILTIN_BACKGROUND: u32 = packed(theme::DEFAULT_BACKGROUND);
/// How many recent custom colours are kept.
pub const RECENT_LIMIT: usize = 12;

/// A named colour offered as a one-click choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub color: u32,
}

/// The accent choices, which the quick menu applies to the frame as well.
pub const ACCENT_PRESETS: [Preset; 7] = [
    Preset {
        name: "Default",
        color: BUILTIN_ACCENT,
    },
    Preset {
        name: "Sky",
        color: 0x38bdf8,
    },
    Preset {
        name: "Blue",
        color: 0x3b82f6,
    },
    Preset {
        name: "Violet",
        color: 0x8b5cf6,
    },
    Preset {
        name: "Emerald",
        color: 0x22c55e,
    },
    Preset {
        name: "Amber",
        color: 0xf59e0b,
    },
    Preset {
        name: "Rose",
        color: 0xfb7185,
    },
];

/// The terminal background choices.
pub const BACKGROUND_PRESETS: [Preset; 6] = [
    Preset {
        name: "Default",
        color: BUILTIN_BACKGROUND,
    },
    Preset {
        name: "Graphite",
        color: 0x111318,
    },
    Preset {
        name: "Ink",
        color: 0x0b1020,
    },
    Preset {
        name: "Evergreen",
        color: 0x07150f,
    },
    Preset {
        name: "Aubergine",
        color: 0x1a1024,
    },
    Preset {
        name: "Paper",
        color: 0xf6f4ef,
    },
];

/// The app level's colours, kept on this machine; its font is the app's
/// [`FontSettings`]. `None` falls through to the built-in value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppColors {
    pub accent_color: Option<String>,
    pub terminal_background_color: Option<String>,
    pub terminal_frame_color: Option<String>,
}

/// The app level: its colours and its font.
#[derive(Debug, Clone, Copy)]
pub struct AppLevel<'a> {
    pub colors: &'a AppColors,
    pub font: &'a FontSettings,
}

/// The level a resolved value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The tab's font size override.
    Tab,
    Session,
    /// The session's workspace, else its first member's repo.
    Container,
    App,
    BuiltIn,
}

/// A resolved value and the level that set it.
#[derive(Debug, Clone, PartialEq)]
pub struct Field<T> {
    pub value: T,
    pub source: Source,
}

impl<T> Field<T> {
    const fn new(value: T, source: Source) -> Self {
        Self { value, source }
    }
}

/// Every appearance field of a session as it resolves. Colours are
/// `0xRRGGBB`.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub accent: Field<u32>,
    pub background: Field<u32>,
    /// `None`, from no level, follows the terminal's current background.
    pub frame: Field<Option<u32>>,
    /// `None` is [`fonts::DEFAULT_FAMILY`].
    pub font_family: Field<Option<String>>,
    /// Clamped to [`fonts::MIN_SIZE`]..=[`fonts::MAX_SIZE`].
    pub font_size: Field<f32>,
    pub font_bold: Field<bool>,
}

impl Resolved {
    /// These values with a tab's size override, when it has one, on top.
    #[must_use]
    pub fn with_tab_size(mut self, tab: Option<f32>) -> Self {
        if let Some(size) = tab {
            self.font_size = Field::new(fonts::clamp_size(size), Source::Tab);
        }
        self
    }

    /// The font a pane draws with.
    #[must_use]
    pub fn font(&self) -> FontSettings {
        FontSettings {
            family: self.font_family.value.clone(),
            size: self.font_size.value,
            bold: self.font_bold.value,
        }
    }
}

/// The colours a pane paints on its edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneFrame {
    /// The pane's border: the accent while the pane has its tab's focus.
    pub border: u32,
    /// The line down the pane's left edge.
    pub accent_line: u32,
}

/// One appearance field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppearanceField {
    Accent,
    Background,
    Frame,
    FontFamily,
    FontSize,
    FontBold,
}

impl AppearanceField {
    /// Copies this field's value from `from` to `to`.
    fn copy(self, from: &AppearanceOverrides, to: &mut AppearanceOverrides) {
        match self {
            Self::Accent => to.accent_color.clone_from(&from.accent_color),
            Self::Background => to
                .terminal_background_color
                .clone_from(&from.terminal_background_color),
            Self::Frame => to
                .terminal_frame_color
                .clone_from(&from.terminal_frame_color),
            Self::FontFamily => to
                .terminal_font_family
                .clone_from(&from.terminal_font_family),
            Self::FontSize => to.terminal_font_size = from.terminal_font_size,
            Self::FontBold => to.terminal_font_bold = from.terminal_font_bold,
        }
    }

    /// Whether `a` and `b` hold the same value in this field.
    fn same(self, a: &AppearanceOverrides, b: &AppearanceOverrides) -> bool {
        match self {
            Self::Accent => a.accent_color == b.accent_color,
            Self::Background => a.terminal_background_color == b.terminal_background_color,
            Self::Frame => a.terminal_frame_color == b.terminal_frame_color,
            Self::FontFamily => a.terminal_font_family == b.terminal_font_family,
            Self::FontSize => a.terminal_font_size == b.terminal_font_size,
            Self::FontBold => a.terminal_font_bold == b.terminal_font_bold,
        }
    }
}

/// Some fields of a session's appearance set to new values, `None`
/// clearing one: a change about to be sent, or one sent and not yet
/// answered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppearanceChange {
    /// The new values; only the fields in `fields` count.
    values: AppearanceOverrides,
    fields: Vec<AppearanceField>,
}

impl AppearanceChange {
    /// The accent and the frame both set to `color`, or both cleared.
    #[must_use]
    pub fn accent_and_frame(color: Option<String>) -> Self {
        Self {
            values: AppearanceOverrides {
                accent_color: color.clone(),
                terminal_frame_color: color,
                ..AppearanceOverrides::default()
            },
            fields: vec![AppearanceField::Accent, AppearanceField::Frame],
        }
    }

    /// The font size set to `size`, or cleared.
    #[must_use]
    pub fn font_size(size: Option<u16>) -> Self {
        Self {
            values: AppearanceOverrides {
                terminal_font_size: size,
                ..AppearanceOverrides::default()
            },
            fields: vec![AppearanceField::FontSize],
        }
    }

    /// This change as it would be stored; only the values it sets are
    /// checked.
    ///
    /// # Errors
    ///
    /// A value the protocol refuses.
    pub fn normalized(self) -> Result<Self, AppearanceError> {
        Ok(Self {
            values: self.values.normalized()?,
            fields: self.fields,
        })
    }

    /// `base` with this change's fields over it.
    #[must_use]
    pub fn apply_to(&self, base: &AppearanceOverrides) -> AppearanceOverrides {
        let mut out = base.clone();
        for field in &self.fields {
            field.copy(&self.values, &mut out);
        }
        out
    }

    /// Whether this change leaves `base` as it is.
    #[must_use]
    pub fn changes_nothing_in(&self, base: &AppearanceOverrides) -> bool {
        self.fields
            .iter()
            .all(|field| field.same(&self.values, base))
    }
}

/// The appearance changes sent for each session and not yet answered, in
/// the order they went out, each under the `request_id` it was sent with.
/// The daemon handles one connection's messages in order, so an answer to
/// one send answers every send before it.
#[derive(Debug, Default)]
pub struct InFlightAppearance {
    by_session: HashMap<String, Vec<(String, AppearanceChange)>>,
}

impl InFlightAppearance {
    /// `stored` with each of `session_id`'s in-flight changes over it, in
    /// the order they were sent.
    #[must_use]
    pub fn overlay(&self, session_id: &str, stored: &AppearanceOverrides) -> AppearanceOverrides {
        self.by_session
            .get(session_id)
            .into_iter()
            .flatten()
            .fold(stored.clone(), |base, (_, change)| change.apply_to(&base))
    }

    /// Holds `change`, sent for `session_id` under `request_id`.
    pub fn push(&mut self, session_id: &str, request_id: String, change: AppearanceChange) {
        self.by_session
            .entry(session_id.to_owned())
            .or_default()
            .push((request_id, change));
    }

    /// The daemon stored `session_id`'s send `request_id`: drops it and
    /// every send before it.
    pub fn answered(&mut self, session_id: &str, request_id: &str) {
        let Some(sends) = self.by_session.get_mut(session_id) else {
            return;
        };
        if let Some(at) = sends.iter().position(|(id, _)| id == request_id) {
            sends.drain(..=at);
        }
        if sends.is_empty() {
            self.by_session.remove(session_id);
        }
    }

    /// The daemon refused send `request_id`: drops that send alone.
    /// Returns whether `request_id` was an appearance send.
    pub fn refused(&mut self, request_id: &str) -> bool {
        let found = self.by_session.iter_mut().find_map(|(session_id, sends)| {
            let at = sends.iter().position(|(id, _)| id == request_id)?;
            sends.remove(at);
            Some((session_id.clone(), sends.is_empty()))
        });
        if let Some((session_id, true)) = &found {
            self.by_session.remove(session_id);
        }
        found.is_some()
    }

    /// Forgets the sends of every session `listed` says the daemon no
    /// longer holds.
    pub fn retain_sessions(&mut self, listed: impl Fn(&str) -> bool) {
        self.by_session.retain(|session_id, _| listed(session_id));
    }

    /// Forgets every send, as a new connection does.
    pub fn clear(&mut self) {
        self.by_session.clear();
    }
}

/// The overrides of `session`'s container: its workspace's when it belongs
/// to one, else its first member's repo's. A container the registry does
/// not hold sets nothing.
#[must_use]
pub fn container<'a>(
    session: &SessionSnapshot,
    repos: &'a [RepoEntry],
    workspaces: &'a [WorkspaceEntry],
) -> Option<&'a AppearanceOverrides> {
    if let Some(id) = session.workspace_id.as_deref() {
        return workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .map(|workspace| &workspace.appearance);
    }
    let id = session.members.first()?.repo_id.as_str();
    repos
        .iter()
        .find(|repo| repo.id == id)
        .map(|repo| &repo.appearance)
}

/// The session and container levels, most specific first.
type Levels<'a> = [(Option<&'a AppearanceOverrides>, Source); 2];

/// Every field of `session` (or of a pane with none) as it resolves: the
/// session's value, else its container's, else the app's, else the
/// built-in one. A stored colour that does not parse sets nothing.
#[must_use]
pub fn resolve(
    session: Option<&SessionSnapshot>,
    repos: &[RepoEntry],
    workspaces: &[WorkspaceEntry],
    app: AppLevel<'_>,
) -> Resolved {
    let levels: Levels<'_> = [
        (session.map(|s| &s.appearance), Source::Session),
        (
            session.and_then(|s| container(s, repos, workspaces)),
            Source::Container,
        ),
    ];
    let family = app.font.family.clone().map_or_else(
        || Field::new(None, Source::BuiltIn),
        |family| Field::new(Some(family), Source::App),
    );
    Resolved {
        accent: color(
            &levels,
            |a| a.accent_color.as_deref(),
            app.colors.accent_color.as_deref(),
            BUILTIN_ACCENT,
        ),
        background: color(
            &levels,
            |a| a.terminal_background_color.as_deref(),
            app.colors.terminal_background_color.as_deref(),
            BUILTIN_BACKGROUND,
        ),
        frame: colour_field(
            &levels,
            |a| a.terminal_frame_color.as_deref(),
            app.colors.terminal_frame_color.as_deref(),
        )
        .map_or(Field::new(None, Source::BuiltIn), |field| {
            Field::new(Some(field.value), field.source)
        }),
        font_family: first(&levels, |a| non_blank(a.terminal_font_family.as_deref()))
            .map_or(family, |field| Field::new(Some(field.value), field.source)),
        font_size: first(&levels, |a| {
            a.terminal_font_size
                .map(|size| fonts::clamp_size(f32::from(size)))
        })
        .unwrap_or_else(|| Field::new(fonts::clamp_size(app.font.size), Source::App)),
        font_bold: first(&levels, |a| a.terminal_font_bold)
            .unwrap_or_else(|| Field::new(app.font.bold, Source::App)),
    }
}

/// The first level `pick` finds a value in.
fn first<T>(
    levels: &Levels<'_>,
    pick: impl Fn(&AppearanceOverrides) -> Option<T>,
) -> Option<Field<T>> {
    levels.iter().find_map(|(level, source)| {
        level
            .and_then(&pick)
            .map(|value| Field::new(value, *source))
    })
}

/// A colour field: the first level whose value parses, else the app's,
/// else `builtin`.
fn color(
    levels: &Levels<'_>,
    pick: impl Fn(&AppearanceOverrides) -> Option<&str>,
    app: Option<&str>,
    builtin: u32,
) -> Field<u32> {
    colour_field(levels, pick, app).unwrap_or_else(|| Field::new(builtin, Source::BuiltIn))
}

/// A colour field: the first level whose value parses, else the app's.
fn colour_field(
    levels: &Levels<'_>,
    pick: impl Fn(&AppearanceOverrides) -> Option<&str>,
    app: Option<&str>,
) -> Option<Field<u32>> {
    first(levels, |a| pick(a).and_then(parse_color)).or_else(|| {
        app.and_then(parse_color)
            .map(|c| Field::new(c, Source::App))
    })
}

fn non_blank(family: Option<&str>) -> Option<String> {
    let trimmed = family?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// `raw` as `0xRRGGBB` when it is `#RRGGBB` (any case, surrounding space
/// ignored).
#[must_use]
pub fn parse_color(raw: &str) -> Option<u32> {
    let hex = raw.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// `color` as the `#rrggbb` the daemon stores.
#[must_use]
pub fn hex(color: u32) -> String {
    format!("#{:06x}", color & 0x00ff_ffff)
}

/// `color` as the terminal's colour type.
#[must_use]
pub fn term_rgb(color: u32) -> Rgb {
    let [_, r, g, b] = color.to_be_bytes();
    Rgb { r, g, b }
}

/// The terminal's colour type as `0xRRGGBB`.
#[must_use]
pub const fn packed(color: Rgb) -> u32 {
    u32::from_be_bytes([0, color.r, color.g, color.b])
}

/// Puts `color` first in `recent`, dropping its older entry and anything
/// past [`RECENT_LIMIT`]; returns whether it went in. A colour that is not
/// `#RRGGBB` does not.
pub fn push_recent(recent: &mut Vec<String>, color: &str) -> bool {
    let Some(value) = parse_color(color) else {
        return false;
    };
    let color = hex(value);
    recent.retain(|entry| parse_color(entry) != Some(value));
    recent.insert(0, color);
    recent.truncate(RECENT_LIMIT);
    true
}

/// The recent colours to offer: those that parse, each once, newest first,
/// at most [`RECENT_LIMIT`].
#[must_use]
pub fn recent_swatches(recent: &[String]) -> Vec<u32> {
    let mut swatches: Vec<u32> = Vec::new();
    for color in recent.iter().filter_map(|entry| parse_color(entry)) {
        if !swatches.contains(&color) {
            swatches.push(color);
        }
    }
    swatches.truncate(RECENT_LIMIT);
    swatches
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

    fn repo_with(id: &str, appearance: serde_json::Value) -> RepoEntry {
        let mut repo: RepoEntry = serde_json::from_value(json!({
            "id": id,
            "name": id,
            "path": "C:/repos/r",
        }))
        .expect("repo fixture");
        repo.appearance = overrides(appearance);
        repo
    }

    fn workspace_with(id: &str, appearance: serde_json::Value) -> WorkspaceEntry {
        let mut workspace: WorkspaceEntry = serde_json::from_value(json!({
            "id": id,
            "name": id,
            "member_repo_ids": ["r1"],
        }))
        .expect("workspace fixture");
        workspace.appearance = overrides(appearance);
        workspace
    }

    fn overrides(value: serde_json::Value) -> AppearanceOverrides {
        serde_json::from_value(value).expect("appearance fixture")
    }

    /// Resolves session `s1` of repo `r1` with `own` overrides, repo `r1`'s
    /// `repo` overrides and the app level `colors` and `font`.
    fn resolve_levels(
        own: serde_json::Value,
        repo: serde_json::Value,
        colors: &AppColors,
        font: &FontSettings,
    ) -> Resolved {
        let mut session = session_with(None, &["r1"]);
        session.appearance = overrides(own);
        let repos = [repo_with("r1", repo)];
        resolve(Some(&session), &repos, &[], AppLevel { colors, font })
    }

    fn app_colors(color: &str) -> AppColors {
        AppColors {
            accent_color: Some(color.to_owned()),
            terminal_background_color: Some(color.to_owned()),
            terminal_frame_color: Some(color.to_owned()),
        }
    }

    const SESSION_ALL: &str = "#111111";
    const REPO_ALL: &str = "#222222";

    fn every_field(color: &str, family: &str, size: u16, bold: bool) -> serde_json::Value {
        json!({
            "accent_color": color,
            "terminal_background_color": color,
            "terminal_frame_color": color,
            "terminal_font_family": family,
            "terminal_font_size": size,
            "terminal_font_bold": bold,
        })
    }

    fn colors_of(resolved: &Resolved) -> [(Option<u32>, Source); 3] {
        [
            (Some(resolved.accent.value), resolved.accent.source),
            (Some(resolved.background.value), resolved.background.source),
            (resolved.frame.value, resolved.frame.source),
        ]
    }

    #[test]
    fn every_field_prefers_the_session_then_the_container() {
        let font = FontSettings {
            family: Some("App Mono".to_owned()),
            size: 13.0,
            bold: false,
        };
        let colors = app_colors("#333333");
        let both = resolve_levels(
            every_field(SESSION_ALL, "Session Mono", 16, true),
            every_field(REPO_ALL, "Repo Mono", 20, false),
            &colors,
            &font,
        );
        assert_eq!(colors_of(&both), [(Some(0x111111), Source::Session); 3]);
        assert_eq!(
            both.font_family,
            Field::new(Some("Session Mono".to_owned()), Source::Session)
        );
        assert_eq!(both.font_size, Field::new(16.0, Source::Session));
        assert_eq!(both.font_bold, Field::new(true, Source::Session));

        let repo_only = resolve_levels(
            json!({}),
            every_field(REPO_ALL, "Repo Mono", 20, true),
            &colors,
            &font,
        );
        assert_eq!(
            colors_of(&repo_only),
            [(Some(0x222222), Source::Container); 3]
        );
        assert_eq!(
            repo_only.font_family,
            Field::new(Some("Repo Mono".to_owned()), Source::Container)
        );
        assert_eq!(repo_only.font_size, Field::new(20.0, Source::Container));
        assert_eq!(repo_only.font_bold, Field::new(true, Source::Container));
    }

    #[test]
    fn unset_levels_fall_to_the_app_then_the_built_in_values() {
        let font = FontSettings {
            family: Some("App Mono".to_owned()),
            size: 18.0,
            bold: true,
        };
        let app = resolve_levels(json!({}), json!({}), &app_colors("#333333"), &font);
        assert_eq!(colors_of(&app), [(Some(0x333333), Source::App); 3]);
        assert_eq!(
            app.font_family,
            Field::new(Some("App Mono".to_owned()), Source::App)
        );
        assert_eq!(app.font_size, Field::new(18.0, Source::App));
        assert_eq!(app.font_bold, Field::new(true, Source::App));

        let built_in = resolve_levels(
            json!({}),
            json!({}),
            &AppColors::default(),
            &FontSettings::default(),
        );
        assert_eq!(
            colors_of(&built_in),
            [
                (Some(BUILTIN_ACCENT), Source::BuiltIn),
                (Some(BUILTIN_BACKGROUND), Source::BuiltIn),
                (None, Source::BuiltIn),
            ],
            "an unset frame follows the terminal's background"
        );
        assert_eq!(built_in.font_family, Field::new(None, Source::BuiltIn));
        assert_eq!(
            built_in.font_size,
            Field::new(fonts::DEFAULT_SIZE, Source::App),
            "the app font always carries a size"
        );
    }

    #[test]
    fn each_field_resolves_on_its_own() {
        let resolved = resolve_levels(
            json!({ "accent_color": SESSION_ALL }),
            json!({ "terminal_frame_color": REPO_ALL }),
            &AppColors::default(),
            &FontSettings::default(),
        );
        assert_eq!(
            colors_of(&resolved),
            [
                (Some(0x111111), Source::Session),
                (Some(BUILTIN_BACKGROUND), Source::BuiltIn),
                (Some(0x222222), Source::Container),
            ]
        );
    }

    #[test]
    fn a_colour_that_does_not_parse_sets_nothing() {
        let resolved = resolve_levels(
            json!({ "accent_color": "blue" }),
            json!({ "accent_color": "#ABCDEF" }),
            &AppColors::default(),
            &FontSettings::default(),
        );
        assert_eq!(resolved.accent, Field::new(0xabcdef, Source::Container));
    }

    #[test]
    fn a_pane_with_no_session_takes_the_app_level() {
        let colors = app_colors("#333333");
        let font = FontSettings::default();
        let resolved = resolve(
            None,
            &[],
            &[],
            AppLevel {
                colors: &colors,
                font: &font,
            },
        );
        assert_eq!(colors_of(&resolved), [(Some(0x333333), Source::App); 3]);
    }

    #[test]
    fn resolution_prefers_tab_then_session_then_container_then_app() {
        let font = FontSettings::default();
        let size = |tab: Option<f32>, own: serde_json::Value, repo: serde_json::Value| {
            resolve_levels(own, repo, &AppColors::default(), &font)
                .with_tab_size(tab)
                .font_size
        };
        let own = || json!({ "terminal_font_size": 16 });
        let repo = || json!({ "terminal_font_size": 15 });
        assert_eq!(
            size(Some(20.0), own(), repo()),
            Field::new(20.0, Source::Tab)
        );
        assert_eq!(size(None, own(), repo()), Field::new(16.0, Source::Session));
        assert_eq!(
            size(None, json!({}), repo()),
            Field::new(15.0, Source::Container)
        );
        let app = FontSettings {
            size: 18.0,
            ..FontSettings::default()
        };
        assert_eq!(
            resolve_levels(json!({}), json!({}), &AppColors::default(), &app).font_size,
            Field::new(18.0, Source::App)
        );
    }

    #[test]
    fn resolution_clamps_the_winning_level() {
        let default = FontSettings::default();
        let colors = AppColors::default();
        let tab = resolve_levels(json!({}), json!({}), &colors, &default).with_tab_size(Some(99.0));
        assert_eq!(tab.font_size, Field::new(fonts::MAX_SIZE, Source::Tab));
        let own = resolve_levels(
            json!({ "terminal_font_size": 1 }),
            json!({ "terminal_font_size": 9 }),
            &colors,
            &default,
        );
        assert_eq!(own.font_size, Field::new(fonts::MIN_SIZE, Source::Session));
        let nan = FontSettings {
            size: f32::NAN,
            ..FontSettings::default()
        };
        assert_eq!(
            resolve_levels(json!({}), json!({}), &colors, &nan).font_size,
            Field::new(fonts::DEFAULT_SIZE, Source::App),
            "an app size that is not a number is the default"
        );
    }

    #[test]
    fn a_workspace_container_wins_over_the_first_members_repo() {
        let repos = [repo_with("r1", json!({ "accent_color": "#200000" }))];
        let workspaces = [workspace_with("w1", json!({ "accent_color": "#160000" }))];
        let accent = |session: &SessionSnapshot| {
            container(session, &repos, &workspaces).and_then(|a| a.accent_color.clone())
        };
        assert_eq!(
            accent(&session_with(Some("w1"), &["r1"])).as_deref(),
            Some("#160000"),
            "the session's workspace is its container"
        );
        assert_eq!(
            accent(&session_with(None, &["r1"])).as_deref(),
            Some("#200000"),
            "without a workspace, its first member's repo is"
        );
        assert_eq!(
            accent(&session_with(None, &["r1", "r2"])).as_deref(),
            Some("#200000"),
            "the first member's repo, not any member's"
        );
        assert_eq!(
            accent(&session_with(None, &["r2", "r1"])),
            None,
            "a first member the registry does not hold sets nothing"
        );
        assert_eq!(
            accent(&session_with(Some("w9"), &["r1"])),
            None,
            "a container the registry does not hold sets nothing"
        );
    }

    #[test]
    fn colours_parse_and_print_as_hex() {
        assert_eq!(parse_color("#5B9BFF"), Some(0x5b9bff));
        assert_eq!(parse_color("5b9bff"), None);
        assert_eq!(hex(0x0a0b0c), "#0a0b0c");
        assert_eq!(
            term_rgb(0xf6f4ef),
            Rgb {
                r: 0xf6,
                g: 0xf4,
                b: 0xef
            }
        );
        for preset in ACCENT_PRESETS.iter().chain(&BACKGROUND_PRESETS) {
            assert_eq!(parse_color(&hex(preset.color)), Some(preset.color));
        }
    }

    #[test]
    fn recent_colours_put_the_newest_first_once_and_keep_twelve() {
        let mut recent = Vec::new();
        for n in 0..14_u32 {
            assert!(push_recent(&mut recent, &hex(n)));
        }
        assert_eq!(recent.len(), RECENT_LIMIT, "the oldest two went");
        assert_eq!(recent.first().map(String::as_str), Some("#00000d"));
        assert_eq!(recent.last().map(String::as_str), Some("#000002"));

        assert!(push_recent(&mut recent, "#00000A"));
        assert_eq!(recent.len(), RECENT_LIMIT, "a repeat moves, not adds");
        assert_eq!(
            recent.first().map(String::as_str),
            Some("#00000a"),
            "stored lowercase"
        );
        assert_eq!(
            recent.iter().filter(|c| *c == "#00000a").count(),
            1,
            "its older entry went"
        );

        let before = recent.clone();
        assert!(!push_recent(&mut recent, "teal"), "not a colour");
        assert_eq!(recent, before);
    }

    #[test]
    fn recent_swatches_skip_bad_and_repeated_entries() {
        let stored: Vec<String> = ["#AA0000", "junk", "#aa0000", "#00bb00"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(recent_swatches(&stored), [0xaa0000, 0x00bb00]);
        let many: Vec<String> = (0..20_u32).map(hex).collect();
        assert_eq!(recent_swatches(&many).len(), RECENT_LIMIT);
    }

    #[test]
    fn ui_state_round_trips_the_app_colours_and_recent_colours() {
        let state = UiState {
            app_appearance: app_colors("#123456"),
            recent_colors: vec!["#abcdef".to_owned(), "#000001".to_owned()],
            ..UiState::default()
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let back: UiState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);

        let older: UiState = serde_json::from_str(
            r#"{ "sidebar_width": 300.0, "terminal_font": { "size": 20.0 } }"#,
        )
        .expect("a file from before the appearance was saved");
        assert_eq!(older.app_appearance, AppColors::default());
        assert!(older.recent_colors.is_empty());

        let partial: UiState =
            serde_json::from_str(r##"{ "app_appearance": { "accent_color": "#38bdf8" } }"##)
                .expect("an app level with only its accent");
        assert_eq!(
            partial.app_appearance,
            AppColors {
                accent_color: Some("#38bdf8".to_owned()),
                ..AppColors::default()
            }
        );
    }
}
