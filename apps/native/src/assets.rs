//! The icons the client draws, compiled into the binary and served to gpui
//! as its asset source, so nothing is read from disk at runtime.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

/// The rail's Sessions icon.
pub(crate) const SESSIONS_ICON: &str = "icons/sessions.svg";
/// The rail's Source control icon.
pub(crate) const SOURCE_CONTROL_ICON: &str = "icons/source-control.svg";
/// The source-control panel's refresh icon.
pub(crate) const REFRESH_ICON: &str = "icons/refresh.svg";

/// Every bundled asset, by the path the views ask for it under.
const ASSETS: [(&str, &[u8]); 3] = [
    (
        SESSIONS_ICON,
        include_bytes!("../assets/icons/sessions.svg"),
    ),
    (
        SOURCE_CONTROL_ICON,
        include_bytes!("../assets/icons/source-control.svg"),
    ),
    (REFRESH_ICON, include_bytes!("../assets/icons/refresh.svg")),
];

/// The bundled assets; the app registers it with `Application::with_assets`.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(ASSETS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(ASSETS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}
