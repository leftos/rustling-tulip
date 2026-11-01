//! The icons the client draws, compiled into the binary and served to gpui
//! as its asset source, so nothing is read from disk at runtime.

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

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
/// The rail's Settings icon.
pub(crate) const SETTINGS_ICON: &str = "icons/settings.svg";
/// The rail's Dashboard button and a Dashboard tab's pill.
pub(crate) const DASHBOARD_ICON: &str = "icons/dashboard.svg";
/// A tab pill's close icon.
pub(crate) const TAB_CLOSE_ICON: &str = "icons/tab-close.svg";
/// The tab bar's new-tab icon.
pub(crate) const TAB_NEW_ICON: &str = "icons/tab-new.svg";

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
/// A repo or workspace row's launch-last ▶.
pub(crate) const SIDEBAR_PLAY_ICON: &str = "icons/sidebar-play.svg";

/// A narrow diff tab header's whitespace toggle: an open box, `␣`.
pub(crate) const DIFF_WHITESPACE_ICON: &str = "icons/diff-whitespace.svg";
/// A narrow diff tab header's highlight toggle: a pen.
pub(crate) const DIFF_HIGHLIGHT_ICON: &str = "icons/diff-highlight.svg";
/// A diff tab's First change button: a bar and a double chevron, left.
pub(crate) const DIFF_FIRST_ICON: &str = "icons/diff-first.svg";
/// A diff tab's Previous change button: a chevron, left.
pub(crate) const DIFF_PREV_ICON: &str = "icons/diff-prev.svg";
/// A diff tab's Next change button: a chevron, right.
pub(crate) const DIFF_NEXT_ICON: &str = "icons/diff-next.svg";
/// A diff tab's Last change button: a double chevron and a bar, right.
pub(crate) const DIFF_LAST_ICON: &str = "icons/diff-last.svg";

/// The Connections picker's This PC row: a monitor.
pub(crate) const HOST_PC_ICON: &str = "icons/host-pc.svg";
/// The Connections picker's tailnet rows: three linked nodes.
pub(crate) const HOST_TAILNET_ICON: &str = "icons/host-tailnet.svg";
/// The Connections picker's SSH rows: a terminal prompt in a box.
pub(crate) const HOST_SSH_ICON: &str = "icons/host-ssh.svg";
/// A check mark: the Connections picker's passed Test connection.
pub(crate) const CHECK_ICON: &str = "icons/check.svg";
/// The pane header's trusted chip: a shield.
pub(crate) const SHIELD_ICON: &str = "icons/shield.svg";

/// The runtime icon of PowerShell 7 and later.
pub(crate) const RUNTIME_PWSH_ICON: &str = "icons/runtime-pwsh.svg";
/// The runtime icon of Windows PowerShell 5.
pub(crate) const RUNTIME_POWERSHELL_ICON: &str = "icons/runtime-powershell.svg";
/// The runtime icon of cmd.
pub(crate) const RUNTIME_CMD_ICON: &str = "icons/runtime-cmd.svg";
/// The runtime icon of bash, zsh, sh and WSL.
pub(crate) const RUNTIME_POSIX_ICON: &str = "icons/runtime-posix.svg";

/// Where an agent's icon is served from: `agent-icons/<hash>.svg`.
const AGENT_ICONS_DIR: &str = "agent-icons/";

/// The agents' icons registered while the app runs, by the path
/// [`agent_icon_path`] gave them.
static AGENT_ICONS: LazyLock<RwLock<HashMap<String, Arc<[u8]>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// The asset path agent icon `svg` is served under, registering its bytes
/// there the first time; the same markup always gets the same path.
pub(crate) fn agent_icon_path(svg: &str) -> SharedString {
    let mut hasher = DefaultHasher::new();
    svg.hash(&mut hasher);
    let path = format!("{AGENT_ICONS_DIR}{:016x}.svg", hasher.finish());
    let known = AGENT_ICONS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .contains_key(&path);
    if !known {
        AGENT_ICONS
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(path.clone())
            .or_insert_with(|| Arc::from(svg.as_bytes()));
    }
    path.into()
}

/// The bytes of a registered agent icon at `path`.
fn agent_icon_bytes(path: &str) -> Option<Cow<'static, [u8]>> {
    if !path.starts_with(AGENT_ICONS_DIR) {
        return None;
    }
    AGENT_ICONS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(path)
        .map(|bytes| Cow::Owned(bytes.to_vec()))
}

/// Every bundled asset, by the path the views ask for it under.
const ASSETS: [(&str, &[u8]); 42] = [
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
        SETTINGS_ICON,
        include_bytes!("../assets/icons/settings.svg"),
    ),
    (
        DASHBOARD_ICON,
        include_bytes!("../assets/icons/dashboard.svg"),
    ),
    (
        TAB_CLOSE_ICON,
        include_bytes!("../assets/icons/tab-close.svg"),
    ),
    (TAB_NEW_ICON, include_bytes!("../assets/icons/tab-new.svg")),
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
    (
        SIDEBAR_PLAY_ICON,
        include_bytes!("../assets/icons/sidebar-play.svg"),
    ),
    (
        DIFF_WHITESPACE_ICON,
        include_bytes!("../assets/icons/diff-whitespace.svg"),
    ),
    (
        DIFF_HIGHLIGHT_ICON,
        include_bytes!("../assets/icons/diff-highlight.svg"),
    ),
    (
        DIFF_FIRST_ICON,
        include_bytes!("../assets/icons/diff-first.svg"),
    ),
    (
        DIFF_PREV_ICON,
        include_bytes!("../assets/icons/diff-prev.svg"),
    ),
    (
        DIFF_NEXT_ICON,
        include_bytes!("../assets/icons/diff-next.svg"),
    ),
    (
        DIFF_LAST_ICON,
        include_bytes!("../assets/icons/diff-last.svg"),
    ),
    (HOST_PC_ICON, include_bytes!("../assets/icons/host-pc.svg")),
    (
        HOST_TAILNET_ICON,
        include_bytes!("../assets/icons/host-tailnet.svg"),
    ),
    (
        HOST_SSH_ICON,
        include_bytes!("../assets/icons/host-ssh.svg"),
    ),
    (CHECK_ICON, include_bytes!("../assets/icons/check.svg")),
    (SHIELD_ICON, include_bytes!("../assets/icons/shield.svg")),
    (
        RUNTIME_PWSH_ICON,
        include_bytes!("../assets/icons/runtime-pwsh.svg"),
    ),
    (
        RUNTIME_POWERSHELL_ICON,
        include_bytes!("../assets/icons/runtime-powershell.svg"),
    ),
    (
        RUNTIME_CMD_ICON,
        include_bytes!("../assets/icons/runtime-cmd.svg"),
    ),
    (
        RUNTIME_POSIX_ICON,
        include_bytes!("../assets/icons/runtime-posix.svg"),
    ),
];

/// The bundled assets; the app registers it with `Application::with_assets`.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(ASSETS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes))
            .or_else(|| agent_icon_bytes(path)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(ASSETS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;

    fn load(path: &str) -> Option<Vec<u8>> {
        Assets
            .load(path)
            .expect("loading never errs")
            .map(Cow::into_owned)
    }

    #[test]
    fn the_shell_runtime_icons_are_bundled() {
        for path in [
            RUNTIME_PWSH_ICON,
            RUNTIME_POWERSHELL_ICON,
            RUNTIME_CMD_ICON,
            RUNTIME_POSIX_ICON,
        ] {
            assert!(load(path).is_some_and(|bytes| !bytes.is_empty()), "{path}");
        }
    }

    #[test]
    fn an_agent_icon_is_served_under_its_own_path() {
        let spark = "<svg viewBox=\"0 0 16 16\"><path d=\"M8 1v14\"/></svg>";
        let other = "<svg viewBox=\"0 0 16 16\"><path d=\"M1 8h14\"/></svg>";
        let path = agent_icon_path(spark);
        assert!(path.starts_with(AGENT_ICONS_DIR), "{path}");
        assert_eq!(
            agent_icon_path(spark),
            path,
            "the same markup, the same path"
        );
        assert_ne!(agent_icon_path(other), path, "other markup, another path");
        assert_eq!(load(&path), Some(spark.as_bytes().to_vec()));
        assert_eq!(
            load(&agent_icon_path(other)),
            Some(other.as_bytes().to_vec())
        );
    }

    #[test]
    fn an_unregistered_agent_icon_loads_nothing() {
        assert_eq!(load("agent-icons/0000000000000000.svg"), None);
    }
}
