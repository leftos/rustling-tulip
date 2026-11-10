//! The name a tab shows: a tab nobody named reads the repo and workspace
//! names of the sessions it holds; a tab the user (or a preset) named keeps
//! that name. Display only: the derived name is never stored or sent.

/// Whether `name` is one the daemon gives a tab nobody named: blank, `Tab`,
/// or `Tab {n}` with `n` one or more ASCII digits.
#[must_use]
pub fn is_default_tab_name(name: &str) -> bool {
    name.trim().is_empty()
        || name == "Tab"
        || name
            .strip_prefix("Tab ")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The name a tab stored as `stored` shows: a real name as it is; a default
/// one as the `Some` names of `group_names` (its sessions' repos and
/// workspaces, in pane order), each once, first seen kept, joined with
/// `", "`; `stored` again when none of them names anything.
#[must_use]
pub fn display_name<'a>(
    stored: &str,
    group_names: impl IntoIterator<Item = Option<&'a str>>,
) -> String {
    if !is_default_tab_name(stored) {
        return stored.to_owned();
    }
    let mut shown: Vec<&str> = Vec::new();
    for name in group_names
        .into_iter()
        .flatten()
        .filter(|name| !name.trim().is_empty())
    {
        if !shown.contains(&name) {
            shown.push(name);
        }
    }
    if shown.is_empty() {
        stored.to_owned()
    } else {
        shown.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::{display_name, is_default_tab_name};

    #[test]
    fn blank_tab_and_numbered_tab_are_default_names() {
        for name in ["", "  ", "Tab", "Tab 2", "Tab 10", "Tab 007"] {
            assert!(is_default_tab_name(name), "{name:?} is a default name");
        }
    }

    #[test]
    fn preset_and_user_names_are_not_default() {
        for name in [
            "Tab 2 of 3",
            "Tabby",
            "Tab x",
            "My tab",
            "Tab ",
            "Tab -1",
            "tab 2",
        ] {
            assert!(!is_default_tab_name(name), "{name:?} is a real name");
        }
    }

    #[test]
    fn default_name_shows_group_names_in_order() {
        let names = [Some("repo-b"), None, Some("repo-a"), Some("ws")];
        assert_eq!(display_name("Tab 2", names), "repo-b, repo-a, ws");
    }

    #[test]
    fn group_names_are_deduplicated_first_seen() {
        let names = [Some("a"), Some("b"), Some("a"), Some("c"), Some("b")];
        assert_eq!(display_name("Tab", names), "a, b, c");
    }

    #[test]
    fn default_name_with_no_group_names_keeps_stored() {
        assert_eq!(display_name("Tab 3", [None, None]), "Tab 3");
        assert_eq!(display_name("Tab", []), "Tab");
        assert_eq!(
            display_name("Tab", [Some("")]),
            "Tab",
            "a blank name adds nothing"
        );
    }

    #[test]
    fn real_name_is_kept_whatever_the_sessions() {
        assert_eq!(display_name("work", [Some("repo-a")]), "work");
        assert_eq!(display_name("Tab 2 of 3", [Some("repo-a")]), "Tab 2 of 3");
    }
}
