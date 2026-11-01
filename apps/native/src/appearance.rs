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
use crate::palette;
use crate::theme;
use crate::ui_theme::UiTheme;

/// The built-in theme's accent, which a session has when no level sets one
/// and that theme is active.
pub const BUILTIN_ACCENT: u32 = palette::ACCENT;
/// The built-in theme's terminal background, which a session has when no
/// level sets one and that theme is active.
pub const BUILTIN_BACKGROUND: u32 = packed(theme::DEFAULT_BACKGROUND);
/// How many recent custom colours are kept.
pub const RECENT_LIMIT: usize = 12;

/// A named colour offered as a one-click choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    /// `0xRRGGBB`; `None` is the active theme's own colour, which a level
    /// gets by setting nothing.
    pub color: Option<u32>,
}

impl Preset {
    /// What applying this preset stores at a level: its `#rrggbb`, or
    /// nothing, which clears the level's override.
    #[must_use]
    pub fn stored(&self) -> Option<String> {
        self.color.map(hex)
    }

    /// The colour its swatch shows while `theme_color` is the active
    /// theme's colour for the row.
    #[must_use]
    pub fn shown(&self, theme_color: u32) -> u32 {
        self.color.unwrap_or(theme_color)
    }
}

/// The accent choices, which the quick menu applies to the frame as well.
/// The first is the active theme's accent.
pub const ACCENT_PRESETS: [Preset; 6] = [
    Preset {
        name: "Theme",
        color: None,
    },
    Preset {
        name: "Rose",
        color: Some(0xe7849b),
    },
    Preset {
        name: "Ochre",
        color: Some(0xd9a15c),
    },
    Preset {
        name: "Periwinkle",
        color: Some(0x8fb3f0),
    },
    Preset {
        name: "Sky",
        color: Some(0x8fd3e8),
    },
    Preset {
        name: "Lilac",
        color: Some(0xc9b8ff),
    },
];

/// The terminal background choices. The first is the active theme's
/// terminal background.
pub const BACKGROUND_PRESETS: [Preset; 6] = [
    Preset {
        name: "Default",
        color: None,
    },
    Preset {
        name: "Slate",
        color: Some(0x111318),
    },
    Preset {
        name: "Ink",
        color: Some(0x0b1020),
    },
    Preset {
        name: "Evergreen",
        color: Some(0x07150f),
    },
    Preset {
        name: "Aubergine",
        color: Some(0x1a1024),
    },
    Preset {
        name: "Paper",
        color: Some(0xf6f4ef),
    },
];

/// The app-level colours releases up to v0.5.2-alpha kept in
/// `native-ui.json`, read only to move them into a theme
/// ([`crate::theme_store::migrate_app_colors`]); the app's colours are its
/// theme's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppColors {
    pub accent_color: Option<String>,
    pub terminal_background_color: Option<String>,
    pub terminal_frame_color: Option<String>,
}

impl AppColors {
    /// Whether no colour is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The bottom of the cascade: the active theme's accent and terminal
/// background, `0xRRGGBB`, which a session has when no level sets them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeBase {
    pub accent: u32,
    pub background: u32,
}

impl ThemeBase {
    /// The accent and terminal background of `theme`.
    #[must_use]
    pub const fn of(theme: &UiTheme) -> Self {
        Self {
            accent: theme.accent,
            background: packed(theme.terminal.bg),
        }
    }
}

impl Default for ThemeBase {
    /// The built-in theme's.
    fn default() -> Self {
        Self::of(&UiTheme::CONTRAST)
    }
}

/// The app level: its font, over the active theme's base, which is where a
/// colour no level sets comes from.
#[derive(Debug, Clone, Copy)]
pub struct AppLevel<'a> {
    pub font: &'a FontSettings,
    pub theme: ThemeBase,
    /// The app's terminal frame width: the stored one, else
    /// [`DEFAULT_FRAME_WIDTH`].
    pub frame_width: u16,
}

/// The width in px of the frame band a terminal draws when no level sets
/// one.
pub const DEFAULT_FRAME_WIDTH: u16 = 1;

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
    /// One of [`fonts::TERMINAL_WEIGHTS`].
    pub font_weight: Field<u16>,
    /// The frame band's width in px, within
    /// [`protocol::TERMINAL_FRAME_WIDTHS`].
    pub frame_width: Field<u16>,
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
            weight: self.font_weight.value,
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
    /// The weight, and with it the older bold switch, which a weight
    /// change clears so a reset falls back to the next level.
    FontWeight,
    FrameWidth,
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
            Self::FontWeight => {
                to.terminal_font_weight = from.terminal_font_weight;
                to.terminal_font_bold = from.terminal_font_bold;
            }
            Self::FrameWidth => to.terminal_frame_width = from.terminal_frame_width,
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
            Self::FontWeight => {
                a.terminal_font_weight == b.terminal_font_weight
                    && a.terminal_font_bold == b.terminal_font_bold
            }
            Self::FrameWidth => a.terminal_frame_width == b.terminal_frame_width,
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

    /// The terminal background set to `color`, or cleared.
    #[must_use]
    pub fn background(color: Option<String>) -> Self {
        Self {
            values: AppearanceOverrides {
                terminal_background_color: color,
                ..AppearanceOverrides::default()
            },
            fields: vec![AppearanceField::Background],
        }
    }

    /// The font family set to `family`, or cleared.
    #[must_use]
    pub fn font_family(family: Option<String>) -> Self {
        Self {
            values: AppearanceOverrides {
                terminal_font_family: family,
                ..AppearanceOverrides::default()
            },
            fields: vec![AppearanceField::FontFamily],
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

    /// The weight of normal text set to `weight`, or cleared; either way
    /// the older bold switch is cleared.
    #[must_use]
    pub fn font_weight(weight: Option<u16>) -> Self {
        Self {
            values: AppearanceOverrides {
                terminal_font_weight: weight,
                ..AppearanceOverrides::default()
            },
            fields: vec![AppearanceField::FontWeight],
        }
    }

    /// The frame band's width set to `width`, or cleared.
    #[must_use]
    pub fn frame_width(width: Option<u16>) -> Self {
        Self {
            values: AppearanceOverrides {
                terminal_frame_width: width,
                ..AppearanceOverrides::default()
            },
            fields: vec![AppearanceField::FrameWidth],
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
    /// every send before it. Returns whether `request_id` was one of this
    /// client's sends in flight.
    pub fn answered(&mut self, session_id: &str, request_id: &str) -> bool {
        let Some(sends) = self.by_session.get_mut(session_id) else {
            return false;
        };
        let at = sends.iter().position(|(id, _)| id == request_id);
        if let Some(at) = at {
            sends.drain(..=at);
        }
        if sends.is_empty() {
            self.by_session.remove(session_id);
        }
        at.is_some()
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
    resolve_levels(&levels, app)
}

/// Every field as a repo or workspace whose own overrides are `container`
/// resolves it: its value, else the app's, else the built-in one.
#[must_use]
pub fn resolve_container(container: &AppearanceOverrides, app: AppLevel<'_>) -> Resolved {
    resolve_levels(
        &[
            (Some(container), Source::Container),
            (None, Source::Session),
        ],
        app,
    )
}

/// Every field from the first of `levels` that sets it, else the app's,
/// else the built-in one. An app size and an app weight at their defaults
/// set nothing, so they report the built-in level.
fn resolve_levels(levels: &Levels<'_>, app: AppLevel<'_>) -> Resolved {
    let app_size = fonts::clamp_size(app.font.size);
    let size_source = if (app_size - fonts::DEFAULT_SIZE).abs() < f32::EPSILON {
        Source::BuiltIn
    } else {
        Source::App
    };
    let app_weight = fonts::offered_terminal_weight(app.font.weight);
    let weight_source = if app_weight == fonts::DEFAULT_TERMINAL_WEIGHT {
        Source::BuiltIn
    } else {
        Source::App
    };
    let frame_width_source = if app.frame_width == DEFAULT_FRAME_WIDTH {
        Source::BuiltIn
    } else {
        Source::App
    };
    let family = app.font.family.clone().map_or_else(
        || Field::new(None, Source::BuiltIn),
        |family| Field::new(Some(family), Source::App),
    );
    Resolved {
        accent: color(levels, |a| a.accent_color.as_deref(), app.theme.accent),
        background: color(
            levels,
            |a| a.terminal_background_color.as_deref(),
            app.theme.background,
        ),
        frame: colour_field(levels, |a| a.terminal_frame_color.as_deref())
            .map_or(Field::new(None, Source::BuiltIn), |field| {
                Field::new(Some(field.value), field.source)
            }),
        font_family: first(levels, |a| non_blank(a.terminal_font_family.as_deref()))
            .map_or(family, |field| Field::new(Some(field.value), field.source)),
        font_size: first(levels, |a| {
            a.terminal_font_size
                .map(|size| fonts::clamp_size(f32::from(size)))
        })
        .unwrap_or_else(|| Field::new(app_size, size_source)),
        font_weight: first(levels, level_weight)
            .unwrap_or_else(|| Field::new(app_weight, weight_source)),
        frame_width: first(levels, |a| {
            a.terminal_frame_width
                .filter(|width| protocol::TERMINAL_FRAME_WIDTHS.contains(width))
        })
        .unwrap_or_else(|| Field::new(app.frame_width, frame_width_source)),
    }
}

/// The weight level `own` sets: its weight when it is offered, else what
/// its older bold switch stands for (on is Bold, off Regular).
fn level_weight(own: &AppearanceOverrides) -> Option<u16> {
    own.terminal_font_weight
        .filter(|weight| fonts::TERMINAL_WEIGHTS.contains(weight))
        .or_else(|| {
            own.terminal_font_bold.map(|bold| {
                if bold {
                    fonts::BOLD_TERMINAL_WEIGHT
                } else {
                    fonts::DEFAULT_TERMINAL_WEIGHT
                }
            })
        })
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

/// A colour field: the first level whose value parses, else the theme's
/// `builtin`.
fn color(
    levels: &Levels<'_>,
    pick: impl Fn(&AppearanceOverrides) -> Option<&str>,
    builtin: u32,
) -> Field<u32> {
    colour_field(levels, pick).unwrap_or_else(|| Field::new(builtin, Source::BuiltIn))
}

/// A colour field: the first level whose value parses.
fn colour_field(
    levels: &Levels<'_>,
    pick: impl Fn(&AppearanceOverrides) -> Option<&str>,
) -> Option<Field<u32>> {
    first(levels, |a| pick(a).and_then(parse_color))
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

/// What the user typed in a colour field as the `#rrggbb` to store: six
/// hex digits, with or without the `#`, in any case, surrounding space
/// ignored.
#[must_use]
pub fn parse_hex_input(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let digits = trimmed.strip_prefix('#').unwrap_or(trimmed);
    parse_color(&format!("#{digits}")).map(hex)
}

/// The app level as overrides, so a change applies to it as to any other
/// level: it sets no colour, a size and a weight at their defaults set
/// nothing, and the older bold switch is never written.
#[must_use]
pub fn app_overrides(app: AppLevel<'_>) -> AppearanceOverrides {
    let size = fonts::clamp_size(app.font.size);
    let whole = crate::font_size_to_u16(size);
    AppearanceOverrides {
        accent_color: None,
        terminal_background_color: None,
        terminal_frame_color: None,
        terminal_font_family: app.font.family.clone(),
        terminal_font_size: ((size - fonts::DEFAULT_SIZE).abs() >= f32::EPSILON).then_some(whole),
        terminal_font_bold: None,
        terminal_font_weight: Some(fonts::offered_terminal_weight(app.font.weight))
            .filter(|weight| *weight != fonts::DEFAULT_TERMINAL_WEIGHT),
        terminal_frame_width: (app.frame_width != DEFAULT_FRAME_WIDTH).then_some(app.frame_width),
    }
}

/// The app level's frame width `overrides` stands for: its width when it is
/// within [`protocol::TERMINAL_FRAME_WIDTHS`], else [`DEFAULT_FRAME_WIDTH`].
#[must_use]
pub fn app_frame_width_from_overrides(overrides: &AppearanceOverrides) -> u16 {
    overrides
        .terminal_frame_width
        .filter(|width| protocol::TERMINAL_FRAME_WIDTHS.contains(width))
        .unwrap_or(DEFAULT_FRAME_WIDTH)
}

/// The app level's font `overrides` stands for: an unset size is the
/// default, and an unset weight is Bold when the older bold switch is on,
/// else Regular.
#[must_use]
pub fn app_font_from_overrides(overrides: &AppearanceOverrides) -> FontSettings {
    let font = FontSettings {
        family: overrides.terminal_font_family.clone(),
        size: overrides
            .terminal_font_size
            .map_or(fonts::DEFAULT_SIZE, f32::from),
        weight: level_weight(overrides).unwrap_or(fonts::DEFAULT_TERMINAL_WEIGHT),
    };
    font.normalized()
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
    /// `repo` overrides and the app level's `font`.
    fn resolve_levels(
        own: serde_json::Value,
        repo: serde_json::Value,
        font: &FontSettings,
    ) -> Resolved {
        let mut session = session_with(None, &["r1"]);
        session.appearance = overrides(own);
        let repos = [repo_with("r1", repo)];
        resolve(
            Some(&session),
            &repos,
            &[],
            AppLevel {
                font,
                theme: ThemeBase::default(),
                frame_width: DEFAULT_FRAME_WIDTH,
            },
        )
    }

    const SESSION_ALL: &str = "#111111";
    const REPO_ALL: &str = "#222222";

    fn every_field(color: &str, family: &str, size: u16, weight: u16) -> serde_json::Value {
        json!({
            "accent_color": color,
            "terminal_background_color": color,
            "terminal_frame_color": color,
            "terminal_font_family": family,
            "terminal_font_size": size,
            "terminal_font_weight": weight,
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
            weight: 400,
        };
        let both = resolve_levels(
            every_field(SESSION_ALL, "Session Mono", 16, 500),
            every_field(REPO_ALL, "Repo Mono", 20, 600),
            &font,
        );
        assert_eq!(colors_of(&both), [(Some(0x111111), Source::Session); 3]);
        assert_eq!(
            both.font_family,
            Field::new(Some("Session Mono".to_owned()), Source::Session)
        );
        assert_eq!(both.font_size, Field::new(16.0, Source::Session));
        assert_eq!(both.font_weight, Field::new(500, Source::Session));

        let repo_only = resolve_levels(
            json!({}),
            every_field(REPO_ALL, "Repo Mono", 20, 600),
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
        assert_eq!(repo_only.font_weight, Field::new(600, Source::Container));
    }

    #[test]
    fn an_older_bold_switch_reads_as_bold_until_a_weight_is_set() {
        let font = FontSettings {
            weight: 500,
            ..FontSettings::default()
        };
        let weight = |session: serde_json::Value, repo: serde_json::Value| {
            resolve_levels(session, repo, &font).font_weight
        };
        assert_eq!(
            weight(json!({}), json!({"terminal_font_bold": true})),
            Field::new(700, Source::Container),
            "a bold-only level reads as Bold"
        );
        assert_eq!(
            weight(
                json!({"terminal_font_bold": false}),
                json!({"terminal_font_bold": true})
            ),
            Field::new(400, Source::Session),
            "a bold switch that is off reads as Regular"
        );
        assert_eq!(
            weight(
                json!({}),
                json!({"terminal_font_bold": true, "terminal_font_weight": 600})
            ),
            Field::new(600, Source::Container),
            "the weight wins over the switch"
        );
        assert_eq!(
            weight(json!({"terminal_font_weight": 450}), json!({})),
            Field::new(500, Source::App),
            "an unoffered weight sets nothing"
        );

        let own = overrides(json!({"terminal_font_bold": true}));
        let set = AppearanceChange::font_weight(Some(600)).apply_to(&own);
        assert_eq!(set.terminal_font_weight, Some(600));
        assert_eq!(
            set.terminal_font_bold, None,
            "a weight change clears the switch"
        );
        let reset = AppearanceChange::font_weight(None).apply_to(&own);
        assert_eq!(level_weight(&reset), None, "a reset leaves nothing behind");
    }

    #[test]
    fn unset_levels_fall_to_the_app_then_the_built_in_values() {
        let font = FontSettings {
            family: Some("App Mono".to_owned()),
            size: 18.0,
            weight: 700,
        };
        let app = resolve_levels(json!({}), json!({}), &font);
        assert_eq!(
            colors_of(&app),
            [
                (Some(BUILTIN_ACCENT), Source::BuiltIn),
                (Some(BUILTIN_BACKGROUND), Source::BuiltIn),
                (None, Source::BuiltIn),
            ],
            "the app sets no colour: the theme's"
        );
        assert_eq!(
            app.font_family,
            Field::new(Some("App Mono".to_owned()), Source::App)
        );
        assert_eq!(app.font_size, Field::new(18.0, Source::App));
        assert_eq!(app.font_weight, Field::new(700, Source::App));

        let built_in = resolve_levels(json!({}), json!({}), &FontSettings::default());
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
            Field::new(fonts::DEFAULT_SIZE, Source::BuiltIn),
            "an app size at the default sets nothing"
        );
        assert_eq!(
            built_in.font_weight,
            Field::new(400, Source::BuiltIn),
            "an app weight at Regular sets nothing"
        );
    }

    #[test]
    fn a_container_resolves_through_the_app_to_the_built_in_values() {
        let font = FontSettings {
            family: Some("App Mono".to_owned()),
            size: 18.0,
            weight: 400,
        };
        let app = AppLevel {
            font: &font,
            theme: ThemeBase::default(),
            frame_width: DEFAULT_FRAME_WIDTH,
        };
        let own = overrides(json!({
            "terminal_background_color": REPO_ALL,
            "terminal_font_weight": 600,
        }));
        let resolved = resolve_container(&own, app);
        assert_eq!(resolved.background, Field::new(0x222222, Source::Container));
        assert_eq!(resolved.font_weight, Field::new(600, Source::Container));
        assert_eq!(resolved.accent, Field::new(BUILTIN_ACCENT, Source::BuiltIn));
        assert_eq!(
            resolved.font_family,
            Field::new(Some("App Mono".to_owned()), Source::App)
        );
        assert_eq!(resolved.font_size, Field::new(18.0, Source::App));
        assert_eq!(resolved.frame, Field::new(None, Source::BuiltIn));

        let bare = resolve_container(&AppearanceOverrides::default(), app);
        assert_eq!(
            bare.font_weight,
            Field::new(400, Source::BuiltIn),
            "an app weight at Regular is the built-in value"
        );
    }

    #[test]
    fn the_cascades_bottom_is_the_themes_accent_and_terminal_background() {
        let mut ui = UiTheme::CONTRAST;
        ui.accent = 0x123456;
        ui.terminal.bg = Rgb {
            r: 0xfa,
            g: 0xf0,
            b: 0xe6,
        };
        let theme = ThemeBase::of(&ui);
        assert_eq!(
            theme,
            ThemeBase {
                accent: 0x123456,
                background: 0xfaf0e6
            }
        );
        let font = FontSettings::default();
        let app = AppLevel {
            font: &font,
            theme,
            frame_width: DEFAULT_FRAME_WIDTH,
        };
        let resolved = resolve(None, &[], &[], app);
        assert_eq!(resolved.accent, Field::new(0x123456, Source::BuiltIn));
        assert_eq!(resolved.background, Field::new(0xfaf0e6, Source::BuiltIn));
        let container = resolve_container(&AppearanceOverrides::default(), app);
        assert_eq!(container.accent, Field::new(0x123456, Source::BuiltIn));
        assert_eq!(container.background, Field::new(0xfaf0e6, Source::BuiltIn));
        assert_eq!(
            ThemeBase::default(),
            ThemeBase {
                accent: BUILTIN_ACCENT,
                background: BUILTIN_BACKGROUND
            }
        );
    }

    #[test]
    fn the_app_level_resolves_to_the_built_in_values_it_does_not_set() {
        let font = FontSettings::default();
        let app = AppLevel {
            font: &font,
            theme: ThemeBase::default(),
            frame_width: DEFAULT_FRAME_WIDTH,
        };
        let resolved = resolve(None, &[], &[], app);
        assert_eq!(
            resolved.background,
            Field::new(BUILTIN_BACKGROUND, Source::BuiltIn)
        );
        assert_eq!(resolved.accent, Field::new(BUILTIN_ACCENT, Source::BuiltIn));
        assert_eq!(resolved.font_family, Field::new(None, Source::BuiltIn));
        assert_eq!(
            resolved.font_size,
            Field::new(fonts::DEFAULT_SIZE, Source::BuiltIn)
        );
        assert_eq!(resolved.font_weight, Field::new(400, Source::BuiltIn));

        let set = FontSettings {
            family: None,
            size: 20.0,
            weight: 600,
        };
        let resolved = resolve(
            None,
            &[],
            &[],
            AppLevel {
                font: &set,
                theme: ThemeBase::default(),
                frame_width: DEFAULT_FRAME_WIDTH,
            },
        );
        assert_eq!(resolved.font_size, Field::new(20.0, Source::App));
        assert_eq!(resolved.font_weight, Field::new(600, Source::App));
    }

    #[test]
    fn the_app_level_round_trips_through_overrides() {
        let font = FontSettings {
            family: Some("Fira Code".to_owned()),
            size: 20.0,
            weight: 500,
        };
        let own = app_overrides(AppLevel {
            font: &font,
            theme: ThemeBase::default(),
            frame_width: DEFAULT_FRAME_WIDTH,
        });
        assert_eq!(own.terminal_font_size, Some(20));
        assert_eq!(own.terminal_font_weight, Some(500));
        assert_eq!(own.terminal_font_bold, None, "only the weight is written");
        assert_eq!(own.accent_color, None, "the app level sets no colour");
        assert_eq!(app_font_from_overrides(&own), font);

        let older = overrides(json!({"terminal_font_bold": true}));
        assert_eq!(
            app_font_from_overrides(&older).weight,
            700,
            "bold-only reads as Bold"
        );

        let plain = app_overrides(AppLevel {
            font: &FontSettings::default(),
            theme: ThemeBase::default(),
            frame_width: DEFAULT_FRAME_WIDTH,
        });
        assert_eq!(
            plain,
            AppearanceOverrides::default(),
            "the defaults set nothing"
        );
        let cleared = AppearanceChange::font_size(None).apply_to(&own);
        assert_eq!(
            app_font_from_overrides(&cleared),
            FontSettings {
                size: fonts::DEFAULT_SIZE,
                ..font
            },
            "a cleared size is the default"
        );
    }

    /// The frame width session `s1` of repo `r1` resolves to with `own`
    /// overrides, repo `r1`'s `repo` overrides and the app's `app_width`.
    fn frame_width_over(
        own: serde_json::Value,
        repo: serde_json::Value,
        app_width: u16,
    ) -> Field<u16> {
        let mut session = session_with(None, &["r1"]);
        session.appearance = overrides(own);
        let repos = [repo_with("r1", repo)];
        let font = FontSettings::default();
        resolve(
            Some(&session),
            &repos,
            &[],
            AppLevel {
                font: &font,
                theme: ThemeBase::default(),
                frame_width: app_width,
            },
        )
        .frame_width
    }

    #[test]
    fn frame_width_prefers_the_session_then_the_container_then_the_app() {
        let own = || json!({ "terminal_frame_width": 4 });
        let repo = || json!({ "terminal_frame_width": 2 });
        assert_eq!(
            frame_width_over(own(), repo(), 5),
            Field::new(4, Source::Session)
        );
        assert_eq!(
            frame_width_over(json!({}), repo(), 5),
            Field::new(2, Source::Container)
        );
        assert_eq!(
            frame_width_over(json!({}), json!({}), 5),
            Field::new(5, Source::App)
        );
    }

    #[test]
    fn frame_width_set_nowhere_is_the_built_in_one_pixel() {
        assert_eq!(
            frame_width_over(json!({}), json!({}), DEFAULT_FRAME_WIDTH),
            Field::new(1, Source::BuiltIn)
        );
        let bare = resolve_container(
            &AppearanceOverrides::default(),
            AppLevel {
                font: &FontSettings::default(),
                theme: ThemeBase::default(),
                frame_width: DEFAULT_FRAME_WIDTH,
            },
        );
        assert_eq!(bare.frame_width, Field::new(1, Source::BuiltIn));
    }

    #[test]
    fn an_app_frame_width_of_three_reports_the_app_level() {
        assert_eq!(
            frame_width_over(json!({}), json!({}), 3),
            Field::new(3, Source::App)
        );
    }

    #[test]
    fn a_stored_frame_width_of_nine_falls_through() {
        assert_eq!(
            frame_width_over(
                json!({ "terminal_frame_width": 9 }),
                json!({ "terminal_frame_width": 2 }),
                DEFAULT_FRAME_WIDTH
            ),
            Field::new(2, Source::Container),
            "an out-of-range session width sets nothing"
        );
        assert_eq!(
            frame_width_over(
                json!({ "terminal_frame_width": 9 }),
                json!({}),
                DEFAULT_FRAME_WIDTH
            ),
            Field::new(1, Source::BuiltIn)
        );
    }

    #[test]
    fn app_frame_width_round_trips_through_overrides() {
        let font = FontSettings::default();
        let app = |frame_width| AppLevel {
            font: &font,
            theme: ThemeBase::default(),
            frame_width,
        };
        for width in [0, 4] {
            let own = app_overrides(app(width));
            assert_eq!(own.terminal_frame_width, Some(width));
            assert_eq!(app_frame_width_from_overrides(&own), width);
        }
        let plain = app_overrides(app(DEFAULT_FRAME_WIDTH));
        assert_eq!(
            plain.terminal_frame_width, None,
            "the default writes nothing"
        );
        assert_eq!(app_frame_width_from_overrides(&plain), DEFAULT_FRAME_WIDTH);
        let odd = overrides(json!({ "terminal_frame_width": 9 }));
        assert_eq!(app_frame_width_from_overrides(&odd), DEFAULT_FRAME_WIDTH);

        let set = AppearanceChange::frame_width(Some(4)).apply_to(&plain);
        assert_eq!(set.terminal_frame_width, Some(4));
        let cleared = AppearanceChange::frame_width(None).apply_to(&set);
        assert_eq!(cleared.terminal_frame_width, None);
        assert!(AppearanceChange::frame_width(None).changes_nothing_in(&cleared));
    }

    #[test]
    fn typed_hex_takes_six_digits_with_or_without_the_hash() {
        assert_eq!(parse_hex_input("#A1B2C3").as_deref(), Some("#a1b2c3"));
        assert_eq!(parse_hex_input("a1b2c3").as_deref(), Some("#a1b2c3"));
        assert_eq!(parse_hex_input("  ABCDEF ").as_deref(), Some("#abcdef"));
        for bad in [
            "", "#", "#12345", "1234567", "#1234567", "##123456", "#12345g", "red", "#12 456",
        ] {
            assert_eq!(parse_hex_input(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn each_field_resolves_on_its_own() {
        let resolved = resolve_levels(
            json!({ "accent_color": SESSION_ALL }),
            json!({ "terminal_frame_color": REPO_ALL }),
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
            &FontSettings::default(),
        );
        assert_eq!(resolved.accent, Field::new(0xabcdef, Source::Container));
    }

    #[test]
    fn a_pane_with_no_session_takes_the_app_level() {
        let font = FontSettings {
            size: 18.0,
            ..FontSettings::default()
        };
        let resolved = resolve(
            None,
            &[],
            &[],
            AppLevel {
                font: &font,
                theme: ThemeBase::default(),
                frame_width: DEFAULT_FRAME_WIDTH,
            },
        );
        assert_eq!(resolved.font_size, Field::new(18.0, Source::App));
        assert_eq!(resolved.accent, Field::new(BUILTIN_ACCENT, Source::BuiltIn));
    }

    #[test]
    fn resolution_prefers_tab_then_session_then_container_then_app() {
        let font = FontSettings::default();
        let size = |tab: Option<f32>, own: serde_json::Value, repo: serde_json::Value| {
            resolve_levels(own, repo, &font)
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
            resolve_levels(json!({}), json!({}), &app).font_size,
            Field::new(18.0, Source::App)
        );
    }

    #[test]
    fn resolution_clamps_the_winning_level() {
        let default = FontSettings::default();
        let tab = resolve_levels(json!({}), json!({}), &default).with_tab_size(Some(99.0));
        assert_eq!(tab.font_size, Field::new(fonts::MAX_SIZE, Source::Tab));
        let own = resolve_levels(
            json!({ "terminal_font_size": 1 }),
            json!({ "terminal_font_size": 9 }),
            &default,
        );
        assert_eq!(own.font_size, Field::new(fonts::MIN_SIZE, Source::Session));
        let nan = FontSettings {
            size: f32::NAN,
            ..FontSettings::default()
        };
        assert_eq!(
            resolve_levels(json!({}), json!({}), &nan).font_size,
            Field::new(fonts::DEFAULT_SIZE, Source::BuiltIn),
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
        for color in ACCENT_PRESETS
            .iter()
            .chain(&BACKGROUND_PRESETS)
            .filter_map(|preset| preset.color)
        {
            assert_eq!(parse_color(&hex(color)), Some(color));
        }
    }

    #[test]
    fn the_accent_presets_are_the_theme_and_five_petal_colours() {
        let names: Vec<&str> = ACCENT_PRESETS.iter().map(|preset| preset.name).collect();
        assert_eq!(
            names,
            ["Theme", "Rose", "Ochre", "Periwinkle", "Sky", "Lilac"]
        );
        assert_eq!(
            ACCENT_PRESETS.map(|preset| preset.color),
            [
                None,
                Some(0xe7849b),
                Some(0xd9a15c),
                Some(0x8fb3f0),
                Some(0x8fd3e8),
                Some(0xc9b8ff)
            ]
        );
        assert_eq!(
            ACCENT_PRESETS[0].stored(),
            None,
            "the first preset is the theme's accent: it sets nothing"
        );
        assert_eq!(ACCENT_PRESETS[1].stored().as_deref(), Some("#e7849b"));
    }

    /// A level whose overrides are `own`, resolved over the default app
    /// level and the theme base `theme`.
    fn resolved_over(own: &AppearanceOverrides, theme: ThemeBase) -> Resolved {
        let font = FontSettings::default();
        resolve_container(
            own,
            AppLevel {
                font: &font,
                theme,
                frame_width: DEFAULT_FRAME_WIDTH,
            },
        )
    }

    /// A theme base that is not the built-in theme's.
    const OTHER_THEME: ThemeBase = ThemeBase {
        accent: 0x40c080,
        background: 0x102030,
    };

    #[test]
    fn the_default_background_preset_clears_the_override_under_any_theme() {
        let default = BACKGROUND_PRESETS[0];
        assert_eq!((default.name, default.color), ("Default", None));
        let own = AppearanceOverrides {
            terminal_background_color: Some("#111318".to_owned()),
            ..AppearanceOverrides::default()
        };
        assert_eq!(
            resolved_over(&own, OTHER_THEME).background,
            Field::new(0x111318, Source::Container),
            "the level's override before the preset"
        );
        let applied = AppearanceChange::background(default.stored()).apply_to(&own);
        assert_eq!(applied.terminal_background_color, None);
        assert_eq!(
            resolved_over(&applied, OTHER_THEME).background,
            Field::new(OTHER_THEME.background, Source::BuiltIn),
            "the level falls through to the theme's background"
        );
    }

    #[test]
    fn the_theme_accent_preset_clears_the_override_under_any_theme() {
        let accent = ACCENT_PRESETS[0];
        let own = AppearanceOverrides {
            accent_color: Some("#e7849b".to_owned()),
            terminal_frame_color: Some("#e7849b".to_owned()),
            ..AppearanceOverrides::default()
        };
        let applied = AppearanceChange::accent_and_frame(accent.stored()).apply_to(&own);
        assert_eq!(
            (&applied.accent_color, &applied.terminal_frame_color),
            (&None, &None)
        );
        let resolved = resolved_over(&applied, OTHER_THEME);
        assert_eq!(
            resolved.accent,
            Field::new(OTHER_THEME.accent, Source::BuiltIn),
            "the level falls through to the theme's accent"
        );
    }

    #[test]
    fn the_background_presets_offer_slate_and_not_graphite() {
        let names: Vec<&str> = BACKGROUND_PRESETS
            .iter()
            .map(|preset| preset.name)
            .collect();
        assert!(names.contains(&"Slate"), "{names:?}");
        assert!(!names.contains(&"Graphite"), "{names:?}");
        let slate = BACKGROUND_PRESETS
            .iter()
            .find(|preset| preset.name == "Slate")
            .expect("Slate is one of the presets");
        assert_eq!(slate.color, Some(0x111318));
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
            app_appearance: AppColors {
                accent_color: Some("#123456".to_owned()),
                ..AppColors::default()
            },
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
        assert_eq!(older.recent_colors, [] as [std::string::String; 0]);

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
