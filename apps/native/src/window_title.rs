//! The main window's title: `(<count>) <active tab> — rustling-tulip`, where
//! the count is what the App title settings' count mode counts in the
//! active tab.

use crate::sidebar::TitleSettings;
use crate::tabs::{TabBadge, TabCounts};

/// The product name the title ends with.
pub const PRODUCT_NAME: &str = "rustling-tulip";

/// The window title for the active tab `tab_name` and its terminal
/// `counts`, as `settings` shape it. The count is shown only when
/// `show_count` is on and the count mode has something to show: `(n)` for
/// the sessions that need the owner, never 0, `(busy/total)` or
/// `(idle/total)` for a tab with a live terminal. ` — rustling-tulip` ends
/// it when `suffix` is on. With no active tab the title is the product
/// name, or empty with the suffix off.
#[must_use]
pub fn compute_title(
    counts: Option<TabCounts>,
    tab_name: Option<&str>,
    settings: &TitleSettings,
) -> String {
    let Some(name) = tab_name else {
        return if settings.suffix {
            PRODUCT_NAME.to_owned()
        } else {
            String::new()
        };
    };
    let badge = counts
        .filter(|_| settings.show_count)
        .and_then(|counts| counts.badge(settings.count));
    let prefix = match badge {
        Some(TabBadge::NeedsYou(n)) => format!("({n}) "),
        Some(TabBadge::Busy { busy, total }) => format!("({busy}/{total}) "),
        Some(TabBadge::Idle { idle, total }) => format!("({idle}/{total}) "),
        None => String::new(),
    };
    let tail = if settings.suffix {
        format!(" — {PRODUCT_NAME}")
    } else {
        String::new()
    };
    format!("{prefix}{name}{tail}")
}

#[cfg(test)]
mod tests {
    use super::compute_title;
    use crate::sidebar::{CountMode, TitleSettings};
    use crate::tabs::TabCounts;

    fn settings(show_count: bool, suffix: bool, count: CountMode) -> TitleSettings {
        TitleSettings {
            show_count,
            suffix,
            count,
        }
    }

    /// `busy` of `total` terminals busy, the rest idle, none needing you.
    fn busy_of(busy: usize, total: usize) -> TabCounts {
        TabCounts {
            busy,
            idle: total - busy,
            needs_you: 0,
            total,
        }
    }

    #[test]
    fn the_count_comes_before_the_tab_name_and_the_suffix_after() {
        assert_eq!(
            compute_title(
                Some(busy_of(1, 3)),
                Some("Tab name"),
                &settings(true, true, CountMode::Busy)
            ),
            "(1/3) Tab name — rustling-tulip"
        );
    }

    #[test]
    fn the_count_is_hidden_by_the_setting() {
        assert_eq!(
            compute_title(
                Some(busy_of(1, 3)),
                Some("work"),
                &settings(false, true, CountMode::Busy)
            ),
            "work — rustling-tulip"
        );
    }

    #[test]
    fn a_tab_with_no_live_terminal_drops_the_count() {
        assert_eq!(
            compute_title(
                Some(busy_of(0, 0)),
                Some("work"),
                &settings(true, true, CountMode::Busy)
            ),
            "work — rustling-tulip"
        );
        assert_eq!(
            compute_title(None, Some("diff"), &settings(true, true, CountMode::Busy)),
            "diff — rustling-tulip",
            "a diff tab has no counts"
        );
    }

    #[test]
    fn no_active_tab_is_the_product_name_or_nothing() {
        assert_eq!(
            compute_title(None, None, &settings(true, true, CountMode::Busy)),
            "rustling-tulip"
        );
        assert_eq!(
            compute_title(
                Some(busy_of(1, 2)),
                None,
                &settings(true, false, CountMode::Busy)
            ),
            ""
        );
    }

    #[test]
    fn the_suffix_is_dropped_by_the_setting() {
        assert_eq!(
            compute_title(
                Some(busy_of(2, 2)),
                Some("work"),
                &settings(true, false, CountMode::Busy)
            ),
            "(2/2) work"
        );
        assert_eq!(
            compute_title(None, Some("work"), &settings(false, false, CountMode::Busy)),
            "work"
        );
    }

    #[test]
    fn the_title_shows_the_needs_you_count_and_nothing_at_zero() {
        let asking = Some(TabCounts {
            busy: 3,
            idle: 1,
            needs_you: 2,
            total: 4,
        });
        let needs_you = TitleSettings::default();
        assert_eq!(needs_you.count, CountMode::NeedsYou, "the default");
        assert_eq!(
            compute_title(asking, Some("work"), &needs_you),
            "(2) work — rustling-tulip"
        );
        assert_eq!(
            compute_title(Some(busy_of(1, 3)), Some("work"), &needs_you),
            "work — rustling-tulip",
            "nothing needs you: just the tab name"
        );
        assert_eq!(
            compute_title(
                asking,
                Some("work"),
                &settings(false, true, CountMode::NeedsYou)
            ),
            "work — rustling-tulip",
            "the title toggle still hides it"
        );
    }

    #[test]
    fn the_title_shows_busy_or_idle_over_total_in_those_modes() {
        let counts = Some(TabCounts {
            busy: 1,
            idle: 2,
            needs_you: 1,
            total: 3,
        });
        assert_eq!(
            compute_title(counts, Some("work"), &settings(true, true, CountMode::Busy)),
            "(1/3) work — rustling-tulip"
        );
        assert_eq!(
            compute_title(counts, Some("work"), &settings(true, true, CountMode::Idle)),
            "(2/3) work — rustling-tulip"
        );
        assert_eq!(
            compute_title(
                Some(busy_of(3, 3)),
                Some("work"),
                &settings(true, false, CountMode::Idle)
            ),
            "(0/3) work",
            "idle over total shows at 0"
        );
    }
}
