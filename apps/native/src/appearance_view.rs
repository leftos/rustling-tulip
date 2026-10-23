//! The appearance editor: one modal for a session, a repo, a workspace or
//! the app level, whose rows (accent and frame, shell background, font
//! family, size and bold) apply at once and each show the value the level
//! resolves to and the level it comes from. At the app level its rows are
//! the Settings modal's Appearance tab ([`crate::settings_view`]), which
//! adds the font the interface draws in.

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, Focusable as _, Keystroke,
    MouseButton, MouseDownEvent, ScrollHandle, SharedString, Stateful, Subscription, Window, div,
    prelude::*, px,
};
use protocol::{AppearanceOverrides, ClientMessage};

use crate::appearance::{
    self, ACCENT_PRESETS, AppLevel, AppearanceChange, BACKGROUND_PRESETS, Field, Preset, Resolved,
    Source,
};
use crate::appearance_stops::{
    AppearanceStop, ColorRow, FamilyEntry, FamilyPick, FontList, ResetRow, SwatchLine, after_lost,
    family_entries, family_highlight, same_family, step, step_family, step_swatch, stops,
    swatch_cursor,
};
use crate::buttons::{ButtonSize, field_frame_with_ring, focus_ring, outlined_button};
use crate::fonts;
use crate::notices::ToastKind;
use crate::palette::{GROUND, HOVER, LINE_STRONG};
use crate::section::field_column;
use crate::session_menu::{backdrop, dialog_button, muted_row};
use crate::settings_view::SettingsControl;
use crate::spawn_view::{
    checkbox_row, close_button, dialog_card, dialog_footer, dialog_title_bar, field_label,
};
use crate::text_input::{TextChanged, TextInput, TextInputEvent};
use crate::{APPEARANCE_FAILED_TITLE, MUTED, RootView, TEXT, font_size_to_u16, tooltip};

const EDITOR_WIDTH: f32 = 700.0;
const COLUMN_WIDTH: f32 = 320.0;
const FAMILY_LIST_HEIGHT: f32 = 200.0;
/// A colour swatch's side and corner radius.
const SWATCH_SIZE: f32 = 18.0;
const SWATCH_RADIUS: f32 = 5.0;
/// The family list's corner radius, a text field's.
const LIST_RADIUS: f32 = 7.0;
/// A family list row's corner radius, and a reset link's.
const ROW_RADIUS: f32 = 4.0;
const LINK_RADIUS: f32 = 4.0;
/// The padding round the editor's rows.
const BODY_PAD_X: f32 = 18.0;
const BODY_PAD_Y: f32 = 16.0;
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
    /// The app level's UI font filter.
    ui_family_filter: Entity<TextInput>,
    /// The installed families, loaded when the editor opens.
    system_families: Vec<SharedString>,
    /// The colour under the focused swatch line's cursor, once an arrow
    /// moved it off the chosen swatch.
    swatch_cursor: Option<u32>,
    /// The family list row the highlight is on, once an arrow moved it off
    /// the chosen family.
    family_highlight: Option<FamilyPick>,
    /// The UI font list's highlight, as the family list's.
    ui_family_highlight: Option<FamilyPick>,
    /// Where the family list is scrolled to.
    family_scroll: ScrollHandle,
    /// Where the UI font list is scrolled to.
    ui_family_scroll: ScrollHandle,
    /// The stop Tab walks on from in a session's, repo's or workspace's
    /// editor; Settings keeps its own.
    stop: Option<AppearanceStop>,
    _subscriptions: Vec<Subscription>,
}

impl AppearanceEditor {
    /// The installed families the editor listed when it opened.
    pub(crate) fn system_families(&self) -> &[SharedString] {
        &self.system_families
    }

    fn filter(&self, list: FontList) -> &Entity<TextInput> {
        match list {
            FontList::Terminal => &self.family_filter,
            FontList::Ui => &self.ui_family_filter,
        }
    }

    fn highlight(&self, list: FontList) -> Option<&FamilyPick> {
        match list {
            FontList::Terminal => self.family_highlight.as_ref(),
            FontList::Ui => self.ui_family_highlight.as_ref(),
        }
    }

    fn set_highlight(&mut self, list: FontList, pick: Option<FamilyPick>) {
        match list {
            FontList::Terminal => self.family_highlight = pick,
            FontList::Ui => self.ui_family_highlight = pick,
        }
    }

    fn scroll(&self, list: FontList) -> &ScrollHandle {
        match list {
            FontList::Terminal => &self.family_scroll,
            FontList::Ui => &self.ui_family_scroll,
        }
    }
}

impl ColorRow {
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
        let Some((entries, _)) = self.family_list(FontList::Terminal, cx) else {
            return Vec::new();
        };
        entries
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
        self.close_sc_file_menu(window, cx);
        self.close_more_menu(window, cx);
        self.renaming = None;
        self.close_flyout();
        let accent_hex = cx.new(|cx| TextInput::new("", HEX_PLACEHOLDER, cx));
        let background_hex = cx.new(|cx| TextInput::new("", HEX_PLACEHOLDER, cx));
        let family_filter = cx.new(|cx| TextInput::new("", "Filter fonts", cx));
        let ui_family_filter = cx.new(|cx| TextInput::new("", "Filter fonts", cx));
        let mut subscriptions = Self::watch_hex_field(&accent_hex, ColorRow::Accent, window, cx);
        subscriptions.extend(Self::watch_hex_field(
            &background_hex,
            ColorRow::Background,
            window,
            cx,
        ));
        subscriptions.extend(Self::watch_filter_field(&family_filter, window, cx));
        subscriptions.extend(Self::watch_filter_field(&ui_family_filter, window, cx));
        self.appearance_editor = Some(AppearanceEditor {
            level,
            accent_hex,
            background_hex,
            family_filter,
            ui_family_filter,
            system_families: fonts::system_families(cx),
            swatch_cursor: None,
            family_highlight: None,
            ui_family_highlight: None,
            family_scroll: ScrollHandle::new(),
            ui_family_scroll: ScrollHandle::new(),
            stop: None,
            _subscriptions: subscriptions,
        });
        self.appearance_focus.focus(window);
        cx.notify();
    }

    /// Closes the editor and hands the keyboard back to the active pane.
    pub(crate) fn close_appearance_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.appearance_editor.take().is_some() {
            // The Worktrees tab lives with Settings: dropping it here means
            // a picker answer arriving later finds nothing to fill.
            self.worktrees_tab = None;
            self.settings_control = None;
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

    pub(crate) fn appearance_field_focused(&self, window: &Window, cx: &App) -> bool {
        self.appearance_editor.as_ref().is_some_and(|editor| {
            [
                &editor.accent_hex,
                &editor.background_hex,
                &editor.family_filter,
                &editor.ui_family_filter,
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
        let view = self.level_view(&editor.level)?;
        self.hex_ready_in(row, &view, cx)
    }

    /// [`Self::hex_ready`] against the level as `view` has it.
    fn hex_ready_in(&self, row: ColorRow, view: &LevelView, cx: &App) -> Option<String> {
        let editor = self.appearance_editor.as_ref()?;
        let typed = appearance::parse_hex_input(row.input(editor).read(cx).text())?;
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
        if self.change_appearance(&row.change(Some(color.clone())), cx)
            && self.sidebar.push_recent_color(&color)
        {
            self.save_ui();
        }
    }

    /// A swatch picked by click, Enter or Space: sets its colour and, once
    /// the level takes it, puts it first among the recent colours, which
    /// are saved.
    fn pick_swatch(&mut self, row: ColorRow, color: &str, cx: &mut Context<Self>) {
        if self.change_appearance(&row.change(Some(color.to_owned())), cx)
            && self.sidebar.push_recent_color(color)
        {
            self.save_ui();
        }
    }

    /// Sets `change` at the editor's level: a session's goes out in flight
    /// under a request id, a repo's or workspace's with its other stored
    /// fields, the app's is saved on this machine. A change that changes
    /// nothing does nothing; a value the protocol refuses raises a toast.
    /// Returns whether the level took the change, one that changes nothing
    /// included.
    fn change_appearance(&mut self, change: &AppearanceChange, cx: &mut Context<Self>) -> bool {
        let Some(editor) = self.appearance_editor.as_ref() else {
            return false;
        };
        let level = editor.level.clone();
        let Some(view) = self.level_view(&level) else {
            return false;
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
                return false;
            }
        };
        if change.changes_nothing_in(&view.own) {
            return true;
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
        true
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
    pub(crate) fn appearance_editor_layer(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self.appearance_editor.as_ref()?;
        if editor.level == Level::App {
            return None;
        }
        let title = self.appearance_editor_title()?;
        let close = close_button("appearance-close", false).on_click(cx.listener(
            |this, _: &ClickEvent, window, cx| {
                this.close_appearance_editor(window, cx);
            },
        ));
        let focus = self.focused_appearance_stop(window, cx);
        let body = div()
            .px(px(BODY_PAD_X))
            .py(px(BODY_PAD_Y))
            .children(self.appearance_body(focus, cx));
        let panel = dialog_card("appearance-panel", EDITOR_WIDTH)
            .track_focus(&self.appearance_focus)
            .child(dialog_title_bar(title, close))
            .child(body)
            .child(close_footer(
                AppearanceStop::Close.selector(),
                focus == Some(AppearanceStop::Close),
                cx,
            ));
        Some(backdrop("appearance-editor", panel))
    }

    /// The editor's rows in two columns: the colours, then the font; stop
    /// `focus`, which has the keyboard, wears the focus ring.
    pub(crate) fn appearance_body(
        &self,
        focus: Option<AppearanceStop>,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let editor = self.appearance_editor.as_ref()?;
        let view = self.level_view(&editor.level)?;
        let reset = editor.level.reset_label();
        // The interface's font is the app's alone.
        let ui_font = if editor.level == Level::App {
            self.family_section(FontList::Ui, self.ui_font_hint(cx), focus, cx)
        } else {
            None
        };
        let colours = div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .w(px(COLUMN_WIDTH))
            .child(self.color_section(editor, &view, ColorRow::Accent, focus, cx))
            .child(self.color_section(editor, &view, ColorRow::Background, focus, cx))
            .children(ui_font);
        let font = div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .w(px(COLUMN_WIDTH))
            .children(self.family_section(FontList::Terminal, view.family_hint(), focus, cx))
            .child(size_section(&view, reset, focus, cx))
            .child(bold_section(&view, reset, focus, cx));
        Some(div().flex().gap(px(16.0)).child(colours).child(font))
    }

    /// The UI font row's hint: the family the interface draws in, from the
    /// app when the user picked one.
    fn ui_font_hint(&self, cx: &App) -> String {
        ui_font_hint_text(
            self.sidebar.ui_state().ui_font_family.as_deref(),
            &fonts::ui_family(cx),
        )
    }

    /// Font list `list`: its filter, then the list under it, its highlight
    /// ringed while the list has the keyboard, then `hint`.
    fn family_section(
        &self,
        list: FontList,
        hint: String,
        focus: Option<AppearanceStop>,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let editor = self.appearance_editor.as_ref()?;
        let (entries, own) = self.family_list(list, cx)?;
        let rows_of = RowsOf {
            list,
            own: own.as_deref(),
            reset: editor.level.reset_label(),
        };
        let highlight = (focus == Some(list.stop()))
            .then(|| family_highlight(&entries, editor.highlight(list), own.as_deref()))
            .flatten();
        let rows: Vec<AnyElement> = entries
            .into_iter()
            .enumerate()
            .map(|(at, entry)| family_row(&entry, rows_of, highlight == Some(at), cx))
            .collect();
        let selector = list.stop().selector();
        let list_box = div()
            .id(selector)
            .debug_selector(|| selector.to_owned())
            .h(px(FAMILY_LIST_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(editor.scroll(list))
            .flex()
            .flex_col()
            .p(px(3.0))
            .bg(gpui::rgb(GROUND))
            .border_1()
            .border_color(gpui::rgb(LINE_STRONG))
            .rounded(px(LIST_RADIUS))
            .children(rows);
        let (label, hint_selector) = match list {
            FontList::Terminal => ("Font family", "appearance-family-hint"),
            FontList::Ui => ("UI font", "appearance-ui-family-hint"),
        };
        Some(
            field_column(label)
                .child(input_box(
                    list.filter_stop(),
                    editor.filter(list),
                    focus,
                    cx,
                ))
                .child(list_box)
                .child(hint_line(hint_selector, hint)),
        )
    }
}

/// The UI font row's hint for the `stored` family and the `resolved` one
/// the interface draws in: from the app when the stored family drew, else
/// built-in, naming a stored family that is not installed.
fn ui_font_hint_text(stored: Option<&str>, resolved: &str) -> String {
    match stored {
        None => format!("Resolved: {resolved} from built-in"),
        Some(stored) if stored.eq_ignore_ascii_case(resolved) => {
            format!("Resolved: {resolved} from app")
        }
        Some(stored) => format!("Resolved: {resolved} (built-in; {stored:?} is not installed)"),
    }
}

/// A row's reset link: its row, its text, and whether it has the keyboard.
#[derive(Clone, Copy)]
struct ResetLink {
    row: ResetRow,
    label: &'static str,
    focused: bool,
}

/// The change a row's reset link makes: it clears the row's field.
fn reset_change(row: ResetRow) -> AppearanceChange {
    match row {
        ResetRow::Color(row) => row.change(None),
        ResetRow::Size => AppearanceChange::font_size(None),
        ResetRow::Bold => AppearanceChange::font_bold(None),
    }
}

/// Whether row `row`'s reset link acts: the level `own` sets the field.
fn can_reset(row: ResetRow, own: &AppearanceOverrides) -> bool {
    !reset_change(row).changes_nothing_in(own)
}

/// The reset row named `name` (`accent`, `background`, `size` or `bold`).
fn reset_row_named(name: &str) -> Option<ResetRow> {
    match name {
        "size" => Some(ResetRow::Size),
        "bold" => Some(ResetRow::Bold),
        _ => ColorRow::named(name).map(ResetRow::Color),
    }
}

/// `label` with the row's reset link at the right, ringed while it has
/// the keyboard; the link is dimmed and inert while the level `own` does
/// not set the field.
fn header_with_reset(
    label: impl IntoElement,
    reset: ResetLink,
    own: &AppearanceOverrides,
    cx: &mut Context<RootView>,
) -> Div {
    let stop = AppearanceStop::Reset(reset.row);
    let enabled = can_reset(reset.row, own);
    let link = link(stop.selector(), reset.label, enabled)
        .when(reset.focused, |link| link.child(focus_ring(LINK_RADIUS)));
    let link = if enabled {
        link.on_click(click_stop(stop, Aim::Stop, cx))
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
    /// Whether row `row`'s reset link (`accent`, `background`, `size` or
    /// `bold`) acts: the editor's level sets the row's field.
    #[must_use]
    pub fn appearance_can_reset(&self, row: &str) -> bool {
        let Some(view) = self
            .appearance_editor
            .as_ref()
            .and_then(|editor| self.level_view(&editor.level))
        else {
            return false;
        };
        reset_row_named(row).is_some_and(|row| can_reset(row, &view.own))
    }

    /// A colour row: the presets, the recent colours, the hex field with
    /// its preview and Apply, and the hint.
    fn color_section(
        &self,
        editor: &AppearanceEditor,
        view: &LevelView,
        row: ColorRow,
        focus: Option<AppearanceStop>,
        cx: &mut Context<Self>,
    ) -> Div {
        let recent = self.sidebar.recent_colors();
        let ready = self.hex_ready_in(row, view, cx).is_some();
        let own = row.own(&view.own);
        let key = row.key();
        // The swatch under a focused line's cursor wears the ring.
        let cursor = |line: SwatchLine, colors: &[u32]| {
            (focus == Some(AppearanceStop::Swatches(row, line)))
                .then(|| swatch_cursor(colors, editor.swatch_cursor, own))
                .flatten()
        };
        let preset_colors: Vec<u32> = row.presets().iter().map(|preset| preset.color).collect();
        let preset_cursor = cursor(SwatchLine::Presets, &preset_colors);
        let presets: Vec<AnyElement> = row
            .presets()
            .iter()
            .enumerate()
            .map(|(at, preset)| {
                let color = appearance::hex(preset.color);
                swatch(
                    format!(
                        "appearance-{key}-preset-{}",
                        preset.name.to_ascii_lowercase()
                    ),
                    preset.color,
                    format!("{} ({color})", preset.name),
                    Mark {
                        chosen: own == Some(preset.color),
                        focused: preset_cursor == Some(at),
                    },
                )
                .on_click(click_stop(
                    AppearanceStop::Swatches(row, SwatchLine::Presets),
                    Aim::Swatch(preset.color),
                    cx,
                ))
                .into_any_element()
            })
            .collect();
        let recent_cursor = cursor(SwatchLine::Recent, &recent);
        let recent: Vec<AnyElement> = recent
            .iter()
            .enumerate()
            .map(|(at, &value)| {
                let color = appearance::hex(value);
                swatch(
                    format!("appearance-{key}-recent-{value:06x}"),
                    value,
                    format!("Recent color {color}"),
                    Mark {
                        chosen: own == Some(value),
                        focused: recent_cursor == Some(at),
                    },
                )
                .on_click(click_stop(
                    AppearanceStop::Swatches(row, SwatchLine::Recent),
                    Aim::Swatch(value),
                    cx,
                ))
                .into_any_element()
            })
            .collect();
        let label = field_label(row.label());
        div()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(header_with_reset(
                label,
                ResetLink {
                    row: ResetRow::Color(row),
                    label: editor.level.reset_label(),
                    focused: focus == Some(AppearanceStop::Reset(ResetRow::Color(row))),
                },
                &view.own,
                cx,
            ))
            .child(swatch_line(presets))
            .when(!recent.is_empty(), |section| {
                section.child(swatch_line(recent))
            })
            .child(hex_line(editor, row, ready, focus, cx))
            .child(hint_line(
                match row {
                    ColorRow::Accent => "appearance-accent-hint",
                    ColorRow::Background => "appearance-background-hint",
                },
                view.color_hint(row),
            ))
    }
}

/// The keyboard stops of the editor's rows ([`crate::appearance_stops`]).
impl RootView {
    /// The stops Tab walks, Apply and the reset links only while they act;
    /// `close` adds the editor's Close.
    pub(crate) fn appearance_stops(&self, close: bool, cx: &App) -> Vec<AppearanceStop> {
        let has_recent = !self.sidebar.recent_colors().is_empty();
        let view = self
            .appearance_editor
            .as_ref()
            .and_then(|editor| self.level_view(&editor.level));
        stops(has_recent, self.ui_font_shown(), close, |stop| match stop {
            AppearanceStop::Apply(row) => view
                .as_ref()
                .is_some_and(|view| self.hex_ready_in(row, view, cx).is_some()),
            AppearanceStop::Reset(row) => {
                view.as_ref().is_some_and(|view| can_reset(row, &view.own))
            }
            _ => true,
        })
    }

    /// Every stop in the order Tab walks them, enabled or not; `close`
    /// adds the editor's Close.
    pub(crate) fn appearance_stop_order(&self, close: bool) -> Vec<AppearanceStop> {
        let has_recent = !self.sidebar.recent_colors().is_empty();
        stops(has_recent, self.ui_font_shown(), close, |_| true)
    }

    /// Whether the rows show the UI font: the app level's do.
    fn ui_font_shown(&self) -> bool {
        self.appearance_editor
            .as_ref()
            .is_some_and(|editor| editor.level == Level::App)
    }

    /// The Appearance stop that has the keyboard, if one does: Settings'
    /// while Settings is open, else the open editor's.
    pub(crate) fn focused_appearance_stop(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<AppearanceStop> {
        if self.settings_open() {
            return self.focused_settings_control(window, cx)?.appearance_stop();
        }
        let stop = self.appearance_editor.as_ref()?.stop?;
        let has_keyboard = if stop.is_field() {
            self.appearance_field_has_keyboard(stop, window, cx)
        } else {
            self.appearance_focus.is_focused(window)
        };
        (has_keyboard && self.appearance_stops(true, cx).contains(&stop)).then_some(stop)
    }

    /// Whether a session's, repo's or workspace's editor is open.
    fn editor_open(&self) -> bool {
        self.appearance_editor
            .as_ref()
            .is_some_and(|editor| editor.level != Level::App)
    }

    /// Gives the editor's keyboard to `stop`; arriving at a stop puts its
    /// cursor or highlight back on the chosen value.
    fn move_editor_focus(&mut self, stop: AppearanceStop, window: &mut Window, cx: &App) {
        let Some(editor) = self.appearance_editor.as_ref() else {
            return;
        };
        if editor.stop != Some(stop) {
            self.arrive_at_appearance_stop(stop, cx);
        }
        if let Some(editor) = self.appearance_editor.as_mut() {
            editor.stop = Some(stop);
        }
        self.focus_appearance_stop(stop, window, cx);
    }

    /// Tab (Shift-Tab with `back`) in the editor: on to the next stop,
    /// wrapping between Close and the first stop.
    fn step_editor_focus(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let ring = self.appearance_stops(true, cx);
        let at = self.focused_appearance_stop(window, cx);
        if let Some(next) = step(&ring, at, back) {
            self.move_editor_focus(next, window, cx);
        }
        cx.notify();
    }

    /// Space or Enter on the editor's `stop`, or a click on it. When the
    /// press took the stop out of the ring, the keyboard moves to the stop
    /// after its place, so the next Tab walks on from there.
    fn press_editor_stop(
        &mut self,
        stop: AppearanceStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.press_appearance_stop(stop, window, cx);
        if self.editor_open() {
            let ring = self.appearance_stops(true, cx);
            if !ring.contains(&stop)
                && let Some(next) = after_lost(&self.appearance_stop_order(true), &ring, stop)
            {
                self.move_editor_focus(next, window, cx);
            }
        }
        cx.notify();
    }

    /// A key while a session's, repo's or workspace's editor is open;
    /// returns whether it was the editor's stops'. Tab and Shift-Tab walk
    /// the stops, Space or Enter presses the focused one, and the arrows
    /// move along a swatch line or the family list. While a text field has
    /// the keyboard, only Tab is theirs.
    pub(crate) fn on_editor_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mods = &keystroke.modifiers;
        if !self.editor_open() || mods.control || mods.alt || mods.platform || mods.function {
            return false;
        }
        match (keystroke.key.as_str(), mods.shift) {
            ("tab", back) => {
                self.step_editor_focus(back, window, cx);
                true
            }
            _ if self.appearance_field_focused(window, cx) => false,
            ("space" | "enter", false) => match self.focused_appearance_stop(window, cx) {
                Some(stop) => {
                    self.press_editor_stop(stop, window, cx);
                    true
                }
                None => false,
            },
            (key, false) => self
                .focused_appearance_stop(window, cx)
                .is_some_and(|stop| self.on_appearance_stop_key(stop, key, cx)),
            _ => false,
        }
    }

    /// The text field a field stop is.
    fn stop_field(&self, stop: AppearanceStop) -> Option<&Entity<TextInput>> {
        let editor = self.appearance_editor.as_ref()?;
        match stop {
            AppearanceStop::Hex(row) => Some(row.input(editor)),
            AppearanceStop::FamilyFilter => Some(&editor.family_filter),
            AppearanceStop::UiFamilyFilter => Some(&editor.ui_family_filter),
            _ => None,
        }
    }

    /// Whether field stop `stop`'s text field has the keyboard.
    pub(crate) fn appearance_field_has_keyboard(
        &self,
        stop: AppearanceStop,
        window: &Window,
        cx: &App,
    ) -> bool {
        self.stop_field(stop)
            .is_some_and(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Gives the keyboard to `stop`: a field takes it itself, every other
    /// stop through the modal's focus.
    pub(crate) fn focus_appearance_stop(
        &self,
        stop: AppearanceStop,
        window: &mut Window,
        cx: &App,
    ) {
        match self.stop_field(stop) {
            Some(input) => input.read(cx).focus_handle(cx).focus(window),
            None => self.appearance_focus.focus(window),
        }
    }

    /// The keyboard arrives at `stop`: a swatch line's cursor goes back to
    /// the chosen swatch and the family list's highlight to the chosen
    /// family, scrolled into view.
    pub(crate) fn arrive_at_appearance_stop(&mut self, stop: AppearanceStop, cx: &App) {
        let Some(editor) = self.appearance_editor.as_mut() else {
            return;
        };
        editor.swatch_cursor = None;
        editor.family_highlight = None;
        editor.ui_family_highlight = None;
        if let Some(list) = stop.font_list()
            && let Some((entries, chosen)) = self.family_list(list, cx)
        {
            self.reveal_family_highlight(list, &entries, chosen.as_deref());
        }
    }

    /// Space or Enter on `stop`, or a click on it.
    pub(crate) fn press_appearance_stop(
        &mut self,
        stop: AppearanceStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match stop {
            AppearanceStop::Swatches(row, line) => {
                if let Some(color) = self.swatch_under_cursor(row, line) {
                    self.pick_swatch(row, &appearance::hex(color), cx);
                }
            }
            AppearanceStop::Hex(_)
            | AppearanceStop::FamilyFilter
            | AppearanceStop::UiFamilyFilter => {}
            AppearanceStop::Apply(row) => self.apply_hex(row, cx),
            AppearanceStop::Reset(row) => self.reset_appearance_row(row, cx),
            AppearanceStop::FamilyList => self.pick_highlighted_family(FontList::Terminal, cx),
            AppearanceStop::UiFamilyList => self.pick_highlighted_family(FontList::Ui, cx),
            AppearanceStop::SizeDown => self.step_appearance_size(-1.0, cx),
            AppearanceStop::SizeUp => self.step_appearance_size(1.0, cx),
            AppearanceStop::Bold => self.toggle_appearance_bold(cx),
            AppearanceStop::Close => self.close_appearance_editor(window, cx),
        }
    }

    /// An arrow key on `stop`: Left and Right move a swatch line's cursor,
    /// Up and Down the family list's highlight, each stopping at the ends.
    /// Returns whether the key was the stop's.
    pub(crate) fn on_appearance_stop_key(
        &mut self,
        stop: AppearanceStop,
        key: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        match (stop, key) {
            (AppearanceStop::Swatches(row, line), "left" | "right") => {
                let colors = self.swatch_colors(row, line);
                let chosen = self.chosen_color(row);
                if let Some(editor) = self.appearance_editor.as_mut() {
                    editor.swatch_cursor =
                        step_swatch(&colors, editor.swatch_cursor, chosen, key == "right");
                }
            }
            (AppearanceStop::FamilyList | AppearanceStop::UiFamilyList, "up" | "down") => {
                if let Some(list) = stop.font_list()
                    && let Some((entries, chosen)) = self.family_list(list, cx)
                {
                    if let Some(editor) = self.appearance_editor.as_mut() {
                        let next = step_family(
                            &entries,
                            editor.highlight(list),
                            chosen.as_deref(),
                            key == "down",
                        );
                        editor.set_highlight(list, next);
                    }
                    self.reveal_family_highlight(list, &entries, chosen.as_deref());
                }
            }
            _ => return false,
        }
        cx.notify();
        true
    }

    /// The colours of `row`'s swatch line `line`, in order.
    fn swatch_colors(&self, row: ColorRow, line: SwatchLine) -> Vec<u32> {
        match line {
            SwatchLine::Presets => row.presets().iter().map(|preset| preset.color).collect(),
            SwatchLine::Recent => self.sidebar.recent_colors(),
        }
    }

    /// The colour the editor's level sets in `row`.
    fn chosen_color(&self, row: ColorRow) -> Option<u32> {
        let editor = self.appearance_editor.as_ref()?;
        row.own(&self.level_view(&editor.level)?.own)
    }

    fn swatch_under_cursor(&self, row: ColorRow, line: SwatchLine) -> Option<u32> {
        let editor = self.appearance_editor.as_ref()?;
        let colors = self.swatch_colors(row, line);
        let at = swatch_cursor(&colors, editor.swatch_cursor, self.chosen_color(row))?;
        colors.get(at).copied()
    }

    fn reset_appearance_row(&mut self, row: ResetRow, cx: &mut Context<Self>) {
        self.change_appearance(&reset_change(row), cx);
    }

    /// Font list `list` as it shows under its filter, and the family it
    /// has chosen: the level's own terminal family, or the app's UI font.
    fn family_list(&self, list: FontList, cx: &App) -> Option<(Vec<FamilyEntry>, Option<String>)> {
        let editor = self.appearance_editor.as_ref()?;
        let (bundled, own): (&[&'static str], Option<String>) = match list {
            FontList::Terminal => (
                &fonts::BUNDLED_FAMILIES,
                self.level_view(&editor.level)?.own.terminal_font_family,
            ),
            FontList::Ui => (
                &fonts::BUNDLED_UI_FAMILIES,
                self.sidebar.ui_state().ui_font_family.clone(),
            ),
        };
        let filter = editor.filter(list).read(cx).text();
        let entries = family_entries(bundled, &editor.system_families, own.as_deref(), filter);
        Some((entries, own))
    }

    /// The row of font list `list` the highlight stands on.
    fn highlighted_family_row(
        &self,
        list: FontList,
        cx: &App,
    ) -> Option<(Vec<FamilyEntry>, usize)> {
        let editor = self.appearance_editor.as_ref()?;
        let (entries, own) = self.family_list(list, cx)?;
        let at = family_highlight(&entries, editor.highlight(list), own.as_deref())?;
        Some((entries, at))
    }

    /// Scrolls font list `list`, as `entries` lists it with its chosen
    /// family `chosen`, so the highlighted row shows.
    fn reveal_family_highlight(
        &self,
        list: FontList,
        entries: &[FamilyEntry],
        chosen: Option<&str>,
    ) {
        let Some(editor) = self.appearance_editor.as_ref() else {
            return;
        };
        if let Some(at) = family_highlight(entries, editor.highlight(list), chosen) {
            editor.scroll(list).scroll_to_item(at);
        }
    }

    /// Picks the highlighted row of font list `list`: the terminal family
    /// changes at the editor's level, the UI font at the app's.
    fn pick_highlighted_family(&mut self, list: FontList, cx: &mut Context<Self>) {
        let pick = self
            .highlighted_family_row(list, cx)
            .and_then(|(entries, at)| entries.get(at).and_then(FamilyEntry::pick));
        let family = match pick {
            Some(FamilyPick::Reset) => None,
            Some(FamilyPick::Family(name)) => Some(name.to_string()),
            None => return,
        };
        match list {
            FontList::Terminal => {
                self.change_appearance(&AppearanceChange::font_family(family), cx);
            }
            FontList::Ui => {
                if self.sidebar.ui_state().ui_font_family != family {
                    self.set_ui_font(family, cx);
                }
            }
        }
    }

    /// A press in field stop `stop`'s frame gives the field the keyboard
    /// and makes it the stop Tab walks on from.
    fn press_appearance_field(
        &mut self,
        stop: AppearanceStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_open() {
            self.press_settings_control(SettingsControl::Appearance(stop), window, cx);
        } else {
            self.move_editor_focus(stop, window, cx);
        }
    }

    /// A click on `stop`'s control `aim`: in the Settings modal `stop`
    /// takes the keyboard, as every Settings control does; its cursor or
    /// highlight moves to what was clicked, and the stop is pressed.
    fn click_appearance_stop(
        &mut self,
        stop: AppearanceStop,
        aim: Aim,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let aimed = !matches!(aim, Aim::Stop);
        if let Some(editor) = self.appearance_editor.as_mut() {
            match aim {
                Aim::Stop => {}
                Aim::Swatch(color) => editor.swatch_cursor = Some(color),
                Aim::Family(pick) => {
                    if let Some(list) = stop.font_list() {
                        editor.set_highlight(list, Some(pick));
                    }
                }
            }
        }
        if aimed {
            // The clicked swatch or row stands: arriving would move the
            // cursor or highlight, and the list's scroll, back to the
            // chosen one.
            self.hold_appearance_stop(stop);
            self.focus_appearance_stop(stop, window, cx);
        } else if self.settings_open() {
            self.move_settings_focus(SettingsControl::Appearance(stop), window, cx);
        } else {
            self.move_editor_focus(stop, window, cx);
        }
        if self.settings_open() {
            self.press_settings_control(SettingsControl::Appearance(stop), window, cx);
        } else {
            self.press_editor_stop(stop, window, cx);
        }
    }

    /// Makes `stop` the one Tab walks on from, without arriving at it: in
    /// Settings' slot while Settings is open, else in the editor's.
    fn hold_appearance_stop(&mut self, stop: AppearanceStop) {
        if self.settings_open() {
            self.settings_control = Some(SettingsControl::Appearance(stop));
        } else if let Some(editor) = self.appearance_editor.as_mut() {
            editor.stop = Some(stop);
        }
    }
}

/// What a click on a stop lands on: a swatch of a swatch line, a row of
/// the family list, or the stop as a whole.
#[derive(Debug, Clone)]
enum Aim {
    Stop,
    Swatch(u32),
    Family(FamilyPick),
}

/// The click on `stop`'s control `aim`.
fn click_stop(
    stop: AppearanceStop,
    aim: Aim,
    cx: &mut Context<RootView>,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    cx.listener(move |this, _: &ClickEvent, window, cx| {
        this.click_appearance_stop(stop, aim.clone(), window, cx);
    })
}

/// The hex field, its live preview, Apply, and the format hint while the
/// field holds something that does not parse.
fn hex_line(
    editor: &AppearanceEditor,
    row: ColorRow,
    ready: bool,
    focus: Option<AppearanceStop>,
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
        .rounded(px(SWATCH_RADIUS))
        .border_1()
        .border_color(gpui::rgb(LINE_STRONG))
        .when_some(
            parsed.as_deref().and_then(appearance::parse_color),
            |swatch, color| swatch.bg(gpui::rgb(color)),
        );
    let apply_stop = AppearanceStop::Apply(row);
    let apply = apply_button(apply_stop.selector(), ready)
        .when(focus == Some(apply_stop), |apply| {
            apply.child(focus_ring(ButtonSize::Regular.radius()))
        });
    let apply = if ready {
        apply.on_click(click_stop(apply_stop, Aim::Stop, cx))
    } else {
        apply
    };
    let bad = !text.trim().is_empty() && parsed.is_none();
    let hint_name = format!("appearance-{key}-hex-hint");
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(input_box(AppearanceStop::Hex(row), input, focus, cx))
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

/// Apply, an outlined dialog button; a disabled one is dimmed, with the
/// default cursor and no hover.
fn apply_button(selector: &str, enabled: bool) -> Stateful<Div> {
    outlined_button(selector, ButtonSize::Regular, enabled)
        .flex_none()
        .child("Apply")
}

/// A line of colour swatches that wraps.
fn swatch_line(swatches: Vec<AnyElement>) -> Div {
    div().flex().flex_wrap().gap(px(6.0)).children(swatches)
}

/// How a swatch is marked: the level's own colour, and the one under a
/// focused line's cursor.
#[derive(Clone, Copy)]
struct Mark {
    chosen: bool,
    focused: bool,
}

/// A clickable colour swatch with its tooltip; the level's own colour is
/// outlined in the text colour, two pixels wide, and the one under a
/// focused line's cursor wears the focus ring.
fn swatch(selector: String, color: u32, tip: String, mark: Mark) -> Stateful<Div> {
    let swatch = div()
        .id(ElementId::Name(SharedString::from(selector.clone())))
        .debug_selector(|| selector)
        .flex_none()
        .size(px(SWATCH_SIZE))
        .rounded(px(SWATCH_RADIUS))
        .bg(gpui::rgb(color))
        .cursor_pointer()
        .tooltip(tooltip(tip))
        .when(mark.focused, |swatch| {
            swatch.child(focus_ring(SWATCH_RADIUS))
        });
    if mark.chosen {
        swatch.border_2().border_color(gpui::rgb(TEXT))
    } else {
        swatch.border_1().border_color(gpui::rgb(LINE_STRONG))
    }
}

/// What a font list's rows share: the list, its chosen family, and what
/// its reset row says.
#[derive(Clone, Copy)]
struct RowsOf<'a> {
    list: FontList,
    own: Option<&'a str>,
    reset: &'static str,
}

/// One line of a font list: the reset row, a group header, or a family,
/// the chosen one ticked and the `highlighted` one ringed.
fn family_row(
    entry: &FamilyEntry,
    rows_of: RowsOf<'_>,
    highlighted: bool,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let RowsOf { list, own, reset } = rows_of;
    let prefix = list.prefix();
    let ring = |row: Stateful<Div>| row.when(highlighted, |row| row.child(focus_ring(ROW_RADIUS)));
    match entry {
        FamilyEntry::Header(header) => muted_row(*header).into_any_element(),
        FamilyEntry::Reset => ring(list_row(&format!("{prefix}-reset"), reset, own.is_none()))
            .on_click(click_stop(list.stop(), Aim::Family(FamilyPick::Reset), cx))
            .into_any_element(),
        FamilyEntry::Family { name, saved } => {
            let label = if *saved {
                format!("{name} (saved)")
            } else {
                name.to_string()
            };
            let chosen = same_family(own, name);
            ring(list_row(&format!("{prefix}-option-{name}"), label, chosen))
                .on_click(click_stop(
                    list.stop(),
                    Aim::Family(FamilyPick::Family(name.clone())),
                    cx,
                ))
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
        .rounded(px(ROW_RADIUS))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER)))
        .child(
            div()
                .flex_none()
                .w(px(12.0))
                .child(if chosen { "✓" } else { "" }),
        )
        .child(label.into())
}

/// The size: `−`, the size it resolves to, `+`.
fn size_section(
    view: &LevelView,
    reset: &'static str,
    focus: Option<AppearanceStop>,
    cx: &mut Context<RootView>,
) -> Div {
    let focused = |stop| focus == Some(stop);
    let down = dialog_button(
        AppearanceStop::SizeDown.selector(),
        "−".to_owned(),
        false,
        focused(AppearanceStop::SizeDown),
        true,
    )
    .on_click(click_stop(AppearanceStop::SizeDown, Aim::Stop, cx));
    let up = dialog_button(
        AppearanceStop::SizeUp.selector(),
        "+".to_owned(),
        false,
        focused(AppearanceStop::SizeUp),
        true,
    )
    .on_click(click_stop(AppearanceStop::SizeUp, Aim::Stop, cx));
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
    let label = field_label("Font size");
    div()
        .flex()
        .flex_col()
        .gap(px(7.0))
        .child(header_with_reset(
            label,
            ResetLink {
                row: ResetRow::Size,
                label: reset,
                focused: focused(AppearanceStop::Reset(ResetRow::Size)),
            },
            &view.own,
            cx,
        ))
        .child(stepper)
        .child(hint_line("appearance-size-hint", view.size_hint()))
}

/// The bold checkbox, ticked while the level resolves to bold.
fn bold_section(
    view: &LevelView,
    reset: &'static str,
    focus: Option<AppearanceStop>,
    cx: &mut Context<RootView>,
) -> Div {
    let checkbox = checkbox_row(
        AppearanceStop::Bold.selector(),
        view.resolved.font_bold.value,
        focus == Some(AppearanceStop::Bold),
        BOLD_LABEL,
    )
    .on_click(click_stop(AppearanceStop::Bold, Aim::Stop, cx));
    div()
        .flex()
        .flex_col()
        .gap(px(7.0))
        .child(header_with_reset(
            checkbox,
            ResetLink {
                row: ResetRow::Bold,
                label: reset,
                focused: focus == Some(AppearanceStop::Reset(ResetRow::Bold)),
            },
            &view.own,
            cx,
        ))
        .child(hint_line("appearance-bold-hint", view.bold_hint()))
}

/// Field stop `stop`'s text field in its frame, tagged `<selector>`. The
/// frame is ringed while the field has the keyboard, the ring tagged
/// `<selector>-ring`. A press anywhere in the frame gives the field the
/// keyboard and makes it the stop Tab walks on from; the press is kept
/// from the modal's own focus.
fn input_box(
    stop: AppearanceStop,
    input: &Entity<TextInput>,
    focus: Option<AppearanceStop>,
    cx: &mut Context<RootView>,
) -> Div {
    let selector = stop.selector();
    let focused = focus == Some(stop);
    field_frame_with_ring(focused, format!("{selector}-ring"))
        .debug_selector(move || selector.to_owned())
        .flex_1()
        .min_w(px(0.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                // The modal's tracked focus would take the keyboard back.
                window.prevent_default();
                this.press_appearance_field(stop, window, cx);
            }),
        )
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
        .rounded(px(LINK_RADIUS))
        .text_color(gpui::rgb(MUTED))
        .child(label);
    if enabled {
        link.cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER)).text_color(gpui::rgb(TEXT)))
    } else {
        link.opacity(0.5).cursor_default()
    }
}

/// The footer band with its Close button, tagged `selector` and ringed
/// while `focused`, the ring tagged `<selector>-ring`.
pub(crate) fn close_footer(
    selector: &'static str,
    focused: bool,
    cx: &mut Context<RootView>,
) -> Div {
    let ring =
        focus_ring(ButtonSize::Regular.radius()).debug_selector(move || format!("{selector}-ring"));
    dialog_footer().child(
        dialog_button(selector, "Close".to_owned(), false, false, true)
            .when(focused, |close| close.child(ring))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.close_appearance_editor(window, cx);
            })),
    )
}

#[cfg(test)]
mod tests {
    use super::ui_font_hint_text;

    #[test]
    fn the_ui_font_hint_names_its_source_and_a_missing_stored_family() {
        assert_eq!(
            ui_font_hint_text(None, "Geist"),
            "Resolved: Geist from built-in"
        );
        assert_eq!(
            ui_font_hint_text(Some("inter"), "Inter"),
            "Resolved: Inter from app"
        );
        assert_eq!(
            ui_font_hint_text(Some("Nope Sans"), "Geist"),
            "Resolved: Geist (built-in; \"Nope Sans\" is not installed)"
        );
    }
}
