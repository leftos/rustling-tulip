//! The themes modal's model: one row per theme the store lists, holding the
//! theme it loads to, the contrast pairs it fails and whether it reads light,
//! so the view draws the list without touching the filesystem.

use crate::contrast::{self, PairResult};
use crate::theme_store::{self, ThemeEntry, ThemeStore};
use crate::ui_theme::UiTheme;

/// The ground's relative luminance above which a theme reads light.
const LIGHT_ABOVE: f32 = 0.18;

/// What a theme whose file names no terminal source shows instead.
const OWN_PALETTE: &str = "Own palette";

/// One row of the themes modal.
#[derive(Debug, Clone)]
pub struct ThemeRow {
    pub entry: ThemeEntry,
    /// `None` when the row's theme does not load.
    pub theme: Option<UiTheme>,
    /// The pairs below their floor; empty when the theme does not load.
    pub failing: Vec<PairResult>,
    /// Whether the theme's ground reads light rather than dark; false when
    /// the theme does not load.
    pub light: bool,
    /// The theme's terminal source as shown: its name, "Own palette", or the
    /// stored id; empty for a row with an error.
    pub terminal_source: String,
}

/// The themes modal: every listed theme, the one in use, and what the pointer
/// and the keyboard are on.
#[derive(Debug, Clone)]
pub struct ThemesDialog {
    pub rows: Vec<ThemeRow>,
    pub in_use: usize,
    pub cursor: usize,
    pub hover: Option<usize>,
}

impl ThemesDialog {
    /// Lists every theme `store` holds, opening on `selected` (the theme the
    /// default built-in names when it is `None` or loads nothing).
    #[must_use]
    pub fn new(store: &ThemeStore, selected: Option<&str>) -> Self {
        let entries = store.list();
        let names: Vec<(String, String)> = entries
            .iter()
            .map(|entry| (entry.id.clone(), entry.name.clone()))
            .collect();
        let rows: Vec<ThemeRow> = entries
            .into_iter()
            .map(|entry| row(store, entry, &names))
            .collect();
        let in_use = selected
            .and_then(|id| matching(&rows, id))
            .or_else(|| matching(&rows, theme_store::DEFAULT_ID))
            .unwrap_or(0);
        Self {
            rows,
            in_use,
            cursor: in_use,
            hover: None,
        }
    }

    /// The row the preview draws: the pointer's, else the keyboard's.
    #[must_use]
    pub fn preview(&self) -> usize {
        self.hover.unwrap_or(self.cursor)
    }

    /// Moves the keyboard's row by `delta`, stopping at either end, and drops
    /// the pointer's row so the preview follows the keyboard.
    pub fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
        self.hover = None;
    }

    /// Sets the row the pointer is over; `None` while it is over none.
    pub fn set_hover(&mut self, row: Option<usize>) {
        self.hover = row;
    }

    /// Whether the keyboard's row has a theme to draw.
    #[must_use]
    pub fn can_use(&self) -> bool {
        self.rows
            .get(self.cursor)
            .is_some_and(|row| row.theme.is_some())
    }

    /// The id the keyboard's row would select, when it has a theme.
    #[must_use]
    pub fn chosen_id(&self) -> Option<&str> {
        self.rows
            .get(self.cursor)
            .filter(|row| row.theme.is_some())
            .map(|row| row.entry.id.as_str())
    }

    /// The first row of the user's own themes; [`Self::rows`]' length when it
    /// holds none.
    #[must_use]
    pub fn yours_start(&self) -> usize {
        self.rows
            .iter()
            .position(|row| !row.entry.built_in)
            .unwrap_or(self.rows.len())
    }
}

/// The first row `id` names that loads: built-in rows come first, so a file
/// shadowing a built-in's id never answers for it.
fn matching(rows: &[ThemeRow], id: &str) -> Option<usize> {
    rows.iter()
        .position(|row| row.entry.error.is_none() && row.entry.id == id)
}

/// The row for `entry`, loaded with the theme it names.
fn row(store: &ThemeStore, mut entry: ThemeEntry, names: &[(String, String)]) -> ThemeRow {
    let theme = if entry.error.is_some() {
        None
    } else {
        match store.load(&entry.id) {
            Ok(theme) => Some(theme),
            Err(err) => {
                entry.error = Some(err.to_string());
                entry.base = None;
                entry.terminal_source = None;
                None
            }
        }
    };
    let terminal_source = if entry.error.is_some() {
        String::new()
    } else {
        match &entry.terminal_source {
            None => OWN_PALETTE.to_owned(),
            Some(id) => names
                .iter()
                .find(|(known, _)| known == id)
                .map_or_else(|| id.clone(), |(_, name)| name.clone()),
        }
    };
    let failing = theme.map_or_else(Vec::new, |theme| {
        contrast::check(&theme)
            .into_iter()
            .filter(|pair| !pair.passes)
            .collect()
    });
    let light = theme.is_some_and(|theme| contrast::relative_luminance(theme.ground) > LIGHT_ABOVE);
    ThemeRow {
        entry,
        theme,
        failing,
        light,
        terminal_source,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::*;
    use crate::builtin_themes::{CATPPUCCIN_LATTE, NORD};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rt-native-themes-dialog-{}-{tag}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }

        fn store(&self) -> ThemeStore {
            ThemeStore::new(self.0.clone())
        }

        fn file(&self, id: &str) -> PathBuf {
            self.0.join(format!("{id}.json"))
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The row `id` names, from the first row that has no error.
    fn row_of<'a>(dialog: &'a ThemesDialog, id: &str) -> &'a ThemeRow {
        dialog
            .rows
            .iter()
            .find(|row| row.entry.error.is_none() && row.entry.id == id)
            .expect("the row")
    }

    #[test]
    fn opens_on_the_theme_in_use() {
        let dir = TestDir::new("in-use");
        let store = dir.store();
        let id = store
            .save("Mine", &UiTheme::CONTRAST, None, None)
            .expect("save");
        let dialog = ThemesDialog::new(&store, Some(&id));

        assert_eq!(dialog.rows[dialog.in_use].entry.id, id);
        assert_eq!(dialog.cursor, dialog.in_use, "the keyboard starts on it");
        assert!(dialog.can_use());
        assert_eq!(dialog.chosen_id(), Some(id.as_str()));
    }

    #[test]
    fn no_selection_opens_on_the_default() {
        let dir = TestDir::new("default");
        let dialog = ThemesDialog::new(&dir.store(), None);

        assert_eq!(dialog.rows[dialog.in_use].entry.id, theme_store::DEFAULT_ID);
        assert!(dialog.rows[dialog.in_use].entry.built_in);
        assert_eq!(dialog.cursor, dialog.in_use);
    }

    #[test]
    fn an_unknown_selection_opens_on_the_default() {
        let dir = TestDir::new("unknown");
        let dialog = ThemesDialog::new(&dir.store(), Some("no-such-theme"));

        assert_eq!(dialog.rows[dialog.in_use].entry.id, theme_store::DEFAULT_ID);
    }

    #[test]
    fn a_shadow_row_is_never_the_theme_in_use() {
        let dir = TestDir::new("shadow");
        let store = dir.store();
        let id = store
            .save("Mine", &UiTheme::CONTRAST, None, None)
            .expect("save");
        std::fs::copy(dir.file(&id), dir.file("nord")).expect("copy");
        let dialog = ThemesDialog::new(&store, Some("nord"));

        let in_use = &dialog.rows[dialog.in_use];
        assert!(in_use.entry.built_in, "the built-in row");
        assert_eq!(in_use.entry.id, "nord");
        assert_eq!(in_use.theme, Some(NORD));
        let shadow = dialog
            .rows
            .iter()
            .position(|row| row.entry.id == "nord" && !row.entry.built_in)
            .expect("the shadow row");
        assert_ne!(shadow, dialog.in_use);
        assert!(dialog.rows[shadow].theme.is_none());
        assert!(dialog.rows[shadow].entry.error.is_some());
    }

    #[test]
    fn a_broken_row_can_be_reached_but_not_used() {
        let dir = TestDir::new("broken");
        let store = dir.store();
        std::fs::create_dir_all(&dir.0).expect("make the folder");
        std::fs::write(dir.file("broken"), "{").expect("write");
        let mut dialog = ThemesDialog::new(&store, None);
        let broken = dialog
            .rows
            .iter()
            .position(|row| row.entry.id == "broken")
            .expect("the broken row");
        dialog.move_cursor(broken.cast_signed() - dialog.cursor.cast_signed());

        assert_eq!(dialog.cursor, broken);
        assert!(!dialog.can_use());
        assert_eq!(dialog.chosen_id(), None);
        assert!(dialog.rows[broken].failing.is_empty(), "no pairs listed");
        assert!(!dialog.rows[broken].light);
        let error = dialog.rows[broken].entry.error.as_deref().expect("error");
        assert!(error.contains("broken.json"), "{error}");
    }

    #[test]
    fn preview_follows_hover_then_falls_back_to_the_cursor() {
        let dir = TestDir::new("preview");
        let mut dialog = ThemesDialog::new(&dir.store(), None);

        dialog.set_hover(Some(3));
        assert_eq!(dialog.preview(), 3);
        dialog.set_hover(None);
        assert_eq!(dialog.preview(), dialog.cursor);
    }

    #[test]
    fn moving_the_cursor_clears_the_hover() {
        let dir = TestDir::new("cursor-clears-hover");
        let mut dialog = ThemesDialog::new(&dir.store(), None);

        dialog.set_hover(Some(4));
        dialog.move_cursor(1);
        assert_eq!(dialog.hover, None);
        assert_eq!(
            dialog.preview(),
            dialog.cursor,
            "the preview follows the keyboard"
        );
    }

    #[test]
    fn a_row_with_an_error_shows_no_terminal_source() {
        let dir = TestDir::new("error-source");
        let store = dir.store();
        std::fs::create_dir_all(&dir.0).expect("make the folder");
        std::fs::write(dir.file("broken"), "{").expect("write");
        let dialog = ThemesDialog::new(&store, None);

        let broken = dialog
            .rows
            .iter()
            .find(|row| row.entry.id == "broken")
            .expect("the broken row");
        assert!(broken.entry.error.is_some());
        assert_eq!(broken.terminal_source, "");
        assert_eq!(broken.entry.terminal_source, None);
        assert_eq!(broken.entry.base, None);
    }

    #[test]
    fn the_cursor_clamps_at_both_ends() {
        let dir = TestDir::new("clamp");
        let mut dialog = ThemesDialog::new(&dir.store(), None);
        let last = dialog.rows.len() - 1;

        dialog.move_cursor(-100);
        assert_eq!(dialog.cursor, 0);
        dialog.move_cursor(1000);
        assert_eq!(dialog.cursor, last);
        dialog.move_cursor(1);
        assert_eq!(dialog.cursor, last, "no wrap");
    }

    #[test]
    fn a_terminal_source_shows_the_themes_name_or_the_stored_id() {
        let dir = TestDir::new("source");
        let store = dir.store();
        let named = store
            .save("Named", &UiTheme::CONTRAST, None, Some("dracula"))
            .expect("save");
        let stray = store
            .save("Stray", &UiTheme::CONTRAST, None, Some("no-such-theme"))
            .expect("save");
        let dialog = ThemesDialog::new(&store, None);

        assert_eq!(row_of(&dialog, &named).terminal_source, "Dracula");
        assert_eq!(row_of(&dialog, &stray).terminal_source, "no-such-theme");
    }

    #[test]
    fn a_builtin_with_no_source_shows_own_palette() {
        let dir = TestDir::new("own-palette");
        let dialog = ThemesDialog::new(&dir.store(), None);

        assert_eq!(dialog.rows[0].entry.id, "contrast");
        assert_eq!(dialog.rows[0].terminal_source, OWN_PALETTE);
    }

    #[test]
    fn latte_is_light_and_nord_is_dark() {
        let dir = TestDir::new("light");
        let dialog = ThemesDialog::new(&dir.store(), None);

        assert_eq!(
            row_of(&dialog, "catppuccin-latte").theme,
            Some(CATPPUCCIN_LATTE)
        );
        assert_eq!(row_of(&dialog, "nord").theme, Some(NORD));
        assert!(row_of(&dialog, "catppuccin-latte").light);
        assert!(!row_of(&dialog, "nord").light);
    }

    #[test]
    fn failing_pairs_are_those_below_their_floor() {
        let dir = TestDir::new("failing");
        let store = dir.store();
        let mut low = UiTheme::CONTRAST;
        low.text = low.ground;
        let id = store.save("Low", &low, None, None).expect("save");
        let dialog = ThemesDialog::new(&store, None);

        let row = row_of(&dialog, &id);
        let theme = row.theme.expect("the theme loads");
        let expected: Vec<PairResult> = contrast::check(&theme)
            .into_iter()
            .filter(|pair| !pair.passes)
            .collect();
        assert_eq!(row.failing, expected);
        assert!(!row.failing.is_empty(), "text on the ground fails");
        assert!(
            row.failing
                .iter()
                .any(|pair| pair.fg == "text" && pair.bg == "ground"),
            "{:?}",
            row.failing
        );
        assert!(
            row.failing
                .iter()
                .all(|pair| pair.ratio < pair.minimum && !pair.passes)
        );
    }
}
