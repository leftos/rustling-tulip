//! The iPhone app: opens the phone root.

fn main() {
    rustling_tulip_native::phone::run(on_start);
}

/// Runs inside the app before the window opens: light status bar text over
/// the app's dark ground on iOS, nothing elsewhere.
fn on_start(_cx: &mut gpui::App) {
    #[cfg(target_os = "ios")]
    gpui_ios::ios::set_status_bar_style(gpui_ios::StatusBarContentStyle::Light);
}
