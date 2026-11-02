//! Answers each driver request on the UI thread, against the main window.
//!
//! [`handle`] takes the root view's entity rather than the view itself: a
//! key it dispatches reaches the view's own handlers, which update
//! it, so the view must not be borrowed while they run.

use gpui::{App, Bounds, Entity, Pixels, Window};
use serde_json::{Value, json};

use crate::RootView;
use crate::driver::protocol::{ErrorCode, FindParams, KeyParams, Reply, Request};
use crate::driver::registry;

/// The client's version, as `ping` answers it.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

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
        // It awaits a capture thread, so the pipe's dispatcher answers it.
        "screenshot" => Err(request.fail(
            ErrorCode::Refused,
            "screenshot is answered only over the driver pipe".to_owned(),
        )),
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
            Some(json!({
                "name": name,
                "bounds": rect(entry.bounds),
                "visible": rect(entry.visible),
            }))
        })
        .collect();
    Ok(Value::Array(found))
}

fn rect(bounds: Bounds<Pixels>) -> Value {
    json!({
        "x": f32::from(bounds.origin.x),
        "y": f32::from(bounds.origin.y),
        "w": f32::from(bounds.size.width),
        "h": f32::from(bounds.size.height),
    })
}

/// Presses the one keystroke the params name.
fn key(request: &Request, window: &mut Window, cx: &mut App) -> Result<Value, Reply> {
    let params: KeyParams = request.params()?;
    let stroke = params.keystroke().map_err(|refused| {
        Reply::error(
            request.id,
            ErrorCode::BadParams,
            refused.message,
            refused.hint,
        )
    })?;
    window.dispatch_keystroke(stroke, cx);
    Ok(json!({ "pressed": params.keys }))
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
}
