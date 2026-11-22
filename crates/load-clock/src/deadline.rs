//! Deadlines on the load-adjusted clock, and the expiry they report.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::BACKSTOP_FACTOR;
use crate::clock::{LoadClock, Reading};

/// A budget of load-adjusted time, with a backstop of [`BACKSTOP_FACTOR`] times it in wall time.
pub struct Deadline {
    clock: Arc<LoadClock>,
    budget: Duration,
    start: Reading,
}

impl Deadline {
    /// Starts a deadline now.
    ///
    /// Args:
    ///     clock: The clock it counts on.
    ///     budget: The load-adjusted time it allows; its backstop is [`BACKSTOP_FACTOR`] times it in wall time.
    ///
    /// Returns:
    ///     The running deadline.
    #[must_use]
    pub fn start(clock: Arc<LoadClock>, budget: Duration) -> Deadline {
        let start = clock.read();
        Deadline {
            clock,
            budget,
            start,
        }
    }

    /// Some when the ceiling (adjusted >= budget) or the backstop (wall >= 5 x budget) has passed.
    ///
    /// Returns:
    ///     The expiry, the ceiling winning when both have passed; None while neither has.
    #[must_use]
    pub fn expired(&self) -> Option<Expiry> {
        let figures = self.figures();
        let kind = if figures.adjusted >= self.budget {
            ExpiryKind::Ceiling
        } else if figures.wall >= self.backstop() {
            ExpiryKind::Backstop
        } else {
            return None;
        };
        Some(Expiry { kind, figures })
    }

    /// How long a waiter may block before checking again.
    ///
    /// Args:
    ///     max: The longest slice the waiter wants.
    ///
    /// Returns:
    ///     The wall time left to the nearer of the two ends assuming a free machine, at most `max`, at least 1 ms.
    #[must_use]
    pub fn wait_slice(&self, max: Duration) -> Duration {
        let figures = self.figures();
        let to_ceiling = self.budget.saturating_sub(figures.adjusted);
        let to_backstop = self.backstop().saturating_sub(figures.wall);
        to_ceiling
            .min(to_backstop)
            .min(max)
            .max(Duration::from_millis(1))
    }

    /// The deadline's budget and the wall and adjusted time since it started.
    ///
    /// The adjusted time is capped at the wall time: a sample can credit a span that began before the deadline did,
    /// and the cap keeps a deadline from ending before an equal wall-clock timeout would.
    #[must_use]
    pub fn figures(&self) -> Figures {
        let now = self.clock.read();
        let wall = now.wall.saturating_sub(self.start.wall);
        Figures {
            budget: self.budget,
            wall,
            adjusted: now.adjusted.saturating_sub(self.start.adjusted).min(wall),
        }
    }

    fn backstop(&self) -> Duration {
        self.budget.saturating_mul(BACKSTOP_FACTOR)
    }
}

/// A budget and the time spent against it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Figures {
    /// The budget (for a stall, its limit).
    pub budget: Duration,
    /// Wall time spent (for a stall, since the last growth).
    pub wall: Duration,
    /// Load-adjusted time spent (for a stall, equal to the wall time).
    pub adjusted: Duration,
}

impl Figures {
    /// The machine's mean free share over the span: adjusted / wall, 1.0 when wall is zero.
    #[must_use]
    pub fn mean_free(&self) -> f64 {
        if self.wall.is_zero() {
            return 1.0;
        }
        self.adjusted.as_secs_f64() / self.wall.as_secs_f64()
    }
}

/// Which end fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryKind {
    /// The budget of load-adjusted time ran out.
    Ceiling,
    /// [`BACKSTOP_FACTOR`] times the budget passed in wall time first.
    Backstop,
    /// The child showed no log line and no CPU for the stall limit.
    Stall,
}

/// A deadline or stall watch that fired, with its figures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Expiry {
    /// Which end fired.
    pub kind: ExpiryKind,
    /// The budget and the time spent against it.
    pub figures: Figures,
}

impl fmt::Display for Expiry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Figures {
            budget,
            wall,
            adjusted,
        } = self.figures;
        let millis = budget < Duration::from_secs(1);
        let show = |d: Duration| span(d, millis);
        let pct = self.figures.mean_free() * 100.0;
        match self.kind {
            ExpiryKind::Ceiling => write!(
                f,
                "did not finish within {} of load-adjusted time (wall {}, machine free {pct:.0}% on average)",
                show(budget),
                show(wall),
            ),
            ExpiryKind::Backstop => {
                write!(
                    f,
                    "did not finish within {} of wall time, {BACKSTOP_FACTOR} x its {} ceiling ",
                    show(wall),
                    show(budget),
                )?;
                write!(
                    f,
                    "(load-adjusted {}, machine free {pct:.0}% on average)",
                    show(adjusted)
                )
            }
            ExpiryKind::Stall => write!(f, "stalled: no log line and no CPU for {}", show(budget)),
        }
    }
}

/// A duration as whole seconds (`12 s`), or as whole milliseconds (`340 ms`) when `millis`.
fn span(d: Duration, millis: bool) -> String {
    if millis {
        format!("{:.0} ms", d.as_secs_f64() * 1000.0)
    } else {
        format!("{:.0} s", d.as_secs_f64())
    }
}
