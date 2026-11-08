//! Answers each driver request on the UI thread, against the main window.
//!
//! [`handle`] takes the root view's entity rather than the view itself: a
//! key it dispatches reaches the view's own handlers, which update
//! it, so the view must not be borrowed while they run.

use std::path::{Path, PathBuf};

use gpui::{App, Bounds, ClipboardItem, Context, Entity, Keystroke, Modifiers, Pixels, Window};
use serde_json::{Value, json};

use crate::RootView;
use crate::connection_picker::THIS_PC;
use crate::driver::pointer;
use crate::driver::protocol::{
    ActionParams, ClickParams, DragParams, ErrorCode, FindParams, KeyParams, MoveParams,
    PaneTextParams, ParamError, PasteParams, Reply, Request, Target, TypeTextParams, WaitParams,
    WheelParams,
};
use crate::driver::registry::{self, Entry};
use crate::driver::wait::{PREDICATE_SHAPES, Predicate, StateTest, parse_predicate, value_at};
use crate::sidebar::Activity;
use crate::spawn_view::SpawnEntry;

/// The client's version, as `ping` answers it.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a refused `type_text` suggests instead.
const TYPE_TEXT_HINT: &str = "type_text takes printable text; press Enter, Tab or Escape with key";

/// The keystroke `paste` dispatches: the one a user presses to paste, Ctrl+V
/// (Cmd+V on macOS).
const PASTE_KEYSTROKE: &str = "secondary-v";

/// The actions `action` knows, as an unknown one's hint lists them.
const ACTIONS_HINT: &str = "actions: open_settings, new_shell, open_spawn, toggle_sidebar, \
                            select_tab, focus_pane, show_activity, set_hover, add_repo";

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
        "paste" => paste(request, window, cx),
        "pane_text" => pane_text(request, root.read(cx), cx),
        "action" => action(request, root, window, cx),
        // It awaits a capture thread, so the pipe's dispatcher answers it.
        "screenshot" => Err(request.fail(
            ErrorCode::Refused,
            "screenshot is answered only over the driver pipe".to_owned(),
        )),
        "wait_for" => wait_for(request),
        "click" => click(request, window, cx),
        "move" => move_to(request, window, cx),
        "drag" => drag(request, window, cx),
        "wheel" => wheel(request, window, cx),
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

/// Pastes the params' text into what holds focus, the way a user's paste
/// shortcut does: the clipboard carries the text while the keystroke is
/// dispatched, and is put back as it was afterwards.
fn paste(request: &Request, window: &mut Window, cx: &mut App) -> Result<Value, Reply> {
    let params: PasteParams = request.params()?;
    if params.text.is_empty() {
        return Err(request.fail(ErrorCode::BadParams, "text is empty".to_owned()));
    }
    let stroke = Keystroke::parse(PASTE_KEYSTROKE).map_err(|_| {
        request.fail(
            ErrorCode::Refused,
            format!("{PASTE_KEYSTROKE} is not a keystroke"),
        )
    })?;
    let kept = cx.read_from_clipboard();
    cx.write_to_clipboard(ClipboardItem::new_string(params.text.clone()));
    window.dispatch_keystroke(stroke, cx);
    if let Some(kept) = kept {
        cx.write_to_clipboard(kept);
    }
    Ok(json!({ "pasted": params.text.chars().count() }))
}

/// The visible rows of the params' pane, else of the active tab's focused
/// pane.
fn pane_text(request: &Request, root: &RootView, cx: &App) -> Result<Value, Reply> {
    let params: PaneTextParams = request.params()?;
    let Some(pane) = params.pane.or_else(|| root.driver_focused_pane()) else {
        return Err(request.fail(ErrorCode::NotFound, "the active tab has no pane".to_owned()));
    };
    let (Some(text), Some(lines)) = (
        root.pane_visible_text(&pane, cx),
        root.pane_grid_text(&pane, cx),
    ) else {
        return Err(Reply::error(
            request.id,
            ErrorCode::NotFound,
            format!("no pane {pane}"),
            Some(format!("panes: {}", root.active_pane_ids().join(", "))),
        ));
    };
    Ok(json!({ "pane": pane, "text": text, "lines": lines }))
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
        Predicate::PaneText { pane, contains } => match root.read(cx).pane_visible_text(pane, cx) {
            Some(text) => (text.contains(contains.as_str()), Value::String(text)),
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

/// What a pointer request carrying `modifiers` is refused with: gpui reads
/// them from the keyboard's state, which a posted message cannot set.
const MODIFIERS_HINT: &str =
    "posted pointer input cannot carry modifiers; press a chord with key, or use action";

/// The most names a `not_found` hint lists.
const NEAR_NAMES: usize = 10;

/// The `refused` reply to a pointer request with `modifiers`, whatever
/// their value.
fn refuse_modifiers(request: &Request) -> Result<(), Reply> {
    if request.params.get("modifiers").is_some() {
        return Err(Reply::error(
            request.id,
            ErrorCode::Refused,
            format!("{} cannot press modifiers", request.method),
            Some(MODIFIERS_HINT.to_owned()),
        ));
    }
    Ok(())
}

/// The logical window point `target` aims at: a point inside the window,
/// or the centre of an element's visible part in the latest frame.
fn resolve(
    request: &Request,
    target: &Target,
    window: &Window,
    cx: &App,
) -> Result<(f32, f32), Reply> {
    match target {
        Target::Point { x, y } => {
            let size = window.viewport_size();
            let (width, height) = (f32::from(size.width), f32::from(size.height));
            if (0.0..width).contains(x) && (0.0..height).contains(y) {
                Ok((*x, *y))
            } else {
                Err(bad_params(
                    request,
                    ParamError {
                        message: format!(
                            "the point ({x}, {y}) is outside the {width}x{height} window"
                        ),
                        hint: None,
                    },
                ))
            }
        }
        Target::Element(name) => element_centre(request, name, window, cx),
    }
}

/// The centre of the visible part of element `name`.
fn element_centre(
    request: &Request,
    name: &str,
    window: &Window,
    cx: &App,
) -> Result<(f32, f32), Reply> {
    let id = window.window_handle().window_id();
    let Some(entry) = registry::lookup(cx, id, name) else {
        return Err(Reply::error(
            request.id,
            ErrorCode::NotFound,
            format!("no element {name}"),
            Some(near_names(&registry::names(cx, id), name)),
        ));
    };
    if entry.visible.is_empty() {
        return Err(request.fail(
            ErrorCode::NotVisible,
            format!("{name} is painted, but a clipping ancestor hides all of it"),
        ));
    }
    let visible = entry.visible;
    Ok((
        f32::from(visible.origin.x) + f32::from(visible.size.width) / 2.0,
        f32::from(visible.origin.y) + f32::from(visible.size.height) / 2.0,
    ))
}

/// The hint for a name the latest frame did not paint: up to
/// [`NEAR_NAMES`] painted names sharing its longest prefix that ends at a
/// `-` or `:`, else the first painted names.
fn near_names(names: &[String], name: &str) -> String {
    let cuts = name
        .char_indices()
        .filter(|(_, ch)| matches!(ch, '-' | ':'))
        .map(|(at, ch)| at + ch.len_utf8());
    for cut in cuts.rev() {
        let prefix = &name[..cut];
        let near: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|painted| painted.starts_with(prefix))
            .take(NEAR_NAMES)
            .collect();
        if !near.is_empty() {
            return format!(r#"names starting with "{prefix}": {}"#, near.join(", "));
        }
    }
    let first: Vec<&str> = names.iter().map(String::as_str).take(NEAR_NAMES).collect();
    format!("painted names: {}", first.join(", "))
}

/// The window's handle and scale, for posting: refused when it has no
/// Win32 handle.
fn posting_window(request: &Request, window: &Window) -> Result<(usize, f32), Reply> {
    let address = pointer::window_address(window)
        .map_err(|reason| request.fail(ErrorCode::Refused, reason))?;
    Ok((address, window.scale_factor()))
}

fn post(request: &Request, address: usize, messages: &[pointer::Posted]) -> Result<(), Reply> {
    pointer::post(address, messages).map_err(|reason| request.fail(ErrorCode::Refused, reason))
}

fn point(at: (f32, f32)) -> Value {
    json!({ "x": at.0, "y": at.1 })
}

/// Clicks the params' button at their target, once or twice.
fn click(request: &Request, window: &Window, cx: &App) -> Result<Value, Reply> {
    let params: ClickParams = request.params()?;
    let (target, button, count) = params
        .read()
        .map_err(|refused| bad_params(request, refused))?;
    refuse_modifiers(request)?;
    let at = resolve(request, &target, window, cx)?;
    let (address, scale) = posting_window(request, window)?;
    post(
        request,
        address,
        &pointer::click_messages(pointer::physical(at, scale), button, count),
    )?;
    Ok(json!({ "at": point(at) }))
}

/// Moves the pointer to the params' target.
fn move_to(request: &Request, window: &Window, cx: &App) -> Result<Value, Reply> {
    let params: MoveParams = request.params()?;
    let target = params
        .target()
        .map_err(|refused| bad_params(request, refused))?;
    refuse_modifiers(request)?;
    let at = resolve(request, &target, window, cx)?;
    let (address, scale) = posting_window(request, window)?;
    post(
        request,
        address,
        &pointer::move_messages(pointer::physical(at, scale)),
    )?;
    Ok(json!({ "at": point(at) }))
}

/// Drags with the left button from the params' `from` to their `to`.
fn drag(request: &Request, window: &Window, cx: &App) -> Result<Value, Reply> {
    let params: DragParams = request.params()?;
    let (from, to, steps) = params
        .read()
        .map_err(|refused| bad_params(request, refused))?;
    refuse_modifiers(request)?;
    let from = resolve(request, &from, window, cx)?;
    let to = resolve(request, &to, window, cx)?;
    let (address, scale) = posting_window(request, window)?;
    let path: Vec<(i32, i32)> = pointer::drag_path(from, to, steps)
        .into_iter()
        .map(|at| pointer::physical(at, scale))
        .collect();
    post(
        request,
        address,
        &pointer::drag_messages(pointer::physical(from, scale), &path),
    )?;
    Ok(json!({ "from": point(from), "to": point(to) }))
}

/// Turns the wheel the params' notches at their target.
fn wheel(request: &Request, window: &Window, cx: &App) -> Result<Value, Reply> {
    let params: WheelParams = request.params()?;
    let (target, notches) = params
        .read()
        .map_err(|refused| bad_params(request, refused))?;
    refuse_modifiers(request)?;
    let at = resolve(request, &target, window, cx)?;
    let (address, scale) = posting_window(request, window)?;
    let client = pointer::physical(at, scale);
    let screen = pointer::client_to_screen(address, client)
        .map_err(|reason| request.fail(ErrorCode::Refused, reason))?;
    post(
        request,
        address,
        &pointer::wheel_messages(client, screen, notches),
    )?;
    Ok(json!({ "at": point(at) }))
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
    AddRepo(String),
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
            "add_repo" => repo_folder_arg(args).map(Self::AddRepo),
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

/// The folder argument of `add_repo`: an absolute path on this PC that
/// exists and is a folder, which is all the folder picker could answer.
fn repo_folder_arg(args: Option<&Value>) -> Result<String, ParamError> {
    let path = string_arg(args, "add_repo", "path")?;
    let refused = if Path::new(&path).is_absolute() {
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => None,
            Ok(_) => Some("not a folder".to_owned()),
            Err(_) => Some("no such folder".to_owned()),
        }
    } else {
        Some(format!("not an absolute path on {THIS_PC}"))
    };
    match refused {
        Some(reason) => Err(ParamError {
            message: format!("{path}: {reason}"),
            hint: None,
        }),
        None => Ok(path),
    }
}

/// The rail panel `name` names.
fn activity(name: &str) -> Result<Activity, ParamError> {
    match name {
        "sessions" => Ok(Activity::Sessions),
        "needs_you" => Ok(Activity::NeedsYou),
        "source_control" => Ok(Activity::SourceControl),
        "gates" => Ok(Activity::Gates),
        other => Err(ParamError {
            message: format!("unknown activity: {other}"),
            hint: Some("activities: sessions, needs_you, source_control, gates".to_owned()),
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
            || self.flyout_open
            || self.accounts_popover_open();
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
            Action::ShowActivity(item) => {
                self.change_activity(|sidebar| sidebar.show_activity(item), window, cx);
            }
            Action::SetHover(hovered) => self.set_window_hover_override(hovered, cx),
            Action::AddRepo(path) => self.finish_add_repo(Some(PathBuf::from(path))),
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
}
