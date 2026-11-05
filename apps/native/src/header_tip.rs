//! The pane header's own tooltips. gpui opens a tooltip at the mouse, on
//! top of the header row it belongs to; a header tip opens below the row
//! instead, so it never covers the header's Terminal | Chat | Summary
//! labels. This is its state: which header element is hovered, with which
//! text, from which mouse x and since when, and whether its tip shows.

use std::time::{Duration, Instant};

/// How long a hover waits before its tip shows: gpui's own tooltip delay.
pub(crate) const SHOW_DELAY: Duration = Duration::from_millis(500);

/// One header element's tip: the element's id, its text and the mouse's x
/// in the window when the hover began.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HeaderTip {
    pub(crate) owner: String,
    pub(crate) text: String,
    pub(crate) x: f32,
}

/// The hovered header element and whether its tip shows. One at a time,
/// window-wide.
#[derive(Debug, Default)]
pub(crate) struct HeaderTips {
    hovered: Option<(HeaderTip, Instant)>,
    shown: bool,
}

impl HeaderTips {
    /// The mouse entered `owner`, which shows `text`, at window x `x`. It
    /// replaces any other element's hover and hides its tip.
    pub(crate) fn hover(&mut self, owner: &str, text: &str, x: f32, now: Instant) {
        self.hovered = Some((
            HeaderTip {
                owner: owner.to_owned(),
                text: text.to_owned(),
                x,
            },
            now,
        ));
        self.shown = false;
    }

    /// The mouse left `owner`. Leaving an element that is not the hovered
    /// one changes nothing. Returns whether a tip went.
    pub(crate) fn leave(&mut self, owner: &str) -> bool {
        if self.hovered().is_none_or(|tip| tip.owner != owner) {
            return false;
        }
        self.clear()
    }

    /// A mouse button went down anywhere in the window: no tip shows until
    /// the next hover. Returns whether a tip went.
    pub(crate) fn press(&mut self) -> bool {
        self.clear()
    }

    /// Shows the hovered element's tip once its delay has passed at `now`;
    /// returns whether a tip shows.
    pub(crate) fn due(&mut self, now: Instant) -> bool {
        if let Some((_, since)) = &self.hovered
            && now.saturating_duration_since(*since) >= SHOW_DELAY
        {
            self.shown = true;
        }
        self.shown
    }

    /// The tip that shows, if any.
    pub(crate) fn shown(&self) -> Option<&HeaderTip> {
        self.hovered
            .as_ref()
            .filter(|_| self.shown)
            .map(|(tip, _)| tip)
    }

    /// The hovered element's tip, shown or still waiting.
    pub(crate) fn hovered(&self) -> Option<&HeaderTip> {
        self.hovered.as_ref().map(|(tip, _)| tip)
    }

    /// Drops the hover, and its tip, when `drawn` says its element is no
    /// longer drawn. Returns whether a shown tip went.
    pub(crate) fn retain(&mut self, drawn: impl Fn(&str) -> bool) -> bool {
        if self.hovered().is_none_or(|tip| drawn(&tip.owner)) {
            return false;
        }
        self.clear()
    }

    fn clear(&mut self) -> bool {
        let was_shown = self.shown;
        self.hovered = None;
        self.shown = false;
        was_shown
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use super::*;

    const TITLE: &str = "pane-title-p1";
    const CHIP: &str = "pane-chip-p1-runtime";

    fn start() -> Instant {
        Instant::now()
    }

    fn after(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn a_hover_shows_only_after_the_delay() {
        let t0 = start();
        let mut tips = HeaderTips::default();
        tips.hover(TITLE, "repo · main", 40.0, t0);

        assert!(!tips.due(after(t0, 499)), "not before 500 ms");
        assert_eq!(tips.shown(), None);
        assert!(tips.due(after(t0, 500)), "at 500 ms");
        assert_eq!(
            tips.shown(),
            Some(&HeaderTip {
                owner: TITLE.to_owned(),
                text: "repo · main".to_owned(),
                x: 40.0,
            })
        );
    }

    #[test]
    fn leaving_before_the_delay_shows_nothing() {
        let t0 = start();
        let mut tips = HeaderTips::default();
        tips.hover(TITLE, "tip", 10.0, t0);
        assert!(!tips.leave(TITLE), "no tip was up");

        assert!(!tips.due(after(t0, 600)));
        assert_eq!(tips.shown(), None);
        assert_eq!(tips.hovered(), None);
    }

    #[test]
    fn leaving_hides_the_shown_tip() {
        let t0 = start();
        let mut tips = HeaderTips::default();
        tips.hover(TITLE, "tip", 10.0, t0);
        assert!(tips.due(after(t0, 600)));

        assert!(tips.leave(TITLE), "the tip went");
        assert_eq!(tips.shown(), None);
        assert!(!tips.due(after(t0, 1200)), "and stays gone");
    }

    #[test]
    fn a_press_hides_the_tip() {
        let t0 = start();
        let mut tips = HeaderTips::default();
        tips.hover(TITLE, "tip", 10.0, t0);
        assert!(tips.due(after(t0, 600)));

        assert!(tips.press(), "the tip went");
        assert_eq!(tips.shown(), None);
        assert!(
            !tips.due(after(t0, 1200)),
            "the element stays hovered, but no tip shows until the next hover"
        );

        tips.hover(TITLE, "tip", 10.0, after(t0, 1300));
        assert!(tips.due(after(t0, 1800)), "a new hover shows it again");
    }

    #[test]
    fn a_new_hover_replaces_the_old_one() {
        let t0 = start();
        let mut tips = HeaderTips::default();
        tips.hover(TITLE, "title tip", 10.0, t0);
        assert!(tips.due(after(t0, 600)));

        tips.hover(CHIP, "chip tip", 90.0, after(t0, 700));
        assert_eq!(tips.shown(), None, "the old tip went with the old hover");
        assert!(
            !tips.due(after(t0, 1100)),
            "the new one waits its own delay"
        );
        assert!(tips.due(after(t0, 1200)));
        let shown = tips.shown().expect("the chip's tip shows");
        assert_eq!(
            (shown.owner.as_str(), shown.text.as_str(), shown.x),
            (CHIP, "chip tip", 90.0)
        );
    }

    #[test]
    fn leaving_another_element_keeps_the_tip() {
        let t0 = start();
        let mut tips = HeaderTips::default();
        // gpui may report the chip's hover before the title's leave.
        tips.hover(TITLE, "title tip", 10.0, t0);
        tips.hover(CHIP, "chip tip", 90.0, after(t0, 100));
        assert!(!tips.leave(TITLE));
        assert!(tips.due(after(t0, 600)));

        assert!(
            !tips.leave(TITLE),
            "leaving the title again changes nothing"
        );
        assert_eq!(tips.shown().map(|tip| tip.owner.as_str()), Some(CHIP));
    }

    #[test]
    fn an_element_no_longer_drawn_drops_its_tip() {
        let t0 = start();
        let mut tips = HeaderTips::default();
        tips.hover(TITLE, "tip", 10.0, t0);
        assert!(tips.due(after(t0, 600)));

        assert!(!tips.retain(|owner| owner == TITLE), "still drawn: kept");
        assert!(tips.shown().is_some());
        assert!(tips.retain(|owner| owner == CHIP), "no longer drawn: gone");
        assert_eq!(tips.shown(), None);
        assert_eq!(tips.hovered(), None);

        tips.hover(CHIP, "tip", 10.0, after(t0, 700));
        assert!(
            !tips.retain(|_| false),
            "a waiting hover goes, no tip was up"
        );
        assert!(!tips.due(after(t0, 1300)), "and its tip never shows");
    }
}
