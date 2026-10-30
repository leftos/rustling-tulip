//! The source-control panel the activity rail shows: a header with the
//! refresh button and the repo picker, the line naming what the panel
//! follows, and one section per repo tree with its change count. It also
//! asks the daemon for the statuses the panel and the rail's badge read.

use gpui::{
    AnyElement, ClickEvent, Context, Div, Entity, FontWeight, MouseButton, MouseDownEvent,
    SharedString, Stateful, StyledText, Window, anchored, deferred, div, point, prelude::*, px,
    svg,
};
use protocol::{ClientMessage, DaemonMessage, RepoEntry, SessionMember};

use crate::assets::REFRESH_ICON;
use crate::changes_view::{
    HEAD_HEIGHT, ScBucketRow, ScChanges, ScCommit, caret, changes_body, count_pill, head_title,
};
use crate::ellipsis::truncating;
use crate::fonts::ui_px;
use crate::session_menu::MENU_MAX_SHARE;
use crate::session_menu::menu_frame;
use crate::session_menu::menu_item;
use crate::sidebar::{Activity, display_label};
use crate::source_control::{
    Part, ScKey, ScModel, Section, pin_entry, pin_parts, sections, stored_key,
};
use crate::stash_view::{ScStashes, stashes_part};
use crate::stashes::StashModel;
use crate::target::HostKey;
use crate::text_input::TextInput;
use crate::ui_theme::{UiTheme, theme};
use crate::{RootView, UI_TEXT_SIZE, tooltip};

/// The panel's title.
pub const SC_TITLE: &str = "Source control";
/// The space above and below a section head's repo and branch lines.
const HEAD_PAD_Y: f32 = 3.0;
/// The picker's row that follows the active pane.
pub const AUTO_LABEL: &str = "Auto · follow active pane";
/// The panel's text with no repo registered.
pub const NO_REPOS_HINT: &str =
    "Register a repo from the Sessions sidebar to inspect changes here.";
/// The context line's tooltip when nothing focused names a repo.
pub const NO_ACTIVE_PANE_TIP: &str = "No pane is focused right now — falling back to the first registered repo. Pick a session in the sidebar to follow it, or pin a repo from the picker above.";
const REFRESH_TIP: &str = "Refresh status and history";
const HEADER_HEIGHT: f32 = 44.0;
/// The panel header's title size.
const HEADER_TEXT_SIZE: f32 = 13.0;
/// The panel header's padding left of its title.
const HEADER_PAD_LEFT: f32 = 14.0;
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
    pub key: ScKey,
    /// The section's key id, which its selectors end in.
    pub id: String,
    /// The repo name, the head's first line.
    pub repo: String,
    /// The branch, the head's second line, when one is known.
    pub branch: Option<String>,
    /// The distinct changed paths, when loaded and not zero.
    pub count: Option<usize>,
    /// Whether its Changes part is folded, which hides the body below.
    pub collapsed: bool,
    /// The commit box, first in the unfolded body, when it shows.
    pub commit: Option<ScCommit>,
    /// `<operation>: <error>` of the last git write that failed.
    pub banner: Option<String>,
    /// `loading…`, `couldn't load status: <reason>` or `working tree
    /// clean`, shown instead of buckets.
    pub body: Option<String>,
    /// The non-empty buckets, Staged first.
    pub buckets: Vec<ScBucketRow>,
    /// The Stashes part, below the buckets: shown while the Changes part
    /// is unfolded, and while the tree is loaded with nothing changed.
    pub stashes: Option<ScStashes>,
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

/// One host's source-control models: statuses, writes, stashes and
/// History.
#[derive(Default)]
pub(crate) struct ScModels {
    sc: ScModel,
    writes: ScWrites,
    stash: StashModel,
    history: HistoryModel,
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
    /// The session whose trees the source-control panel reads: the active
    /// terminal tab's focused pane, or the session focused last while a
    /// non-terminal tab (a diff tab) is active.
    fn sc_session(&self) -> Option<String> {
        if self
            .tabs
            .active_tab()
            .is_some_and(|tab| tab.grid().is_some())
        {
            return self.focused_session();
        }
        self.last_focused_session.clone()
    }

    /// The panel's session and its host, when it names any repos.
    fn sc_session_members(&self) -> Option<(HostKey, &[SessionMember])> {
        let id = self.sc_session()?;
        self.sidebar
            .find_session(&id)
            .map(|(host, session)| (host, session.members.as_slice()))
            .filter(|(_, members)| !members.is_empty())
    }

    /// The members of the panel's session, when it has any and its host is
    /// the one the panel shows.
    pub(crate) fn sc_focused_members(&self) -> Option<&[SessionMember]> {
        self.sc_session_members()
            .filter(|(host, _)| *host == self.sc_shown)
            .map(|(_, members)| members)
    }

    /// The pinned repo's id, when its host is the one the panel shows.
    pub(crate) fn sc_pin_on_shown(&self) -> Option<&str> {
        let entry = self.sidebar.source_control().pinned_repo.as_deref()?;
        let (host, id) = pin_parts(entry);
        (host == self.sc_shown).then_some(id)
    }

    /// The pinned repo, when it is still registered on the shown host.
    fn sc_pinned(&self) -> Option<&str> {
        self.sc_pin_on_shown()
            .filter(|id| self.sc_repos().iter().any(|repo| repo.id == *id))
    }

    /// The shown host's registered repos.
    pub(crate) fn sc_repos(&self) -> &[RepoEntry] {
        self.sidebar.repos_of(&self.sc_shown)
    }

    pub(crate) fn sc_sections(&self) -> Vec<Section> {
        sections(
            self.sc_repos(),
            self.sc_focused_members(),
            self.sc_pin_on_shown(),
        )
    }

    /// `key` as the persisted collapse and height entries name it on the
    /// shown host.
    pub(crate) fn sc_stored_key(&self, key: &ScKey) -> ScKey {
        stored_key(&self.sc_shown, key)
    }

    /// The host the panel should show: the focused session's when it names
    /// repos, else the pinned repo's while the window holds its host, else
    /// This PC.
    fn sc_wanted_host(&self) -> HostKey {
        if let Some((host, _)) = self.sc_session_members() {
            return host;
        }
        self.sidebar
            .source_control()
            .pinned_repo
            .as_deref()
            .map(|entry| pin_parts(entry).0)
            .filter(|host| self.conn_of(host).is_some())
            .unwrap_or(HostKey::Local)
    }

    /// Makes `host`'s source-control models the live ones, parking the
    /// shown host's; sends nothing.
    pub(crate) fn show_sc_host(&mut self, host: &HostKey) {
        if *host == self.sc_shown {
            return;
        }
        let incoming = self.sc_parked.remove(host).unwrap_or_default();
        let outgoing = ScModels {
            sc: std::mem::replace(&mut self.sc, incoming.sc),
            writes: std::mem::replace(&mut self.changes.writes, incoming.writes),
            stash: std::mem::replace(&mut self.stash.model, incoming.stash),
            history: std::mem::replace(&mut self.history, incoming.history),
        };
        let shown = std::mem::replace(&mut self.sc_shown, host.clone());
        self.sc_parked.insert(shown, outgoing);
    }

    /// Drops remote `host`'s source-control models, showing This PC's when
    /// they were the live ones.
    pub(crate) fn forget_sc_host(&mut self, host: &HostKey) {
        if *host == HostKey::Local {
            return;
        }
        if self.sc_shown == *host {
            self.show_sc_host(&HostKey::Local);
        }
        self.sc_parked.remove(host);
    }

    /// Folds a message into `host`'s source-control models, live or
    /// parked; returns whether nothing else is owed it. A parked host's
    /// message moves only its models: the commit inputs, the drop confirm
    /// and the keyboard belong to the shown host.
    pub(crate) fn on_sc_panel_message(
        &mut self,
        host: &HostKey,
        msg: &DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let shown = self.sc_shown.clone();
        let live = *host == shown;
        self.show_sc_host(host);
        let done = self.on_sc_message(live, msg, window, cx)
            || self.on_stash_message(live, msg, window, cx)
            || self.apply_history(host, msg, cx);
        self.show_sc_host(&shown);
        done
    }

    /// Every registered repo's main tree on the shown host.
    fn sc_main_keys(&self) -> Vec<ScKey> {
        self.sc_repos()
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
            && self.sidebar.all_repos().nth(1).is_some()
    }

    /// Closes the picker once its button no longer shows, so its menu and
    /// the blocking layer under it never outlive the button; returns
    /// whether it closed, so the caller hands the keyboard back.
    pub(crate) fn drop_stale_sc_picker(&mut self) -> bool {
        let stale = self.sc_picker_open && !self.sc_picker_offered();
        if stale {
            self.sc_picker_open = false;
        }
        stale
    }

    /// The picker's rows: Auto, then every host's repos, This PC's first,
    /// the current choice checked. A row's repo id is the pin it saves: a
    /// remote host's carries `<host key>::` before the id.
    #[must_use]
    pub fn sc_picker_rows(&self) -> Vec<ScPickerRow> {
        let pinned = self.sc_pinned().map(|id| pin_entry(&self.sc_shown, id));
        let auto = ScPickerRow {
            selector: "sc-picker-auto".to_owned(),
            label: AUTO_LABEL.to_owned(),
            repo_id: None,
            checked: pinned.is_none(),
        };
        let repos = self.sidebar.all_repos().map(|(host, repo)| {
            let entry = pin_entry(host, &repo.id);
            ScPickerRow {
                selector: format!("sc-picker-{entry}"),
                label: repo.name.clone(),
                checked: pinned.as_deref() == Some(entry.as_str()),
                repo_id: Some(entry),
            }
        });
        std::iter::once(auto).chain(repos).collect()
    }

    /// What the panel shows now.
    #[must_use]
    pub fn source_control_panel(&self) -> ScPanel {
        let repos = self.sc_repos();
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
            let label = self
                .sidebar
                .session(&session)
                .map(|s| display_label(s, self.sidebar.agents(&self.sc_shown)))?;
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
        let loaded = self.sc.is_loaded(&section.key);
        let count = self.sc.badge_total(std::slice::from_ref(&section.key));
        let collapsed = self.sidebar.source_control().is_collapsed(
            &self.sc_stored_key(&section.key),
            Part::Changes,
            loaded.then_some(count),
        );
        let changes = if collapsed {
            ScChanges::default()
        } else {
            self.sc_changes(&section.key)
        };
        let clean = loaded && count == 0;
        let stashes = (!collapsed || clean).then(|| self.sc_stashes(&section.key));
        ScSectionRow {
            key: section.key.clone(),
            id: section.key.id(),
            repo: section.repo_name.clone(),
            branch: section.branch.clone(),
            count: (loaded && count > 0).then_some(count),
            collapsed,
            commit: changes.commit,
            banner: changes.banner,
            body: changes.body,
            buckets: changes.buckets,
            stashes,
        }
    }

    /// Asks for the status of every tree the panel or the badge reads that
    /// has none and no request out: the current sections, and every
    /// registered repo's main tree; and the stash list of every section's
    /// repo that has none. The file menu, the discard confirm, the drop
    /// confirm and the commit and push inputs of a tree that left the
    /// sections go; returns whether a menu, a confirm or the input holding
    /// the keyboard did, so the caller hands the keyboard back.
    pub(crate) fn seed_source_control(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let host = self.sc_wanted_host();
        self.show_sc_host(&host);
        let mut wanted: Vec<ScKey> = self
            .sc_sections()
            .into_iter()
            .map(|section| section.key)
            .collect();
        let dropped_changes = self.drop_stale_sc_changes(&wanted);
        let dropped_stash = self.drop_stale_stash_ui(&wanted, window, cx);
        self.make_stash_inputs(&wanted, window, cx);
        let dropped_commit = self.drop_stale_commit_ui(&wanted, window, cx);
        self.make_commit_inputs(&wanted, window, cx);
        let dropped = dropped_changes || dropped_stash || dropped_commit;
        self.seed_stashes(&wanted);
        wanted.extend(self.sc_main_keys());
        for key in self.sc.wanted_missing(&wanted) {
            self.request_sc_status(key);
        }
        self.seed_history();
        dropped
    }

    /// Asks again for every current section's status, a failed one
    /// included, its repo's stash list and its history, and forgets the
    /// section's pending write, a commit included, and its repo's pending
    /// stash write; the stored ones stay on screen until the answers
    /// replace them.
    fn refresh_source_control(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<ScKey> = self
            .sc_sections()
            .into_iter()
            .map(|section| section.key)
            .collect();
        for key in &keys {
            self.changes.writes.clear_pending(key);
            self.request_sc_status(key.clone());
        }
        self.sync_commit_inputs(cx);
        self.refresh_stashes(&keys);
        self.refresh_history();
    }

    /// Sends a status request for `key` under a fresh request id, so a
    /// failure comes back to its section.
    fn request_sc_status(&mut self, key: ScKey) {
        let request_id = self.sc.request(key.clone());
        self.send_to(
            &self.host_of_repo(&key.repo_id),
            ClientMessage::RepoStatus {
                repo_id: key.repo_id,
                worktree_path: key.worktree,
                request_id: Some(request_id),
            },
        );
    }

    /// Opens the picker's menu, closing any other menu, unless a dialog or
    /// a modal notice is up or the connection is down.
    fn open_sc_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let blocked = self.exit.is_some()
            || self.spawn_dialog.is_some()
            || self.shell_dialog.is_some()
            || self.fetch_dialog.is_some()
            || self.appearance_editor.is_some()
            || self.delete_dialog.is_some()
            || self.run_confirm.is_some()
            || self.changes.discard.is_some()
            || self.stash.drop.is_some()
            || self.notices.has_modal()
            || self.conn().overlay().is_some();
        if blocked {
            return;
        }
        self.close_session_menu(window, cx);
        self.close_container_menu(window, cx);
        self.close_shell_menu(window, cx);
        self.close_tab_menu(window, cx);
        self.close_sc_file_menu(window, cx);
        self.close_more_menu(window, cx);
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
            if self.seed_source_control(window, cx) {
                self.after_notice_closed(window, cx);
            }
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
    pub(crate) fn source_control_view(
        &self,
        width: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
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
                        .text_color(gpui::rgb(t.muted))
                        .child(hint),
                ),
            None => self.sc_split_body(
                panel
                    .sections
                    .iter()
                    .map(|row| {
                        let inputs = SectionInputs {
                            commit: self.commit_input(&row.key),
                            stash: self.stash_input(&row.key),
                        };
                        section_view(row, inputs, cx)
                    })
                    .collect(),
                cx,
            ),
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(width))
            .h_full()
            .debug_selector(|| "sc-panel".to_owned())
            .track_focus(&self.sidebar_focus)
            .bg(gpui::rgb(t.panel_bg))
            .text_size(ui_px(cx, UI_TEXT_SIZE))
            .text_color(gpui::rgb(t.text))
            .child(self.sc_header(&panel, window, cx))
            .when_some(panel.context.clone(), |view, context| {
                view.child(context_line(context, &t))
            })
            .child(body)
    }

    fn sc_header(&self, panel: &ScPanel, window: &Window, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let actions = div()
            .flex()
            .min_w(px(0.0))
            .items_center()
            .gap(px(4.0))
            .when(panel.refresh, |row| row.child(refresh_button(cx)))
            .when_some(panel.picker.clone(), |row, label| {
                row.child(self.picker(label, window, cx))
            });
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(px(6.0))
            .h(ui_px(cx, HEADER_HEIGHT))
            .pl(px(HEADER_PAD_LEFT))
            .pr(px(ROW_PADDING))
            .border_b_1()
            .border_color(gpui::rgb(t.border))
            .text_color(gpui::rgb(t.muted))
            .child(
                div()
                    .flex_none()
                    .text_size(ui_px(cx, HEADER_TEXT_SIZE))
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::BOLD))
                    .text_color(gpui::rgb(t.text))
                    .child(SC_TITLE),
            )
            .child(actions)
    }

    /// The picker's button, with its menu under it while open.
    fn picker(&self, label: String, window: &Window, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let button = div()
            .id("sc-picker")
            .debug_selector(|| "sc-picker".to_owned())
            .flex()
            .min_w(px(0.0))
            .items_center()
            .h(ui_px(cx, PICKER_HEIGHT))
            .px(px(6.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(gpui::rgb(t.border))
            .cursor_pointer()
            .hover(|style| {
                style
                    .bg(gpui::rgb(t.hover_bg))
                    .text_color(gpui::rgb(t.text))
            })
            .child(truncating("sc-picker-label", StyledText::new(label)))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_sc_picker(window, cx);
            }));
        let menu = self.sc_picker_open.then(|| {
            let frame = menu_frame(
                crate::fonts::ui_text_size(cx),
                "sc-picker-menu",
                &self.menu_focus,
                window.viewport_size().height * MENU_MAX_SHARE,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.close_sc_picker(window, cx);
                    cx.stop_propagation();
                }),
                &theme(cx),
            )
            .children(
                self.sc_picker_rows()
                    .into_iter()
                    .map(|row| picker_row(row, cx)),
            );
            let panel = anchored()
                .offset(point(px(0.0), ui_px(cx, PICKER_HEIGHT) + px(2.0)))
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
    menu_item(&selector, label, false, &theme(cx))
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
    let t = theme(cx);
    div()
        .id("sc-refresh")
        .debug_selector(|| "sc-refresh".to_owned())
        .flex()
        .items_center()
        .justify_center()
        .size(ui_px(cx, PICKER_HEIGHT))
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(t.hover_bg)))
        .child(
            svg()
                .path(REFRESH_ICON)
                .size(px(REFRESH_ICON_SIZE))
                .text_color(gpui::rgb(t.muted)),
        )
        .tooltip(tooltip(REFRESH_TIP))
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            this.refresh_source_control(cx);
            cx.notify();
        }))
}

fn context_line(context: ScContext, t: &UiTheme) -> Stateful<Div> {
    div()
        .id("sc-context")
        .debug_selector(|| "sc-context".to_owned())
        .flex_none()
        .px(px(ROW_PADDING))
        .py(px(4.0))
        .border_b_1()
        .border_color(gpui::rgb(t.border))
        .text_color(gpui::rgb(t.muted))
        .child(truncating("sc-context", StyledText::new(context.text)))
        .when_some(context.tooltip, |line, tip| line.tooltip(tooltip(tip)))
}

/// The inputs a section's body holds, once made.
struct SectionInputs {
    commit: Option<Entity<TextInput>>,
    stash: Option<Entity<TextInput>>,
}

/// A section's header row, which folds its Changes part, and its body:
/// the changes unless folded, then the Stashes part when it shows.
fn section_view(
    row: &ScSectionRow,
    inputs: SectionInputs,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let t = theme(cx);
    let name = format!("sc-section-{}", row.id);
    let body_name = format!("sc-section-body-{}", row.id);
    let offset = crate::fonts::ui_text_size(cx);
    let title = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .child(head_title(
            offset,
            crate::fonts::ui_weight_shift(cx),
            &format!("{name}-repo"),
            &row.repo,
            &t,
        ))
        .children(row.branch.as_deref().map(|branch| {
            head_title(
                offset,
                crate::fonts::ui_weight_shift(cx),
                &format!("{name}-branch"),
                branch,
                &t,
            )
        }));
    let key = row.key.clone();
    let header = div()
        .id(SharedString::from(name.clone()))
        .debug_selector(|| name)
        .flex()
        .items_center()
        .gap(px(6.0))
        .min_h(ui_px(cx, HEAD_HEIGHT))
        .py(px(HEAD_PAD_Y))
        .px(px(ROW_PADDING))
        .cursor_pointer()
        .hover(|style| style.bg(gpui::rgb(t.hover_bg)))
        .on_mouse_down(MouseButton::Left, {
            let key = key.clone();
            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                // The panel would take the keyboard on this press; the
                // section's input keeps it, so the fold can hand it on.
                if this.commit_input_focused(&key, window, cx)
                    || this.stash_input_focused(&key, window, cx)
                {
                    window.prevent_default();
                }
            })
        })
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.toggle_sc_changes(&key, window, cx);
        }))
        .child(caret(row.collapsed, &t))
        .child(title)
        .when_some(row.count, |header, count| {
            header.child(count_pill(offset, count, &t))
        });
    let body = (!row.collapsed || row.stashes.is_some()).then(|| {
        div()
            .id(SharedString::from(body_name.clone()))
            .debug_selector(|| body_name)
            .flex()
            .flex_col()
            .pb(px(6.0))
            .children(changes_body(row, inputs.commit, cx))
            .children(stashes_part(row, inputs.stash, cx))
    });
    div()
        .flex()
        .flex_col()
        .child(header)
        .children(body)
        .into_any_element()
}
