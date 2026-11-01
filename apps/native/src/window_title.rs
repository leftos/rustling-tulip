//! The main window's title: `<active tab> — rustling-tulip`. The App title
//! settings' count is the taskbar badge's (`taskbar_badge.rs`), never the
//! title's.

use crate::sidebar::TitleSettings;

/// The product name the title ends with.
pub const PRODUCT_NAME: &str = "rustling-tulip";

/// The window title for the active tab `tab_name`, as `settings` shape it:
/// the name, then ` — rustling-tulip` when `suffix` is on. With no active
/// tab the title is the product name, or empty with the suffix off.
#[must_use]
pub fn compute_title(tab_name: Option<&str>, settings: &TitleSettings) -> String {
    let Some(name) = tab_name else {
        return if settings.suffix {
            PRODUCT_NAME.to_owned()
        } else {
            String::new()
        };
    };
    if settings.suffix {
        format!("{name} — {PRODUCT_NAME}")
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::compute_title;
    use crate::sidebar::{CountMode, TitleSettings};

    fn settings(show_count: bool, suffix: bool, count: CountMode) -> TitleSettings {
        TitleSettings {
            show_count,
            suffix,
            count,
        }
    }

    #[test]
    fn the_suffix_comes_after_the_tab_name() {
        assert_eq!(
            compute_title(Some("Tab name"), &settings(true, true, CountMode::Busy)),
            "Tab name — rustling-tulip"
        );
    }

    #[test]
    fn no_active_tab_is_the_product_name_or_nothing() {
        assert_eq!(
            compute_title(None, &settings(true, true, CountMode::Busy)),
            "rustling-tulip"
        );
        assert_eq!(
            compute_title(None, &settings(true, false, CountMode::Busy)),
            ""
        );
    }

    #[test]
    fn the_suffix_is_dropped_by_the_setting() {
        assert_eq!(
            compute_title(Some("work"), &settings(true, false, CountMode::Busy)),
            "work"
        );
        assert_eq!(
            compute_title(Some("work"), &settings(false, false, CountMode::Busy)),
            "work"
        );
    }

    #[test]
    fn the_title_never_carries_the_count() {
        for count in [CountMode::NeedsYou, CountMode::Busy, CountMode::Idle] {
            assert_eq!(
                compute_title(Some("work"), &settings(true, true, count)),
                "work — rustling-tulip",
                "{count:?}"
            );
        }
    }
}
