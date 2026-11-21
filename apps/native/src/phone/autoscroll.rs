//! How far a scrolling page moves in one frame while a dragged row is held
//! near its visible top or bottom edge: nothing outside the edge zones,
//! faster the nearer the finger is to the edge.

/// The edge zones' depth and the speeds a page scrolls at inside them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AutoScroll {
    /// How far in from each visible edge its zone reaches, in points.
    pub(crate) zone: f32,
    /// The step at a zone's inner edge, in points per frame.
    pub(crate) min_speed: f32,
    /// The step at the visible edge and past it, in points per frame.
    pub(crate) max_speed: f32,
}

/// The phone's Settings lists: 48 pt zones, 2 to 12 pt per frame.
pub(crate) const SETTINGS: AutoScroll = AutoScroll {
    zone: 48.0,
    min_speed: 2.0,
    max_speed: 12.0,
};

/// The page's visible span in window points: its top and bottom edges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Visible {
    /// The window y of the visible top edge.
    pub(crate) top: f32,
    /// The window y of the visible bottom edge.
    pub(crate) bottom: f32,
}

/// The step a page scrolls by this frame with the finger at window `y`:
/// negative toward the top, positive toward the bottom, 0 outside both
/// zones. The speed runs from `min_speed` at a zone's inner edge to
/// `max_speed` at the visible edge, and stays there past it. When the zones
/// overlap the nearer edge wins, the top on a tie.
pub(crate) fn autoscroll_step(y: f32, visible: Visible, tuning: AutoScroll) -> f32 {
    if tuning.zone <= 0.0 {
        return 0.0;
    }
    let (to_top, to_bottom) = (y - visible.top, visible.bottom - y);
    let (gap, sign) = if to_top <= to_bottom {
        (to_top, -1.0)
    } else {
        (to_bottom, 1.0)
    };
    if gap > tuning.zone {
        return 0.0;
    }
    let depth = ((tuning.zone - gap) / tuning.zone).clamp(0.0, 1.0);
    sign * (tuning.min_speed + (tuning.max_speed - tuning.min_speed) * depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page visible from y 100 to y 900.
    fn page() -> Visible {
        Visible {
            top: 100.0,
            bottom: 900.0,
        }
    }

    /// The step at `y` on [`page`] with the Settings tuning.
    fn step(y: f32) -> f32 {
        autoscroll_step(y, page(), SETTINGS)
    }

    #[test]
    fn outside_the_zones_the_page_stays() {
        assert_eq!(step(500.0), 0.0, "the middle");
        assert_eq!(step(148.5), 0.0, "just past the top zone");
        assert_eq!(step(851.5), 0.0, "just before the bottom zone");
    }

    #[test]
    fn at_a_zones_inner_edge_it_creeps() {
        assert_eq!(step(148.0), -2.0, "the top zone's inner edge");
        assert_eq!(step(852.0), 2.0, "the bottom zone's inner edge");
    }

    #[test]
    fn at_the_visible_edge_it_runs_fastest() {
        assert_eq!(step(100.0), -12.0, "the top edge");
        assert_eq!(step(900.0), 12.0, "the bottom edge");
    }

    #[test]
    fn halfway_into_a_zone_it_runs_halfway() {
        assert_eq!(step(124.0), -7.0);
        assert_eq!(step(876.0), 7.0);
    }

    #[test]
    fn past_the_visible_edge_it_stays_at_the_fastest() {
        assert_eq!(step(40.0), -12.0, "above the top");
        assert_eq!(step(1000.0), 12.0, "below the bottom");
    }

    #[test]
    fn overlapping_zones_follow_the_nearer_edge() {
        let short = Visible {
            top: 100.0,
            bottom: 160.0,
        };
        assert!(
            autoscroll_step(110.0, short, SETTINGS) < 0.0,
            "nearer the top"
        );
        assert!(
            autoscroll_step(150.0, short, SETTINGS) > 0.0,
            "nearer the bottom"
        );
        assert_eq!(
            autoscroll_step(110.0, short, SETTINGS),
            -(2.0 + 10.0 * (38.0 / 48.0)),
            "10 pt from the top edge"
        );
        assert!(
            autoscroll_step(130.0, short, SETTINGS) < 0.0,
            "a tie goes to the top"
        );
    }
}
