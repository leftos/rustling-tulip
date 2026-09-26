//! The appearance editor: one modal for a session, a repo, a workspace or
//! the app level, whose rows (accent and frame, shell background, font
//! family, size and bold) apply at once and each show the value the level
//! resolves to and the level it comes from. At the app level its rows are
//! the Settings modal's Appearance tab ([`crate::settings_view`]).

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, Focusable as _, FontWeight,
    Keystroke, SharedString, Stateful, Subscription, Window, div, prelude::*, px,
};
use protocol::{AppearanceOverrides, ClientMessage};

use crate::appearance::{
    self, ACCENT_PRESETS, AppLevel, AppearanceChange, BACKGROUND_PRESETS, Field, Preset, Resolved,
    Source,
};
use crate::fonts::{self, BUNDLED_FAMILIES};
use crate::notices::ToastKind;
use crate::session_menu::{backdrop, dialog_button, muted_row};
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::{
    APPEARANCE_FAILED_TITLE, BORDER, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE,
    font_size_to_u16, tooltip,
};

const EDITOR_WIDTH: f32 = 700.0;
const COLUMN_WIDTH: f32 = 320.0;
const FAMILY_LIST_HEIGHT: f32 = 200.0;
const SWATCH_SIZE: f32 = 16.0;
const HEX_PLACEHOLDER: &str = "#rrggbb";
const HEX_HINT: &str = "Use #RRGGBB";
const BOLD_LABEL: &str = "Render terminal text in bold";
/// What a family no level sets draws with.
const DEFAULT_CASCADE: &str = "default cascade";

/// The level the editor edits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Level {
    Session(String),
    Repo(String),
    Workspace(String),
    App,
}

impl Level {
    /// What a row's reset link says: the level it falls back to.
    const fn reset_label(&self) -> &'static str {
        match self {
            Self::Session(_) => "Inherit",
            Self::Repo(_) | Self::Workspace(_) => "Inherit app",
            Self::App => "Use built-in",
        }
    }
}

/// The open editor: its level, its text fields and the families it lists.
pub(crate) struct AppearanceEditor {
    pub(crate) level: Level,
    accent_hex: Entity<TextInput>,
    background_hex: Entity<TextInput>,
    family_filter: Entity<TextInput>,
    /// The installed families, loaded when the editor opens.
    system_families: Vec<SharedString>,
    _subscriptions: Vec<Subscription>,
}

/// A colour row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorRow {
    /// The accent, which sets the pane frame with it.
    Accent,
    Background,
}

impl ColorRow {
    const fn key(self) -> &'static str {
        match self {
            Self::Accent => "accent",
            Self::Background => "background",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Accent => "Sidebar accent and pane frame",
            Self::Background => "Shell background",
        }
    }

    const fn presets(self) -> &'static [Preset] {
        match self {
            Self::Accent => &ACCENT_PRESETS,
            Self::Background => &BACKGROUND_PRESETS,
        }
    }

    fn change(self, color: Option<String>) -> AppearanceChange {
        match self {
            Self::Accent => AppearanceChange::accent_and_frame(color),
            Self::Background => AppearanceChange::background(color),
        }
    }

    /// The colour the level itself sets in this row.
    fn own(self, own: &AppearanceOverrides) -> Option<u32> {
        let raw = match self {
            Self::Accent => own.accent_color.as_deref(),
            Self::Background => own.terminal_background_color.as_deref(),
        };
        raw.and_then(appearance::parse_color)
    }

    const fn resolved(self, resolved: &Resolved) -> &Field<u32> {
        match self {
            Self::Accent => &resolved.accent,
            Self::Background => &resolved.background,
        }
    }

    fn input(self, editor: &AppearanceEditor) -> &Entity<TextInput> {
        match self {
            Self::Accent => &editor.accent_hex,
            Self::Background => &editor.background_hex,
        }
    }

    fn named(name: &str) -> Option<Self> {
        match name {
            "accent" => Some(Self::Accent),
            "background" => Some(Self::Background),
            _ => None,
        }
    }
}

/// The editor's level as it stands: its own values, what they resolve to,
/// the source a value set here reports, and what its container is called.
struct LevelView {
    own: AppearanceOverrides,
    resolved: Resolved,
    here: Source,
    /// What the session's container is, `repo` or `workspace`; the app
    /// level has none.
    container: Option<&'static str>,
}

impl LevelView {
    fn source_name(&self, source: Source) -> &'static str {
        match source {
            Source::Tab => "tab",
            Source::Session => "session",
            Source::Container => self.container.unwrap_or("container"),
            Source::App => "app",
            Source::BuiltIn => "built-in",
        }
    }

    /// A colour row's hint: a value set at this level comes from `current`.
    fn color_hint(&self, row: ColorRow) -> String {
        let field = row.resolved(&self.resolved);
        let from = if field.source == self.here {
            "current"
        } else {
            self.source_name(field.source)
        };
        format!("Resolved: {} from {from}", appearance::hex(field.value))
    }

    fn family_hint(&self) -> String {
        let field = &self.resolved.font_family;
        let name = field.value.as_deref().unwrap_or(DEFAULT_CASCADE);
        format!("Resolved: {name} from {}", self.source_name(field.source))
    }

    fn size_hint(&self) -> String {
        let field = &self.resolved.font_size;
        format!(
            "Resolved: {}px from {}",
            field.value,
            self.source_name(field.source)
        )
    }

    fn bold_hint(&self) -> String {
        let field = &self.resolved.font_bold;
        let weight = if field.value { "bold" } else { "normal" };
        format!("Resolved: {weight} from {}", self.source_name(field.source))
    }

    fn hint(&self, row: &str) -> Option<String> {
        match row {
            "family" => Some(self.family_hint()),
            "size" => Some(self.size_hint()),
            "bold" => Some(self.bold_hint()),
            _ => ColorRow::named(row).map(|row| self.color_hint(row)),
        }
    }
}

/// One line of the font family list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FamilyEntry {
    /// The row that clears the level's family.
    Reset,
    Header(&'static str),
    /// A family, `saved` when it is the level's value from neither list.
    Family {
        name: SharedString,
        saved: bool,
    },
}

/// The family list: the reset row, then each group with a match for
/// `filter` (case-insensitive) under its header.
fn family_entries(system: &[SharedString], own: Option<&str>, filter: &str) -> Vec<FamilyEntry> {
    let needle = filter.trim().to_lowercase();
    let matches = |name: &str| name.to_lowercase().contains(&needle);
    let bundled: Vec<SharedString> = BUNDLED_FAMILIES
        .iter()
        .map(|name| SharedString::from(*name))
        .collect();
    let saved: Vec<SharedString> = own
        .filter(|name| !bundled.iter().chain(system).any(|family| family == name))
        .map(|name| SharedString::from(name.to_owned()))
        .into_iter()
        .collect();
    let mut entries = vec![FamilyEntry::Reset];
    for (header, names, is_saved) in [
        ("Current", &saved, true),
        ("Bundled", &bundled, false),
        ("System", &system.to_vec(), false),
    ] {
        let shown: Vec<&SharedString> = names.iter().filter(|name| matches(name)).collect();
        if shown.is_empty() {
            continue;
        }
        entries.push(FamilyEntry::Header(header));
        entries.extend(shown.into_iter().map(|name| FamilyEntry::Family {
            name: name.clone(),
            saved: is_saved,
        }));
    }
    entries
}

impl RootView {
    /// The editor's heading, while it is open: `Settings` at the app level.
    #[must_use]
    pub fn appearance_editor_title(&self) -> Option<String> {
        let editor = self.appearance_editor.as_ref()?;
        match &editor.level {
            Level::Session(_) => Some("Session appearance".to_owned()),
            Level::Repo(id) => {
                let repo = self.sidebar.repos().iter().find(|repo| &repo.id == id)?;
                Some(format!("{} appearance", repo.name))
            }
            Level::Workspace(id) => {
                let workspace = self.sidebar.workspaces().iter().find(|ws| &ws.id == id)?;
                Some(format!("{} appearance", workspace.name))
            }
            Level::App => Some("Settings".to_owned()),
        }
    }

    /// The hint under row `row` (`accent`, `background`, `family`, `size`
    /// or `bold`), while the editor is open.
    #[must_use]
    pub fn appearance_hint(&self, row: &str) -> Option<String> {
        let editor = self.appearance_editor.as_ref()?;
        self.level_view(&editor.level)?.hint(row)
    }

    /// What the rows' reset links say, while the editor is open.
    #[must_use]
    pub fn appearance_reset_label(&self) -> Option<&'static str> {
        Some(self.appearance_editor.as_ref()?.level.reset_label())
    }

    /// Whether colour row `row`'s Apply acts: its field parses and differs
    /// from the level's value.
    #[must_use]
    pub fn appearance_can_apply(&self, row: &str, cx: &App) -> bool {
        ColorRow::named(row).is_some_and(|row| self.hex_ready(row, cx).is_some())
    }

    /// The selectors of colour row `row`'s recent swatches.
    #[must_use]
    pub fn appearance_recent_rows(&self, row: &str) -> Vec<String> {
        if self.appearance_editor.is_none() {
            return Vec::new();
        }
        self.sidebar
            .recent_colors()
            .into_iter()
            .map(|color| format!("appearance-{row}-recent-{color:06x}"))
            .collect()
    }

    /// The families the font list shows under its filter, in order; the
    /// level's own family from neither list reads `<name> (saved)`.
    #[must_use]
    pub fn appearance_family_names(&self, cx: &App) -> Vec<String> {
        let Some(editor) = self.appearance_editor.as_ref() else {
            return Vec::new();
        };
        let Some(view) = self.level_view(&editor.level) else {
            return Vec::new();
        };
        let filter = editor.family_filter.read(cx).text();
        family_entries(
            &editor.system_families,
            view.own.terminal_font_family.as_deref(),
            filter,
        )
        .into_iter()
        .filter_map(|entry| match entry {
            FamilyEntry::Family { name, saved: true } => Some(format!("{name} (saved)")),
            FamilyEntry::Family { name, .. } => Some(name.to_string()),
            FamilyEntry::Reset | FamilyEntry::Header(_) => None,
        })
        .collect()
    }

    /// Opens the editor at `level`, closing any menu, unless another dialog
    /// or a modal notice is up or the connection is down. The installed
    /// families are read now.
    pub(crate) fn open_appearance_editor(
        &mut self,
        level: Level,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let blocked = self.appearance_editor.is_some()
            || self.exit.is_some()
            || self.spawn_dialog.is_some()
            || self.shell_dialog.is_some()
            || self.delete_dialog.is_some()
            || self.notices.has_modal()
            || self.conn.overlay().is_some();
        if blocked {
            return;
        }
        self.close_session_menu(window, cx);
        self.close_container_menu(window, cx);
        self.close_shell_menu(window, cx);
        self.close_tab_menu(window, cx);
        self.close_sc_picker(window, cx);
        self.renaming = None;
        self.close_flyout();
        let accent_hex = cx.new(|cx| TextInput::new("", HEX_PLACEHOLDER, cx));
        let background_hex = cx.new(|cx| TextInput::new("", HEX_PLACEHOLDER, cx));
        let family_filter = cx.new(|cx| TextInput::new("", "Filter fonts", cx));
        let mut subscriptions = Self::watch_hex_field(&accent_hex, ColorRow::Accent, window, cx);
        subscriptions.extend(Self::watch_hex_field(
            &background_hex,
            ColorRow::Background,
            window,
            cx,
        ));
        subscriptions.extend(Self::watch_filter_field(&family_filter, window, cx));
        self.appearance_editor = Some(AppearanceEditor {
            level,
            accent_hex,
            background_hex,
            family_filter,
            system_families: fonts::system_families(cx),
            _subscriptions: subscriptions,
        });
        self.appearance_focus.focus(window);
        cx.notify();
    }

    /// Closes the editor and hands the keyboard back to the active pane.
    pub(crate) fn close_appearance_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.appearance_editor.take().is_some() {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// Closes the editor of a session, repo or workspace the daemon no
    /// longer lists.
    pub(crate) fn drop_stale_appearance_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stale = self
            .appearance_editor
            .as_ref()
            .is_some_and(|editor| self.level_view(&editor.level).is_none());
        if stale {
            self.close_appearance_editor(window, cx);
        }
    }

    /// Enter applies the field's colour, Esc closes, an edit redraws.
    fn watch_hex_field(
        input: &Entity<TextInput>,
        row: ColorRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let keys = cx.subscribe_in(
            input,
            window,
            move |this, _, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submit => this.apply_hex(row, cx),
                TextInputEvent::Cancel => this.close_appearance_editor(window, cx),
            },
        );
        let edits = cx.subscribe_in(input, window, |_, _, _: &TextChanged, _, cx| {
            cx.notify();
        });
        vec![keys, edits]
    }

    /// Esc closes, an edit narrows the list; Enter does nothing.
    fn watch_filter_field(
        input: &Entity<TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let keys = cx.subscribe_in(
            input,
            window,
            |this, _, event: &TextInputEvent, window, cx| {
                if *event == TextInputEvent::Cancel {
                    this.close_appearance_editor(window, cx);
                }
            },
        );
        let edits = cx.subscribe_in(input, window, |_, _, _: &TextChanged, _, cx| {
            cx.notify();
        });
        vec![keys, edits]
    }

    /// A key while the editor is open; returns whether it was the
    /// editor's (every key but typing in one of its fields is). Esc
    /// closes.
    pub(crate) fn on_appearance_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if keystroke.key == "escape" {
            self.close_appearance_editor(window, cx);
            return true;
        }
        !self.appearance_field_focused(window, cx)
    }

    fn appearance_field_focused(&self, window: &Window, cx: &App) -> bool {
        self.appearance_editor.as_ref().is_some_and(|editor| {
            [
                &editor.accent_hex,
                &editor.background_hex,
                &editor.family_filter,
            ]
            .into_iter()
            .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
        })
    }

    /// The editor's level as it stands; `None` for a session, repo or
    /// workspace the lists do not hold.
    fn level_view(&self, level: &Level) -> Option<LevelView> {
        let ui = self.sidebar.ui_state();
        let app = AppLevel {
            colors: &ui.app_appearance,
            font: &ui.terminal_font,
        };
        // A container's last send, until the daemon's list echoes it, is
        // what the next change builds on.
        let container_view = |stored: &AppearanceOverrides, container| {
            let own = self.container_sends.get(level).unwrap_or(stored);
            LevelView {
                resolved: appearance::resolve_container(own, app),
                own: own.clone(),
                here: Source::Container,
                container: Some(container),
            }
        };
        match level {
            Level::Session(id) => {
                let session = self.sidebar.session(id)?;
                let own = self.effective_appearance(id)?;
                Some(LevelView {
                    resolved: self.sidebar.appearance_with(id, &own)?,
                    own,
                    here: Source::Session,
                    container: Some(if session.workspace_id.is_some() {
                        "workspace"
                    } else {
                        "repo"
                    }),
                })
            }
            Level::Repo(id) => {
                let repo = self.sidebar.repos().iter().find(|repo| &repo.id == id)?;
                Some(container_view(&repo.appearance, "repo"))
            }
            Level::Workspace(id) => {
                let workspaces = self.sidebar.workspaces();
                let workspace = workspaces.iter().find(|ws| &ws.id == id)?;
                Some(container_view(&workspace.appearance, "workspace"))
            }
            Level::App => Some(LevelView {
                own: appearance::app_overrides(app),
                resolved: appearance::resolve(None, &[], &[], app),
                here: Source::App,
                container: None,
            }),
        }
    }

    /// The colour row `row`'s field holds, when it parses and setting it
    /// would change something at the level: the accent row sets the frame
    /// too.
    fn hex_ready(&self, row: ColorRow, cx: &App) -> Option<String> {
        let editor = self.appearance_editor.as_ref()?;
        let typed = appearance::parse_hex_input(row.input(editor).read(cx).text())?;
        let view = self.level_view(&editor.level)?;
        let change = row.change(Some(typed.clone())).normalized().ok()?;
        (!change.changes_nothing_in(&view.own)).then_some(typed)
    }

    /// The daemon's repo or workspace list arrived: a container whose
    /// stored overrides now equal its last send, or that is gone, no
    /// longer has one pending.
    pub(crate) fn settle_container_sends(&mut self) {
        let sidebar = &self.sidebar;
        self.container_sends.retain(|level, sent| {
            let stored = match level {
                Level::Repo(id) => sidebar
                    .repos()
                    .iter()
                    .find(|repo| &repo.id == id)
                    .map(|repo| &repo.appearance),
                Level::Workspace(id) => sidebar
                    .workspaces()
                    .iter()
                    .find(|ws| &ws.id == id)
                    .map(|ws| &ws.appearance),
                Level::Session(_) | Level::App => None,
            };
            stored.is_some_and(|stored| stored != sent)
        });
    }

    /// Apply, or Enter in the field: sets the typed colour and puts it
    /// first among the recent colours, which are saved.
    fn apply_hex(&mut self, row: ColorRow, cx: &mut Context<Self>) {
        let Some(color) = self.hex_ready(row, cx) else {
            return;
        };
        if self.sidebar.push_recent_color(&color) {
            self.save_ui();
        }
        self.change_appearance(&row.change(Some(color)), cx);
    }

    /// Sets `change` at the editor's level: a session's goes out in flight
    /// under a request id, a repo's or workspace's with its other stored
    /// fields, the app's is saved on this machine. A change that changes
    /// nothing does nothing; a value the protocol refuses raises a toast.
    fn change_appearance(&mut self, change: &AppearanceChange, cx: &mut Context<Self>) {
        let Some(editor) = self.appearance_editor.as_ref() else {
            return;
        };
        let level = editor.level.clone();
        let Some(view) = self.level_view(&level) else {
            return;
        };
        let change = match change.clone().normalized() {
            Ok(change) => change,
            Err(err) => {
                tracing::warn!("not changing the appearance of {level:?}: {err}");
                self.push_toast(
                    ToastKind::Error,
                    APPEARANCE_FAILED_TITLE,
                    Some(err.to_string()),
                    cx,
                );
                return;
            }
        };
        if change.changes_nothing_in(&view.own) {
            return;
        }
        let appearance = change.apply_to(&view.own);
        if matches!(level, Level::Repo(_) | Level::Workspace(_)) {
            self.container_sends
                .insert(level.clone(), appearance.clone());
        }
        match level {
            Level::Session(id) => self.send_session_appearance(&id, &view.own, &change),
            Level::Repo(repo_id) => self.send(ClientMessage::SetRepoAppearance {
                repo_id,
                appearance,
            }),
            Level::Workspace(workspace_id) => self.send(ClientMessage::SetWorkspaceAppearance {
                workspace_id,
                appearance,
            }),
            Level::App => self.set_app_appearance(&appearance, cx),
        }
        cx.notify();
    }

    /// Makes `overrides` the app level: its colours and its font, each
    /// written only when it changed.
    fn set_app_appearance(&mut self, overrides: &AppearanceOverrides, cx: &mut Context<Self>) {
        let (colors, font) = appearance::app_from_overrides(overrides);
        let ui = self.sidebar.ui_state();
        let colors_changed = ui.app_appearance != colors;
        let font_changed = ui.terminal_font != font;
        if colors_changed {
            self.set_app_colors(colors, cx);
        }
        if font_changed {
            self.set_app_font(font, cx);
        }
    }

    /// Steps the level's size one `delta` from what it resolves to; at a
    /// clamp limit nothing changes.
    fn step_appearance_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        let Some(view) = self
            .appearance_editor
            .as_ref()
            .and_then(|editor| self.level_view(&editor.level))
        else {
            return;
        };
        if let Some(next) = fonts::stepped(view.resolved.font_size.value, delta) {
            let change = AppearanceChange::font_size(Some(font_size_to_u16(next)));
            self.change_appearance(&change, cx);
        }
    }

    /// Sets the level's bold to the opposite of what it resolves to.
    fn toggle_appearance_bold(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self
            .appearance_editor
            .as_ref()
            .and_then(|editor| self.level_view(&editor.level))
        else {
            return;
        };
        let change = AppearanceChange::font_bold(Some(!view.resolved.font_bold.value));
        self.change_appearance(&change, cx);
    }

    /// The editor of a session, repo or workspace over a backdrop that takes
    /// every click beneath it; the app level's is the Settings modal's.
    pub(crate) fn appearance_editor_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let editor = self.appearance_editor.as_ref()?;
        if editor.level == Level::App {
            return None;
        }
        let title = self.appearance_editor_title()?;
        let close =
            dialog_button("appearance-close", "×".to_owned(), false, false).on_click(cx.listener(
                |this, _: &ClickEvent, window, cx| this.close_appearance_editor(window, cx),
            ));
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(close);
        let panel = div()
            .id("appearance-panel")
            .track_focus(&self.appearance_focus)
            .flex()
            .flex_col()
            .gap(px(10.0))
            .w(px(EDITOR_WIDTH))
            .p(px(14.0))
            .bg(gpui::rgb(PANEL_BG))
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .rounded(px(6.0))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .child(header)
            .children(self.appearance_body(cx))
            .child(close_footer("appearance-footer-close", cx));
        Some(backdrop("appearance-editor", panel))
    }

    /// The editor's rows in two columns: the colours, then the font.
    pub(crate) fn appearance_body(&self, cx: &mut Context<Self>) -> Option<Div> {
        let editor = self.appearance_editor.as_ref()?;
        let view = self.level_view(&editor.level)?;
        let reset = editor.level.reset_label();
        let colours = div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .w(px(COLUMN_WIDTH))
            .child(self.color_section(editor, &view, ColorRow::Accent, cx))
            .child(self.color_section(editor, &view, ColorRow::Background, cx));
        let font = div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .w(px(COLUMN_WIDTH))
            .child(Self::family_section(editor, &view, cx))
            .child(size_section(&view, reset, cx))
            .child(bold_section(&view, reset, cx));
        Some(div().flex().gap(px(16.0)).child(colours).child(font))
    }

    /// The font family: its filter, then the list under it.
    fn family_section(editor: &AppearanceEditor, view: &LevelView, cx: &mut Context<Self>) -> Div {
        let own = view.own.terminal_font_family.as_deref();
        let filter = editor.family_filter.read(cx).text().to_owned();
        let entries = family_entries(&editor.system_families, own, &filter);
        let reset = editor.level.reset_label();
        let rows: Vec<AnyElement> = entries
            .into_iter()
            .map(|entry| family_row(&entry, own, reset, cx))
            .collect();
        let list = div()
            .id("appearance-family-list")
            .h(px(FAMILY_LIST_HEIGHT))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .rounded(px(4.0))
            .children(rows);
        section("Font family")
            .child(input_box("appearance-family-filter", &editor.family_filter))
            .child(list)
            .child(hint_line("appearance-family-hint", view.family_hint()))
    }
}

/// A row's column: its label, then what the caller adds.
fn section(label: &'static str) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(div().font_weight(FontWeight::SEMIBOLD).child(label))
}

/// A row's reset link: its selector, its text and the change it makes.
struct Reset {
    selector: &'static str,
    label: &'static str,
    change: AppearanceChange,
}

/// The change a row's reset link makes: it clears the row's field.
fn reset_change(row: &str) -> Option<AppearanceChange> {
    match row {
        "family" => Some(AppearanceChange::font_family(None)),
        "size" => Some(AppearanceChange::font_size(None)),
        "bold" => Some(AppearanceChange::font_bold(None)),
        _ => ColorRow::named(row).map(|row| row.change(None)),
    }
}

/// `label` with the row's reset link at the right; the link is dimmed and
/// inert while the level `own` does not set the field.
fn header_with_reset(
    label: impl IntoElement,
    reset: Reset,
    own: &AppearanceOverrides,
    cx: &mut Context<RootView>,
) -> Div {
    let enabled = !reset.change.changes_nothing_in(own);
    let link = link(reset.selector, reset.label, enabled);
    let link = if enabled {
        let change = reset.change;
        link.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.change_appearance(&change, cx);
        }))
    } else {
        link
    };
    div()
        .flex()
        .items_center()
        .justify_between()
        .child(label)
        .child(link)
}

impl RootView {
    /// Whether row `row`'s reset link acts: the editor's level sets the
    /// row's field.
    #[must_use]
    pub fn appearance_can_reset(&self, row: &str) -> bool {
        let Some(view) = self
            .appearance_editor
            .as_ref()
            .and_then(|editor| self.level_view(&editor.level))
        else {
            return false;
        };
        reset_change(row).is_some_and(|change| !change.changes_nothing_in(&view.own))
    }

    /// A colour row: the presets, the recent colours, the hex field with
    /// its preview and Apply, and the hint.
    fn color_section(
        &self,
        editor: &AppearanceEditor,
        view: &LevelView,
        row: ColorRow,
        cx: &mut Context<Self>,
    ) -> Div {
        let recent = self.sidebar.recent_colors();
        let ready = self.hex_ready(row, cx).is_some();
        let own = row.own(&view.own);
        let key = row.key();
        let presets: Vec<AnyElement> = row
            .presets()
            .iter()
            .map(|preset| {
                let color = appearance::hex(preset.color);
                swatch(
                    format!(
                        "appearance-{key}-preset-{}",
                        preset.name.to_ascii_lowercase()
                    ),
                    preset.color,
                    format!("{} ({color})", preset.name),
                    own == Some(preset.color),
                )
                .on_click(pick(row, color, cx))
                .into_any_element()
            })
            .collect();
        let recent: Vec<AnyElement> = recent
            .iter()
            .map(|&value| {
                let color = appearance::hex(value);
                swatch(
                    format!("appearance-{key}-recent-{value:06x}"),
                    value,
                    format!("Recent custom color {color}"),
                    own == Some(value),
                )
                .on_click(pick(row, color, cx))
                .into_any_element()
            })
            .collect();
        let label = div().font_weight(FontWeight::SEMIBOLD).child(row.label());
        let reset_selector = match row {
            ColorRow::Accent => "appearance-accent-reset",
            ColorRow::Background => "appearance-background-reset",
        };
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(header_with_reset(
                label,
                Reset {
                    selector: reset_selector,
                    label: editor.level.reset_label(),
                    change: row.change(None),
                },
                &view.own,
                cx,
            ))
            .child(swatch_line(presets))
            .when(!recent.is_empty(), |section| {
                section.child(swatch_line(recent))
            })
            .child(hex_line(editor, row, ready, cx))
            .child(hint_line(
                match row {
                    ColorRow::Accent => "appearance-accent-hint",
                    ColorRow::Background => "appearance-background-hint",
                },
                view.color_hint(row),
            ))
    }
}

/// The click that sets `color` in `row`, without entering it among the
/// recent colours.
fn pick(
    row: ColorRow,
    color: String,
    cx: &mut Context<RootView>,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    cx.listener(move |this, _: &ClickEvent, _, cx| {
        this.change_appearance(&row.change(Some(color.clone())), cx);
    })
}

/// The hex field, its live preview, Apply, and the format hint while the
/// field holds something that does not parse.
fn hex_line(
    editor: &AppearanceEditor,
    row: ColorRow,
    ready: bool,
    cx: &mut Context<RootView>,
) -> Div {
    let input = row.input(editor);
    let text = input.read(cx).text().to_owned();
    let parsed = appearance::parse_hex_input(&text);
    let key = row.key();
    let preview_name = format!("appearance-{key}-preview");
    let preview = div()
        .debug_selector(|| preview_name)
        .flex_none()
        .size(px(SWATCH_SIZE))
        .rounded(px(3.0))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .when_some(
            parsed.as_deref().and_then(appearance::parse_color),
            |swatch, color| swatch.bg(gpui::rgb(color)),
        );
    let apply = apply_button(&format!("appearance-{key}-apply"), ready);
    let apply = if ready {
        apply.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.apply_hex(row, cx)))
    } else {
        apply
    };
    let bad = !text.trim().is_empty() && parsed.is_none();
    let hint_name = format!("appearance-{key}-hex-hint");
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(input_box(
            match row {
                ColorRow::Accent => "appearance-accent-hex",
                ColorRow::Background => "appearance-background-hex",
            },
            input,
        ))
        .child(preview)
        .child(apply)
        .when(bad, |line| {
            line.child(
                div()
                    .debug_selector(|| hint_name)
                    .flex_none()
                    .text_color(gpui::rgb(MUTED))
                    .child(HEX_HINT),
            )
        })
}

/// Apply, shaped like the dialog buttons; a disabled one is dimmed, with
/// the default cursor and no hover.
fn apply_button(selector: &str, enabled: bool) -> Stateful<Div> {
    let name = selector.to_owned();
    let button = div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex_none()
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .text_color(gpui::rgb(TEXT))
        .child("Apply");
    if enabled {
        button
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
    } else {
        button.opacity(0.5).cursor_default()
    }
}

/// A line of colour swatches that wraps.
fn swatch_line(swatches: Vec<AnyElement>) -> Div {
    div().flex().flex_wrap().gap(px(4.0)).children(swatches)
}

/// A clickable colour swatch with its tooltip; the level's own colour is
/// outlined.
fn swatch(selector: String, color: u32, tip: String, chosen: bool) -> Stateful<Div> {
    div()
        .id(ElementId::Name(SharedString::from(selector.clone())))
        .debug_selector(|| selector)
        .flex_none()
        .size(px(SWATCH_SIZE))
        .rounded(px(3.0))
        .border_1()
        .border_color(gpui::rgb(if chosen { TEXT } else { BORDER }))
        .bg(gpui::rgb(color))
        .cursor_pointer()
        .tooltip(tooltip(tip))
}

/// One line of the family list: the reset row, a group header, or a
/// family, the chosen one ticked.
fn family_row(
    entry: &FamilyEntry,
    own: Option<&str>,
    reset: &'static str,
    cx: &mut Context<RootView>,
) -> AnyElement {
    match entry {
        FamilyEntry::Header(header) => muted_row(*header).into_any_element(),
        FamilyEntry::Reset => list_row("appearance-family-reset", reset, own.is_none())
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.change_appearance(&AppearanceChange::font_family(None), cx);
            }))
            .into_any_element(),
        FamilyEntry::Family { name, saved } => {
            let label = if *saved {
                format!("{name} (saved)")
            } else {
                name.to_string()
            };
            let chosen = own == Some(name.as_ref());
            let family = name.to_string();
            list_row(&format!("appearance-family-option-{name}"), label, chosen)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let change = AppearanceChange::font_family(Some(family.clone()));
                    this.change_appearance(&change, cx);
                }))
                .into_any_element()
        }
    }
}

/// A clickable row of a list, `✓` before the chosen one.
fn list_row(selector: &str, label: impl Into<SharedString>, chosen: bool) -> Stateful<Div> {
    let name = selector.to_owned();
    div()
        .id(ElementId::Name(SharedString::from(name.clone())))
        .debug_selector(|| name)
        .flex()
        .flex_none()
        .gap(px(6.0))
        .px(px(6.0))
        .py(px(2.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(
            div()
                .flex_none()
                .w(px(12.0))
                .child(if chosen { "✓" } else { "" }),
        )
        .child(label.into())
}

/// The size: `−`, the size it resolves to, `+`.
fn size_section(view: &LevelView, reset: &'static str, cx: &mut Context<RootView>) -> Div {
    let down = dialog_button("appearance-size-down", "−".to_owned(), false, false)
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.step_appearance_size(-1.0, cx)));
    let up = dialog_button("appearance-size-up", "+".to_owned(), false, false)
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.step_appearance_size(1.0, cx)));
    let value = format!("{}px", view.resolved.font_size.value);
    let stepper = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(down)
        .child(
            div()
                .debug_selector(|| "appearance-size-value".to_owned())
                .child(value),
        )
        .child(up);
    let label = div().font_weight(FontWeight::SEMIBOLD).child("Font size");
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(header_with_reset(
            label,
            Reset {
                selector: "appearance-size-reset",
                label: reset,
                change: AppearanceChange::font_size(None),
            },
            &view.own,
            cx,
        ))
        .child(stepper)
        .child(hint_line("appearance-size-hint", view.size_hint()))
}

/// The bold checkbox, ticked while the level resolves to bold.
fn bold_section(view: &LevelView, reset: &'static str, cx: &mut Context<RootView>) -> Div {
    let mark = if view.resolved.font_bold.value {
        "☑"
    } else {
        "☐"
    };
    let checkbox = div()
        .id("appearance-bold")
        .debug_selector(|| "appearance-bold".to_owned())
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(4.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(mark)
        .child(BOLD_LABEL)
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_appearance_bold(cx)));
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(header_with_reset(
            checkbox,
            Reset {
                selector: "appearance-bold-reset",
                label: reset,
                change: AppearanceChange::font_bold(None),
            },
            &view.own,
            cx,
        ))
        .child(hint_line("appearance-bold-hint", view.bold_hint()))
}

/// A text field in its box.
fn input_box(selector: &'static str, input: &Entity<TextInput>) -> Div {
    div()
        .debug_selector(move || selector.to_owned())
        .flex()
        .flex_1()
        .min_w(px(0.0))
        .px(px(6.0))
        .py(px(3.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(gpui::rgb(BORDER))
        .child(input.clone())
}

/// A row's muted `Resolved: … from …` line.
fn hint_line(selector: &'static str, text: String) -> Div {
    div()
        .debug_selector(move || selector.to_owned())
        .text_color(gpui::rgb(MUTED))
        .child(text)
}

/// A link-style button: plain text that lights up under the pointer; a
/// disabled one is dimmed, with the default cursor and no hover.
fn link(selector: &'static str, label: &'static str, enabled: bool) -> Stateful<Div> {
    let link = div()
        .id(selector)
        .debug_selector(move || selector.to_owned())
        .px(px(4.0))
        .rounded(px(4.0))
        .text_color(gpui::rgb(MUTED))
        .child(label);
    if enabled {
        link.cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
    } else {
        link.opacity(0.5).cursor_default()
    }
}

/// The footer with its Close button, tagged `selector`.
pub(crate) fn close_footer(selector: &'static str, cx: &mut Context<RootView>) -> Div {
    div().flex().justify_end().child(
        dialog_button(selector, "Close".to_owned(), false, false).on_click(cx.listener(
            |this, _: &ClickEvent, window, cx| {
                this.close_appearance_editor(window, cx);
            },
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(entries: &[FamilyEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                FamilyEntry::Reset => "reset".to_owned(),
                FamilyEntry::Header(header) => format!("[{header}]"),
                FamilyEntry::Family { name, saved } => {
                    format!("{name}{}", if *saved { " (saved)" } else { "" })
                }
            })
            .collect()
    }

    #[test]
    fn the_family_list_groups_and_filters() {
        let system = [SharedString::from("Arial"), SharedString::from("Consolas")];
        assert_eq!(
            names(&family_entries(&system, None, "")),
            [
                "reset",
                "[Bundled]",
                "Geist Mono",
                "Fira Code",
                "JetBrains Mono",
                "Cascadia Code",
                "[System]",
                "Arial",
                "Consolas",
            ]
        );
        assert_eq!(
            names(&family_entries(&system, Some("Lost Mono"), "MONO")),
            [
                "reset",
                "[Current]",
                "Lost Mono (saved)",
                "[Bundled]",
                "Geist Mono",
                "JetBrains Mono",
            ],
            "a case-insensitive substring; a saved family neither list has"
        );
        assert_eq!(
            names(&family_entries(&system, Some("Arial"), "zzz")),
            ["reset"],
            "a listed family is not saved; nothing matches"
        );
    }
}
