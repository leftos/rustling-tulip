//! The keyboard stops Settings → Appearance and the appearance editor
//! share: which stops Tab walks and in what order, where a swatch line's
//! cursor stands, and where the font family list's highlight stands. The
//! view ([`crate::appearance_view`]) draws them and forwards keys.

use gpui::SharedString;

/// A colour row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ColorRow {
    /// The accent, which sets the pane frame with it.
    Accent,
    Background,
}

impl ColorRow {
    /// The rows in reading order.
    pub(crate) const ALL: [Self; 2] = [Self::Accent, Self::Background];

    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Accent => "accent",
            Self::Background => "background",
        }
    }
}

/// One of a colour row's swatch lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwatchLine {
    Presets,
    /// The recent custom colours, shown only while there are some.
    Recent,
}

/// A row with a reset link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResetRow {
    Color(ColorRow),
    Size,
    Bold,
}

/// A control of the Appearance rows that Tab reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppearanceStop {
    /// A whole swatch line is one stop; Left and Right move along it.
    Swatches(ColorRow, SwatchLine),
    Hex(ColorRow),
    Apply(ColorRow),
    Reset(ResetRow),
    FamilyFilter,
    /// The whole family list is one stop; Up and Down move its highlight.
    FamilyList,
    /// The app level's UI font filter.
    UiFamilyFilter,
    /// The app level's UI font list, one stop like the family list.
    UiFamilyList,
    SizeDown,
    SizeUp,
    Bold,
    /// The session, repo or workspace editor's Close.
    Close,
}

impl AppearanceStop {
    /// The stop's selector, as the focus probe reports it.
    pub(crate) const fn selector(self) -> &'static str {
        match self {
            Self::Swatches(ColorRow::Accent, SwatchLine::Presets) => "appearance-accent-presets",
            Self::Swatches(ColorRow::Accent, SwatchLine::Recent) => "appearance-accent-recents",
            Self::Swatches(ColorRow::Background, SwatchLine::Presets) => {
                "appearance-background-presets"
            }
            Self::Swatches(ColorRow::Background, SwatchLine::Recent) => {
                "appearance-background-recents"
            }
            Self::Hex(ColorRow::Accent) => "appearance-accent-hex",
            Self::Hex(ColorRow::Background) => "appearance-background-hex",
            Self::Apply(ColorRow::Accent) => "appearance-accent-apply",
            Self::Apply(ColorRow::Background) => "appearance-background-apply",
            Self::Reset(ResetRow::Color(ColorRow::Accent)) => "appearance-accent-reset",
            Self::Reset(ResetRow::Color(ColorRow::Background)) => "appearance-background-reset",
            Self::Reset(ResetRow::Size) => "appearance-size-reset",
            Self::Reset(ResetRow::Bold) => "appearance-bold-reset",
            Self::FamilyFilter => "appearance-family-filter",
            Self::FamilyList => "appearance-family-list",
            Self::UiFamilyFilter => "appearance-ui-family-filter",
            Self::UiFamilyList => "appearance-ui-family-list",
            Self::SizeDown => "appearance-size-down",
            Self::SizeUp => "appearance-size-up",
            Self::Bold => "appearance-bold",
            Self::Close => "appearance-footer-close",
        }
    }

    /// Whether the stop is a text field, which takes typed keys.
    pub(crate) const fn is_field(self) -> bool {
        matches!(
            self,
            Self::Hex(_) | Self::FamilyFilter | Self::UiFamilyFilter
        )
    }

    /// The font list a list stop is.
    pub(crate) const fn font_list(self) -> Option<FontList> {
        match self {
            Self::FamilyList => Some(FontList::Terminal),
            Self::UiFamilyList => Some(FontList::Ui),
            _ => None,
        }
    }

    /// Whether the stop can be disabled, and so left out of the ring.
    const fn can_disable(self) -> bool {
        matches!(self, Self::Apply(_) | Self::Reset(_))
    }
}

/// One of the font family lists: the terminal's, at every level, or the
/// interface's, at the app level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FontList {
    Terminal,
    Ui,
}

impl FontList {
    /// The prefix of the list's row and hint selectors.
    pub(crate) const fn prefix(self) -> &'static str {
        match self {
            Self::Terminal => "appearance-family",
            Self::Ui => "appearance-ui-family",
        }
    }

    /// The list's stop.
    pub(crate) const fn stop(self) -> AppearanceStop {
        match self {
            Self::Terminal => AppearanceStop::FamilyList,
            Self::Ui => AppearanceStop::UiFamilyList,
        }
    }

    /// The list's filter stop.
    pub(crate) const fn filter_stop(self) -> AppearanceStop {
        match self {
            Self::Terminal => AppearanceStop::FamilyFilter,
            Self::Ui => AppearanceStop::UiFamilyFilter,
        }
    }
}

/// The stops Tab walks, in reading order: per colour row its preset line,
/// its recent line (`has_recent`), its hex field, Apply and its reset link;
/// the UI font filter and list (`ui_font`); then the font filter and list,
/// size − and + and the size reset, the bold checkbox and its reset, and
/// Close (`close`). Apply and the reset links are left out while `enabled`
/// says they do nothing.
pub(crate) fn stops(
    has_recent: bool,
    ui_font: bool,
    close: bool,
    enabled: impl Fn(AppearanceStop) -> bool,
) -> Vec<AppearanceStop> {
    let mut ring = Vec::new();
    for row in ColorRow::ALL {
        ring.push(AppearanceStop::Swatches(row, SwatchLine::Presets));
        if has_recent {
            ring.push(AppearanceStop::Swatches(row, SwatchLine::Recent));
        }
        ring.extend([
            AppearanceStop::Hex(row),
            AppearanceStop::Apply(row),
            AppearanceStop::Reset(ResetRow::Color(row)),
        ]);
    }
    if ui_font {
        ring.extend([AppearanceStop::UiFamilyFilter, AppearanceStop::UiFamilyList]);
    }
    ring.extend([
        AppearanceStop::FamilyFilter,
        AppearanceStop::FamilyList,
        AppearanceStop::SizeDown,
        AppearanceStop::SizeUp,
        AppearanceStop::Reset(ResetRow::Size),
        AppearanceStop::Bold,
        AppearanceStop::Reset(ResetRow::Bold),
    ]);
    if close {
        ring.push(AppearanceStop::Close);
    }
    ring.retain(|stop| !stop.can_disable() || enabled(*stop));
    ring
}

/// The stop Tab (or Shift-Tab, `back`) moves to from `at` along `ring`,
/// wrapping at both ends; from no stop, or one the ring no longer holds,
/// the first stop (the last with `back`).
pub(crate) fn step(
    ring: &[AppearanceStop],
    at: Option<AppearanceStop>,
    back: bool,
) -> Option<AppearanceStop> {
    let last = ring.len().checked_sub(1)?;
    let next = match at.and_then(|at| ring.iter().position(|stop| *stop == at)) {
        None | Some(0) if back => last,
        None => 0,
        Some(index) if back => index - 1,
        Some(index) if index == last => 0,
        Some(index) => index + 1,
    };
    ring.get(next).copied()
}

/// The stop the keyboard moves to when `stop` left `ring`: the first stop
/// after its place in `order` (every stop, enabled or not) that `ring`
/// still holds, searching on round to the start.
pub(crate) fn after_lost<T: Copy + PartialEq>(order: &[T], ring: &[T], stop: T) -> Option<T> {
    let at = order.iter().position(|s| *s == stop)?;
    let (before, after) = order.split_at(at);
    after
        .iter()
        .skip(1)
        .chain(before)
        .find(|s| ring.contains(s))
        .copied()
}

/// Where a swatch line's cursor stands among `colors`: on `cursor` while
/// the line holds it, else on the `chosen` colour, else on the first.
pub(crate) fn swatch_cursor(
    colors: &[u32],
    cursor: Option<u32>,
    chosen: Option<u32>,
) -> Option<usize> {
    if colors.is_empty() {
        return None;
    }
    let find = |color: Option<u32>| color.and_then(|color| colors.iter().position(|c| *c == color));
    Some(find(cursor).or_else(|| find(chosen)).unwrap_or(0))
}

/// The colour the cursor moves to, one swatch right (`forward`) or left,
/// staying put at the line's ends.
pub(crate) fn step_swatch(
    colors: &[u32],
    cursor: Option<u32>,
    chosen: Option<u32>,
    forward: bool,
) -> Option<u32> {
    let at = swatch_cursor(colors, cursor, chosen)?;
    let next = if forward {
        (at + 1).min(colors.len() - 1)
    } else {
        at.saturating_sub(1)
    };
    colors.get(next).copied()
}

/// One line of the font family list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FamilyEntry {
    /// The row that clears the level's family.
    Reset,
    Header(&'static str),
    /// A family, `saved` when it is the level's value from neither list.
    Family {
        name: SharedString,
        saved: bool,
    },
}

/// What a family list row picks: the reset row or a family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FamilyPick {
    Reset,
    Family(SharedString),
}

impl FamilyEntry {
    /// What the row picks; a header picks nothing.
    pub(crate) fn pick(&self) -> Option<FamilyPick> {
        match self {
            Self::Reset => Some(FamilyPick::Reset),
            Self::Header(_) => None,
            Self::Family { name, .. } => Some(FamilyPick::Family(name.clone())),
        }
    }
}

/// The family list over the `bundled` and `system` families: the reset
/// row, then each group with a match for `filter` (case-insensitive) under
/// its header.
pub(crate) fn family_entries(
    bundled: &[&'static str],
    system: &[SharedString],
    own: Option<&str>,
    filter: &str,
) -> Vec<FamilyEntry> {
    let needle = filter.trim().to_lowercase();
    let matches = |name: &str| name.to_lowercase().contains(&needle);
    let bundled: Vec<SharedString> = bundled
        .iter()
        .map(|name| SharedString::from(*name))
        .collect();
    let saved: Vec<SharedString> = own
        .filter(|name| {
            !bundled
                .iter()
                .chain(system)
                .any(|family| same_family(Some(name), family))
        })
        .map(|name| SharedString::from(name.to_owned()))
        .into_iter()
        .collect();
    let mut entries = vec![FamilyEntry::Reset];
    for (header, names, is_saved) in [
        ("Current", &saved, true),
        ("Bundled", &bundled, false),
        ("System", &system.to_vec(), false),
    ] {
        let shown: Vec<&SharedString> = names.iter().filter(|name| matches(name)).collect();
        if shown.is_empty() {
            continue;
        }
        entries.push(FamilyEntry::Header(header));
        entries.extend(shown.into_iter().map(|name| FamilyEntry::Family {
            name: name.clone(),
            saved: is_saved,
        }));
    }
    entries
}

/// Whether the `chosen` family is the listed `name`: families match
/// without regard to case, as they resolve.
#[must_use]
pub(crate) fn same_family(chosen: Option<&str>, name: &str) -> bool {
    chosen.is_some_and(|chosen| chosen.eq_ignore_ascii_case(name))
}

/// Where the family list's highlight stands among `entries`: on
/// `highlight` while it is listed, else on the first row; with no
/// highlight yet, on the `chosen` family (the reset row when the level
/// sets none) while it is listed, else on the first row.
pub(crate) fn family_highlight(
    entries: &[FamilyEntry],
    highlight: Option<&FamilyPick>,
    chosen: Option<&str>,
) -> Option<usize> {
    let find = |pick: &FamilyPick| {
        entries.iter().position(|entry| match (entry.pick(), pick) {
            (Some(FamilyPick::Reset), FamilyPick::Reset) => true,
            (Some(FamilyPick::Family(listed)), FamilyPick::Family(name)) => {
                same_family(Some(name), &listed)
            }
            _ => false,
        })
    };
    let first = entries.iter().position(|entry| entry.pick().is_some());
    let wanted = highlight.cloned().unwrap_or_else(|| {
        chosen.map_or(FamilyPick::Reset, |name| {
            FamilyPick::Family(SharedString::from(name.to_owned()))
        })
    });
    find(&wanted).or(first)
}

/// What the highlight moves to, one row down (`down`) or up past any
/// header, staying put at the list's ends.
pub(crate) fn step_family(
    entries: &[FamilyEntry],
    highlight: Option<&FamilyPick>,
    chosen: Option<&str>,
    down: bool,
) -> Option<FamilyPick> {
    let at = family_highlight(entries, highlight, chosen)?;
    let next = if down {
        entries.iter().skip(at + 1).find_map(FamilyEntry::pick)
    } else {
        entries.iter().take(at).rev().find_map(FamilyEntry::pick)
    };
    next.or_else(|| entries.get(at).and_then(FamilyEntry::pick))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fonts::{BUNDLED_FAMILIES, BUNDLED_UI_FAMILIES};

    use AppearanceStop::{
        Apply, Bold, Close, FamilyFilter, FamilyList, Hex, Reset, SizeDown, SizeUp, Swatches,
    };
    use ColorRow::{Accent, Background};
    use SwatchLine::{Presets, Recent};

    fn names(entries: &[FamilyEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                FamilyEntry::Reset => "reset".to_owned(),
                FamilyEntry::Header(header) => format!("[{header}]"),
                FamilyEntry::Family { name, saved } => {
                    format!("{name}{}", if *saved { " (saved)" } else { "" })
                }
            })
            .collect()
    }

    fn family(name: &str) -> FamilyPick {
        FamilyPick::Family(SharedString::from(name.to_owned()))
    }

    /// The reset row, `[Bundled]` with Geist Mono and Fira Code, then
    /// `[System]` with Arial.
    fn short_list() -> Vec<FamilyEntry> {
        let entry = |name: &str| FamilyEntry::Family {
            name: SharedString::from(name.to_owned()),
            saved: false,
        };
        vec![
            FamilyEntry::Reset,
            FamilyEntry::Header("Bundled"),
            entry("Geist Mono"),
            entry("Fira Code"),
            FamilyEntry::Header("System"),
            entry("Arial"),
        ]
    }

    #[test]
    fn the_family_list_groups_and_filters() {
        let system = [SharedString::from("Arial"), SharedString::from("Consolas")];
        assert_eq!(
            names(&family_entries(&BUNDLED_FAMILIES, &system, None, "")),
            [
                "reset",
                "[Bundled]",
                "Geist Mono",
                "Fira Code",
                "JetBrains Mono",
                "Cascadia Code",
                "[System]",
                "Arial",
                "Consolas",
            ]
        );
        assert_eq!(
            names(&family_entries(
                &BUNDLED_FAMILIES,
                &system,
                Some("Lost Mono"),
                "MONO"
            )),
            [
                "reset",
                "[Current]",
                "Lost Mono (saved)",
                "[Bundled]",
                "Geist Mono",
                "JetBrains Mono",
            ],
            "a case-insensitive substring; a saved family neither list has"
        );
        assert_eq!(
            names(&family_entries(
                &BUNDLED_FAMILIES,
                &system,
                Some("Arial"),
                "zzz"
            )),
            ["reset"],
            "a listed family is not saved; nothing matches"
        );
    }

    #[test]
    fn a_chosen_family_matches_without_regard_to_case() {
        let entries = family_entries(&BUNDLED_UI_FAMILIES, &[], Some("inter"), "");
        assert_eq!(
            names(&entries),
            ["reset", "[Bundled]", "Geist", "Inter", "Schibsted Grotesk"],
            "a stored `inter` is the bundled Inter, not a saved row"
        );
        let at = family_highlight(&entries, None, Some("inter"));
        assert_eq!(
            at.and_then(|at| entries[at].pick()),
            Some(family("Inter")),
            "the highlight lands on Inter"
        );
        assert!(same_family(Some("inter"), "Inter"), "Inter is ticked");
        assert!(!same_family(Some("inter"), "Geist"));
        assert!(!same_family(None, "Inter"));
    }

    #[test]
    fn stops_follow_the_reading_order_with_everything_enabled() {
        let tail = [
            FamilyFilter,
            FamilyList,
            SizeDown,
            SizeUp,
            Reset(ResetRow::Size),
            Bold,
            Reset(ResetRow::Bold),
        ];
        let mut without: Vec<AppearanceStop> = Vec::new();
        for row in ColorRow::ALL {
            without.extend([
                Swatches(row, Presets),
                Hex(row),
                Apply(row),
                Reset(ResetRow::Color(row)),
            ]);
        }
        without.extend(tail);
        assert_eq!(
            stops(false, false, false, |_| true),
            without,
            "no recent line"
        );

        let with = stops(true, false, true, |_| true);
        assert_eq!(
            with[..5],
            [
                Swatches(Accent, Presets),
                Swatches(Accent, Recent),
                Hex(Accent),
                Apply(Accent),
                Reset(ResetRow::Color(Accent)),
            ],
            "the recent line follows the presets"
        );
        assert_eq!(
            with[5..7],
            [Swatches(Background, Presets), Swatches(Background, Recent)]
        );
        assert_eq!(with.last(), Some(&Close), "Close comes last when asked for");
        assert_eq!(with.len(), without.len() + 3);
    }

    #[test]
    fn disabled_apply_and_reset_links_are_skipped() {
        let ring = stops(false, false, false, |stop| {
            stop != Apply(Background) && !matches!(stop, Reset(_))
        });
        assert_eq!(
            ring,
            [
                Swatches(Accent, Presets),
                Hex(Accent),
                Apply(Accent),
                Swatches(Background, Presets),
                Hex(Background),
                FamilyFilter,
                FamilyList,
                SizeDown,
                SizeUp,
                Bold,
            ]
        );
        let all_off = stops(false, false, false, |_| false);
        assert!(
            all_off.contains(&FamilyList) && all_off.contains(&Bold),
            "only Apply and the reset links can be disabled"
        );
    }

    #[test]
    fn step_wraps_at_both_ends() {
        let ring = [Swatches(Accent, Presets), Hex(Accent), Bold, Close];
        assert_eq!(
            step(&ring, None, false),
            Some(Swatches(Accent, Presets)),
            "Tab from no stop: the first"
        );
        assert_eq!(
            step(&ring, None, true),
            Some(Close),
            "Shift-Tab from no stop: the last"
        );
        assert_eq!(step(&ring, Some(Hex(Accent)), false), Some(Bold));
        assert_eq!(
            step(&ring, Some(Hex(Accent)), true),
            Some(Swatches(Accent, Presets))
        );
        assert_eq!(
            step(&ring, Some(Close), false),
            Some(Swatches(Accent, Presets)),
            "forward from the last wraps to the first"
        );
        assert_eq!(
            step(&ring, Some(Swatches(Accent, Presets)), true),
            Some(Close),
            "back from the first wraps to the last"
        );
        assert_eq!(
            step(&ring, Some(SizeUp), false),
            Some(Swatches(Accent, Presets)),
            "a stop the ring lost counts as none"
        );
    }

    #[test]
    fn after_lost_moves_to_the_next_stop_the_ring_holds() {
        let order = stops(false, false, true, |_| true);
        let ring = stops(false, false, true, |stop| {
            !matches!(stop, Apply(Accent) | Reset(ResetRow::Color(Accent)))
        });
        assert_eq!(
            after_lost(&order, &ring, Apply(Accent)),
            Some(Swatches(Background, Presets)),
            "past the disabled reset link to the next enabled stop"
        );
        let ring_without_bold_reset =
            stops(false, false, false, |stop| stop != Reset(ResetRow::Bold));
        assert_eq!(
            after_lost(
                &stops(false, false, false, |_| true),
                &ring_without_bold_reset,
                Reset(ResetRow::Bold)
            ),
            Some(Swatches(Accent, Presets)),
            "the last stop lost: round to the first"
        );
        assert_eq!(
            after_lost(&order, &[], Apply(Accent)),
            None,
            "a ring that lost every stop"
        );
        assert_eq!(
            after_lost(&[Bold], &[Bold], Close),
            None,
            "a stop the order does not hold"
        );
    }

    #[test]
    fn step_on_an_empty_ring_goes_nowhere() {
        assert_eq!(step(&[], None, false), None);
        assert_eq!(step(&[], Some(Close), true), None);
    }

    #[test]
    fn a_swatch_line_lands_on_the_chosen_swatch_else_the_first() {
        let colors = [0x10, 0x20, 0x30];
        assert_eq!(swatch_cursor(&colors, None, Some(0x20)), Some(1));
        assert_eq!(
            swatch_cursor(&colors, None, Some(0x99)),
            Some(0),
            "a custom colour"
        );
        assert_eq!(swatch_cursor(&colors, None, None), Some(0));
        assert_eq!(
            swatch_cursor(&colors, Some(0x30), Some(0x20)),
            Some(2),
            "the cursor wins"
        );
        assert_eq!(
            swatch_cursor(&colors, Some(0x99), Some(0x20)),
            Some(1),
            "a gone cursor"
        );
        assert_eq!(swatch_cursor(&[], None, None), None);
    }

    #[test]
    fn the_swatch_cursor_stops_at_the_ends() {
        let colors = [0x10, 0x20, 0x30];
        assert_eq!(step_swatch(&colors, None, Some(0x20), true), Some(0x30));
        assert_eq!(step_swatch(&colors, None, Some(0x20), false), Some(0x10));
        assert_eq!(
            step_swatch(&colors, Some(0x10), None, false),
            Some(0x10),
            "Left at the first"
        );
        assert_eq!(
            step_swatch(&colors, Some(0x30), None, true),
            Some(0x30),
            "Right at the last"
        );
        assert_eq!(step_swatch(&[], None, None, true), None);
    }

    #[test]
    fn the_family_highlight_lands_on_the_chosen_family() {
        let list = short_list();
        assert_eq!(family_highlight(&list, None, Some("Fira Code")), Some(3));
        assert_eq!(
            family_highlight(&list, None, None),
            Some(0),
            "the reset row when none is set"
        );
        assert_eq!(
            family_highlight(&list, None, Some("Lost")),
            Some(0),
            "unlisted: the first row"
        );
        assert_eq!(
            family_highlight(&list, Some(&family("Arial")), Some("Fira Code")),
            Some(5),
            "a moved highlight wins"
        );
    }

    #[test]
    fn the_family_highlight_skips_headers_and_stops_at_the_ends() {
        let list = short_list();
        assert_eq!(
            step_family(&list, None, None, true),
            Some(family("Geist Mono")),
            "past [Bundled]"
        );
        assert_eq!(
            step_family(&list, Some(&family("Fira Code")), None, true),
            Some(family("Arial")),
            "past [System]"
        );
        assert_eq!(
            step_family(&list, Some(&family("Geist Mono")), None, false),
            Some(FamilyPick::Reset)
        );
        assert_eq!(
            step_family(&list, Some(&family("Arial")), None, true),
            Some(family("Arial")),
            "Down at the last row"
        );
        assert_eq!(
            step_family(&list, Some(&FamilyPick::Reset), None, false),
            Some(FamilyPick::Reset),
            "Up at the first row"
        );
    }

    #[test]
    fn the_family_highlight_survives_a_narrowing_filter() {
        let system = [SharedString::from("Arial")];
        let highlight = family("Fira Code");
        let narrowed = family_entries(&BUNDLED_FAMILIES, &system, None, "fira");
        let at = family_highlight(&narrowed, Some(&highlight), None);
        assert_eq!(
            at.and_then(|at| narrowed[at].pick()),
            Some(highlight.clone()),
            "still listed: the same family"
        );
        let gone = family_entries(&BUNDLED_FAMILIES, &system, None, "arial");
        assert_eq!(
            family_highlight(&gone, Some(&highlight), Some("Arial")),
            Some(0),
            "filtered out: the first row"
        );
    }
}
