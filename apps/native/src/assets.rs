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
/// The rail's Recover sessions icon.
pub(crate) const RECOVER_ICON: &str = "icons/recover.svg";
/// The rail's Needs You icon.
pub(crate) const NEEDS_YOU_ICON: &str = "icons/needs-you.svg";

/// The working glyph's spinning arc.
pub(crate) const STATUS_ARC_ICON: &str = "icons/status-arc.svg";
/// The working glyph's filled core.
pub(crate) const STATUS_CORE_ICON: &str = "icons/status-core.svg";
/// The `×` inside the error dot.
pub(crate) const STATUS_CROSS_ICON: &str = "icons/status-cross.svg";
/// The asking glyph's diamond.
pub(crate) const STATUS_DIAMOND_ICON: &str = "icons/status-diamond.svg";
/// The filled dot of the idle, stopped and error glyphs.
pub(crate) const STATUS_DOT_ICON: &str = "icons/status-dot.svg";
/// The `?` inside the asking diamond.
pub(crate) const STATUS_QUESTION_ICON: &str = "icons/status-question.svg";
/// The hollow ring of the waiting and spawning glyphs.
pub(crate) const STATUS_RING_ICON: &str = "icons/status-ring.svg";
/// The working glyph's faint ring.
pub(crate) const STATUS_TRACK_ICON: &str = "icons/status-track.svg";

/// The sessions panel's expanded-container chevron.
pub(crate) const SIDEBAR_CHEVRON_DOWN_ICON: &str = "icons/sidebar-chevron-down.svg";
/// The sessions panel's collapsed-container chevron.
pub(crate) const SIDEBAR_CHEVRON_RIGHT_ICON: &str = "icons/sidebar-chevron-right.svg";
/// A workspace container's icon.
pub(crate) const SIDEBAR_WORKSPACE_ICON: &str = "icons/sidebar-workspace.svg";
/// A repo container's git-branch icon.
pub(crate) const SIDEBAR_BRANCH_ICON: &str = "icons/sidebar-branch.svg";
/// A folder or detached container's icon.
pub(crate) const SIDEBAR_FOLDER_ICON: &str = "icons/sidebar-folder.svg";
/// A shell container's icon and the toolbar's Shell prompt.
pub(crate) const SIDEBAR_SHELL_ICON: &str = "icons/sidebar-shell.svg";
/// A tab container's icon.
pub(crate) const SIDEBAR_TABS_ICON: &str = "icons/sidebar-tabs.svg";
/// The toolbar's Session plus.
pub(crate) const SIDEBAR_PLUS_ICON: &str = "icons/sidebar-plus.svg";
/// The toolbar's ⋯ menu dots.
pub(crate) const SIDEBAR_MORE_ICON: &str = "icons/sidebar-more.svg";

/// Every bundled asset, by the path the views ask for it under.
const ASSETS: [(&str, &[u8]); 22] = [
    (
        SESSIONS_ICON,
        include_bytes!("../assets/icons/sessions.svg"),
    ),
    (
        SOURCE_CONTROL_ICON,
        include_bytes!("../assets/icons/source-control.svg"),
    ),
    (REFRESH_ICON, include_bytes!("../assets/icons/refresh.svg")),
    (RECOVER_ICON, include_bytes!("../assets/icons/recover.svg")),
    (
        NEEDS_YOU_ICON,
        include_bytes!("../assets/icons/needs-you.svg"),
    ),
    (
        STATUS_ARC_ICON,
        include_bytes!("../assets/icons/status-arc.svg"),
    ),
    (
        STATUS_CORE_ICON,
        include_bytes!("../assets/icons/status-core.svg"),
    ),
    (
        STATUS_CROSS_ICON,
        include_bytes!("../assets/icons/status-cross.svg"),
    ),
    (
        STATUS_DIAMOND_ICON,
        include_bytes!("../assets/icons/status-diamond.svg"),
    ),
    (
        STATUS_DOT_ICON,
        include_bytes!("../assets/icons/status-dot.svg"),
    ),
    (
        STATUS_QUESTION_ICON,
        include_bytes!("../assets/icons/status-question.svg"),
    ),
    (
        STATUS_RING_ICON,
        include_bytes!("../assets/icons/status-ring.svg"),
    ),
    (
        STATUS_TRACK_ICON,
        include_bytes!("../assets/icons/status-track.svg"),
    ),
    (
        SIDEBAR_CHEVRON_DOWN_ICON,
        include_bytes!("../assets/icons/sidebar-chevron-down.svg"),
    ),
    (
        SIDEBAR_CHEVRON_RIGHT_ICON,
        include_bytes!("../assets/icons/sidebar-chevron-right.svg"),
    ),
    (
        SIDEBAR_WORKSPACE_ICON,
        include_bytes!("../assets/icons/sidebar-workspace.svg"),
    ),
    (
        SIDEBAR_BRANCH_ICON,
        include_bytes!("../assets/icons/sidebar-branch.svg"),
    ),
    (
        SIDEBAR_FOLDER_ICON,
        include_bytes!("../assets/icons/sidebar-folder.svg"),
    ),
    (
        SIDEBAR_SHELL_ICON,
        include_bytes!("../assets/icons/sidebar-shell.svg"),
    ),
    (
        SIDEBAR_TABS_ICON,
        include_bytes!("../assets/icons/sidebar-tabs.svg"),
    ),
    (
        SIDEBAR_PLUS_ICON,
        include_bytes!("../assets/icons/sidebar-plus.svg"),
    ),
    (
        SIDEBAR_MORE_ICON,
        include_bytes!("../assets/icons/sidebar-more.svg"),
    ),
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
