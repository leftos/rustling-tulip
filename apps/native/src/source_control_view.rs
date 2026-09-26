//! The source-control panel the activity rail shows: a header with the
//! refresh button and the repo picker, the line naming what the panel
//! follows, and one section per repo tree with its change count. It also
//! asks the daemon for the statuses the panel and the rail's badge read.

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, MouseDownEvent, SharedString, Stateful,
    Window, anchored, deferred, div, point, prelude::*, px, svg,
};
use protocol::{ClientMessage, SessionMember};

use crate::assets::REFRESH_ICON;
use crate::session_menu::menu_frame;
use crate::session_menu::menu_item;
use crate::sidebar::{Activity, display_label};
use crate::source_control::{ScKey, Section, sections};
use crate::{BORDER, HOVER_BG, MUTED, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, tooltip};

/// The panel's title.
pub const SC_TITLE: &str = "Source control";
/// The picker's row that follows the active pane.
pub const AUTO_LABEL: &str = "Auto · follow active pane";
/// The panel's text with no repo registered.
pub const NO_REPOS_HINT: &str =
    "Register a repo from the Sessions sidebar to inspect changes here.";
/// The context line's tooltip when nothing focused names a repo.
pub const NO_ACTIVE_PANE_TIP: &str = "No pane is focused right now — falling back to the first registered repo. Pick a session in the sidebar to follow it, or pin a repo from the picker above.";
const REFRESH_TIP: &str = "Refresh status and history";
const HEADER_HEIGHT: f32 = 26.0;
const PICKER_HEIGHT: f32 = 20.0;
const ROW_PADDING: f32 = 8.0;
const REFRESH_ICON_SIZE: f32 = 14.0;

/// The line under the header naming what the panel follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScContext {
    pub text: String,
    pub tooltip: Option<&'static str>,
}

/// One section as the panel draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScSectionRow {
    /// The section's key id, which its selectors end in.
    pub id: String,
    /// The repo name, then ` · <branch>` when a branch is known.
    pub title: String,
    /// The distinct changed paths, when loaded and not zero.
    pub count: Option<usize>,
    /// `loading…`, `working tree clean` or `N changed`.
    pub body: String,
}

/// Everything the source-control panel draws, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScPanel {
    /// Whether the refresh button shows: only with a repo registered.
    pub refresh: bool,
    /// The picker button's label, when the picker shows.
    pub picker: Option<String>,
    pub context: Option<ScContext>,
    pub sections: Vec<ScSectionRow>,
    /// The text shown instead of sections with no repo registered.
    pub empty_hint: Option<&'static str>,
}

/// One row of the open picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScPickerRow {
    pub selector: String,
    pub label: String,
    /// The repo it pins; `None` is Auto.
    pub repo_id: Option<String>,
    pub checked: bool,
}

impl RootView {
    /// The members of the focused pane's session, when it has any.
    fn sc_focused_members(&self) -> Option<&[SessionMember]> {
        let id = self.focused_session()?;
        self.sidebar
            .session(&id)
            .map(|session| session.members.as_slice())
            .filter(|members| !members.is_empty())
    }

    /// The pinned repo, when it is still registered.
    fn sc_pinned(&self) -> Option<&str> {
        self.sidebar
            .source_control()
            .pinned_repo
            .as_deref()
            .filter(|id| self.sidebar.repos().iter().any(|repo| repo.id == *id))
    }

    fn sc_sections(&self) -> Vec<Section> {
        sections(
            self.sidebar.repos(),
            self.sc_focused_members(),
            self.sidebar.source_control().pinned_repo.as_deref(),
        )
    }

    /// Every registered repo's main tree.
    fn sc_main_keys(&self) -> Vec<ScKey> {
        self.sidebar
            .repos()
            .iter()
            .map(|repo| ScKey {
                repo_id: repo.id.clone(),
                worktree: None,
            })
            .collect()
    }

    /// The rail's badge: the distinct changed paths of the focused session's
    /// trees, or of every registered repo's main tree when it has no
    /// members.
    #[must_use]
    pub fn sc_badge(&self) -> usize {
        if self.sc_focused_members().is_some() {
            let keys: Vec<ScKey> = self
                .sc_sections()
                .into_iter()
                .map(|section| section.key)
                .collect();
            self.sc.badge_total(&keys)
        } else {
            self.sc.badge_total(&self.sc_main_keys())
        }
    }

    /// The repo the panel is pinned to, as persisted.
    #[must_use]
    pub fn sc_pinned_repo(&self) -> Option<&str> {
        self.sidebar.source_control().pinned_repo.as_deref()
    }

    /// Whether the repo picker's menu is open.
    #[must_use]
    pub fn sc_picker_open(&self) -> bool {
        self.sc_picker_open
    }

    /// Whether the picker's button shows: the source-control panel is up,
    /// the focused session names no repos, and two or more are registered.
    fn sc_picker_offered(&self) -> bool {
        self.sidebar.activity() == Activity::SourceControl
            && !self.sidebar.is_collapsed()
            && self.sc_focused_members().is_none()
            && self.sidebar.repos().len() >= 2
    }

    /// Closes the picker once its button no longer shows, so its menu and
    /// the blocking layer under it never outlive the button.
    pub(crate) fn drop_stale_sc_picker(&mut self) {
        if self.sc_picker_open && !self.sc_picker_offered() {
            self.sc_picker_open = false;
        }
    }

    /// The picker's rows: Auto, then every repo, the current choice checked.
    #[must_use]
    pub fn sc_picker_rows(&self) -> Vec<ScPickerRow> {
        let pinned = self.sc_pinned();
        let auto = ScPickerRow {
            selector: "sc-picker-auto".to_owned(),
            label: AUTO_LABEL.to_owned(),
            repo_id: None,
            checked: pinned.is_none(),
        };
        let repos = self.sidebar.repos().iter().map(|repo| ScPickerRow {
            selector: format!("sc-picker-{}", repo.id),
            label: repo.name.clone(),
            repo_id: Some(repo.id.clone()),
            checked: pinned == Some(repo.id.as_str()),
        });
        std::iter::once(auto).chain(repos).collect()
    }

    /// What the panel shows now.
    #[must_use]
    pub fn source_control_panel(&self) -> ScPanel {
        let repos = self.sidebar.repos();
        if repos.is_empty() {
            return ScPanel {
                refresh: false,
                picker: None,
                context: None,
                sections: Vec::new(),
                empty_hint: Some(NO_REPOS_HINT),
            };
        }
        let members = self.sc_focused_members();
        let sections = self.sc_sections();
        let picker = self.sc_picker_offered().then(|| {
            let name = self
                .sc_pinned()
                .and_then(|id| repos.iter().find(|repo| repo.id == id))
                .map_or(AUTO_LABEL, |repo| repo.name.as_str());
            format!("{name} ▾")
        });
        ScPanel {
            refresh: true,
            picker,
            context: self.sc_context(members.map(<[SessionMember]>::len), &sections),
            sections: sections
                .iter()
                .map(|section| self.sc_row(section))
                .collect(),
            empty_hint: None,
        }
    }

    /// The context line: the focused session and its repo count, else the
    /// shown repo and whether it is pinned.
    fn sc_context(&self, member_count: Option<usize>, sections: &[Section]) -> Option<ScContext> {
        if let Some(count) = member_count {
            let session = self.focused_session()?;
            let label = self.sidebar.session(&session).map(display_label)?;
            let plural = if count == 1 { "" } else { "s" };
            return Some(ScContext {
                text: format!("{label} · {count} repo{plural}"),
                tooltip: None,
            });
        }
        let name = &sections.first()?.repo_name;
        Some(if self.sc_pinned().is_some() {
            ScContext {
                text: format!("{name} · pinned"),
                tooltip: None,
            }
        } else {
            ScContext {
                text: format!("{name} · no active pane"),
                tooltip: Some(NO_ACTIVE_PANE_TIP),
            }
        })
    }

    fn sc_row(&self, section: &Section) -> ScSectionRow {
        let title = match &section.branch {
            Some(branch) => format!("{} · {branch}", section.repo_name),
            None => section.repo_name.clone(),
        };
        let loaded = self.sc.is_loaded(&section.key);
        let count = self.sc.badge_total(std::slice::from_ref(&section.key));
        let body = if !loaded {
            "loading…".to_owned()
        } else if count == 0 {
            "working tree clean".to_owned()
        } else {
            format!("{count} changed")
        };
        ScSectionRow {
            id: section.key.id(),
            title,
            count: (loaded && count > 0).then_some(count),
            body,
        }
    }

    /// Asks for the status of every tree the panel or the badge reads that
    /// has none and no request out: the current sections, and every
    /// registered repo's main tree.
    pub(crate) fn seed_source_control(&mut self) {
        let mut wanted: Vec<ScKey> = self
            .sc_sections()
            .into_iter()
            .map(|section| section.key)
            .collect();
        wanted.extend(self.sc_main_keys());
        for key in self.sc.wanted_missing(&wanted) {
            self.sc.mark_requested(key.clone());
            self.send(ClientMessage::RepoStatus {
                repo_id: key.repo_id,
                worktree_path: key.worktree,
                request_id: None,
            });
        }
        self.seed_history();
    }

    /// Asks again for every current section's status and history; the
    /// stored ones stay on screen until the answers replace them.
    fn refresh_source_control(&mut self) {
        for section in self.sc_sections() {
            self.sc.mark_requested(section.key.clone());
            self.send(ClientMessage::RepoStatus {
                repo_id: section.key.repo_id,
                worktree_path: section.key.worktree,
                request_id: None,
            });
        }
        self.refresh_history();
    }

    /// Opens the picker's menu, closing any other menu, unless a dialog or
    /// a modal notice is up or the connection is down.
    fn open_sc_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let blocked = self.exit.is_some()
            || self.spawn_dialog.is_some()
            || self.shell_dialog.is_some()
            || self.appearance_editor.is_some()
            || self.delete_dialog.is_some()
            || self.run_confirm.is_some()
            || self.notices.has_modal()
            || self.conn.overlay().is_some();
        if blocked {
            return;
        }
        self.close_session_menu(window, cx);
        self.close_container_menu(window, cx);
        self.close_shell_menu(window, cx);
        self.close_tab_menu(window, cx);
        self.sc_picker_open = true;
        self.menu_focus.focus(window);
        cx.notify();
    }

    /// Closes the picker's menu and hands the keyboard back to the active
    /// pane.
    pub(crate) fn close_sc_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.sc_picker_open) {
            self.focus_active_pane(window, cx);
            cx.notify();
        }
    }

    /// A picker row: pins `repo_id` (`None` follows the active pane), saves,
    /// and asks for what the new section needs.
    fn pick_sc_repo(
        &mut self,
        repo_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_sc_picker(window, cx);
        if self.sidebar.set_pinned_repo(repo_id) {
            self.save_ui();
            self.seed_source_control();
        }
        cx.notify();
    }

    /// The layer under the open picker that keeps a click outside it from
    /// reaching what lies beneath.
    pub(crate) fn sc_picker_layer(&self) -> Option<AnyElement> {
        self.sc_picker_open.then(|| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .into_any_element()
        })
    }

    /// The panel, `width` wide.
    pub(crate) fn source_control_view(&self, width: f32, cx: &mut Context<Self>) -> Div {
        let panel = self.source_control_panel();
        let body = match panel.empty_hint {
            Some(hint) => div()
                .id("sc-body")
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .child(
                    div()
                        .px(px(ROW_PADDING))
                        .py(px(6.0))
                        .text_color(gpui::rgb(MUTED))
                        .child(hint),
                ),
            None => self.sc_split_body(panel.sections.iter().map(section_view).collect(), cx),
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(width))
            .h_full()
            .debug_selector(|| "sc-panel".to_owned())
            .track_focus(&self.sidebar_focus)
            .bg(gpui::rgb(PANEL_BG))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .child(self.sc_header(&panel, cx))
            .when_some(panel.context.clone(), |view, context| {
                view.child(context_line(context))
            })
            .child(body)
    }

    fn sc_header(&self, panel: &ScPanel, cx: &mut Context<Self>) -> Div {
        let actions = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .when(panel.refresh, |row| row.child(refresh_button(cx)))
            .when_some(panel.picker.clone(), |row, label| {
                row.child(self.picker(label, cx))
            });
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(px(6.0))
            .h(px(HEADER_HEIGHT))
            .px(px(ROW_PADDING))
            .border_b_1()
            .border_color(gpui::rgb(BORDER))
            .text_color(gpui::rgb(MUTED))
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SC_TITLE),
            )
            .child(actions)
    }

    /// The picker's button, with its menu under it while open.
    fn picker(&self, label: String, cx: &mut Context<Self>) -> Div {
        let button = div()
            .id("sc-picker")
            .debug_selector(|| "sc-picker".to_owned())
            .flex()
            .items_center()
            .h(px(PICKER_HEIGHT))
            .px(px(6.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .cursor_pointer()
            .hover(|style| style.bg(gpui::rgb(HOVER_BG)).text_color(gpui::rgb(TEXT)))
            .child(div().truncate().child(label))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_sc_picker(window, cx);
            }));
        let menu = self.sc_picker_open.then(|| {
            let frame = menu_frame(
                "sc-picker-menu",
                &self.menu_focus,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.close_sc_picker(window, cx);
                    cx.stop_propagation();
                }),
            )
            .children(
                self.sc_picker_rows()
                    .into_iter()
                    .map(|row| picker_row(row, cx)),
            );
            let panel = anchored()
                .offset(point(px(0.0), px(PICKER_HEIGHT + 2.0)))
                .snap_to_window()
                .child(frame);
            deferred(panel).with_priority(1)
        });
        div().relative().min_w(px(0.0)).child(button).children(menu)
    }
}

/// A row of the picker's menu, with a check after the current choice.
fn picker_row(row: ScPickerRow, cx: &mut Context<RootView>) -> Stateful<Div> {
    let ScPickerRow {
        selector,
        label,
        repo_id,
        checked,
    } = row;
    menu_item(&selector, label, false)
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .when(checked, |item| item.child("✓"))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.pick_sc_repo(repo_id.clone(), window, cx);
        }))
}

fn refresh_button(cx: &mut Context<RootView>) -> Stateful<Div> {
    div()
        .id("sc-refresh")
        .debug_selector(|| "sc-refresh".to_owned())
        .flex()
        .items_center()
        .justify_center()
        .size(px(PICKER_HEIGHT))
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(HOVER_BG)))
        .child(
            svg()
                .path(REFRESH_ICON)
                .size(px(REFRESH_ICON_SIZE))
                .text_color(gpui::rgb(MUTED)),
        )
        .tooltip(tooltip(REFRESH_TIP))
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.refresh_source_control();
            cx.notify();
        }))
}

fn context_line(context: ScContext) -> Stateful<Div> {
    div()
        .id("sc-context")
        .debug_selector(|| "sc-context".to_owned())
        .flex_none()
        .px(px(ROW_PADDING))
        .py(px(4.0))
        .border_b_1()
        .border_color(gpui::rgb(BORDER))
        .text_color(gpui::rgb(MUTED))
        .truncate()
        .child(context.text)
        .when_some(context.tooltip, |line, tip| line.tooltip(tooltip(tip)))
}

/// A section's header row and its body.
fn section_view(row: &ScSectionRow) -> AnyElement {
    let name = format!("sc-section-{}", row.id);
    let body_name = format!("sc-section-body-{}", row.id);
    let header = div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(22.0))
        .px(px(ROW_PADDING))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .child(row.title.clone()),
        )
        .when_some(row.count, |header, count| {
            header.child(
                div()
                    .flex_none()
                    .text_color(gpui::rgb(MUTED))
                    .child(count.to_string()),
            )
        });
    let body = div()
        .id(SharedString::from(body_name.clone()))
        .debug_selector(|| body_name)
        .pl(px(ROW_PADDING * 2.0))
        .pr(px(ROW_PADDING))
        .pb(px(6.0))
        .text_color(gpui::rgb(MUTED))
        .child(row.body.clone());
    div()
        .flex()
        .flex_col()
        .child(header)
        .child(body)
        .into_any_element()
}
