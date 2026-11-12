//! When a pane header is too narrow to hold its buttons and still leave the
//! session's name readable: the buttons from Pop out onwards, Close aside,
//! then move into the header's ⋯ menu, all of them together.

/// The least width the session name keeps before any button moves: about
/// a dozen characters at the title's size, enough to tell sessions apart.
pub(crate) const TITLE_MIN: f32 = 120.0;

/// The header row's left padding, as `pane_header` draws it.
pub(crate) const PAD_LEFT: f32 = 12.0;

/// The header row's right padding, as `pane_header` draws it.
pub(crate) const PAD_RIGHT: f32 = 8.0;

/// The gap between two items of the header row.
pub(crate) const GAP: f32 = 8.0;

/// What a pane header has to fit in one row, in pixels.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HeaderFit {
    /// The header's whole width.
    pub(crate) width: f32,
    /// The status glyph's width; 0 for an empty pane, which draws none.
    pub(crate) glyph: f32,
    /// Each chip's natural width, in header order.
    pub(crate) chips: Vec<f32>,
    /// Each button of the cluster that moves (Pop out to the last prompt
    /// arrow), in header order; a segmented control counts as one item.
    pub(crate) cluster: Vec<f32>,
    /// Each item that stays drawn beside the cluster or the ⋯ button
    /// however narrow the header (a followed dispatch run's Stop), in
    /// header order.
    pub(crate) fixed: Vec<f32>,
    /// Close's width.
    pub(crate) close: f32,
}

/// The width of `items` laid out in a row with [`GAP`] between them.
fn row_width(items: &[f32]) -> f32 {
    let gaps = items.len().saturating_sub(1);
    #[expect(
        clippy::cast_precision_loss,
        reason = "a header holds a handful of items"
    )]
    let gaps = gaps as f32 * GAP;
    items.iter().sum::<f32>() + gaps
}

/// The width the row's spacer takes; it holds no text, but it is an item
/// of the row like any other, and the row puts a [`GAP`] on either side of
/// it.
const SPACER: f32 = 0.0;

/// The width a header needs to show everything inline with the title at
/// [`TITLE_MIN`]: padding, glyph, title, chips, the spacer, the fixed items,
/// the cluster and Close, a [`GAP`] between each two items.
pub(crate) fn inline_width(fit: &HeaderFit) -> f32 {
    let mut items = Vec::with_capacity(fit.chips.len() + fit.cluster.len() + fit.fixed.len() + 4);
    if fit.glyph > 0.0 {
        items.push(fit.glyph);
    }
    items.push(TITLE_MIN);
    items.extend(&fit.chips);
    items.push(SPACER);
    items.extend(&fit.fixed);
    items.extend(&fit.cluster);
    items.push(fit.close);
    PAD_LEFT + row_width(&items) + PAD_RIGHT
}

/// Whether the header moves its cluster into the ⋯ menu: its row cannot
/// hold the cluster inline with the title at [`TITLE_MIN`]. A header with
/// no cluster never collapses.
pub(crate) fn collapses(fit: &HeaderFit) -> bool {
    !fit.cluster.is_empty() && inline_width(fit) > fit.width
}

/// Whether the header's row holds every chip at its floor, `floor` or the
/// chip's own width when that is less, beside the parts that never shrink:
/// padding, glyph, the title shrunk to nothing, the spacer, the fixed
/// items, the cluster or, when the header [`collapses`], the ⋯ button
/// `overflow`, and Close, a [`GAP`] between each two items. When it does
/// not, the chips shrink past their floor, as far as nothing.
pub(crate) fn floors_fit(fit: &HeaderFit, floor: f32, overflow: f32) -> bool {
    let mut items = Vec::with_capacity(fit.chips.len() + fit.cluster.len() + fit.fixed.len() + 4);
    if fit.glyph > 0.0 {
        items.push(fit.glyph);
    }
    items.push(0.0);
    items.extend(fit.chips.iter().map(|chip| chip.min(floor)));
    items.push(SPACER);
    items.extend(&fit.fixed);
    if collapses(fit) {
        items.push(overflow);
    } else {
        items.extend(&fit.cluster);
    }
    items.push(fit.close);
    PAD_LEFT + row_width(&items) + PAD_RIGHT <= fit.width
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session pane: glyph, two chips, Pop out, Stop, the three-segment
    /// switch, split right, split down, move, two prompt arrows and Close.
    fn session(width: f32) -> HeaderFit {
        HeaderFit {
            width,
            glyph: 14.0,
            chips: vec![20.0, 90.0],
            cluster: vec![54.0, 36.0, 190.0, 26.0, 26.0, 26.0, 26.0, 26.0],
            fixed: Vec::new(),
            close: 26.0,
        }
    }

    #[test]
    fn a_run_header_counts_its_chip_status_and_stop() {
        // A pane that follows a dispatch run: no glyph, its chip and status
        // as chips, split right and down, its Stop fixed, Close.
        let bare = HeaderFit {
            width: 0.0,
            glyph: 0.0,
            chips: Vec::new(),
            cluster: vec![26.0, 26.0],
            fixed: Vec::new(),
            close: 26.0,
        };
        let mut run = HeaderFit {
            chips: vec![110.0, 50.0],
            fixed: vec![34.0],
            ..bare.clone()
        };
        let need = inline_width(&run);
        let added = 110.0 + 50.0 + 34.0 + 3.0 * GAP;
        assert!(
            (need - inline_width(&bare) - added).abs() < 0.01,
            "the chip, the status and Stop each take their width and a gap"
        );
        run.width = need;
        assert!(!collapses(&run));
        run.width = need - 1.0;
        assert!(collapses(&run), "a pixel short of them collapses");
    }

    #[test]
    fn a_wide_header_keeps_its_buttons() {
        assert!(!collapses(&session(1200.0)));
    }

    #[test]
    fn it_collapses_one_pixel_below_the_threshold() {
        let need = inline_width(&session(0.0));
        assert!(!collapses(&session(need)));
        assert!(collapses(&session(need - 1.0)));
    }

    #[test]
    fn the_threshold_counts_every_item_and_gap() {
        // Padding 12 and 8, the glyph, the title, two chips, the
        // zero-width spacer, eight buttons summing 410 and Close: fourteen
        // items, thirteen gaps.
        let need = 12.0 + 14.0 + 120.0 + 20.0 + 90.0 + 410.0 + 26.0 + 8.0 + 13.0 * 8.0;
        assert!((inline_width(&session(0.0)) - need).abs() < 0.01);
    }

    #[test]
    fn a_long_chip_set_collapses_earlier() {
        let short = inline_width(&session(0.0));
        let mut long = session(short);
        long.chips.push(160.0);
        assert!(!collapses(&session(short)));
        assert!(collapses(&long));
    }

    #[test]
    fn a_pane_without_prompt_arrows_collapses_later() {
        let with_arrows = session(0.0);
        let need = inline_width(&with_arrows);
        let mut without = session(need - 1.0);
        without.cluster.truncate(without.cluster.len() - 2);
        assert!(collapses(&session(need - 1.0)));
        assert!(!collapses(&without));
    }

    #[test]
    fn the_title_minimum_holds_however_short_the_name() {
        // The rule takes no title width: a two-letter name still reserves
        // the 120 px minimum, so the header collapses at the same width.
        let need = inline_width(&session(0.0));
        let without_title = need - TITLE_MIN - GAP;
        assert!(collapses(&session(without_title + 30.0)));
    }

    #[test]
    fn a_header_with_nothing_to_move_never_collapses() {
        let mut empty = session(10.0);
        empty.cluster.clear();
        assert!(!collapses(&empty));
    }
}
