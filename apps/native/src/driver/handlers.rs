//! Answers each driver request on the UI thread, against the main window.
//!
//! [`handle`] takes the root view's entity rather than the view itself: a
//! key it dispatches reaches the view's own handlers, which update
//! it, so the view must not be borrowed while they run.

use gpui::{App, Bounds, Context, Entity, Keystroke, Modifiers, Pixels, Window};
use serde_json::{Value, json};

use crate::RootView;
use crate::driver::protocol::{
    ActionParams, ErrorCode, FindParams, KeyParams, PaneTextParams, ParamError, Reply, Request,
    TypeTextParams, WaitParams,
};
use crate::driver::registry::{self, Entry};
use crate::driver::wait::{PREDICATE_SHAPES, Predicate, StateTest, parse_predicate, value_at};
use crate::sidebar::Activity;
use crate::spawn_view::SpawnEntry;

/// The client's version, as `ping` answers it.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a refused `type_text` suggests instead.
const TYPE_TEXT_HINT: &str = "type_text takes printable text; press Enter, Tab or Escape with key";

/// The actions `action` knows, as an unknown one's hint lists them.
const ACTIONS_HINT: &str = "actions: open_settings, new_shell, open_spawn, toggle_sidebar, \
                            select_tab, focus_pane, show_activity, set_hover";

/// Answers `request` against `root` and `window`, the window it draws in.
pub fn handle(
    request: &Request,
    root: &Entity<RootView>,
    window: &mut Window,
    cx: &mut App,
) -> Reply {
    let outcome = match request.method.as_str() {
        "ping" => Ok(json!({ "version": CLIENT_VERSION })),
        "state" => Ok(state(root.read(cx), window)),
        "find" => find(request, window, cx),
        "key" => key(request, window, cx),
        "type_text" => type_text(request, window, cx),
        "pane_text" => pane_text(request, root.read(cx), cx),
        "action" => action(request, root, window, cx),
        // It awaits a capture thread, so the pipe's dispatcher answers it.
        "screenshot" => Err(request.fail(
            ErrorCode::Refused,
            "screenshot is answered only over the driver pipe".to_owned(),
        )),
        "wait_for" => wait_for(request),
        _ => return Reply::unknown_method(request.id, &request.method),
    };
    outcome.map_or_else(|failed| failed, |result| Reply::ok(request.id, result))
}

/// The window's logical size and scale, the tabs, the shown tab's panes,
/// every session, and what is drawn over the panes.
fn state(root: &RootView, window: &Window) -> Value {
    let size = window.viewport_size();
    json!({
        "window": {
            "width": f32::from(size.width),
            "height": f32::from(size.height),
            "scale": window.scale_factor(),
        },
        "active_tab": root.active_tab_id(),
        "tabs": root.driver_tabs(),
        "panes": root
            .active_pane_ids()
            .into_iter()
            .map(|id| json!({ "session_id": root.pane_session(&id), "id": id }))
            .collect::<Vec<_>>(),
        "sessions": root.driver_sessions(),
        "overlay": root.driver_overlay(),
    })
}

/// Every name the window's latest frame painted that starts with the
/// params' prefix, sorted, with its bounds and its visible part.
///
/// It reads the latest painted frame and does not redraw, so after a key
/// it answers from the frame painted before that key unless the window has
/// drawn since.
fn find(request: &Request, window: &Window, cx: &App) -> Result<Value, Reply> {
    let params: FindParams = request.params()?;
    let prefix = params.prefix.unwrap_or_default();
    let id = window.window_handle().window_id();
    let found: Vec<Value> = registry::names(cx, id)
        .into_iter()
        .filter(|name| name.starts_with(&prefix))
        .filter_map(|name| {
            let entry = registry::lookup(cx, id, &name)?;
            Some(found_entry(&name, entry))
        })
        .collect();
    Ok(Value::Array(found))
}

/// One element as `find` lists it.
fn found_entry(name: &str, entry: Entry) -> Value {
    json!({
        "name": name,
        "bounds": rect(entry.bounds),
        "visible": rect(entry.visible),
    })
}

fn rect(bounds: Bounds<Pixels>) -> Value {
    json!({
        "x": f32::from(bounds.origin.x),
        "y": f32::from(bounds.origin.y),
        "w": f32::from(bounds.size.width),
        "h": f32::from(bounds.size.height),
    })
}

/// The `bad_params` reply to `request` that `refused` describes.
fn bad_params(request: &Request, refused: ParamError) -> Reply {
    Reply::error(
        request.id,
        ErrorCode::BadParams,
        refused.message,
        refused.hint,
    )
}

/// Presses the one keystroke the params name.
fn key(request: &Request, window: &mut Window, cx: &mut App) -> Result<Value, Reply> {
    let params: KeyParams = request.params()?;
    let stroke = params
        .keystroke()
        .map_err(|refused| bad_params(request, refused))?;
    window.dispatch_keystroke(stroke, cx);
    Ok(json!({ "pressed": params.keys }))
}

/// Types the params' text into what holds focus, one keystroke per
/// character, after checking that none of it is a control character.
fn type_text(request: &Request, window: &mut Window, cx: &mut App) -> Result<Value, Reply> {
    let params: TypeTextParams = request.params()?;
    if let Some(control) = params
        .text
        .chars()
        .find(|ch| matches!(ch, '\u{0}'..='\u{1f}' | '\u{7f}'))
    {
        return Err(bad_params(
            request,
            ParamError {
                message: format!(
                    "text holds the control character U+{:04X}",
                    u32::from(control)
                ),
                hint: Some(TYPE_TEXT_HINT.to_owned()),
            },
        ));
    }
    for ch in params.text.chars() {
        let key = if ch == ' ' {
            "space".to_owned()
        } else {
            ch.to_string()
        };
        let stroke = Keystroke {
            modifiers: Modifiers::none(),
            key,
            key_char: Some(ch.to_string()),
        };
        window.dispatch_keystroke(stroke, cx);
    }
    Ok(json!({ "typed": params.text.chars().count() }))
}

/// The visible rows of the params' pane, else of the active tab's focused
/// pane.
fn pane_text(request: &Request, root: &RootView, cx: &App) -> Result<Value, Reply> {
    let params: PaneTextParams = request.params()?;
    let Some(pane) = params.pane.or_else(|| root.driver_focused_pane()) else {
        return Err(request.fail(ErrorCode::NotFound, "the active tab has no pane".to_owned()));
    };
    let Some(lines) = root.pane_grid_text(&pane, cx) else {
        return Err(Reply::error(
            request.id,
            ErrorCode::NotFound,
            format!("no pane {pane}"),
            Some(format!("panes: {}", root.active_pane_ids().join(", "))),
        ));
    };
    Ok(json!({ "pane": pane, "text": lines.join("\n"), "lines": lines }))
}

/// `wait_for`'s params and the predicate they write.
///
/// # Errors
///
/// The `bad_params` reply when the params do not decode or `until` is not
/// a predicate, naming what is wrong.
pub fn wait_request(request: &Request) -> Result<(WaitParams, Predicate), Reply> {
    let params: WaitParams = request.params()?;
    let predicate = parse_predicate(&params.until).map_err(|problem| {
        bad_params(
            request,
            ParamError {
                message: format!("bad until: {problem}"),
                hint: Some(PREDICATE_SHAPES.to_owned()),
            },
        )
    })?;
    Ok((params, predicate))
}

/// `wait_for` in-process: it sleeps between reads, so the pipe's
/// dispatcher answers it; a malformed predicate is refused here all the
/// same.
fn wait_for(request: &Request) -> Result<Value, Reply> {
    wait_request(request)?;
    Err(request.fail(
        ErrorCode::Refused,
        "wait_for is answered only over the driver pipe".to_owned(),
    ))
}

/// Reads `predicate` once against `root` and `window`: whether it holds,
/// and the value it read (`null` when there was none): the value at a
/// state path, a pane's text, or an element as `find` lists it.
pub fn wait_read(
    predicate: &Predicate,
    root: &Entity<RootView>,
    window: &Window,
    cx: &App,
) -> (bool, Value) {
    match predicate {
        Predicate::State { path, test } => {
            let state = state(root.read(cx), window);
            let found = value_at(&state, path);
            let met = match test {
                StateTest::Equals(expected) => found == Some(expected),
                StateTest::Exists(exists) => found.is_some_and(|value| !value.is_null()) == *exists,
            };
            (met, found.cloned().unwrap_or(Value::Null))
        }
        Predicate::PaneText { pane, contains } => match root.read(cx).pane_grid_text(pane, cx) {
            Some(lines) => {
                let text = lines.join("\n");
                (text.contains(contains.as_str()), Value::String(text))
            }
            None => (false, Value::Null),
        },
        Predicate::Element { name, visible } => {
            let entry = registry::lookup(cx, window.window_handle().window_id(), name);
            let shows = entry.is_some_and(|entry| !entry.visible.is_empty());
            let last = entry.map_or(Value::Null, |entry| found_entry(name, entry));
            (shows == *visible, last)
        }
    }
}

/// One named action and its arguments.
#[derive(Debug)]
enum Action {
    OpenSettings,
    NewShell,
    OpenSpawn,
    ToggleSidebar,
    SelectTab(String),
    FocusPane(String),
    ShowActivity(Activity),
    SetHover(Option<bool>),
}

impl Action {
    /// The action `params` name, with its arguments read.
    fn parse(params: &ActionParams) -> Result<Self, ParamError> {
        let args = params.args.as_ref();
        match params.name.as_str() {
            "open_settings" => Ok(Self::OpenSettings),
            "new_shell" => Ok(Self::NewShell),
            "open_spawn" => Ok(Self::OpenSpawn),
            "toggle_sidebar" => Ok(Self::ToggleSidebar),
            "select_tab" => string_arg(args, "select_tab", "id").map(Self::SelectTab),
            "focus_pane" => string_arg(args, "focus_pane", "id").map(Self::FocusPane),
            "show_activity" => string_arg(args, "show_activity", "name")
                .and_then(|name| activity(&name))
                .map(Self::ShowActivity),
            "set_hover" => hover_arg(args).map(Self::SetHover),
            other => Err(ParamError {
                message: format!("unknown action: {other}"),
                hint: Some(ACTIONS_HINT.to_owned()),
            }),
        }
    }
}

/// The string argument `key` of `action`.
fn string_arg(args: Option<&Value>, action: &str, key: &str) -> Result<String, ParamError> {
    args.and_then(|args| args.get(key))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ParamError {
            message: format!(r#"{action} takes args {{"{key}": <string>}}"#),
            hint: None,
        })
}

/// The rail panel `name` names.
fn activity(name: &str) -> Result<Activity, ParamError> {
    match name {
        "sessions" => Ok(Activity::Sessions),
        "needs_you" => Ok(Activity::NeedsYou),
        "source_control" => Ok(Activity::SourceControl),
        other => Err(ParamError {
            message: format!("unknown activity: {other}"),
            hint: Some("activities: sessions, needs_you, source_control".to_owned()),
        }),
    }
}

/// `set_hover`'s value: `true`, `false`, or `null` to ask the platform.
fn hover_arg(args: Option<&Value>) -> Result<Option<bool>, ParamError> {
    match args.and_then(|args| args.get("value")) {
        Some(Value::Bool(hovered)) => Ok(Some(*hovered)),
        Some(Value::Null) => Ok(None),
        _ => Err(ParamError {
            message: r#"set_hover takes args {"value": true, false or null}"#.to_owned(),
            hint: None,
        }),
    }
}

/// Why the view would not run an action whose params were well formed.
struct Refusal {
    code: ErrorCode,
    message: String,
    hint: Option<String>,
}

/// Runs the named action the params ask for.
fn action(
    request: &Request,
    root: &Entity<RootView>,
    window: &mut Window,
    cx: &mut App,
) -> Result<Value, Reply> {
    let params: ActionParams = request.params()?;
    let action = Action::parse(&params).map_err(|refused| bad_params(request, refused))?;
    root.update(cx, |root, cx| root.driver_action(action, window, cx))
        .map_err(|refused| Reply::error(request.id, refused.code, refused.message, refused.hint))?;
    Ok(json!({ "action": params.name }))
}

impl RootView {
    /// Every tab's id and title, in strip order.
    fn driver_tabs(&self) -> Vec<Value> {
        self.tabs
            .tabs()
            .iter()
            .map(|tab| json!({ "id": tab.id, "title": tab.name }))
            .collect()
    }

    /// Every host's sessions, with each one's status and label.
    fn driver_sessions(&self) -> Vec<Value> {
        self.sidebar
            .all_sessions()
            .map(|session| {
                json!({
                    "id": session.id,
                    "status": session.status,
                    "label": session.label,
                })
            })
            .collect()
    }

    /// What is drawn over the panes: the Settings modal, a menu, another
    /// dialog or modal, or nothing.
    fn driver_overlay(&self) -> Option<&'static str> {
        let menu_open = self.menu.is_some()
            || self.tab_menu.is_some()
            || self.container_menu_open()
            || self.shell_menu_open()
            || self.sidebar_more_open()
            || self.sc_file_menu_open()
            || self.sc_picker_open()
            || self.flyout_open;
        if self.settings_open() {
            Some("settings")
        } else if menu_open {
            Some("menu")
        } else if self.overlay_open() {
            Some("dialog")
        } else {
            None
        }
    }

    /// The active tab's focused pane, else its first.
    fn driver_focused_pane(&self) -> Option<String> {
        self.tabs
            .active_id()
            .and_then(|tab_id| self.tabs.focused_pane(tab_id))
            .or_else(|| self.active_pane_ids().into_iter().next())
    }

    /// Runs `action` as the control it stands for would.
    fn driver_action(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        match action {
            Action::OpenSettings => self.open_settings(window, cx),
            Action::NewShell => self.open_shell_dialog(window, cx),
            Action::OpenSpawn => self.driver_open_spawn(window, cx)?,
            Action::ToggleSidebar => self.toggle_sidebar(window, cx),
            Action::SelectTab(tab_id) => self.driver_select_tab(&tab_id, window, cx)?,
            Action::FocusPane(pane_id) => self.driver_focus_pane(&pane_id, window, cx)?,
            Action::ShowActivity(item) => self.driver_show_activity(item, window, cx),
            Action::SetHover(hovered) => self.set_window_hover_override(hovered, cx),
        }
        cx.notify();
        Ok(())
    }

    /// Opens the spawn dialog as "+ Session" does.
    fn driver_open_spawn(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        if self.open_spawn_dialog(SpawnEntry::Toolbar, window, cx) {
            Ok(())
        } else {
            Err(Refusal {
                code: ErrorCode::Refused,
                message: "the spawn dialog did not open".to_owned(),
                hint: Some("close the dialog or menu that is open first".to_owned()),
            })
        }
    }

    /// Shows tab `tab_id` as a click on its pill does.
    fn driver_select_tab(
        &mut self,
        tab_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        if self.tabs.tab(tab_id).is_none() {
            return Err(Refusal {
                code: ErrorCode::NotFound,
                message: format!("no tab {tab_id}"),
                hint: Some(format!("tabs: {}", self.tab_ids().join(", "))),
            });
        }
        self.tabs.activate(tab_id);
        self.after_tabs_change(window, cx);
        Ok(())
    }

    /// Gives pane `pane_id` of the active tab the keyboard.
    fn driver_focus_pane(
        &mut self,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), Refusal> {
        let panes = self.active_pane_ids();
        let tab_id = self.tabs.active_id().map(str::to_owned);
        let Some(tab_id) = tab_id.filter(|_| panes.iter().any(|pane| pane == pane_id)) else {
            return Err(Refusal {
                code: ErrorCode::NotFound,
                message: format!("no pane {pane_id} in the active tab"),
                hint: Some(format!("panes: {}", panes.join(", "))),
            });
        };
        self.tabs.focus_pane(&tab_id, pane_id);
        self.after_tabs_change(window, cx);
        Ok(())
    }

    /// Shows `item`'s rail panel as the rail does: the menus the switch
    /// would strand close, and an unfold gives the panes their new width.
    fn driver_show_activity(
        &mut self,
        item: Activity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_shell_menu(window, cx);
        self.close_sc_picker(window, cx);
        self.close_sc_file_menu(window, cx);
        self.close_more_menu(window, cx);
        let was_collapsed = self.sidebar.is_collapsed();
        self.sidebar.show_activity(item);
        if was_collapsed {
            self.mark_active_panes_resized(cx);
        }
        if item != Activity::Sessions {
            self.detached_stop_all.disarm();
            self.row_focus = None;
            self.hovered_container = None;
        }
        self.drag = None;
        self.save_ui();
        self.seed_history();
        self.arm_needs_you_repaint(cx);
    }
}
