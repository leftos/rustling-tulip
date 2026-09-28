//! The main window's title: `(M/N) <active tab> — rustling-tulip`, where M
//! counts the busy terminals of the active tab and N all of its live ones.

/// The product name the title ends with.
pub const PRODUCT_NAME: &str = "rustling-tulip";

/// The window title for the active tab `tab_name` and its `(busy, total)`
/// terminal counts. The count is shown only when `show_count` is on and the
/// tab has a live terminal; ` — rustling-tulip` ends it when `suffix` is on.
/// With no active tab the title is the product name, or empty with the
/// suffix off.
#[must_use]
pub fn compute_title(
    counts: Option<(usize, usize)>,
    tab_name: Option<&str>,
    show_count: bool,
    suffix: bool,
) -> String {
    let Some(name) = tab_name else {
        return if suffix {
            PRODUCT_NAME.to_owned()
        } else {
            String::new()
        };
    };
    let prefix = match counts {
        Some((busy, total)) if show_count && total > 0 => format!("({busy}/{total}) "),
        _ => String::new(),
    };
    let tail = if suffix {
        format!(" — {PRODUCT_NAME}")
    } else {
        String::new()
    };
    format!("{prefix}{name}{tail}")
}

#[cfg(test)]
mod tests {
    use super::compute_title;

    #[test]
    fn the_count_comes_before_the_tab_name_and_the_suffix_after() {
        assert_eq!(
            compute_title(Some((1, 3)), Some("Tab name"), true, true),
            "(1/3) Tab name — rustling-tulip"
        );
    }

    #[test]
    fn the_count_is_hidden_by_the_setting() {
        assert_eq!(
            compute_title(Some((1, 3)), Some("work"), false, true),
            "work — rustling-tulip"
        );
    }

    #[test]
    fn a_tab_with_no_live_terminal_drops_the_count() {
        assert_eq!(
            compute_title(Some((0, 0)), Some("work"), true, true),
            "work — rustling-tulip"
        );
        assert_eq!(
            compute_title(None, Some("diff"), true, true),
            "diff — rustling-tulip",
            "a diff tab has no counts"
        );
    }

    #[test]
    fn no_active_tab_is_the_product_name_or_nothing() {
        assert_eq!(compute_title(None, None, true, true), "rustling-tulip");
        assert_eq!(compute_title(Some((1, 2)), None, true, false), "");
    }

    #[test]
    fn the_suffix_is_dropped_by_the_setting() {
        assert_eq!(
            compute_title(Some((2, 2)), Some("work"), true, false),
            "(2/2) work"
        );
        assert_eq!(compute_title(None, Some("work"), false, false), "work");
    }
}
