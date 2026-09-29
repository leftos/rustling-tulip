//! The glyph a session's status shows in its leaf, pane header and Needs
//! You row: which glyph a status maps to, the tinted SVG layers that draw
//! it, and the element that stacks them.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, SharedString, Transformation, div, percentage, prelude::*,
    px, svg,
};
use protocol::{SessionMode, SessionStatus};

use crate::assets::{
    STATUS_ARC_ICON, STATUS_CORE_ICON, STATUS_CROSS_ICON, STATUS_DIAMOND_ICON, STATUS_DOT_ICON,
    STATUS_QUESTION_ICON, STATUS_RING_ICON, STATUS_TRACK_ICON,
};
use crate::palette::{ASKING, DANGER, FAINT, ON_ASKING, WAITING, WORKING};

/// The `×` on the error dot.
const CROSS_COLOR: u32 = 0x00ff_ffff;
/// A stopped session's idle dot is drawn this opaque.
const STOPPED_OPACITY: f32 = 0.55;
/// The idle dot's diameter against the glyph's size.
const IDLE_DOT_SCALE: f32 = 0.5;
/// One turn of the working arc.
const SPIN_PERIOD: Duration = Duration::from_millis(1000);

/// Which glyph a status draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A faint ring, a spinning arc and a filled core.
    Working,
    /// A diamond holding a `?`.
    Asking,
    /// A hollow ring: an agent's turn ended while this client looked away.
    Waiting,
    /// A small dot.
    Idle,
    /// The hollow ring, faint.
    Spawning,
    /// The idle dot, dimmed.
    Stopped,
    /// A filled dot holding a `×`.
    Error,
}

impl Shape {
    /// The name the views' debug selectors end in.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Asking => "asking",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Spawning => "spawning",
            Self::Stopped => "stopped",
            Self::Error => "error",
        }
    }
}

/// A status glyph: its shape, its colour, how opaque it draws, and whether
/// its arc spins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    pub shape: Shape,
    pub color: u32,
    pub opacity: f32,
    pub spins: bool,
}

/// One tinted SVG in a glyph's stack, drawn `scale` times the glyph's size
/// and centred in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layer {
    pub icon: &'static str,
    pub color: u32,
    pub scale: f32,
    pub spins: bool,
}

/// The size a glyph draws at, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GlyphSize {
    /// Sidebar leaves, pane headers and Needs You rows.
    Leaf = 12,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no view draws a glyph inside a tab pill")
    )]
    Pill = 9,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no view draws a glyph in the footer")
    )]
    Footer = 11,
}

impl GlyphSize {
    #[must_use]
    pub fn px(self) -> f32 {
        f32::from(self as u8)
    }
}

/// The glyph a session with `status` in `mode` shows. An idle agent
/// session whose turn this client has not seen (`unseen`) shows the waiting
/// ring; an idle plain shell always shows the idle dot.
#[must_use]
pub fn glyph(status: SessionStatus, mode: SessionMode, unseen: bool) -> Glyph {
    let agent = mode != SessionMode::PlainShell;
    let (shape, color) = match status {
        SessionStatus::Working => (Shape::Working, WORKING),
        SessionStatus::AwaitingInput => (Shape::Asking, ASKING),
        SessionStatus::Idle if agent && unseen => (Shape::Waiting, WAITING),
        SessionStatus::Idle => (Shape::Idle, FAINT),
        SessionStatus::Spawning => (Shape::Spawning, FAINT),
        SessionStatus::Stopped => (Shape::Stopped, FAINT),
        SessionStatus::Error => (Shape::Error, DANGER),
    };
    Glyph {
        shape,
        color,
        opacity: if shape == Shape::Stopped {
            STOPPED_OPACITY
        } else {
            1.0
        },
        spins: shape == Shape::Working,
    }
}

impl Glyph {
    /// The SVGs the glyph stacks, bottom first.
    #[must_use]
    pub fn layers(&self) -> Vec<Layer> {
        let own = |icon| Layer {
            icon,
            color: self.color,
            scale: 1.0,
            spins: false,
        };
        match self.shape {
            Shape::Working => vec![
                own(STATUS_TRACK_ICON),
                Layer {
                    spins: self.spins,
                    ..own(STATUS_ARC_ICON)
                },
                own(STATUS_CORE_ICON),
            ],
            Shape::Asking => vec![
                own(STATUS_DIAMOND_ICON),
                Layer {
                    color: ON_ASKING,
                    ..own(STATUS_QUESTION_ICON)
                },
            ],
            Shape::Waiting | Shape::Spawning => vec![own(STATUS_RING_ICON)],
            Shape::Idle | Shape::Stopped => vec![Layer {
                scale: IDLE_DOT_SCALE,
                ..own(STATUS_DOT_ICON)
            }],
            Shape::Error => vec![
                own(STATUS_DOT_ICON),
                Layer {
                    color: CROSS_COLOR,
                    ..own(STATUS_CROSS_ICON)
                },
            ],
        }
    }
}

/// Draws `glyph` at `size`, tagged `<id>-<shape>` for the specs; `id` also
/// keys the arc's spin, so it is unique per drawn glyph.
pub fn glyph_view(glyph: Glyph, size: GlyphSize, id: &str) -> AnyElement {
    let size = size.px();
    let selector = format!("{id}-{}", glyph.shape.name());
    div()
        .flex_none()
        .relative()
        .size(px(size))
        .opacity(glyph.opacity)
        .debug_selector(move || selector)
        .children(
            glyph
                .layers()
                .into_iter()
                .enumerate()
                .map(|(i, layer)| layer_view(layer, size, format!("{id}-layer-{i}"))),
        )
        .into_any_element()
}

/// One layer, centred in the glyph's box; a spinning layer turns once a
/// `SPIN_PERIOD`, forever.
fn layer_view(layer: Layer, size: f32, id: String) -> AnyElement {
    let icon = svg()
        .path(layer.icon)
        .size(px(size * layer.scale))
        .text_color(gpui::rgb(layer.color));
    let icon = if layer.spins {
        icon.with_animation(
            SharedString::from(id),
            Animation::new(SPIN_PERIOD).repeat(),
            |icon, delta| icon.with_transformation(Transformation::rotate(percentage(delta))),
        )
        .into_any_element()
    } else {
        icon.into_any_element()
    };
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(icon)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUSES: [SessionStatus; 6] = [
        SessionStatus::Spawning,
        SessionStatus::Idle,
        SessionStatus::Working,
        SessionStatus::AwaitingInput,
        SessionStatus::Stopped,
        SessionStatus::Error,
    ];
    const MODES: [SessionMode; 3] = [
        SessionMode::Interactive,
        SessionMode::Headless,
        SessionMode::PlainShell,
    ];

    /// The shape a status shows for a seen agent, an unseen agent, and a
    /// plain shell seen or not.
    fn expected(status: SessionStatus) -> (Shape, Shape, Shape) {
        match status {
            SessionStatus::Spawning => (Shape::Spawning, Shape::Spawning, Shape::Spawning),
            SessionStatus::Idle => (Shape::Idle, Shape::Waiting, Shape::Idle),
            SessionStatus::Working => (Shape::Working, Shape::Working, Shape::Working),
            SessionStatus::AwaitingInput => (Shape::Asking, Shape::Asking, Shape::Asking),
            SessionStatus::Stopped => (Shape::Stopped, Shape::Stopped, Shape::Stopped),
            SessionStatus::Error => (Shape::Error, Shape::Error, Shape::Error),
        }
    }

    #[test]
    fn every_status_mode_and_unseen_maps_to_its_shape() {
        for status in STATUSES {
            let (seen_agent, unseen_agent, shell) = expected(status);
            for mode in MODES {
                for unseen in [false, true] {
                    let want = match (mode, unseen) {
                        (SessionMode::PlainShell, _) => shell,
                        (_, false) => seen_agent,
                        (_, true) => unseen_agent,
                    };
                    assert_eq!(
                        glyph(status, mode, unseen).shape,
                        want,
                        "{status:?} {mode:?} unseen={unseen}"
                    );
                }
            }
        }
    }

    #[test]
    fn each_shape_has_its_colour_opacity_and_spin() {
        for status in STATUSES {
            for mode in MODES {
                for unseen in [false, true] {
                    let g = glyph(status, mode, unseen);
                    let (color, opacity, spins) = match g.shape {
                        Shape::Working => (WORKING, 1.0, true),
                        Shape::Asking => (ASKING, 1.0, false),
                        Shape::Waiting => (WAITING, 1.0, false),
                        Shape::Idle | Shape::Spawning => (FAINT, 1.0, false),
                        Shape::Stopped => (FAINT, STOPPED_OPACITY, false),
                        Shape::Error => (DANGER, 1.0, false),
                    };
                    let want = Glyph {
                        shape: g.shape,
                        color,
                        opacity,
                        spins,
                    };
                    assert_eq!(g, want, "{status:?} {mode:?} unseen={unseen}");
                }
            }
        }
    }

    fn layers_of(status: SessionStatus, unseen: bool) -> Vec<(&'static str, u32, bool)> {
        glyph(status, SessionMode::Interactive, unseen)
            .layers()
            .into_iter()
            .map(|l| (l.icon, l.color, l.spins))
            .collect()
    }

    #[test]
    fn layers_stack_the_marks_in_their_own_colours() {
        assert_eq!(
            layers_of(SessionStatus::Working, false),
            [
                (STATUS_TRACK_ICON, WORKING, false),
                (STATUS_ARC_ICON, WORKING, true),
                (STATUS_CORE_ICON, WORKING, false),
            ],
            "only the arc spins"
        );
        assert_eq!(
            layers_of(SessionStatus::AwaitingInput, false),
            [
                (STATUS_DIAMOND_ICON, ASKING, false),
                (STATUS_QUESTION_ICON, ON_ASKING, false),
            ]
        );
        assert_eq!(
            layers_of(SessionStatus::Idle, true),
            [(STATUS_RING_ICON, WAITING, false)]
        );
        assert_eq!(
            layers_of(SessionStatus::Spawning, false),
            [(STATUS_RING_ICON, FAINT, false)]
        );
        assert_eq!(
            layers_of(SessionStatus::Error, false),
            [
                (STATUS_DOT_ICON, DANGER, false),
                (STATUS_CROSS_ICON, CROSS_COLOR, false),
            ]
        );
    }

    #[test]
    fn idle_and_stopped_draw_the_small_dot() {
        for status in [SessionStatus::Idle, SessionStatus::Stopped] {
            let layers = glyph(status, SessionMode::PlainShell, false).layers();
            let scales: Vec<(&str, f32)> = layers.iter().map(|l| (l.icon, l.scale)).collect();
            assert_eq!(scales, [(STATUS_DOT_ICON, IDLE_DOT_SCALE)], "{status:?}");
        }
        let error = glyph(SessionStatus::Error, SessionMode::Interactive, false).layers();
        assert!(error.iter().all(|l| (l.scale - 1.0).abs() < f32::EPSILON));
    }

    #[test]
    fn sizes_are_the_leaf_pill_and_footer_pixels() {
        assert!((GlyphSize::Leaf.px() - 12.0).abs() < f32::EPSILON);
        assert!((GlyphSize::Pill.px() - 9.0).abs() < f32::EPSILON);
        assert!((GlyphSize::Footer.px() - 11.0).abs() < f32::EPSILON);
    }
}
