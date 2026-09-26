//! The "copied" chip: the confirmation the client shows after it puts
//! something on the clipboard, wherever the copy came from.

use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt as _, AnyElement, ClipboardItem, Context, ElementId, SharedString, div,
    prelude::*, px,
};

use crate::{BORDER, FOOTER_HEIGHT, PANEL_BG, RootView, TEXT, UI_TEXT_SIZE, tooltip};

/// How long the chip takes to fade out.
pub const CHIP_LIFETIME: Duration = Duration::from_millis(1200);
/// What the chip reads.
pub const CHIP_LABEL: &str = "✓ copied";

/// What the chip's tooltip says for a copy of `chars` characters.
#[must_use]
pub fn chip_tooltip(chars: usize) -> String {
    format!("copied {chars} chars")
}

/// The client's last copy and whether its chip is still up. The chip reports
/// the copy's size, and every copy restarts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Copied {
    shown: Option<CopyChip>,
    /// The copies made so far, counted from one: it only grows, so a copy
    /// made after an earlier chip has gone still gets an animation id of its
    /// own.
    copies: u64,
}

/// One copy's chip: how many characters it reports, which copy it is (each
/// one restarts the fade), and when it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CopyChip {
    chars: usize,
    generation: u64,
    expires_at: Instant,
}

impl Copied {
    /// Shows the chip for a `chars`-character copy made at `now`, restarting
    /// the chip before it.
    pub fn show(&mut self, chars: usize, now: Instant) {
        self.copies += 1;
        self.shown = Some(CopyChip {
            chars,
            generation: self.copies,
            expires_at: now + CHIP_LIFETIME,
        });
    }

    /// Drops the chip when `now` has reached its end; returns whether it went.
    pub fn expire(&mut self, now: Instant) -> bool {
        if self.shown.is_some_and(|chip| chip.expires_at <= now) {
            self.shown = None;
            return true;
        }
        false
    }

    /// When the chip goes, for the timer that wakes the view.
    #[must_use]
    pub fn next_expiry(&self) -> Option<Instant> {
        self.shown.map(|chip| chip.expires_at)
    }

    /// The characters of the copy the chip reports while it is shown.
    #[must_use]
    pub fn chars(&self) -> Option<usize> {
        self.shown.map(|chip| chip.chars)
    }

    /// Which copy the chip reports, counted from the client's first copy.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.shown.map_or(0, |chip| chip.generation)
    }
}

impl RootView {
    /// The characters of the copy the "copied" chip reports while it is
    /// shown, and `None` while it is not.
    #[must_use]
    pub fn copied_chip(&self) -> Option<usize> {
        self.copy_chip.chars()
    }

    /// Which copy the chip reports; a spec tells one copy from two by it.
    #[must_use]
    pub fn copied_chip_generation(&self) -> u64 {
        self.copy_chip.generation()
    }

    /// Puts `text` on the clipboard and shows the chip for it. Every copy the
    /// client makes goes through here: the terminal's selection and the
    /// program's OSC 52 stores alike.
    pub(crate) fn copy_to_clipboard(&mut self, text: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        if !text.is_empty() {
            self.copy_chip.show(text.chars().count(), (self.now)());
            self.schedule_chip_expiry(cx);
        }
        cx.notify();
    }

    /// Arms a timer for the chip's end; replacing the timer restarts it.
    fn schedule_chip_expiry(&mut self, cx: &mut Context<Self>) {
        let now = (self.now)();
        self.chip_timer = self.copy_chip.next_expiry().map(|deadline| {
            let delay = deadline.saturating_duration_since(now);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                // Fails only when the view is gone, and the chip with it.
                this.update(cx, Self::expire_copied_chip).ok();
            })
        });
    }

    /// The timer fired: drop the chip whose time is up by the clock, which
    /// may lag the timer.
    fn expire_copied_chip(&mut self, cx: &mut Context<Self>) {
        if self.copy_chip.expire((self.now)()) {
            cx.notify();
        }
        self.schedule_chip_expiry(cx);
    }

    /// The chip, centred just above the footer bar while a copy is recent.
    pub(crate) fn chip_layer(&self) -> Option<AnyElement> {
        let chars = self.copy_chip.chars()?;
        let generation = self.copy_chip.generation();
        let chip = div()
            .id(ElementId::Name(SharedString::from(format!(
                "copied-chip-{generation}"
            ))))
            .debug_selector(|| CHIP_LABEL.to_owned())
            .px(px(10.0))
            .py(px(3.0))
            .bg(gpui::rgb(PANEL_BG))
            .border_1()
            .border_color(gpui::rgb(BORDER))
            .rounded(px(6.0))
            .text_size(px(UI_TEXT_SIZE))
            .text_color(gpui::rgb(TEXT))
            .child(CHIP_LABEL)
            .tooltip(tooltip(chip_tooltip(chars)));
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(FOOTER_HEIGHT + 8.0))
                .flex()
                .justify_center()
                .child(chip.with_animation(
                    ElementId::Name(SharedString::from(format!("copied-fade-{generation}"))),
                    Animation::new(CHIP_LIFETIME).with_easing(|delta| 1.0 - delta),
                    Styled::opacity,
                ))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{CHIP_LIFETIME, Copied, chip_tooltip};

    #[test]
    fn a_copy_shows_the_chip_until_its_lifetime_ends() {
        let t0 = Instant::now();
        let mut copied = Copied::default();
        copied.show(5, t0);
        assert_eq!(copied.chars(), Some(5));
        assert_eq!(copied.next_expiry(), Some(t0 + CHIP_LIFETIME));
        assert!(!copied.expire(t0 + Duration::from_millis(1199)));
        assert_eq!(copied.chars(), Some(5), "still shown a millisecond before");
        assert!(copied.expire(t0 + CHIP_LIFETIME));
        assert_eq!(copied.chars(), None);
        assert_eq!(copied.next_expiry(), None);
    }

    #[test]
    fn a_second_copy_restarts_the_chip() {
        let t0 = Instant::now();
        let mut copied = Copied::default();
        copied.show(5, t0);
        copied.show(2, t0 + Duration::from_millis(1000));
        assert_eq!(copied.chars(), Some(2));
        assert_eq!(copied.generation(), 2, "the second copy is its own fade");
        assert!(!copied.expire(t0 + Duration::from_millis(2199)));
        assert!(copied.expire(t0 + Duration::from_millis(2200)));
    }

    #[test]
    fn a_copy_after_the_chip_is_gone_gets_its_own_fade() {
        let t0 = Instant::now();
        let mut copied = Copied::default();
        copied.show(5, t0);
        assert_eq!(copied.generation(), 1);
        assert!(
            copied.expire(t0 + CHIP_LIFETIME),
            "the first chip has gone before the second copy"
        );
        copied.show(2, t0 + Duration::from_millis(1300));
        assert_eq!(copied.generation(), 2, "the counter only grows");
        assert!(!copied.expire(t0 + Duration::from_millis(2499)));
        assert!(copied.expire(t0 + Duration::from_millis(2500)));
    }

    #[test]
    fn an_unshown_chip_reports_nothing_and_expires_to_nothing() {
        let mut copied = Copied::default();
        assert_eq!(copied.chars(), None);
        assert_eq!(copied.generation(), 0);
        assert_eq!(copied.next_expiry(), None);
        assert!(!copied.expire(Instant::now()));
    }

    #[test]
    fn the_tooltip_names_the_copy() {
        assert_eq!(chip_tooltip(2), "copied 2 chars");
        assert_eq!(chip_tooltip(0), "copied 0 chars");
    }
}
