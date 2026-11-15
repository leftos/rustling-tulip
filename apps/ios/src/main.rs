//! The iPhone app: opens the phone root.

fn main() {
    rustling_tulip_native::phone::run(register_delegates, on_start);
}

/// Registers `gpui_ios`'s app delegate classes before the app starts, so the
/// phone adds its background-push method to them before `UIKit` can call it;
/// nothing elsewhere.
fn register_delegates() {
    #[cfg(target_os = "ios")]
    gpui_ios::register_delegate_classes();
}

/// Runs inside the app before the window opens: light status bar text over
/// the app's dark ground on iOS, nothing elsewhere.
fn on_start(_cx: &mut gpui::App) {
    #[cfg(target_os = "ios")]
    gpui_ios::ios::set_status_bar_style(gpui_ios::StatusBarContentStyle::Light);
}
