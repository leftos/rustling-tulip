//! Pace: when a usage window runs out at its rate, and whether it lasts to its reset.
//!
//! [`pace`] projects a window's use to its reset (the pace, the projected exhaustion, whether it lasts to the reset) and
//! covers the 5-hour window as well as the 7-day ones. A window starts one window length before its reset, and its rate
//! is its average use since that start. [`recent_pace`] runs the same window at its rise over the account's samples of
//! a span the caller gives instead, falling back to [`pace`] with under 10 minutes of samples. A reset
//! already past, or more than one window length away, is not computable here.
//! Time is a parameter: a reading is judged at the moment it was fetched, so a stale reading keeps the pace it had.
//!
//! [`fleet`] runs the counted accounts' pooled allowance, in 5-hour points, along a timeline over the next 7 days: an
//! account with a 5-hour limit brings its 5-hour window, held to what its 7-day window leaves, and an account with only
//! a 7-day limit brings what its 7-day window leaves. The pool is spent at the accounts' summed recent rate (each read
//! over the caller's `rate_span`), on one account at a time, in the order the auto-switch engine's own trigger and
//! ranking take them, and refilled at each reset, each account's 7-day window rising only with what is drawn on it and
//! refilled to a whole week at its own 7-day reset: whether some account is always usable, and, if not, every gap of 5
//! minutes or more in the 7 days, the first of which the verdict's fields describe.

use jiff::{RoundMode, SignedDuration, Timestamp, TimestampRound, Unit};

use crate::engine::{
    AccountUsage, ActiveHealth, ActiveReading, AutoswitchState, CandidateAccount, RankInputs,
    Trigger, TriggerDecision, WindowSource, binding_recovery, candidates, classify_trigger,
    in_cooldown, rank,
};
use crate::store::{AccountKind, AutoSwitchSettings};
use crate::usage::{UsageWindow, secs_from_timestamp};

/// The length of the account-wide 5-hour window.
pub const FIVE_HOUR: SignedDuration = SignedDuration::from_hours(5);

/// The length of a 7-day window, the account-wide one and each per-model one.
pub const SEVEN_DAY: SignedDuration = SignedDuration::from_hours(7 * 24);

/// How long a window must have run before its rate is trusted: in a younger window one request reads as a run-out.
pub const MIN_ELAPSED: SignedDuration = SignedDuration::from_mins(10);

/// A window's pace at the moment its reading was fetched.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Pace {
    /// The percentage an even rate would have used by now: the share of the window already elapsed, at most 100.
    pub expected_pct: f64,
    /// When the window reaches 100 % at its rate, rounded down to the minute, which can fall after the reset: its
    /// average rate since it began for [`pace`], its rise over the caller's span for [`recent_pace`]; the fetch time for a window
    /// already used up, and `None` for an unused one or, under [`recent_pace`], one that did not rise.
    pub runs_out_at: Option<Timestamp>,
    /// Whether the window stays at or under 100 % until its reset at that same rate.
    pub lasts_to_reset: bool,
}

/// The pace of a window `period` long that was `pct` percent used at `fetched_at` and resets at `resets_at`.
///
/// `None` when the reset is not after `fetched_at` or is more than `period` away, when the window had run less than
/// [`MIN_ELAPSED`] at `fetched_at`, or when `pct` is not a finite number.
#[must_use]
pub fn pace(
    pct: f64,
    resets_at: Timestamp,
    fetched_at: Timestamp,
    period: SignedDuration,
) -> Option<Pace> {
    if !pct.is_finite() {
        return None;
    }
    let remaining = resets_at.duration_since(fetched_at);
    if remaining <= SignedDuration::ZERO || remaining > period {
        return None;
    }
    let elapsed = period - remaining;
    if elapsed < MIN_ELAPSED {
        return None;
    }
    let elapsed_s = elapsed.as_secs_f64();
    let expected_pct = (elapsed_s / period.as_secs_f64() * 100.0).min(100.0);
    if pct >= 100.0 {
        return Some(Pace {
            expected_pct,
            runs_out_at: Some(fetched_at),
            lasts_to_reset: false,
        });
    }
    if pct <= 0.0 {
        return Some(Pace {
            expected_pct,
            runs_out_at: None,
            lasts_to_reset: true,
        });
    }
    let projected_pct = pct + pct * remaining.as_secs_f64() / elapsed_s;
    Some(Pace {
        expected_pct,
        runs_out_at: run_out(pct, elapsed_s, fetched_at),
        lasts_to_reset: projected_pct <= 100.0,
    })
}

/// When a window `pct` percent used `elapsed_s` seconds after it began reaches 100 % at that rate, from
/// `fetched_at`, rounded down to the minute; `None` when that time is past what a timestamp can hold.
fn run_out(pct: f64, elapsed_s: f64, fetched_at: Timestamp) -> Option<Timestamp> {
    // Multiplying before dividing keeps a whole-number run-out exact, so it is not floored a minute early.
    minute_after(fetched_at, elapsed_s * (100.0 - pct) / pct)
}

/// `secs` seconds after `fetched_at`, rounded down to the minute; `None` when that time is past what a timestamp can
/// hold.
fn minute_after(fetched_at: Timestamp, secs: f64) -> Option<Timestamp> {
    let to_empty = SignedDuration::try_from_secs_f64(secs).ok()?;
    let at = fetched_at.checked_add(to_empty).ok()?;
    floor_minute(at)
}

/// The pace of `account`'s `window`, read as `read` at `fetched_at`, at its recent rate rather than its average since it
/// began: its rises over `account`'s samples in `rates.history` fetched in the last `rates.span` before `fetched_at`, in
/// points per hour over the time from the oldest of them to `fetched_at` (a fall, or a pair of samples across a reset,
/// adds nothing). `rates.history` must already hold the reading at `fetched_at` itself, else the rise since the newest
/// earlier sample is missed and the rate understated.
///
/// The gates and `expected_pct` are [`pace`]'s: `None` where [`pace`] finds none. A window at 100 % or more, an unused
/// one at 0 % or less, and a window whose oldest sample in the span is under 10 minutes old get [`pace`]'s result as it
/// is. A window that did not rise in the span lasts to its reset with no run-out, however full it is. One that rose
/// runs out when that rate takes it to 100 %, rounded down to the minute, and lasts when that is at or after its reset.
#[must_use]
pub fn recent_pace(
    window: FleetWindow,
    account: u32,
    read: WindowReading,
    rates: RateSource<'_>,
    fetched_at: Timestamp,
) -> Option<Pace> {
    let since_start = pace(read.pct, read.resets_at, fetched_at, window.period())?;
    if read.pct >= 100.0 || read.pct <= 0.0 {
        return Some(since_start);
    }
    let Some(rate) = recent_rate(window, account, rates, fetched_at) else {
        return Some(since_start);
    };
    if rate <= 0.0 {
        return Some(Pace {
            runs_out_at: None,
            lasts_to_reset: true,
            ..since_start
        });
    }
    let remaining_s = read.resets_at.duration_since(fetched_at).as_secs_f64();
    Some(Pace {
        runs_out_at: minute_after(fetched_at, (100.0 - read.pct) * 3600.0 / rate),
        lasts_to_reset: read.pct + rate * remaining_s / 3600.0 <= 100.0,
        ..since_start
    })
}

/// `at` rounded down to the minute; `None` when that is past what a timestamp can hold.
fn floor_minute(at: Timestamp) -> Option<Timestamp> {
    at.round(
        TimestampRound::new()
            .smallest(Unit::Minute)
            .mode(RoundMode::Floor),
    )
    .ok()
}

/// The shortest span of samples a member's rise is trusted over; with less, its rate is its window's average.
const MIN_RATE_SPAN: SignedDuration = SignedDuration::from_mins(10);

/// The span the fleet verdict's draw ratio reads: the in-use account's samples fetched at most this long before the
/// judging time.
const RATIO_SPAN: SignedDuration = SignedDuration::from_mins(75);

/// How far a window's reset may move between two readings and still be the same window: the usage endpoint's reset
/// times are not stable to the second.
const SAME_RESET_SLACK: SignedDuration = SignedDuration::from_mins(1);

/// The shortest gap the fleet verdict reports, in seconds; a shorter one is passed over.
const MIN_GAP_SECS: f64 = 5.0 * 60.0;

/// The draw ratio the fleet verdict runs on when the in-use account's history measures none: the 7-day points an
/// account's 7-day window rises by per 5-hour point drawn on it.
const DEFAULT_DRAW_RATIO: f64 = 0.2;

/// The least 5-hour rise, in points, from which the in-use account's history measures a draw ratio.
const MIN_RATIO_FIVE_HOUR_RISE: f64 = 10.0;

/// The least 7-day rise, in points, from which the in-use account's history measures a draw ratio.
const MIN_RATIO_SEVEN_DAY_RISE: f64 = 2.0;

/// One account-wide window of a reading as the fleet verdict reads it: how much of it is used, and when it resets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowReading {
    /// Utilization in percent; 100 or more means the window is exhausted.
    pub pct: f64,
    /// When the window resets.
    pub resets_at: Timestamp,
}

/// One account's usage reading as fetched: its account-wide windows, each when the reading had it with a reset, and the
/// use of a 5-hour window not yet started.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccountReading {
    /// The account's number in the store's roster.
    pub account: u32,
    /// When the reading was fetched.
    pub fetched_at: Timestamp,
    /// The 5-hour window.
    pub five_hour: Option<WindowReading>,
    /// The 7-day window.
    pub seven_day: Option<WindowReading>,
    /// The 5-hour window's `pct` when the reading has it finite with no reset: a window not yet started.
    pub unstarted_five_hour: Option<f64>,
}

impl AccountReading {
    fn window(&self, window: FleetWindow) -> Option<WindowReading> {
        match window {
            FleetWindow::FiveHour => self.five_hour,
            FleetWindow::SevenDay => self.seven_day,
        }
    }
}

/// An account-wide window: the one a gap in the fleet's timeline belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FleetWindow {
    /// The 5-hour window, [`FIVE_HOUR`] long.
    FiveHour,
    /// The 7-day window, [`SEVEN_DAY`] long.
    SevenDay,
}

impl FleetWindow {
    /// The window's length.
    #[must_use]
    pub const fn period(self) -> SignedDuration {
        match self {
            Self::FiveHour => FIVE_HOUR,
            Self::SevenDay => SEVEN_DAY,
        }
    }

    /// The window's short name: `5h` or `7d`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::SevenDay => "7d",
        }
    }
}

/// Where the fleet verdict's draw ratio came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RatioSource {
    /// Measured from the in-use account's history.
    Measured,
    /// The default ratio, 0.2, since the history measures none.
    Default,
}

impl RatioSource {
    /// The source's name: `measured` or `default`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Default => "default",
        }
    }
}

/// The fleet's verdict over its timeline of the next 7 days: whether the pooled allowance, spent at the members' summed
/// rate, each 5-hour window refilled at its reset and held to what its account's 7-day window leaves, each 7-day window
/// refilled at its own reset, leaves a gap of at least 5 minutes in those 7 days, when the first such gap starts and
/// ends, and every gap there is.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct WindowVerdict {
    /// The window the gap belongs to: [`FleetWindow::SevenDay`] when every account's 7-day window has run out at the
    /// gap's start, else [`FleetWindow::FiveHour`], which a fleet that lasts also carries.
    pub window: FleetWindow,
    /// How many accounts are in the pool: each with a 5-hour limit whose 5-hour window is in range or not yet started,
    /// and each with only a 7-day limit whose 7-day window is in range, either under a 7-day window below 100 %.
    pub members: usize,
    /// How many accounts are on the timeline: the pool members, and the accounts left out of the pool for a spent
    /// 7-day window.
    pub accounts: usize,
    /// The pooled remaining allowance, in 5-hour points: the sum of each member's `100 - pct` of its 5-hour window, or,
    /// for a member with only a 7-day limit, of its 7-day window over `ratio`, each `100 - pct` held to 0 through 100.
    pub remaining: f64,
    /// The summed rate of the accounts on the timeline, per hour in the points of `rate_window`: each member's rise over
    /// its samples of the rate span the caller passes, or its window's average with under 10 minutes of samples; for a
    /// member with a 5-hour limit its 5-hour window's (the rise alone for a window not yet started), for one with only a
    /// 7-day limit its 7-day window's over `ratio`; and each account left out for a spent 7-day window's rise over those
    /// samples, so the burn stays when the account it was on runs out. In 5-hour points when some account on the
    /// timeline has a 5-hour limit; else that sum times `ratio`, in 7-day points.
    pub rate: f64,
    /// The window whose points `rate` is in: [`FleetWindow::FiveHour`] when some account on the timeline has a 5-hour
    /// limit, else [`FleetWindow::SevenDay`].
    pub rate_window: FleetWindow,
    /// The 7-day points every account's 7-day window rises by per 5-hour point drawn on it, which also turns a 7-day-only
    /// account's 7-day points into 5-hour points: the in-use account's 7-day rise over its 5-hour rise across its
    /// samples of the last `RATIO_SPAN`, counting only pairs of samples within one 5-hour window, or 0.2 when its
    /// 5-hour rise is under 10 points or its 7-day rise under 2 (as it always is for an in-use account with only a
    /// 7-day limit).
    pub ratio: f64,
    /// Whether `ratio` was measured from the history or is the 0.2 default.
    pub ratio_source: RatioSource,
    /// When the first gap starts, rounded down to the minute: the judging time when nothing remains; `None` when the
    /// fleet lasts or the time is past what a timestamp can hold.
    pub blocked_at: Option<Timestamp>,
    /// When the first gap ends, as the next account is back, rounded down to the minute; `None` when the fleet lasts,
    /// when no account comes back, or when the time is past what a timestamp can hold.
    pub resumes_at: Option<Timestamp>,
    /// The earliest reset among the accounts, rounded down to the minute: each member's started 5-hour reset and the
    /// 7-day reset of each account left out of the pool for a spent 7-day window, else the earliest 7-day reset on the
    /// timeline; `None` when no account has one.
    pub first_reset: Option<Timestamp>,
    /// Whether the timeline has a gap.
    pub runs_dry: bool,
    /// Every gap of at least 5 minutes in the 7 days, in order; the first is the one `blocked_at` and `resumes_at`
    /// describe, except that `resumes_at` keeps a first gap's end past the horizon where its `end` is `None`. Empty when
    /// the fleet lasts.
    pub gaps: Vec<FleetGap>,
}

/// A gap of at least 5 minutes on the fleet's timeline: when it starts and ends, each rounded down to the minute,
/// and the window it belongs to.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct FleetGap {
    /// When the pool runs out.
    pub start: Timestamp,
    /// When the next account is back; `None` when no account comes back before the 7-day horizon (an end exactly at it
    /// included).
    pub end: Option<Timestamp>,
    /// [`FleetWindow::SevenDay`] when every account's 7-day window has run out at `start`, else [`FleetWindow::FiveHour`].
    pub window: FleetWindow,
}

/// The fleet's verdict; `None` when no account is on its timeline.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct Fleet {
    /// The timeline's verdict.
    pub verdict: Option<WindowVerdict>,
}

impl Fleet {
    /// The verdict when the fleet runs dry; `None` when it lasts or has no verdict.
    #[must_use]
    pub fn dry(&self) -> Option<&WindowVerdict> {
        self.verdict.as_ref().filter(|verdict| verdict.runs_dry)
    }
}

/// The auto-switch the fleet's timeline follows: where it starts, its settings and its state.
#[derive(Debug, Clone, Copy)]
pub struct Rotation<'a> {
    /// The account Claude Code is logged in as, which the timeline starts on and the draw ratio is measured on when it
    /// is on the timeline.
    pub active: Option<u32>,
    /// The auto-switch settings.
    pub settings: &'a AutoSwitchSettings,
    /// The auto-switch state: the last switch, its departure snapshot and the quarantine.
    pub state: &'a AutoswitchState,
}

/// The fleet's verdict at `now` over the counted accounts' current `readings` and the reading `history`: a timeline of
/// the pooled allowance, in 5-hour points, over the [`SEVEN_DAY`] from `now`, each account's 7-day window refilled at
/// its reset.
///
/// The pool holds each reading whose 5-hour window has a finite `pct` and either a reset after `now` at most
/// [`FIVE_HOUR`] away or no reset at all (a window not yet started), and each reading with no 5-hour window at all, a
/// 7-day-only account with no 5-hour limit, whose 7-day window has a finite `pct` and a reset after `now` at most
/// [`SEVEN_DAY`] away; neither when its 7-day window is at 100 % or more. Such an account adds nothing, and is back at its
/// 7-day reset, or at its 5-hour reset when that is later and its 5-hour window is spent.
///
/// The pool burns at its members' summed rate, in 5-hour points per hour. A member's rate is its rises over its
/// `history` samples fetched in the last `rate_span`, the rate span the caller passes, over the time from the oldest of
/// them to `now` (a fall or a reset moved by over a minute adds nothing), or its window's average use since the window
/// began when that oldest sample is under 10 minutes old (none for a window younger than [`MIN_ELAPSED`], or not yet
/// started). That window is a member's 5-hour window, or a 7-day-only member's 7-day window, whose rate counts over the
/// draw ratio. An account left out of the pool for a spent 7-day window still adds its rises, on its running 5-hour window
/// or else its 7-day window over the draw ratio, so the burn stays when the account it was on runs out.
///
/// The timeline follows the auto-switch as the engine runs it, with `rotation`'s settings: it draws on one account
/// at a time, starting on the account Claude Code is logged in as when it is on the timeline, else the first pool member.
/// Every `interval_seconds` the engine's own trigger classification reads the drawn account's binding window, the fuller
/// of its 5-hour and 7-day windows: below the threshold it stays (or, under `consume-first`, looks for a sooner weekly
/// reset); at or over it, it takes the first account the engine's own ranking returns, once `cooldown_seconds` have
/// passed since the last switch. When every readable account is at or over the threshold the ranking returns none, so
/// the drawn account runs on until its binding window reaches 100 %. At 100 % it moves at once, whatever the cooldown,
/// and when the ranking returns no account the gap starts. The ranking reads a state that starts as `rotation`'s and
/// records each simulated switch the way the engine does, so its no-return bar and the bar's release apply at every
/// step.
///
/// Not modelled: consume-first's two-phase commit, the target refetch around the ranking; the scoped windows
/// the settings' `models` name, since the timeline reads only the 5-hour and 7-day windows; and the engine's wait for its
/// next tick before an at-limit move, which the timeline takes at the instant the account runs out.
///
/// A reset refills an account to 100 points and loses what it had left. A refilled account's next window, like a window
/// not yet started, starts when it is first drawn on, so its next reset is [`FIVE_HOUR`] after that draw. A 7-day-only
/// account has no 5-hour window at all: none starts when it is drawn on, nothing stops its draw but its 7-day window,
/// and the ranking reads only that window.
///
/// A 7-day reset gives an account a whole week to draw again and loses what its 7-day window had left; that window's
/// next reset is [`SEVEN_DAY`] after the one passed, and its 5-hour window is left as it was, or unused when none is
/// running. An account left out of the pool for a spent 7-day window is drawn on again from that reset.
///
/// An account's 7-day window rises only while it is drawn on, by the draw ratio's 7-day points per 5-hour point drawn,
/// and stays flat while it is not; a draw stops when either window reaches 100 %. The draw ratio is the in-use account's
/// (the logged-in account when it is on the timeline, else the first account on it) 7-day rise over its 5-hour
/// rise across its `history` samples fetched in the last `RATIO_SPAN`, counting only pairs of samples within one
/// 5-hour window, when those rises are at least 2 and 10 points, else 0.2, and every account draws at it.
///
/// A reset at the moment the pool runs out leaves no gap, and a gap under 5 minutes is passed over: the verdict reports
/// the first gap of at least 5 minutes, from when the pool runs out to when the next account is back, and the timeline
/// runs on past it to the 7 days, reporting every further gap the same way (see [`WindowVerdict::gaps`]).
#[must_use]
pub fn fleet(
    readings: &[AccountReading],
    history: &[AccountReading],
    rotation: Rotation<'_>,
    rate_span: SignedDuration,
    now: Timestamp,
) -> Fleet {
    Fleet {
        verdict: timeline_verdict(readings, history, rotation, rate_span, now),
    }
}

/// The verdict of the fleet's timeline at `now`, its members' rates read over `rate_span`; `None` when no account is on
/// it.
fn timeline_verdict(
    readings: &[AccountReading],
    history: &[AccountReading],
    rotation: Rotation<'_>,
    rate_span: SignedDuration,
    now: Timestamp,
) -> Option<WindowVerdict> {
    let members: Vec<(&AccountReading, MemberWindow)> = readings
        .iter()
        .filter_map(|reading| Some((reading, pool_member(reading, now)?)))
        .collect();
    let (ratio, ratio_source) = draw_ratio(
        in_use_account(readings, rotation.active, now)?,
        history,
        now,
    );
    let remaining: f64 = members
        .iter()
        .map(|(_, member)| member.points_left(ratio))
        .sum();
    let rates = RateSource {
        history,
        span: rate_span,
    };
    let members_rate: f64 = members
        .iter()
        .map(|(reading, member)| member.rate(reading, rates, ratio, now))
        .sum();
    let spent_rate: f64 = readings
        .iter()
        .filter(|reading| spent_week(reading, now).is_some())
        .map(|reading| spent_rate(reading, rates, ratio, now))
        .sum();
    let rate = members_rate + spent_rate;
    let lanes: Vec<Lane> = readings
        .iter()
        .filter_map(|reading| lane_of(reading, ratio, now))
        .collect();
    let accounts = lanes.len();
    let (reported_rate, rate_window) = if lanes.iter().any(|lane| lane.capped) {
        (rate, FleetWindow::FiveHour)
    } else {
        (rate * ratio, FleetWindow::SevenDay)
    };
    let first_member = members.first().map(|(reading, _)| reading.account);
    let start = rotation
        .active
        .and_then(|active| lanes.iter().position(|lane| lane.account == active))
        .or_else(|| {
            lanes
                .iter()
                .position(|lane| Some(lane.account) == first_member)
        })
        .unwrap_or(0);
    let mut timeline = Timeline::new(lanes, start, Burn { rate, ratio }, rotation, now);
    let horizon = SEVEN_DAY.as_secs_f64();
    let gaps = timeline.gaps(horizon);
    let first = gaps.first();
    Some(WindowVerdict {
        window: first.map_or(FleetWindow::FiveHour, |gap| gap.window),
        members: members.len(),
        accounts,
        remaining,
        rate: reported_rate,
        rate_window,
        ratio,
        ratio_source,
        blocked_at: first.and_then(|gap| time_at(now, gap.start)),
        resumes_at: first
            .and_then(|gap| gap.end)
            .and_then(|end| time_at(now, end)),
        first_reset: first_reset(readings, now),
        runs_dry: !gaps.is_empty(),
        gaps: gaps
            .iter()
            .filter_map(|gap| {
                Some(FleetGap {
                    start: time_at(now, gap.start)?,
                    end: gap
                        .end
                        .filter(|end| *end < horizon)
                        .and_then(|end| time_at(now, end)),
                    window: gap.window,
                })
            })
            .collect(),
    })
}

/// The window a pool member brings to the pool: its 5-hour window, or, for an account with no 5-hour limit, its 7-day
/// window.
#[derive(Debug, Clone, Copy)]
enum MemberWindow {
    /// The 5-hour window of an account with a 5-hour limit.
    FiveHour {
        /// Its use in percent.
        pct: f64,
        /// Its reading, with its reset; `None` for a window not yet started.
        started: Option<WindowReading>,
    },
    /// The 7-day window of an account with no 5-hour limit.
    WeekOnly {
        /// Its reading, in range (see [`in_range`]).
        week: WindowReading,
    },
}

impl MemberWindow {
    /// The 5-hour window's reading when it has started; `None` for one not yet started and for a 7-day-only member.
    fn started(self) -> Option<WindowReading> {
        match self {
            Self::FiveHour { started, .. } => started,
            Self::WeekOnly { .. } => None,
        }
    }

    /// The 5-hour points the member brings: its 5-hour window's points left, or its 7-day window's over the draw `ratio`.
    fn points_left(self, ratio: f64) -> f64 {
        match self {
            Self::FiveHour { pct, .. } => points_left(pct),
            Self::WeekOnly { week } => points_left(week.pct) / ratio,
        }
    }

    /// `reading`'s rate at `now` in 5-hour points per hour (see [`member_rate`]): its started 5-hour window's, its 5-hour
    /// rise alone for one not yet started, which rose only if a window reset inside the span, or its 7-day window's over
    /// the draw `ratio`.
    fn rate(
        self,
        reading: &AccountReading,
        rates: RateSource<'_>,
        ratio: f64,
        now: Timestamp,
    ) -> f64 {
        match self {
            Self::FiveHour {
                started: Some(five),
                ..
            } => member_rate(FleetWindow::FiveHour, reading, five, rates, now),
            Self::FiveHour { started: None, .. } => {
                recent_rate(FleetWindow::FiveHour, reading.account, rates, now).unwrap_or(0.0)
            }
            Self::WeekOnly { week } => {
                member_rate(FleetWindow::SevenDay, reading, week, rates, now) / ratio
            }
        }
    }
}

/// Whether `reading` has no 5-hour limit: it carries no 5-hour window, started or not.
fn uncapped(reading: &AccountReading) -> bool {
    reading.five_hour.is_none() && reading.unstarted_five_hour.is_none()
}

/// The window `reading` brings to the pool at `now`, under a 7-day window below 100 %: its 5-hour window when that is
/// in range (see [`in_range`]) or not yet started, or, for an account with no 5-hour limit, its 7-day window when that
/// is in range.
fn pool_member(reading: &AccountReading, now: Timestamp) -> Option<MemberWindow> {
    if reading.seven_day.is_some_and(|week| week.pct >= 100.0) {
        return None;
    }
    if let Some(five) = in_range(reading.five_hour, FleetWindow::FiveHour, now) {
        return Some(MemberWindow::FiveHour {
            pct: five.pct,
            started: Some(five),
        });
    }
    if let Some(pct) = reading
        .unstarted_five_hour
        .filter(|pct| reading.five_hour.is_none() && pct.is_finite())
    {
        return Some(MemberWindow::FiveHour { pct, started: None });
    }
    let week =
        in_range(reading.seven_day, FleetWindow::SevenDay, now).filter(|_| uncapped(reading))?;
    Some(MemberWindow::WeekOnly { week })
}

/// The account the draw ratio is measured on at `now`: `active` when it is on the timeline, else the first account
/// that is; `None` when no account is on it.
fn in_use_account(readings: &[AccountReading], active: Option<u32>, now: Timestamp) -> Option<u32> {
    let on_timeline: Vec<u32> = readings
        .iter()
        .filter(|reading| pool_member(reading, now).is_some() || spent_week(reading, now).is_some())
        .map(|reading| reading.account)
        .collect();
    active
        .filter(|active| on_timeline.contains(active))
        .or_else(|| on_timeline.first().copied())
}

/// The earliest reset among `readings` at `now`, rounded down to the minute: each pool member's started 5-hour reset
/// and each spent 7-day window's reset, else the earliest 7-day reset of a pool member; `None` when there is none.
fn first_reset(readings: &[AccountReading], now: Timestamp) -> Option<Timestamp> {
    let started = readings
        .iter()
        .filter_map(|reading| Some(pool_member(reading, now)?.started()?.resets_at));
    let spent = readings
        .iter()
        .filter_map(|reading| Some(spent_week(reading, now)?.resets_at));
    let first = started.chain(spent).min().or_else(|| {
        readings
            .iter()
            .filter(|reading| pool_member(reading, now).is_some())
            .filter_map(|reading| {
                Some(in_range(reading.seven_day, FleetWindow::SevenDay, now)?.resets_at)
            })
            .min()
    })?;
    Some(floor_minute(first).unwrap_or(first))
}

/// `reading`'s 7-day window when it is at a finite 100 % or more, which leaves the account out of the pool, its reset
/// held by [`clamped_reset`].
fn spent_week(reading: &AccountReading, now: Timestamp) -> Option<WindowReading> {
    let week = reading
        .seven_day
        .filter(|week| week.pct.is_finite() && week.pct >= 100.0)?;
    Some(WindowReading {
        pct: week.pct,
        resets_at: clamped_reset(week.resets_at, now),
    })
}

/// `resets_at` held into the 7 days from `now`: one at or before `now` reads as `now`, and one past `now + SEVEN_DAY`
/// reads as that horizon; when the horizon is past what a timestamp can hold, `resets_at` as it is.
fn clamped_reset(resets_at: Timestamp, now: Timestamp) -> Timestamp {
    let clamped = now
        .checked_add(SEVEN_DAY)
        .map_or(resets_at, |horizon| resets_at.min(horizon));
    clamped.max(now)
}

/// `read` when it has a finite `pct` and a reset after `now` at most one `window` length away.
fn in_range(
    read: Option<WindowReading>,
    window: FleetWindow,
    now: Timestamp,
) -> Option<WindowReading> {
    read.filter(|read| {
        let to_reset = read.resets_at.duration_since(now);
        read.pct.is_finite() && to_reset > SignedDuration::ZERO && to_reset <= window.period()
    })
}

/// The points a window `pct` used has left, held to 0 through 100.
fn points_left(pct: f64) -> f64 {
    (100.0 - pct).clamp(0.0, 100.0)
}

/// The seconds from `now` to `at`.
fn seconds_after(now: Timestamp, at: Timestamp) -> f64 {
    at.duration_since(now).as_secs_f64()
}

/// The time `seconds` after `now`, rounded down to the minute; `None` when that is past what a timestamp can hold.
fn time_at(now: Timestamp, seconds: f64) -> Option<Timestamp> {
    let offset = SignedDuration::try_from_secs_f64(seconds).ok()?;
    floor_minute(now.checked_add(offset).ok()?)
}

/// One account on the fleet's timeline, its times in seconds after the judging time: an account with a 5-hour limit,
/// drawn on until its 5-hour or its 7-day window runs out, or one with only a 7-day limit, whose 5-hour use is 0 at all
/// times, which never has a 5-hour reset and is drawn on until its 7-day window runs out.
#[derive(Debug, Clone, Copy)]
struct Lane {
    /// The account's number.
    account: u32,
    /// Whether the account has a 5-hour limit.
    capped: bool,
    /// Its 5-hour window's use in percent, held to 0 through 100; for the account being drawn on, its use when the
    /// draw began; always 0 without a 5-hour limit.
    used: f64,
    /// When its running 5-hour window resets; `None` while it is refilled or not yet started, and not yet drawn on, and
    /// always without a 5-hour limit.
    reset: Option<f64>,
    /// Its 7-day window: a pool member's when its reading has a finite 7-day `pct` (always for one with only a 7-day
    /// limit), else none and no weekly limit; a spent account's always. Its reset is held into the 7 days from the
    /// judging time.
    week: Option<Week>,
}

/// A lane's 7-day window, which rises by the draw ratio's 7-day points per 5-hour point drawn on the lane and stays flat
/// while the lane is not drawn on; its times in seconds after the judging time. For a lane with only a 7-day limit it
/// is the whole of what the lane may draw.
#[derive(Debug, Clone, Copy)]
struct Week {
    /// The 5-hour points the lane may still draw before the window reaches 100 %: its points left over the draw ratio,
    /// 0 once it is spent, and 100 over the draw ratio again from its reset; for the account being drawn on, what it had
    /// when the draw began.
    left: f64,
    /// When it next resets, [`SEVEN_DAY`] after the reset before.
    reset: f64,
}

impl Lane {
    /// When its 7-day window resets, when it has one (see [`Lane::week`]).
    fn week_reset(&self) -> Option<f64> {
        self.week.map(|week| week.reset)
    }

    /// When the lane, `used` and with nothing to draw at `t`, is back: at its 5-hour reset when its week is not
    /// `spent` and that reset is after `t`, else at its 7-day reset, or at its 5-hour reset when that is later and its
    /// 5-hour window is spent; `None` without a 7-day reset.
    fn back_at(&self, used: f64, spent: bool, t: f64) -> Option<f64> {
        if let Some(reset) = self.reset.filter(|reset| !spent && *reset > t) {
            return Some(reset);
        }
        let week = self.week_reset()?;
        let five = self.reset.filter(|_| used >= 100.0);
        Some(five.map_or(week, |five| five.max(week)))
    }
}

/// `reading`'s lane at `now` under the draw `ratio`: a pool member's, with its 5-hour window and the points its 7-day
/// window leaves to draw, or, for an account left out of the pool for its spent 7-day window, one with nothing left to
/// draw until that window resets; `None` for neither. A lane with only a 7-day limit has no 5-hour use or reset.
fn lane_of(reading: &AccountReading, ratio: f64, now: Timestamp) -> Option<Lane> {
    let capped = !uncapped(reading);
    if let Some(member) = pool_member(reading, now) {
        let week = reading
            .seven_day
            .filter(|week| week.pct.is_finite())
            .map(|week| Week {
                left: points_left(week.pct) / ratio,
                reset: seconds_after(now, clamped_reset(week.resets_at, now)),
            });
        let used = match member {
            MemberWindow::FiveHour { pct, .. } => 100.0 - points_left(pct),
            MemberWindow::WeekOnly { .. } => 0.0,
        };
        return Some(Lane {
            account: reading.account,
            capped,
            used,
            reset: member
                .started()
                .map(|started| seconds_after(now, started.resets_at)),
            week,
        });
    }
    let week = spent_week(reading, now)?;
    let five = in_range(reading.five_hour, FleetWindow::FiveHour, now);
    let unstarted = reading
        .unstarted_five_hour
        .filter(|pct| reading.five_hour.is_none() && pct.is_finite());
    let used = five
        .map(|five| five.pct)
        .or(unstarted)
        .map_or(100.0, |pct| 100.0 - points_left(pct));
    Some(Lane {
        account: reading.account,
        capped,
        used: if capped { used } else { 0.0 },
        reset: five
            .filter(|_| capped)
            .map(|five| seconds_after(now, five.resets_at)),
        week: Some(Week {
            left: 0.0,
            reset: seconds_after(now, week.resets_at),
        }),
    })
}

/// A gap on the timeline, in seconds after the judging time.
#[derive(Debug, Clone, Copy)]
struct Gap {
    /// When the pool runs out.
    start: f64,
    /// When the next account is back; `None` when none comes back.
    end: Option<f64>,
    /// [`FleetWindow::SevenDay`] when every lane's week is spent at `start`.
    window: FleetWindow,
}

/// The draw on the active lane since it began, its times in seconds after the judging time: it spends the lane's 5-hour
/// allowance and what its week has left together, and stops when either runs out.
#[derive(Debug, Clone, Copy)]
struct Draw {
    /// When the draw began.
    since: f64,
    /// The lane's 5-hour use when it began.
    from: f64,
    /// When the lane's 5-hour window reaches 100 % at the pool's rate; infinite with no use.
    empty_at: f64,
    /// When the lane's 7-day window reaches 100 %, what its week had left drawn at the pool's rate; infinite with no use
    /// or no 7-day window.
    spent_at: f64,
}

impl Draw {
    /// When the draw stops: at the first of its run-outs.
    fn stop(&self) -> f64 {
        self.empty_at.min(self.spent_at)
    }

    /// Whether the draw has run the lane's 5-hour window out by `t`.
    fn five_hour_out(&self, t: f64) -> bool {
        t >= self.empty_at && self.empty_at <= self.spent_at
    }

    /// Whether the draw has run the lane's 7-day window out by `t`.
    fn week_out(&self, t: f64) -> bool {
        t >= self.spent_at && self.spent_at <= self.empty_at
    }
}

/// When `rate` points an hour, drawn from `t`, have drawn `points`; infinite with no use.
fn drawn_by(t: f64, points: f64, rate: f64) -> f64 {
    if rate > 0.0 {
        t + points * 3600.0 / rate
    } else {
        f64::INFINITY
    }
}

/// How the timeline draws: the pool's rate, and the draw ratio each lane's 7-day window rises by.
#[derive(Debug, Clone, Copy)]
struct Burn {
    /// The pool's rate, in 5-hour points per hour.
    rate: f64,
    /// The 7-day points per 5-hour point drawn.
    ratio: f64,
}

/// The fleet's timeline: its lanes in rotation order, the one drawn on, and the auto-switch state its ranking reads.
struct Timeline<'a> {
    lanes: Vec<Lane>,
    /// The lanes as the engine's candidate census reads them.
    census: Vec<CandidateAccount>,
    /// The index of the lane drawn on.
    active: usize,
    /// The draw on the active lane; `None` until it begins again after a switch or a refill.
    draw: Option<Draw>,
    /// The pool's rate, in points per hour.
    rate: f64,
    /// The 7-day points per 5-hour point drawn.
    ratio: f64,
    settings: &'a AutoSwitchSettings,
    /// The state the ranking and the cooldown read: the live one, then each simulated switch's.
    state: AutoswitchState,
    /// The judging time.
    now: Timestamp,
}

impl<'a> Timeline<'a> {
    fn new(
        lanes: Vec<Lane>,
        active: usize,
        burn: Burn,
        rotation: Rotation<'a>,
        now: Timestamp,
    ) -> Self {
        let census = lanes
            .iter()
            .map(|lane| CandidateAccount::new(lane.account, AccountKind::Oauth, false))
            .collect();
        Self {
            lanes,
            census,
            active,
            draw: None,
            rate: burn.rate,
            ratio: burn.ratio,
            settings: rotation.settings,
            state: rotation.state.clone(),
            now,
        }
    }

    /// Every gap of at least 5 minutes that starts before `horizon`, in order: the timeline runs on from when the next
    /// account is back past each gap, and stops at the horizon or at a gap no account comes back from.
    /// The engine decides on each tick, and at once whenever the lane drawn on runs out.
    fn gaps(&mut self, horizon: f64) -> Vec<Gap> {
        let mut gaps = Vec::new();
        // The settings refuse an interval under 15 s; the floor only keeps the ticks moving on a hand-built one.
        let interval = self.settings.interval_seconds.max(1.0);
        let mut t = 0.0;
        let mut next_tick = 0.0;
        while t < horizon {
            self.refill(t);
            let at_tick = t >= next_tick;
            if at_tick {
                next_tick = ((t / interval).floor() + 1.0) * interval;
            }
            self.decide(t, at_tick);
            if !self.can_draw(self.active, t) {
                self.settle(t);
                let end = self.back_at(t);
                match end {
                    Some(end) if end - t < MIN_GAP_SECS => t = end,
                    _ => {
                        gaps.push(Gap {
                            start: t,
                            end,
                            window: self.gap_window(t),
                        });
                        match end {
                            Some(end) if end < horizon => t = end,
                            _ => break,
                        }
                    }
                }
                continue;
            }
            let next = self.next_event(t).min(next_tick).min(horizon);
            t = self.draw_until(t, next);
        }
        gaps
    }

    /// The 5-hour points `draw` has drawn by `t`, up to where it stops.
    fn drawn(&self, draw: Draw, t: f64) -> f64 {
        self.rate * (t.min(draw.stop()) - draw.since) / 3600.0
    }

    /// Lane `index`'s 5-hour use at `t`: 100 once the draw on it has run its 5-hour window out, and what it drew when
    /// the draw stopped for its week; always 0 for a lane with no 5-hour limit.
    fn used(&self, index: usize, t: f64) -> f64 {
        match self.draw {
            Some(draw) if index == self.active && self.lanes[index].capped => {
                if draw.five_hour_out(t) {
                    100.0
                } else {
                    draw.from + self.drawn(draw, t)
                }
            }
            _ => self.lanes[index].used,
        }
    }

    /// The 5-hour points lane `index` may still draw at `t` before its 7-day window reaches 100 %: 0 once the draw on
    /// it has run its week out; infinite without a 7-day window.
    fn left(&self, index: usize, t: f64) -> f64 {
        let Some(week) = self.lanes[index].week else {
            return f64::INFINITY;
        };
        match self.draw {
            Some(draw) if index == self.active => {
                if draw.week_out(t) {
                    0.0
                } else {
                    (week.left - self.drawn(draw, t)).max(0.0)
                }
            }
            _ => week.left,
        }
    }

    /// Whether lane `index` has allowance at `t`: its 5-hour window below 100 % and its week not spent.
    fn can_draw(&self, index: usize, t: f64) -> bool {
        self.used(index, t) < 100.0 && self.left(index, t) > 0.0
    }

    /// Ends the draw on the active lane at `t`, keeping its 5-hour use and what its week has left.
    fn settle(&mut self, t: f64) {
        let (used, left) = (self.used(self.active, t), self.left(self.active, t));
        let lane = &mut self.lanes[self.active];
        lane.used = used;
        if let Some(week) = lane.week.as_mut() {
            week.left = left;
        }
        self.draw = None;
    }

    /// Refills each lane whose 5-hour window resets at or before `t` to 100 points, its next window not yet started, and
    /// each lane whose 7-day window resets at or before `t` to a whole week, its next reset [`SEVEN_DAY`] later and its
    /// 5-hour window unused when none is running; the draw on the active one ends there, keeping what was drawn.
    fn refill(&mut self, t: f64) {
        let active = &self.lanes[self.active];
        if active.reset.is_some_and(|reset| reset <= t)
            || active.week_reset().is_some_and(|reset| reset <= t)
        {
            self.settle(t);
        }
        let whole_week = 100.0 / self.ratio;
        for lane in &mut self.lanes {
            if lane.reset.is_some_and(|reset| reset <= t) {
                lane.used = 0.0;
                lane.reset = None;
            }
            if let Some(week) = lane.week.as_mut().filter(|week| week.reset <= t) {
                week.left = whole_week;
                week.reset += SEVEN_DAY.as_secs_f64();
                if lane.reset.is_none() {
                    lane.used = 0.0;
                }
            }
        }
    }

    /// The time `t` after the judging time; `None` when that is past what a timestamp can hold.
    fn at(&self, t: f64) -> Option<Timestamp> {
        let offset = SignedDuration::try_from_secs_f64(t).ok()?;
        self.now.checked_add(offset).ok()
    }

    /// Lane `index`'s usage at `t` as the ranking reads it: its 5-hour window when it has a 5-hour limit, then its 7-day
    /// window when it has one, at 100 less the draw ratio times the points it has left to draw, held to 0 through 100.
    fn usage(&self, index: usize, t: f64) -> AccountUsage {
        let lane = &self.lanes[index];
        let mut windows = Vec::with_capacity(2);
        if lane.capped {
            windows.push(UsageWindow {
                pct: self.used(index, t),
                resets_at: lane.reset.and_then(|reset| self.at(reset)),
            });
        }
        let weekly_reset = lane.week.and_then(|week| {
            let resets_at = self.at(week.reset);
            windows.push(UsageWindow {
                pct: (100.0 - self.left(index, t) * self.ratio).clamp(0.0, 100.0),
                resets_at,
            });
            resets_at
        });
        AccountUsage {
            windows,
            weekly_reset,
        }
    }

    /// The engine's decision at `t`: its trigger classification of the active lane, and when that looks for another
    /// account (an escape at any time, the other triggers on a tick once the cooldown is over), the first account of
    /// its ranking.
    fn decide(&mut self, t: f64, at_tick: bool) {
        let Some(at) = self.at(t) else {
            return;
        };
        let left = self.usage(self.active, t);
        let Some(headroom) = left.headroom() else {
            return;
        };
        let classified = classify_trigger(
            ActiveReading::Headroom(headroom),
            ActiveHealth::default(),
            self.settings,
            at,
        );
        let TriggerDecision::Switch(trigger) = classified.decision else {
            return;
        };
        if !(trigger.is_escape() || at_tick) || in_cooldown(trigger, &self.state, self.settings, at)
        {
            return;
        }
        let active = self.lanes[self.active].account;
        let census = candidates(&self.census, active, &self.state);
        let usage = LaneUsage { timeline: self, t };
        let ranking = rank(&RankInputs {
            trigger,
            active,
            candidates: &census,
            usage: &usage,
            state: &self.state,
            settings: self.settings,
            now: at,
        });
        if let Some(&to) = ranking.ordered.first() {
            self.switch(to, trigger, t, at, &left);
        }
    }

    /// Moves the draw to account `to` at `t` for `trigger`, and records the switch in the timeline's state as the
    /// engine does: when, from and to which account, and the headroom and binding reset of `left`, the usage the
    /// switch leaves.
    fn switch(&mut self, to: u32, trigger: Trigger, t: f64, at: Timestamp, left: &AccountUsage) {
        let Some(index) = self.lanes.iter().position(|lane| lane.account == to) else {
            return;
        };
        self.state.last_switch_at = Some(secs_from_timestamp(at));
        self.state.last_switch_to = Some(to.to_string());
        self.state.last_switch_from = Some(self.lanes[self.active].account);
        self.state.left_headroom = Some(left.headroom());
        self.state.left_recovery_at =
            Some(binding_recovery(Some(left), at).map(secs_from_timestamp));
        self.state.left_trigger = Some(trigger.as_str().to_owned());
        self.settle(t);
        self.active = index;
    }

    /// Draws on the active lane from `t` until `next`, its 5-hour reset or the first of its 5-hour and 7-day run-outs,
    /// whichever is first, starting its 5-hour window when it is refilled or not yet started; returns when it stops. A
    /// lane with no 5-hour limit starts no 5-hour window and stops only at its 7-day run-out.
    fn draw_until(&mut self, t: f64, next: f64) -> f64 {
        let rate = self.rate;
        let lane = &mut self.lanes[self.active];
        let capped = lane.capped;
        let reset = if capped {
            *lane.reset.get_or_insert(t + FIVE_HOUR.as_secs_f64())
        } else {
            f64::INFINITY
        };
        let from = lane.used;
        let left = lane.week.map_or(f64::INFINITY, |week| week.left);
        let draw = *self.draw.get_or_insert_with(|| Draw {
            since: t,
            from,
            empty_at: if capped {
                drawn_by(t, 100.0 - from, rate)
            } else {
                f64::INFINITY
            },
            spent_at: drawn_by(t, left, rate),
        });
        next.min(reset).min(draw.stop())
    }

    /// The first 5-hour or 7-day reset of any lane after `t`; infinite when there is none.
    fn next_event(&self, t: f64) -> f64 {
        self.lanes
            .iter()
            .flat_map(|lane| [lane.reset, lane.week_reset()])
            .flatten()
            .filter(|at| *at > t)
            .fold(f64::INFINITY, f64::min)
    }

    /// When the first lane is back after `t`; `None` when none is.
    fn back_at(&self, t: f64) -> Option<f64> {
        self.lanes
            .iter()
            .enumerate()
            .filter_map(|(index, lane)| {
                lane.back_at(self.used(index, t), self.left(index, t) <= 0.0, t)
            })
            .min_by(f64::total_cmp)
    }

    /// [`FleetWindow::SevenDay`] when every lane's week is spent at `t`, else [`FleetWindow::FiveHour`].
    fn gap_window(&self, t: f64) -> FleetWindow {
        if (0..self.lanes.len()).all(|index| self.left(index, t) <= 0.0) {
            FleetWindow::SevenDay
        } else {
            FleetWindow::FiveHour
        }
    }
}

/// The timeline's lanes at one moment, as the engine's ranking reads usage.
struct LaneUsage<'t, 'a> {
    timeline: &'t Timeline<'a>,
    /// The moment, in seconds after the judging time.
    t: f64,
}

impl WindowSource for LaneUsage<'_, '_> {
    fn usage(&self, account: u32) -> Option<AccountUsage> {
        let index = self
            .timeline
            .lanes
            .iter()
            .position(|lane| lane.account == account)?;
        Some(self.timeline.usage(index, self.t))
    }
}

/// The samples a rate is read over: the readings to take them from, and the span before the moment judged that they must
/// fall in, the rate span the caller passes.
#[derive(Debug, Clone, Copy)]
pub struct RateSource<'a> {
    /// The usage samples, of any accounts and in any order.
    pub history: &'a [AccountReading],
    /// How long before the moment judged a sample may have been fetched and still count.
    pub span: SignedDuration,
}

/// `reading`'s rate in `window` at `now`, in points per hour: its rise over the last `rates.span` of `rates.history`, the
/// rate span the caller passes, or, with under 10 minutes of samples, `read`'s average since its window began.
fn member_rate(
    window: FleetWindow,
    reading: &AccountReading,
    read: WindowReading,
    rates: RateSource<'_>,
    now: Timestamp,
) -> f64 {
    recent_rate(window, reading.account, rates, now)
        .unwrap_or_else(|| window_average(read, reading.fetched_at, window.period()))
}

/// `account`'s rises in `window` over its samples in `rates.history` fetched in the last `rates.span` before `now`, the
/// rate span the caller passes, in points per hour over the time from the oldest of them to `now`; `None` when that
/// oldest one is under 10 minutes old.
fn recent_rate(
    window: FleetWindow,
    account: u32,
    rates: RateSource<'_>,
    now: Timestamp,
) -> Option<f64> {
    let since = now.checked_sub(rates.span).ok()?;
    let samples = samples_since(account, rates.history, since, now);
    let span = now.duration_since(samples.first()?.fetched_at);
    if span < MIN_RATE_SPAN {
        return None;
    }
    Some(rises(window, &samples) * 3600.0 / span.as_secs_f64())
}

/// The rate `reading`, an account whose 7-day window is spent, was drawn on at `now`, in 5-hour points per hour: its rise
/// in its running 5-hour window, else in its 7-day window over the draw `ratio`, so the burn stays in the pool's rate when
/// the account it was on runs out; 0 with under 10 minutes of samples, since a spent window's average says nothing of
/// the draw now.
fn spent_rate(reading: &AccountReading, rates: RateSource<'_>, ratio: f64, now: Timestamp) -> f64 {
    let rate = if in_range(reading.five_hour, FleetWindow::FiveHour, now).is_some() {
        recent_rate(FleetWindow::FiveHour, reading.account, rates, now)
    } else {
        recent_rate(FleetWindow::SevenDay, reading.account, rates, now).map(|rate| rate / ratio)
    };
    rate.unwrap_or(0.0)
}

/// The draw ratio at `now`: `account`'s 7-day rises over its 5-hour rises across its `history` samples fetched in the
/// last [`RATIO_SPAN`], when those come to at least [`MIN_RATIO_SEVEN_DAY_RISE`] and [`MIN_RATIO_FIVE_HOUR_RISE`]
/// points; else [`DEFAULT_DRAW_RATIO`]; with the ratio's source. Only a pair of consecutive samples that both hold the
/// same 5-hour window (a reset moved by at most a minute) counts, for both windows: a pair across a window's start
/// would lose the 5-hour points drawn since it started and keep the 7-day ones. A fall adds nothing.
fn draw_ratio(account: u32, history: &[AccountReading], now: Timestamp) -> (f64, RatioSource) {
    let Ok(since) = now.checked_sub(RATIO_SPAN) else {
        return (DEFAULT_DRAW_RATIO, RatioSource::Default);
    };
    let samples = samples_since(account, history, since, now);
    let (five_hour, seven_day) = samples
        .iter()
        .zip(samples.iter().skip(1))
        .filter(|(before, after)| same_five_hour_window(before.five_hour, after.five_hour))
        .fold((0.0, 0.0), |(five_hour, seven_day), (before, after)| {
            (
                five_hour + rise(before.five_hour, after.five_hour),
                seven_day + rise(before.seven_day, after.seven_day),
            )
        });
    let ratio = seven_day / five_hour;
    if five_hour >= MIN_RATIO_FIVE_HOUR_RISE
        && seven_day >= MIN_RATIO_SEVEN_DAY_RISE
        && ratio.is_finite()
    {
        (ratio, RatioSource::Measured)
    } else {
        (DEFAULT_DRAW_RATIO, RatioSource::Default)
    }
}

/// Whether `before` and `after` are readings of the same 5-hour window: both present, their resets at most
/// [`SAME_RESET_SLACK`] apart.
fn same_five_hour_window(before: Option<WindowReading>, after: Option<WindowReading>) -> bool {
    matches!((before, after), (Some(before), Some(after)) if after.resets_at.duration_since(before.resets_at).abs() <= SAME_RESET_SLACK)
}

/// `account`'s samples in `history` fetched from `since` to `now`, oldest first.
fn samples_since(
    account: u32,
    history: &[AccountReading],
    since: Timestamp,
    now: Timestamp,
) -> Vec<&AccountReading> {
    let mut samples: Vec<&AccountReading> = history
        .iter()
        .filter(|sample| {
            sample.account == account && sample.fetched_at >= since && sample.fetched_at <= now
        })
        .collect();
    samples.sort_by_key(|sample| sample.fetched_at);
    samples
}

/// The rises in `window` from each of `samples` to the next, summed.
fn rises(window: FleetWindow, samples: &[&AccountReading]) -> f64 {
    samples
        .iter()
        .zip(samples.iter().skip(1))
        .map(|(before, after)| rise(before.window(window), after.window(window)))
        .sum()
}

/// A window's average use since it began, in points per hour, judged at `fetched_at`; 0 for a window younger than
/// [`MIN_ELAPSED`].
fn window_average(read: WindowReading, fetched_at: Timestamp, period: SignedDuration) -> f64 {
    let elapsed = fetched_at.duration_since(read.resets_at) + period;
    if elapsed < MIN_ELAPSED {
        return 0.0;
    }
    read.pct.max(0.0) * 3600.0 / elapsed.as_secs_f64()
}

/// How far a window rose from `before` to `after`: 0 when either is missing, when it fell, or when its reset moved,
/// since a window that reset starts a new series.
fn rise(before: Option<WindowReading>, after: Option<WindowReading>) -> f64 {
    let (Some(before), Some(after)) = (before, after) else {
        return 0.0;
    };
    let rise = after.pct - before.pct;
    let reset_moved = after.resets_at.duration_since(before.resets_at).abs() > SAME_RESET_SLACK;
    if reset_moved || !rise.is_finite() || rise <= 0.0 {
        0.0
    } else {
        rise
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test whose fixed times do not parse or add must fail loudly"
)]
mod tests {
    use jiff::{SignedDuration, Timestamp};

    use super::{
        AccountReading, FIVE_HOUR, Fleet, FleetGap, FleetWindow, MIN_ELAPSED, MIN_GAP_SECS, Pace,
        RateSource, RatioSource, Rotation, SEVEN_DAY, WindowReading, WindowVerdict, fleet, pace,
        recent_pace,
    };
    use crate::engine::AutoswitchState;
    use crate::store::AutoSwitchSettings;
    use crate::usage::secs_from_timestamp;

    /// The fetch time every test judges its window at.
    const FETCHED: &str = "2026-10-07T12:00:00Z";

    /// The rate span every fleet helper judges with unless a test passes its own: the 30 minutes the expectations were
    /// worked out over.
    const RATE_SPAN_30: SignedDuration = SignedDuration::from_mins(30);

    fn fetched() -> Timestamp {
        FETCHED.parse().expect("the fetch time parses")
    }

    /// `fetched()` moved by `offset`.
    fn after(offset: SignedDuration) -> Timestamp {
        fetched().checked_add(offset).expect("the time is in range")
    }

    fn hours(hours: i64) -> SignedDuration {
        SignedDuration::from_hours(hours)
    }

    fn mins(mins: i64) -> SignedDuration {
        SignedDuration::from_mins(mins)
    }

    /// The pace of a window `period` long, `pct` used, that resets `remaining` after the fetch.
    fn pace_with(pct: f64, remaining: SignedDuration, period: SignedDuration) -> Option<Pace> {
        pace(pct, after(remaining), fetched(), period)
    }

    fn assert_near(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn pace_is_none_under_ten_minutes() {
        for period in [FIVE_HOUR, SEVEN_DAY] {
            let young = period - MIN_ELAPSED + SignedDuration::from_secs(1);
            assert_eq!(
                pace_with(50.0, young, period),
                None,
                "a window {young:?} from its reset has run under ten minutes"
            );
            assert!(
                pace_with(50.0, period - MIN_ELAPSED, period).is_some(),
                "a window ten minutes old has a pace"
            );
        }
    }

    #[test]
    fn pace_is_none_for_a_reset_in_the_past() {
        for period in [FIVE_HOUR, SEVEN_DAY] {
            assert_eq!(
                pace_with(50.0, mins(-1), period),
                None,
                "a reset a minute ago"
            );
            assert_eq!(
                pace_with(50.0, SignedDuration::ZERO, period),
                None,
                "a reset at the fetch"
            );
        }
    }

    #[test]
    fn pace_is_none_for_a_reset_more_than_a_period_away() {
        for period in [FIVE_HOUR, SEVEN_DAY] {
            let beyond = period + SignedDuration::from_secs(1);
            assert_eq!(
                pace_with(50.0, beyond, period),
                None,
                "a reset one second past a period away is not folded"
            );
        }
        assert_eq!(
            pace_with(64.0, hours(30), FIVE_HOUR),
            None,
            "a 5-hour window resetting in 30 hours"
        );
    }

    #[test]
    fn pace_is_none_for_a_non_finite_pct() {
        for pct in [f64::NAN, f64::INFINITY] {
            assert_eq!(
                pace_with(pct, hours(2), FIVE_HOUR),
                None,
                "{pct} is not a finite percentage"
            );
        }
    }

    #[test]
    fn pace_of_an_unused_window_lasts_with_no_run_out() {
        let five = pace_with(0.0, hours(3), FIVE_HOUR).expect("2 h into a 5 h window");
        assert_near(five.expected_pct, 40.0);
        assert_eq!(five.runs_out_at, None);
        assert!(five.lasts_to_reset);

        let seven = pace_with(0.0, hours(3 * 24), SEVEN_DAY).expect("4 d into a 7 d window");
        assert_near(seven.expected_pct, 400.0 / 7.0);
        assert_eq!(seven.runs_out_at, None);
        assert!(seven.lasts_to_reset);
    }

    #[test]
    fn pace_of_a_used_up_window_runs_out_at_the_fetch() {
        for (pct, remaining, period) in [
            (100.0, hours(2), FIVE_HOUR),
            (104.0, hours(2), FIVE_HOUR),
            (100.0, hours(24), SEVEN_DAY),
        ] {
            let used_up =
                pace_with(pct, remaining, period).expect("the window is past ten minutes old");
            assert_eq!(
                used_up.runs_out_at,
                Some(fetched()),
                "{pct}% runs out at the fetch"
            );
            assert!(!used_up.lasts_to_reset, "{pct}% does not last");
        }
    }

    #[test]
    fn pace_of_steady_use_lasts_past_the_reset() {
        // 30 % in 2 h of 5: 15 % an hour, 75 % at the reset in 3 h, 100 % in 4 h 40 min.
        let five = pace_with(30.0, hours(3), FIVE_HOUR).expect("2 h into a 5 h window");
        assert_near(five.expected_pct, 40.0);
        assert_eq!(five.runs_out_at, Some(after(hours(4) + mins(40))));
        assert!(five.lasts_to_reset);

        // 12 % in 4 d of 7: 3 % a day, 21 % at the reset in 3 d, 100 % in 29 d 8 h.
        let seven = pace_with(12.0, hours(3 * 24), SEVEN_DAY).expect("4 d into a 7 d window");
        assert_near(seven.expected_pct, 400.0 / 7.0);
        assert_eq!(seven.runs_out_at, Some(after(hours(29 * 24 + 8))));
        assert!(seven.lasts_to_reset);
    }

    #[test]
    fn pace_of_use_that_reaches_a_hundred_at_the_reset_lasts() {
        // 40 % in 2 h of 5: 60 % more over the 3 h left, exactly 100 % at the reset, which is also the run-out.
        let reset = after(hours(3));
        let ties = pace_with(40.0, hours(3), FIVE_HOUR).expect("2 h into a 5 h window");
        assert!(
            ties.lasts_to_reset,
            "a projection of exactly 100 % still lasts to the reset"
        );
        assert_eq!(
            ties.runs_out_at,
            Some(reset),
            "the run-out is the reset itself"
        );
    }

    #[test]
    fn pace_of_fast_use_runs_out_before_the_reset() {
        // 82 % in 3 h of 5: 18 % left at 27.3 % an hour runs out 39 min 30.7 s after the fetch, shown as the minute.
        let five = pace_with(82.0, hours(2), FIVE_HOUR).expect("3 h into a 5 h window");
        assert_near(five.expected_pct, 60.0);
        assert_eq!(five.runs_out_at, Some(after(mins(39))));
        assert!(!five.lasts_to_reset);

        // 30 % in 1 d of 7: 70 % left at 30 % a day runs out in 56 h, well before the reset in 6 d.
        let seven = pace_with(30.0, hours(6 * 24), SEVEN_DAY).expect("1 d into a 7 d window");
        assert_near(seven.expected_pct, 100.0 / 7.0);
        assert_eq!(seven.runs_out_at, Some(after(hours(56))));
        assert!(!seven.lasts_to_reset);
    }

    /// Account 1's recent pace in its 5-hour window at [`FETCHED`], `pct` used and resetting in 3 h, over the samples
    /// `earlier` and the reading itself in the last `span`.
    fn recent_five_hour_over(
        pct: f64,
        earlier: &[AccountReading],
        span: SignedDuration,
    ) -> Option<Pace> {
        let read = window(pct, hours(3));
        let current = reading(1, SignedDuration::ZERO, Some(read), None);
        let history: Vec<AccountReading> = earlier.iter().copied().chain([current]).collect();
        let rates = RateSource {
            history: &history,
            span,
        };
        recent_pace(FleetWindow::FiveHour, 1, read, rates, fetched())
    }

    /// [`recent_five_hour_over`] the last 30 minutes.
    fn recent_five_hour(pct: f64, earlier: &[AccountReading]) -> Option<Pace> {
        recent_five_hour_over(pct, earlier, mins(30))
    }

    #[test]
    fn recent_pace_reads_the_span_it_is_given() {
        // 50 % 60 min ago and 30 min ago, 60 % now. Over 30 min: 10 points, 20 an hour, so the 40 left run out in 2 h,
        // before the reset in 3 h. Over 2 h: 10 points in 60 min, 10 an hour, so they run out in 4 h, after it.
        let earlier = [
            reading(1, -mins(60), Some(window(50.0, hours(3))), None),
            half_hour_ago(1, 50.0, hours(3)),
        ];
        let thirty =
            recent_five_hour_over(60.0, &earlier, mins(30)).expect("2 h into a 5 h window");
        let two_hours =
            recent_five_hour_over(60.0, &earlier, hours(2)).expect("2 h into a 5 h window");
        assert_eq!(thirty.runs_out_at, Some(after(hours(2))));
        assert!(!thirty.lasts_to_reset);
        assert_eq!(two_hours.runs_out_at, Some(after(hours(4))));
        assert!(two_hours.lasts_to_reset);
    }

    #[test]
    fn recent_pace_lasts_at_exactly_full() {
        // 10 points in the last 30 min is 20 an hour: the 60 left reach 100 % exactly at the reset in 3 h.
        let full = recent_five_hour(40.0, &[half_hour_ago(1, 30.0, hours(3))])
            .expect("2 h into a 5 h window");
        assert!(
            full.lasts_to_reset,
            "a projection of exactly 100 % still lasts to the reset"
        );
        assert_eq!(
            full.runs_out_at,
            Some(after(hours(3))),
            "the run-out is the reset itself"
        );
    }

    #[test]
    fn an_unused_window_never_runs_out_after_a_reset() {
        // The window reset 12 min before the fetch and is unused; the previous window rose 20 points over the 18 min
        // before that reset, which a 30 min span still reads as a rise.
        let reset = window(0.0, FIVE_HOUR - mins(12));
        let current = reading(1, SignedDuration::ZERO, Some(reset), None);
        let before =
            |ago: i64, pct: f64| reading(1, -mins(ago), Some(window(pct, -mins(12))), None);
        let history = [before(30, 0.0), before(13, 20.0), current];
        let rates = RateSource {
            history: &history,
            span: mins(30),
        };
        let unused = recent_pace(FleetWindow::FiveHour, 1, reset, rates, fetched());
        assert!(unused.is_some(), "a window 12 min old has a pace");
        assert_eq!(unused, pace(0.0, reset.resets_at, fetched(), FIVE_HOUR));
        assert_eq!(unused.and_then(|unused| unused.runs_out_at), None);
    }

    #[test]
    fn recent_pace_projects_the_last_half_hour_rise() {
        // 10 points in the last 30 min is 20 an hour: the 40 left run out in 2 h, before the reset in 3 h. The average
        // since the window began, 60 % in 2 h, would run out in 1 h 20 min.
        let recent = recent_five_hour(60.0, &[half_hour_ago(1, 50.0, hours(3))])
            .expect("2 h into a 5 h window");
        assert_near(recent.expected_pct, 40.0);
        assert_eq!(recent.runs_out_at, Some(after(hours(2))));
        assert!(!recent.lasts_to_reset);
    }

    #[test]
    fn recent_pace_falls_back_to_since_start_under_ten_minutes_of_samples() {
        let since_start = pace_with(60.0, hours(3), FIVE_HOUR);
        assert!(since_start.is_some(), "2 h into a 5 h window has a pace");
        let nine_minutes = reading(1, -mins(9), Some(window(40.0, hours(3))), None);
        assert_eq!(
            recent_five_hour(60.0, &[nine_minutes]),
            since_start,
            "samples spanning 9 min"
        );
        assert_eq!(
            recent_five_hour(60.0, &[]),
            since_start,
            "only the reading itself"
        );
    }

    #[test]
    fn a_flat_half_hour_lasts_to_reset_even_near_full() {
        let flat = recent_five_hour(95.0, &[half_hour_ago(1, 95.0, hours(3))])
            .expect("2 h into a 5 h window");
        assert_near(flat.expected_pct, 40.0);
        assert_eq!(flat.runs_out_at, None);
        assert!(
            flat.lasts_to_reset,
            "no rise in 30 min lasts, whatever the average since the start says"
        );
    }

    #[test]
    fn recent_pace_at_or_over_100_runs_out_at_the_fetch() {
        for pct in [100.0, 104.0] {
            let used_up = recent_five_hour(pct, &[half_hour_ago(1, pct, hours(3))])
                .expect("2 h into a 5 h window");
            assert_eq!(
                used_up.runs_out_at,
                Some(fetched()),
                "{pct}% runs out at the fetch"
            );
            assert!(!used_up.lasts_to_reset, "{pct}% does not last");
        }
    }

    /// A window `pct` used that resets `remaining` after [`FETCHED`].
    fn window(pct: f64, remaining: SignedDuration) -> WindowReading {
        WindowReading {
            pct,
            resets_at: after(remaining),
        }
    }

    /// Account `account`'s reading fetched `offset` from [`FETCHED`], with its 5-hour and 7-day windows.
    fn reading(
        account: u32,
        offset: SignedDuration,
        five_hour: Option<WindowReading>,
        seven_day: Option<WindowReading>,
    ) -> AccountReading {
        AccountReading {
            account,
            fetched_at: after(offset),
            five_hour,
            seven_day,
            unstarted_five_hour: None,
        }
    }

    /// Account `account`'s reading at [`FETCHED`] with a 5-hour window `pct` used that has not started, so has no reset,
    /// and its 7-day window.
    fn unstarted(account: u32, pct: f64, seven_day: Option<WindowReading>) -> AccountReading {
        AccountReading {
            unstarted_five_hour: Some(pct),
            ..reading(account, SignedDuration::ZERO, None, seven_day)
        }
    }

    /// Account `account`'s reading at [`FETCHED`] with only a 5-hour window.
    fn five_only(account: u32, pct: f64, remaining: SignedDuration) -> AccountReading {
        reading(
            account,
            SignedDuration::ZERO,
            Some(window(pct, remaining)),
            None,
        )
    }

    /// The fleet at [`FETCHED`] over `readings` with no earlier history, under the default settings.
    fn fleet_now(readings: &[AccountReading]) -> Fleet {
        fleet_with(readings, &[])
    }

    /// The fleet at [`FETCHED`] over `readings` with a history of the `earlier` samples and the readings themselves,
    /// under the default settings.
    fn fleet_with(readings: &[AccountReading], earlier: &[AccountReading]) -> Fleet {
        fleet_under(readings, earlier, &AutoSwitchSettings::default())
    }

    /// The fleet at [`FETCHED`] over `readings` with a history of the `earlier` samples and the readings themselves,
    /// under `settings`, with no account named active and no switch recorded.
    fn fleet_under(
        readings: &[AccountReading],
        earlier: &[AccountReading],
        settings: &AutoSwitchSettings,
    ) -> Fleet {
        let rotation = Rotation {
            active: None,
            settings,
            state: &AutoswitchState::default(),
        };
        fleet_rotating(readings, earlier, rotation)
    }

    /// The fleet at [`FETCHED`] over `readings` with a history of the `earlier` samples and the readings themselves,
    /// along `rotation`.
    fn fleet_rotating(
        readings: &[AccountReading],
        earlier: &[AccountReading],
        rotation: Rotation<'_>,
    ) -> Fleet {
        fleet_spanning(readings, earlier, rotation, RATE_SPAN_30)
    }

    /// The fleet at [`FETCHED`] over `readings` with a history of the `earlier` samples and the readings themselves,
    /// along `rotation`, its rates read over `rate_span`.
    fn fleet_spanning(
        readings: &[AccountReading],
        earlier: &[AccountReading],
        rotation: Rotation<'_>,
        rate_span: SignedDuration,
    ) -> Fleet {
        let history: Vec<AccountReading> = earlier.iter().chain(readings).copied().collect();
        fleet(readings, &history, rotation, rate_span, fetched())
    }

    /// The fleet at [`FETCHED`] over `readings` and `earlier`, under the default settings, starting on `active` with
    /// the live `state`.
    fn fleet_live(
        readings: &[AccountReading],
        earlier: &[AccountReading],
        active: Option<u32>,
        state: &AutoswitchState,
    ) -> Fleet {
        let settings = AutoSwitchSettings::default();
        fleet_rotating(
            readings,
            earlier,
            Rotation {
                active,
                settings: &settings,
                state,
            },
        )
    }

    /// Account `account`'s sample `ago` before [`FETCHED`] with only a 5-hour window, `pct` used and resetting
    /// `remaining` after [`FETCHED`].
    fn sample(
        account: u32,
        ago: SignedDuration,
        pct: f64,
        remaining: SignedDuration,
    ) -> AccountReading {
        reading(account, -ago, Some(window(pct, remaining)), None)
    }

    fn verdict(fleet: &Fleet) -> WindowVerdict {
        fleet.verdict.clone().expect("the fleet has a verdict")
    }

    /// The pool's rate at [`FETCHED`] when account 1's current reading is `current` and its earlier samples `earlier`.
    fn rate_of(current: AccountReading, earlier: &[AccountReading]) -> f64 {
        verdict(&fleet_with(&[current], earlier)).rate
    }

    /// Account `account`'s sample 30 min before [`FETCHED`] with only a 5-hour window, `pct` used and resetting
    /// `remaining` after [`FETCHED`].
    fn half_hour_ago(account: u32, pct: f64, remaining: SignedDuration) -> AccountReading {
        reading(account, -mins(30), Some(window(pct, remaining)), None)
    }

    #[test]
    fn fleet_of_one_account_runs_dry_before_its_reset() {
        // 60 % in 2 h of 5: 30 points an hour, 40 left run out in 1 h 20 min, and the reset in 3 h brings it back.
        let dry = fleet_now(&[five_only(1, 60.0, hours(3))]);
        let judged = verdict(&dry);
        assert_eq!(judged.members, 1);
        assert_near(judged.rate, 30.0);
        assert_eq!(judged.blocked_at, Some(after(hours(1) + mins(20))));
        assert_eq!(judged.resumes_at, Some(after(hours(3))));
        assert_eq!(judged.first_reset, Some(after(hours(3))));
        assert!(judged.runs_dry);
        assert_eq!(
            dry.dry().map(|verdict| verdict.window),
            Some(FleetWindow::FiveHour)
        );

        // 30 % in 2 h of 5: 15 points an hour, 70 left last past the reset in 3 h, and each refill's 100 past the next.
        let lasts = fleet_now(&[five_only(1, 30.0, hours(3))]);
        assert_eq!(verdict(&lasts).blocked_at, None);
        assert_eq!(verdict(&lasts).resumes_at, None);
        assert!(!verdict(&lasts).runs_dry);
        assert_eq!(lasts.dry(), None);
    }

    #[test]
    fn fleet_pools_the_allowance_of_two_accounts() {
        // Account 1 alone runs dry in 1 h 20 min. At 30 points an hour it reaches the threshold, 90 %, at 1 h and the
        // engine moves to account 2, whose 90 points carry the pool past account 1's reset in 3 h; from then each account
        // spends 90 points in 3 h of its 5-hour window before the engine moves to the other, refilled one.
        let pooled = fleet_now(&[five_only(1, 60.0, hours(3)), five_only(2, 0.0, hours(4))]);
        let judged = verdict(&pooled);
        assert_eq!(judged.members, 2);
        assert_near(judged.remaining, 140.0);
        assert_near(judged.rate, 30.0);
        assert_eq!(judged.blocked_at, None);
        assert_eq!(
            judged.first_reset,
            Some(after(hours(3))),
            "the earliest member reset"
        );
        assert!(!judged.runs_dry);
    }

    #[test]
    fn fleet_runs_on_the_thirty_minute_rise_over_the_window_average() {
        // 60 % in 2 h of 5 is 30 points an hour, whose 40 left run out before the reset in 3 h; the last 30 min rose only
        // 5 points, 10 an hour, which the verdict runs on though it is the slower.
        let current = five_only(1, 60.0, hours(3));
        let judged = verdict(&fleet_with(&[current], &[half_hour_ago(1, 55.0, hours(3))]));
        assert_near(judged.rate, 10.0);
        assert!(!judged.runs_dry);
    }

    #[test]
    fn fleet_rate_falls_back_to_the_window_average_under_ten_minutes() {
        // 40 % in 2 h of 5 is 20 points an hour. A sample 9 min old is too short a span; one 10 min old gives its rise.
        let current = five_only(1, 40.0, hours(3));
        let nine = reading(1, -mins(9), Some(window(10.0, hours(3))), None);
        assert_near(rate_of(current, &[nine]), 20.0);
        assert_near(rate_of(current, &[]), 20.0);
        let ten = reading(1, -mins(10), Some(window(10.0, hours(3))), None);
        assert_near(rate_of(current, &[ten]), 180.0);
    }

    #[test]
    fn fleet_rate_is_the_summed_thirty_minute_rise() {
        // Account 1 rose 30 points in 30 min (its sample 40 min old is past the span); account 2 rose 10 in the 20 min
        // since its oldest sample: 60 + 30 an hour, where the window averages would give 20 + 5.
        let one = five_only(1, 40.0, hours(3));
        let two = five_only(2, 10.0, hours(3));
        let earlier = [
            reading(1, -mins(40), Some(window(0.0, hours(3))), None),
            half_hour_ago(1, 10.0, hours(3)),
            reading(2, -mins(20), Some(window(0.0, hours(3))), None),
            reading(2, -mins(10), Some(window(5.0, hours(3))), None),
        ];
        assert_near(verdict(&fleet_with(&[one, two], &earlier)).rate, 90.0);
    }

    #[test]
    fn fleet_rate_counts_no_fall_across_a_reset() {
        // 90 % in the window before, a reset 20 min ago, then 5 % rising to 20 %: only the 15-point rise counts, over the
        // 29 min since the oldest sample.
        let current = five_only(1, 20.0, hours(4) + mins(40));
        let earlier = [
            reading(1, -mins(29), Some(window(90.0, -mins(20))), None),
            reading(1, -mins(10), Some(window(5.0, hours(4) + mins(40))), None),
        ];
        assert_near(rate_of(current, &earlier), 15.0 * 60.0 / 29.0);
    }

    #[test]
    fn fleet_with_no_pool_member_has_no_verdict() {
        assert_eq!(fleet_now(&[]), Fleet::default());
        let past = five_only(1, 50.0, -mins(1));
        let beyond = five_only(2, 50.0, FIVE_HOUR + SignedDuration::from_secs(1));
        let none = fleet_now(&[past, beyond]);
        assert_eq!(
            none.verdict, None,
            "a reset past or more than a window away is no member"
        );
        assert_eq!(none.dry(), None);
    }

    #[test]
    fn fleet_leaves_an_account_whose_seven_day_is_spent_out_of_the_five_hour_pool() {
        let spent = reading(
            1,
            SignedDuration::ZERO,
            Some(window(50.0, hours(3))),
            Some(window(100.0, hours(48))),
        );
        let open = reading(
            2,
            SignedDuration::ZERO,
            Some(window(10.0, hours(3))),
            Some(window(10.0, hours(48))),
        );
        let judged = verdict(&fleet_now(&[spent, open]));
        assert_eq!(
            judged.members, 1,
            "account 1's 5-hour allowance cannot be spent"
        );
        assert_eq!(
            judged.accounts, 2,
            "account 1 is still on the timeline, back at its 7-day reset"
        );
        assert_near(judged.remaining, 90.0);
        assert_near(judged.rate, 5.0);
        assert!(!judged.runs_dry);
    }

    #[test]
    fn fleet_with_every_member_spent_is_blocked_now() {
        let judged = verdict(&fleet_now(&[
            five_only(1, 100.0, hours(3)),
            five_only(2, 104.0, hours(2)),
        ]));
        assert_near(judged.remaining, 0.0);
        assert_eq!(judged.blocked_at, Some(fetched()));
        assert_eq!(judged.resumes_at, Some(after(hours(2))));
        assert_eq!(judged.first_reset, Some(after(hours(2))));
        assert!(judged.runs_dry);
    }

    #[test]
    fn fleet_with_no_use_does_not_run_dry() {
        let judged = verdict(&fleet_now(&[five_only(1, 0.0, hours(3))]));
        assert_near(judged.rate, 0.0);
        assert_eq!(judged.blocked_at, None);
        assert!(!judged.runs_dry);
    }

    #[test]
    fn fleet_blocked_at_the_reset_itself_does_not_run_dry() {
        // 40 % in 2 h of 5: 60 points left at 20 an hour run out at the reset in 3 h, not before it, and each refill's 100
        // at the next reset.
        let judged = verdict(&fleet_now(&[five_only(1, 40.0, hours(3))]));
        assert_eq!(judged.blocked_at, None);
        assert!(!judged.runs_dry);
    }

    #[test]
    fn fleet_window_rate_skips_a_window_younger_than_ten_minutes() {
        let young = five_only(1, 50.0, FIVE_HOUR - mins(9));
        let judged = verdict(&fleet_now(&[young, five_only(2, 50.0, hours(3))]));
        assert_eq!(
            judged.members, 2,
            "the young window still pools its allowance"
        );
        assert_near(judged.rate, 25.0);
    }

    #[test]
    fn fleet_skips_a_non_finite_pct() {
        for pct in [f64::NAN, f64::INFINITY] {
            let judged = verdict(&fleet_now(&[
                five_only(1, pct, hours(3)),
                five_only(2, 30.0, hours(3)),
            ]));
            assert_eq!(judged.members, 1, "{pct} is not a reading");
            assert_near(judged.remaining, 70.0);
        }
    }

    #[test]
    fn fleet_finds_the_gap_after_the_first_reset() {
        // The issue's example: 15 points left, all account 1's, at 60 an hour (its last 30 min rose 30); accounts 2 and 3
        // are spent and idle until their resets in 4 h. Account 1's reset at 10 min loses its 5 points left and refills
        // 100, which run out at 1 h 50 min; the pool is back at 4 h.
        let readings = [
            five_only(1, 85.0, mins(10)),
            five_only(2, 100.0, hours(4)),
            five_only(3, 100.0, hours(4)),
        ];
        let earlier = [
            half_hour_ago(1, 55.0, mins(10)),
            half_hour_ago(2, 100.0, hours(4)),
            half_hour_ago(3, 100.0, hours(4)),
        ];
        let judged = verdict(&fleet_with(&readings, &earlier));
        assert_near(judged.remaining, 15.0);
        assert_near(judged.rate, 60.0);
        assert_eq!(judged.first_reset, Some(after(mins(10))));
        assert_eq!(judged.blocked_at, Some(after(hours(1) + mins(50))));
        assert_eq!(judged.resumes_at, Some(after(hours(4))));
        assert_eq!(judged.window, FleetWindow::FiveHour);
        assert!(judged.runs_dry);
    }

    #[test]
    fn fleet_lasts_to_the_horizon_when_refills_cover_the_burn() {
        // The issue's example at 40 an hour with accounts 2 and 3 unused: each window's 100 points last 2 h 30 min, so two
        // refills cover every 5 hours to the end of the 7 days.
        let readings = [
            five_only(1, 85.0, mins(10)),
            five_only(2, 0.0, hours(4)),
            five_only(3, 0.0, hours(4)),
        ];
        let earlier = [
            half_hour_ago(1, 65.0, mins(10)),
            half_hour_ago(2, 0.0, hours(4)),
            half_hour_ago(3, 0.0, hours(4)),
        ];
        let judged = verdict(&fleet_with(&readings, &earlier));
        assert_near(judged.rate, 40.0);
        assert_eq!(judged.blocked_at, None);
        assert_eq!(judged.resumes_at, None);
        assert!(!judged.runs_dry);
    }

    #[test]
    fn fleet_caps_a_five_hour_refill_at_the_seven_day_run_out() {
        // 5-hour: 40 points at 30 an hour, out in 1 h 20 min. 7-day: 94 % with no measured ratio leaves 6 / 0.2 = 30
        // points to draw, gone at 1 h: the week runs out first, so the refill at the 5-hour reset in 3 h counts for
        // nothing and the account is back at its 7-day reset.
        let capped = reading(
            1,
            SignedDuration::ZERO,
            Some(window(60.0, hours(3))),
            Some(window(94.0, hours(6 * 24))),
        );
        let judged = verdict(&fleet_now(&[capped]));
        assert_eq!(judged.blocked_at, Some(after(hours(1))));
        assert_eq!(judged.resumes_at, Some(after(hours(6 * 24))));
        assert_eq!(
            judged.window,
            FleetWindow::SevenDay,
            "the gap starts at the 7-day run-out"
        );
        assert!(judged.runs_dry);

        // Without the 7-day window the 5-hour run-out at 1 h 20 min lasts until the refill at 3 h.
        let uncapped = verdict(&fleet_now(&[five_only(1, 60.0, hours(3))]));
        assert_eq!(uncapped.blocked_at, Some(after(hours(1) + mins(20))));
        assert_eq!(uncapped.resumes_at, Some(after(hours(3))));
    }

    #[test]
    fn fleet_ignores_a_gap_shorter_than_five_minutes() {
        // Account 1's 28 points at 30 an hour run out at 56 min; account 2 is spent. With both resets at 1 h the 4 min gap
        // is passed over, and from then the two refills, 100 points each per 5 h, outlast 30 an hour.
        let judged_at = |reset: SignedDuration| {
            let readings = [five_only(1, 72.0, reset), five_only(2, 100.0, reset)];
            verdict(&fleet_with(
                &readings,
                &[
                    half_hour_ago(1, 57.0, reset),
                    half_hour_ago(2, 100.0, reset),
                ],
            ))
        };
        let blip = judged_at(hours(1));
        assert_eq!(blip.blocked_at, None);
        assert!(!blip.runs_dry);

        // With both resets at 61 min the gap is 5 min, which counts.
        let gap = judged_at(mins(61));
        assert_eq!(gap.blocked_at, Some(after(mins(56))));
        assert_eq!(gap.resumes_at, Some(after(mins(61))));
        assert!(gap.runs_dry);
    }

    #[test]
    fn fleet_reports_the_first_gap_of_several() {
        // 20 points at 60 an hour run out at 20 min, back at the reset at 30 min. That refill's 100 run out at 2 h 10 min,
        // back 5 h after the draw at 5 h 30 min: the first gap is reported, though the second is longer. The timeline runs
        // on past it, so each 5 h from then brings 100 points, spent in 1 h 40 min: a gap of 3 h 20 min, the last one
        // ending past the horizon.
        let judged = verdict(&fleet_with(
            &[five_only(1, 80.0, mins(30))],
            &[half_hour_ago(1, 50.0, mins(30))],
        ));
        assert_eq!(judged.blocked_at, Some(after(mins(20))));
        assert_eq!(judged.resumes_at, Some(after(mins(30))));
        let mut expected = vec![FleetGap {
            start: after(mins(20)),
            end: Some(after(mins(30))),
            window: FleetWindow::FiveHour,
        }];
        expected.extend((0..34).map(|cycle| {
            let start = mins(130) + hours(5 * cycle);
            let end = start + hours(3) + mins(20);
            FleetGap {
                start: after(start),
                end: (end < SEVEN_DAY).then(|| after(end)),
                window: FleetWindow::FiveHour,
            }
        }));
        assert_eq!(judged.gaps, expected);
    }

    #[test]
    fn fleet_gaps_skip_a_gap_shorter_than_five_minutes() {
        // The 4 min gap of `fleet_ignores_a_gap_shorter_than_five_minutes` is passed over, and the fleet lasts from then
        // on, so no gap is listed; with the refills at 61 min the gap is 5 min, which counts, and it is the first.
        let judged_at = |reset: SignedDuration| {
            let readings = [five_only(1, 72.0, reset), five_only(2, 100.0, reset)];
            verdict(&fleet_with(
                &readings,
                &[
                    half_hour_ago(1, 57.0, reset),
                    half_hour_ago(2, 100.0, reset),
                ],
            ))
        };
        let blip = judged_at(hours(1));
        assert!(
            blip.gaps.is_empty(),
            "the 4 min gap is passed over: {blip:?}"
        );
        let counted = judged_at(mins(61));
        assert_eq!(
            counted.gaps.first(),
            Some(&FleetGap {
                start: after(mins(56)),
                end: Some(after(mins(61))),
                window: FleetWindow::FiveHour,
            }),
            "{counted:?}"
        );
        for gap in &counted.gaps {
            if let Some(end) = gap.end {
                assert!(
                    end.duration_since(gap.start).as_secs_f64() >= MIN_GAP_SECS,
                    "a gap of at least 5 min: {gap:?}"
                );
            }
        }
    }

    #[test]
    fn a_lasting_fleet_has_no_gaps() {
        let readings = [
            five_only(1, 85.0, mins(10)),
            five_only(2, 0.0, hours(4)),
            five_only(3, 0.0, hours(4)),
        ];
        let earlier = [
            half_hour_ago(1, 65.0, mins(10)),
            half_hour_ago(2, 0.0, hours(4)),
            half_hour_ago(3, 0.0, hours(4)),
        ];
        let judged = verdict(&fleet_with(&readings, &earlier));
        assert!(
            judged.gaps.is_empty(),
            "a lasting fleet has no gaps: {judged:?}"
        );
    }

    #[test]
    fn fleet_reports_seven_day_when_every_account_runs_out_its_week() {
        // Each 5-hour window is half used at 25 an hour, 50 for the pool. With no measured ratio, account 1's 7-day
        // window at 99 % leaves 1 / 0.2 = 5 points to draw and account 2's at 98 % leaves 10: 15 points, gone at 18 min,
        // then neither can be drawn on until 6 d.
        let one = reading(
            1,
            SignedDuration::ZERO,
            Some(window(50.0, hours(3))),
            Some(window(99.0, hours(6 * 24))),
        );
        let two = reading(
            2,
            SignedDuration::ZERO,
            Some(window(50.0, hours(3))),
            Some(window(98.0, hours(6 * 24))),
        );
        let judged = verdict(&fleet_now(&[one, two]));
        assert_eq!(judged.window, FleetWindow::SevenDay);
        assert_eq!(judged.blocked_at, Some(after(mins(18))));
        assert_eq!(judged.resumes_at, Some(after(hours(6 * 24))));

        // Every 7-day window already spent: no pool member, blocked now, and back at the earliest return. Account 2's
        // 5-hour window is spent too, so it is back at the later of its 7-day reset at 30 h and its 5-hour one at 3 h.
        let spent = [
            reading(
                1,
                SignedDuration::ZERO,
                Some(window(10.0, hours(3))),
                Some(window(100.0, hours(48))),
            ),
            reading(
                2,
                SignedDuration::ZERO,
                Some(window(100.0, hours(3))),
                Some(window(100.0, hours(30))),
            ),
        ];
        let blocked = verdict(&fleet_now(&spent));
        assert_eq!(blocked.members, 0);
        assert_eq!(blocked.accounts, 2);
        assert_eq!(blocked.window, FleetWindow::SevenDay);
        assert_eq!(blocked.blocked_at, Some(fetched()));
        assert_eq!(blocked.resumes_at, Some(after(hours(30))));
        assert_eq!(blocked.first_reset, Some(after(hours(30))));
        assert!(blocked.runs_dry);
    }

    #[test]
    fn fleet_refills_a_week_at_its_seven_day_reset() {
        // 60 % in 2 h of 5: 30 points an hour. The 7-day window at 99 % with no measured ratio leaves 1 / 0.2 = 5 points
        // to draw, spent at 10 min with the 5-hour window at 65 %; its reset at 12 min, 2 min later, gives a whole week
        // back. The 5-hour window's last 35 points then run out at 1 h 22 min, past that reset, back at the 5-hour reset
        // in 3 h.
        let one = reading(
            1,
            SignedDuration::ZERO,
            Some(window(60.0, hours(3))),
            Some(window(99.0, mins(12))),
        );
        let judged = verdict(&fleet_now(&[one]));
        assert_eq!(judged.blocked_at, Some(after(hours(1) + mins(22))));
        assert_eq!(judged.resumes_at, Some(after(hours(3))));
        assert_eq!(judged.window, FleetWindow::FiveHour);
        assert!(judged.runs_dry);
    }

    #[test]
    fn fleet_runs_the_whole_week() {
        // 10 % in 2 h of 5: 5 points an hour, which the 5-hour refills cover. The 7-day window at 70 % leaves 30 / 0.2 =
        // 150 points to draw, past its reset in 1 d at 120; that reset gives 500 points, spent 100 h later at 5 d 4 h,
        // before the 7 days are out, and the account is back at its next 7-day reset, 7 d after the first, at 8 d.
        let one = reading(
            1,
            SignedDuration::ZERO,
            Some(window(10.0, hours(3))),
            Some(window(70.0, hours(24))),
        );
        let judged = verdict(&fleet_now(&[one]));
        assert_eq!(judged.blocked_at, Some(after(hours(5 * 24 + 4))));
        assert_eq!(judged.resumes_at, Some(after(hours(8 * 24))));
        assert_eq!(judged.window, FleetWindow::SevenDay);
        assert!(judged.runs_dry);
    }

    /// The fleet of account 1, its 7-day window spent until its reset in 1 d, read as `spent`, and account 2, in use.
    /// Account 2 draws 10 % in 2 h of 5: 5 points an hour, which the 5-hour refills cover; its 7-day window at 74 %
    /// leaves 26 / 0.2 = 130 points to draw, gone at 1 d 2 h, and resets at 6 d.
    fn beside_a_spent_week(spent: AccountReading) -> WindowVerdict {
        let two = reading(
            2,
            SignedDuration::ZERO,
            Some(window(10.0, hours(3))),
            Some(window(74.0, hours(6 * 24))),
        );
        verdict(&fleet_now(&[spent, two]))
    }

    #[test]
    fn fleet_brings_back_a_spent_week_at_its_reset() {
        // Account 1's 5-hour window has not started. Its 7-day reset at 1 d gives it a whole week, 500 points, drawable
        // from then on: however the engine moves, account 2's 130 and account 1's 500 run out at 5 d 6 h, back at account
        // 2's 7-day reset at 6 d. Without the refill the gap would start when account 2 runs out at 1 d 2 h.
        let judged = beside_a_spent_week(unstarted(1, 0.0, Some(window(100.0, hours(24)))));
        assert_eq!(judged.members, 1);
        assert_eq!(judged.accounts, 2);
        assert_eq!(judged.blocked_at, Some(after(hours(5 * 24 + 6))));
        assert_eq!(judged.resumes_at, Some(after(hours(6 * 24))));
        assert_eq!(judged.window, FleetWindow::SevenDay);
    }

    #[test]
    fn fleet_brings_back_a_spent_week_without_a_five_hour_reading_at_its_reset() {
        // As above with no 5-hour reading for account 1 at all: with no 5-hour window running, its week's refill at 1 d
        // leaves it fresh, and the gap starts at 5 d 6 h.
        let judged = beside_a_spent_week(reading(
            1,
            SignedDuration::ZERO,
            None,
            Some(window(100.0, hours(24))),
        ));
        assert_eq!(judged.accounts, 2);
        assert_eq!(judged.blocked_at, Some(after(hours(5 * 24 + 6))));
        assert_eq!(judged.resumes_at, Some(after(hours(6 * 24))));
        assert_eq!(judged.window, FleetWindow::SevenDay);
    }

    /// Account `account`'s reading at [`FETCHED`] with no 5-hour limit: only a 7-day window `pct` used that resets
    /// `remaining` after [`FETCHED`].
    fn week_only(account: u32, pct: f64, remaining: SignedDuration) -> AccountReading {
        reading(
            account,
            SignedDuration::ZERO,
            None,
            Some(window(pct, remaining)),
        )
    }

    /// Account `account`'s sample 30 min before [`FETCHED`] with only a 7-day window, `pct` used and resetting
    /// `remaining` after [`FETCHED`].
    fn week_half_hour_ago(account: u32, pct: f64, remaining: SignedDuration) -> AccountReading {
        reading(account, -mins(30), None, Some(window(pct, remaining)))
    }

    #[test]
    fn fleet_of_only_seven_day_only_accounts_has_a_verdict() {
        // Neither account has a 5-hour limit: their weeks at 10 % and 20 % leave 90 and 80 points, 450 and 400 to draw
        // at the default ratio of 0.2.
        let judged = verdict(&fleet_now(&[
            week_only(1, 10.0, hours(48)),
            week_only(2, 20.0, hours(48)),
        ]));
        assert_eq!(judged.members, 2);
        assert_eq!(judged.accounts, 2);
        assert_eq!(judged.rate_window, FleetWindow::SevenDay);
        assert_near(judged.remaining, (90.0 + 80.0) / 0.2);
    }

    #[test]
    fn a_seven_day_only_account_runs_out_at_its_weekly_rate() {
        // Its week rose 5 points in 30 min, 10 an hour, which is 50 five-hour points an hour at the ratio of 0.2: its 50
        // points left, 250 to draw, run out at 5 h, and its reset at 48 h brings it back.
        let current = week_only(1, 50.0, hours(48));
        let judged = verdict(&fleet_with(
            &[current],
            &[week_half_hour_ago(1, 45.0, hours(48))],
        ));
        assert_near(judged.rate, 10.0);
        assert_eq!(judged.blocked_at, Some(after(hours(5))));
        assert_eq!(judged.resumes_at, Some(after(hours(48))));
        assert!(judged.runs_dry);
        assert_eq!(
            judged.gaps.first().map(|gap| gap.window),
            Some(FleetWindow::SevenDay)
        );

        // 0.25 points in 30 min is 0.5 an hour: its 50 points last 100 h, past its reset at 48 h, and the refilled 100
        // last 200 h, past the 7 days.
        let slow = verdict(&fleet_with(
            &[current],
            &[week_half_hour_ago(1, 49.75, hours(48))],
        ));
        assert!(!slow.runs_dry);
    }

    #[test]
    fn a_seven_day_only_account_is_never_stopped_by_a_five_hour_run_out() {
        // 3 points in 30 min is 6 an hour, 30 five-hour points an hour at the ratio of 0.2, which would spend a 5-hour
        // window in 3 h 20 min. With none, its 97 points left, 485 to draw, run out at 16 h 10 min.
        let current = week_only(1, 3.0, hours(100));
        let judged = verdict(&fleet_with(
            &[current],
            &[week_half_hour_ago(1, 0.0, hours(100))],
        ));
        assert_eq!(judged.blocked_at, Some(after(hours(16) + mins(10))));
        assert_eq!(judged.resumes_at, Some(after(hours(100))));
    }

    #[test]
    fn a_mixed_fleet_counts_both_kinds_and_moves_onto_the_seven_day_only_account() {
        // At 30 an hour (account 1's 60 % in 2 h of 5) and the ratio of 0.2. Account 1 has 40 five-hour points and a
        // week of 90 / 0.2 = 450 to draw; account 2, with no 5-hour limit, a week of 500.
        // - 0 to 1 h on account 1, to 90 %; the engine moves to account 2. Account 1's week has 420 left; its 5-hour
        //   window refills at 3 h.
        // - 1 h to 16 h on account 2, 450 drawn, its week at 90 %: the engine moves back to account 1 (week at 16 %).
        // - 16 h to 19 h 20 min on account 1, a new 5-hour window to 21 h: 100 drawn, week 320 left. Account 2 at 90 %
        //   is no proactive target, so account 1 runs to 100 % and escapes to account 2.
        // - 19 h 20 min to 21 h on account 2: its last 50, its week spent as account 1's window refills.
        // - 21 h to 24 h 20 min on account 1, a new window to 26 h: 100 drawn; account 2 has nothing, so the gap starts
        //   at 24 h 20 min and ends at account 1's reset at 26 h.
        let one = reading(
            1,
            SignedDuration::ZERO,
            Some(window(60.0, hours(3))),
            Some(window(10.0, hours(100))),
        );
        let judged = verdict(&fleet_now(&[one, week_only(2, 0.0, hours(100))]));
        assert_eq!(judged.members, 2);
        assert_eq!(judged.accounts, 2);
        assert_eq!(judged.rate_window, FleetWindow::FiveHour);
        assert!(
            judged.blocked_at.is_some_and(|at| at > after(hours(20))),
            "later than account 1 alone"
        );
        assert_eq!(judged.blocked_at, Some(after(hours(24) + mins(20))));
        assert_eq!(judged.resumes_at, Some(after(hours(26))));
        assert_eq!(judged.window, FleetWindow::FiveHour);
    }

    #[test]
    fn a_seven_day_only_account_with_a_spent_week_is_back_at_its_weekly_reset() {
        // Account 1, with no 5-hour limit, has spent its week until its reset at 24 h. Account 2, also 7-day-only, rose 3
        // points in 30 min: 6 an hour, the pool's 30 five-hour points an hour at the ratio of 0.2. Its 50 points left,
        // 250 to draw, run out at 8 h 20 min; account 1 is back at 24 h with a whole week, 500 to draw, gone at
        // 40 h 40 min with no 5-hour window stopping it (one would at 3 h 20 min). Account 2 is back at 144 h.
        let spent = week_only(1, 100.0, hours(24));
        let two = week_only(2, 50.0, hours(144));
        let judged = verdict(&fleet_with(
            &[spent, two],
            &[week_half_hour_ago(2, 47.0, hours(144))],
        ));
        assert_eq!(judged.members, 1);
        assert_eq!(judged.accounts, 2);
        assert_eq!(judged.blocked_at, Some(after(hours(8) + mins(20))));
        assert_eq!(judged.resumes_at, Some(after(hours(24))));
        assert_eq!(
            judged.gaps.get(1),
            Some(&FleetGap {
                start: after(hours(40) + mins(40)),
                end: Some(after(hours(144))),
                window: FleetWindow::SevenDay,
            })
        );
    }

    #[test]
    fn a_fleet_that_just_moved_off_a_spent_account_keeps_its_burn() {
        // Account 6 reached 100 % in the last hour, 4 of its points inside the span, and the engine moved to account 5,
        // which rose 3: 7 points an hour. That is 1176 points over the 7 days against the 726 the seven accounts have
        // (26 left and 700 in refills), so the fleet runs dry; account 5's 3 points an hour alone would last.
        let readings = [
            week_only(1, 95.0, hours(123)),
            week_only(2, 95.0, hours(61)),
            week_only(3, 95.0, hours(127)),
            week_only(4, 95.0, hours(22)),
            week_only(5, 94.0, hours(2)),
            week_only(6, 100.0, hours(18)),
            week_only(7, 95.0, hours(83)),
        ];
        let flat = [
            (1, 95.0, 123),
            (2, 95.0, 61),
            (3, 95.0, 127),
            (4, 95.0, 22),
            (5, 91.0, 2),
            (6, 96.0, 18),
            (7, 95.0, 83),
        ];
        let earlier = flat.map(|(account, pct, remaining)| {
            reading(
                account,
                -hours(1),
                None,
                Some(window(pct, hours(remaining))),
            )
        });
        let settings = AutoSwitchSettings::default();
        let rotation = Rotation {
            active: Some(5),
            settings: &settings,
            state: &AutoswitchState::default(),
        };
        let judged = verdict(&fleet_spanning(&readings, &earlier, rotation, hours(1)));
        assert_eq!(judged.members, 6, "account 6 still brings no allowance");
        assert_near(judged.rate, 7.0);
        assert!(judged.runs_dry);
    }

    #[test]
    fn fleet_rate_reads_a_spent_week_on_its_running_five_hour_window() {
        // Account 1's 5-hour window rose 20 points in 30 min before its week ran out: 40 an hour.
        let spent = reading(
            1,
            SignedDuration::ZERO,
            Some(window(60.0, hours(3))),
            Some(window(100.0, hours(24))),
        );
        let earlier = [half_hour_ago(1, 40.0, hours(3))];
        let judged = verdict(&fleet_with(&[spent], &earlier));
        assert_eq!(judged.members, 0);
        assert_near(judged.rate, 40.0);
    }

    #[test]
    fn fleet_rate_reads_a_spent_week_with_no_running_five_hour_window_on_that_week() {
        // Account 1's week rose 1 point in 30 min: 2 an hour, 10 five-hour points an hour at the ratio of 0.2.
        let spent = unstarted(1, 0.0, Some(window(100.0, hours(24))));
        let earlier = [week_half_hour_ago(1, 99.0, hours(24))];
        let judged = verdict(&fleet_with(&[spent], &earlier));
        assert_eq!(judged.rate_window, FleetWindow::FiveHour);
        assert_near(judged.rate, 10.0);
    }

    #[test]
    fn fleet_rate_keeps_the_rise_of_a_five_hour_window_that_reset_in_the_span() {
        // Account 1's 5-hour window rose 10 points in the 20 min before it reset, and reads as not started now: 20 an hour
        // over the 30 min, not the nothing a window not yet started has used.
        let current = unstarted(1, 0.0, Some(window(50.0, hours(100))));
        let earlier = [
            half_hour_ago(1, 60.0, -mins(5)),
            reading(1, -mins(10), Some(window(70.0, -mins(5))), None),
        ];
        assert_near(verdict(&fleet_with(&[current], &earlier)).rate, 20.0);
    }

    #[test]
    fn fleet_keeps_a_spent_week_whose_reset_has_passed_back_now() {
        // Account 1's 7-day window reads 100 % with its reset a minute gone, a stale reading: it is back now with a whole
        // week to draw, so it stays on the timeline, its reset read as now. Account 2, its 5-hour window not yet
        // started, is the pool member.
        let spent = unstarted(1, 0.0, Some(window(100.0, -mins(1))));
        let judged = verdict(&fleet_now(&[
            spent,
            unstarted(2, 0.0, Some(window(10.0, hours(6 * 24)))),
        ]));
        assert_eq!(
            judged.accounts, 2,
            "account 1 is on the timeline, back at its 7-day reset"
        );
        let lane = super::lane_of(&spent, judged.ratio, fetched()).expect("the spent week's lane");
        assert_eq!(
            lane.week_reset(),
            Some(0.0),
            "a reset at or before now reads as now"
        );
        assert_eq!(
            judged.first_reset,
            Some(fetched()),
            "its reset counts, read as now"
        );
    }

    #[test]
    fn fleet_keeps_a_spent_week_whose_reset_is_past_seven_days_at_the_horizon() {
        // Account 1's 7-day window reads 100 % with its reset 8 d out, past the 7 d the timeline runs: it is held to the
        // horizon, so it stays on the timeline and comes back at the end of the 7 days. Account 2, its 5-hour window not
        // yet started, is the pool member.
        let spent = unstarted(1, 0.0, Some(window(100.0, hours(8 * 24))));
        let judged = verdict(&fleet_now(&[
            spent,
            unstarted(2, 0.0, Some(window(10.0, hours(6 * 24)))),
        ]));
        assert_eq!(
            judged.accounts, 2,
            "account 1 is on the timeline, back at the horizon"
        );
        let lane = super::lane_of(&spent, judged.ratio, fetched()).expect("the spent week's lane");
        assert_eq!(
            lane.week_reset(),
            Some(SEVEN_DAY.as_secs_f64()),
            "a reset past the horizon reads as the horizon"
        );
        assert_eq!(
            judged.first_reset,
            Some(after(SEVEN_DAY)),
            "its reset counts, read as the horizon"
        );
    }

    #[test]
    fn fleet_gap_that_ends_past_the_horizon_has_no_end() {
        // Both accounts' 7-day windows at 99 % leave 1 / 0.2 = 5 points each, gone at 5 min at the pool's 120 an hour: the
        // single gap runs from there to the 7-day horizon, where both weeks reset, so the verdict's `resumes_at` is that
        // raw end while the listed gap, ending past the horizon, carries no end.
        let spent = |account: u32, offset: SignedDuration, pct: f64| {
            reading(
                account,
                offset,
                Some(window(pct, hours(3))),
                Some(window(99.0, hours(8 * 24))),
            )
        };
        let judged = verdict(&fleet_with(
            &[
                spent(1, SignedDuration::ZERO, 90.0),
                spent(2, SignedDuration::ZERO, 90.0),
            ],
            &[spent(1, -mins(30), 60.0), spent(2, -mins(30), 60.0)],
        ));
        assert_eq!(judged.blocked_at, Some(after(mins(5))));
        assert_eq!(
            judged.resumes_at,
            Some(after(SEVEN_DAY)),
            "the raw end, past the horizon"
        );
        assert_eq!(judged.gaps.len(), 1, "{judged:?}");
        let gap = judged.gaps.first().expect("the fleet has a gap");
        assert_eq!(Some(gap.start), judged.blocked_at);
        assert_eq!(gap.end, None);
        assert_eq!(gap.window, FleetWindow::SevenDay);
    }

    #[test]
    fn fleet_clamps_a_members_passed_seven_day_reset_to_now() {
        // Account 1's 5-hour window is 10 % used in 2 h of 5: 5 points an hour, which the 5-hour refills cover. Its 7-day
        // window reads 99 % with its reset 30 min gone, a reading up to 1 h stale: held to now, it has its whole week back
        // at once, 500 points at the 0.2 ratio, gone at 100 h, and it is back at its next 7-day reset, 7 d out. Unheld,
        // its week would be no limit at all and the pool would last to the end of the 7 days.
        let member = reading(
            1,
            SignedDuration::ZERO,
            Some(window(10.0, hours(3))),
            Some(window(99.0, -mins(30))),
        );
        let judged = verdict(&fleet_now(&[member]));
        assert_eq!(judged.accounts, 1);
        assert_eq!(judged.blocked_at, Some(after(hours(100))));
        assert_eq!(judged.resumes_at, Some(after(SEVEN_DAY)));
        assert_eq!(judged.window, FleetWindow::SevenDay);
        assert!(judged.runs_dry);
    }

    #[test]
    fn fleet_clamps_a_members_seven_day_reset_past_the_horizon() {
        // Account 1 rose 30 points in the 30 min before now, 60 points an hour, on a 5-hour window with 40 points left.
        // Its 7-day window reads 99 % with its reset 10 min past the 7 days: held to the horizon, it has 1 / 0.2 = 5
        // points of week left, drawn out at 5 min, and the account is back at that horizon, not unlimited.
        let member = reading(
            1,
            SignedDuration::ZERO,
            Some(window(60.0, hours(3))),
            Some(window(99.0, hours(7 * 24) + mins(10))),
        );
        let judged = verdict(&fleet_with(&[member], &[half_hour_ago(1, 30.0, hours(3))]));
        assert_eq!(judged.members, 1);
        assert_near(judged.rate, 60.0);
        assert_eq!(judged.blocked_at, Some(after(mins(5))));
        assert_eq!(judged.resumes_at, Some(after(SEVEN_DAY)));
        assert_eq!(judged.window, FleetWindow::SevenDay);
        assert!(judged.runs_dry);
    }

    #[test]
    fn fleet_resets_a_refilled_week_seven_days_after_its_reset() {
        // Account 2 draws 20 % in 2 h of 5: 10 points an hour, which the 5-hour refills cover. Its 7-day window at 52 %
        // leaves 48 / 0.2 = 240 points to draw, spent at 1 d. Account 1's week is spent until its reset at 1 d, which
        // gives it a whole week, 500 points, and account 2's reset at 1 d 1 h gives it 500 more: however the engine moves
        // between them, the 1000 points run out 100 h after 1 d, at 5 d 4 h. Account 1 is back at its next 7-day reset,
        // 7 d after the first, at 8 d, before account 2's at 8 d 1 h.
        let one = reading(
            1,
            SignedDuration::ZERO,
            Some(window(0.0, hours(3))),
            Some(window(100.0, hours(24))),
        );
        let two = reading(
            2,
            SignedDuration::ZERO,
            Some(window(20.0, hours(3))),
            Some(window(52.0, hours(25))),
        );
        let judged = verdict(&fleet_now(&[one, two]));
        assert_eq!(judged.members, 1);
        assert_eq!(judged.accounts, 2);
        assert_eq!(judged.blocked_at, Some(after(hours(5 * 24 + 4))));
        assert_eq!(judged.resumes_at, Some(after(hours(8 * 24))));
        assert_eq!(judged.window, FleetWindow::SevenDay);
        assert!(judged.runs_dry);
    }

    #[test]
    fn fleet_next_five_hour_reset_counts_from_the_first_draw() {
        // At 60 an hour (account 1 rose 30 in 30 min). Account 1 is spent until its reset at 10 min, so the engine leaves
        // it at once for account 2, whose 7-day window at 82 % leaves 18 / 0.2 = 90 points to draw and reaches the
        // threshold, 90 %, at 40 min. The engine moves to account 1, refilled since 10 min and first drawn on now: its next
        // reset is 5 h after that draw, at 5 h 40 min, not 5 h after the refill. However the engine moves after, account
        // 1's 100 points and account 2's last 50 run out at 3 h 10 min, account 2 with its week spent, so account 1's
        // reset ends the gap.
        let one = five_only(1, 100.0, mins(10));
        let two = reading(
            2,
            SignedDuration::ZERO,
            Some(window(0.0, hours(4))),
            Some(window(82.0, hours(6 * 24))),
        );
        let earlier = [
            half_hour_ago(1, 70.0, mins(10)),
            reading(
                2,
                -mins(30),
                Some(window(0.0, hours(4))),
                Some(window(82.0, hours(6 * 24))),
            ),
        ];
        let judged = verdict(&fleet_with(&[one, two], &earlier));
        assert_eq!(judged.blocked_at, Some(after(hours(3) + mins(10))));
        assert_eq!(judged.resumes_at, Some(after(hours(5) + mins(40))));
        assert_eq!(judged.window, FleetWindow::FiveHour);
    }

    #[test]
    fn fleet_follows_the_engine_rotation_order() {
        // At 60 an hour (account 1 rose 30 in 30 min). Account 1, active, is at the threshold, so the engine moves at once
        // to the account with the most headroom, 3, not to 2, whose 50 points reset unused at 30 min. Account 3 reaches
        // 90 % at 1 h 30 min, the refilled 2 at 3 h. Every account is then at or over the threshold, so the engine holds
        // 2 until it reaches 100 % at 3 h 10 min; the at-limit escape then takes 1 over 3 on the headroom tie and
        // rotation order, not because its reset is soonest, and the last 10 points of each run out at 3 h 30 min, half
        // an hour before 1's reset at 4 h. Drawing the soonest reset first would spend 2's 50 points before its reset
        // and last.
        let readings = [
            five_only(1, 90.0, hours(4)),
            five_only(2, 50.0, mins(30)),
            five_only(3, 0.0, hours(5)),
        ];
        let earlier = [
            sample(1, mins(30), 60.0, hours(4)),
            sample(2, mins(30), 50.0, mins(30)),
            sample(3, mins(30), 0.0, hours(5)),
        ];
        let judged = verdict(&fleet_with(&readings, &earlier));
        assert_near(judged.rate, 60.0);
        assert_eq!(judged.blocked_at, Some(after(hours(3) + mins(30))));
        assert_eq!(judged.resumes_at, Some(after(hours(4))));
        assert_eq!(judged.window, FleetWindow::FiveHour);
        assert!(judged.runs_dry);
    }

    #[test]
    fn fleet_all_above_holds_the_active_lane_until_its_limit() {
        // Every lane is at or over the 90 % threshold, so the engine holds the active lane until its 5-hour window
        // reaches 100 %. Account 2 resets at 5 min, during that burn: account 1 reaches 100 % at 5 min, the engine moves
        // to the refilled 2, and the pool runs dry at 1 h 45 min.
        let readings = [five_only(1, 95.0, hours(4)), five_only(2, 95.0, mins(5))];
        let earlier = [
            sample(1, mins(30), 80.0, hours(4)),
            sample(2, mins(30), 80.0, mins(5)),
        ];
        let judged = verdict(&fleet_live(
            &readings,
            &earlier,
            Some(1),
            &AutoswitchState::default(),
        ));
        assert_near(judged.rate, 60.0);
        assert_eq!(judged.blocked_at, Some(after(hours(1) + mins(45))));
        assert_eq!(judged.resumes_at, Some(after(hours(4))));
        assert_eq!(judged.window, FleetWindow::FiveHour);
    }

    #[test]
    fn fleet_waits_out_the_cooldown_before_a_proactive_move() {
        // At 60 an hour. Account 1, at the threshold, moves at once to 2 (3 is spent until its reset at 5 min), whose 80 %
        // reaches 90 % at 10 min. With the default 5 min cooldown the engine moves then to the refilled 3, and 2's last 10
        // points are lost at its reset at 25 min: 3 and the refilled 2 each spend 90 points, then 1, 2 and 3 their last 10
        // each, out at 3 h 40 min, before 1's reset at 4 h.
        let readings = [
            five_only(1, 90.0, hours(4)),
            five_only(2, 80.0, mins(25)),
            five_only(3, 100.0, mins(5)),
        ];
        let earlier = [
            sample(1, mins(30), 60.0, hours(4)),
            sample(2, mins(30), 80.0, mins(25)),
            sample(3, mins(30), 100.0, mins(5)),
        ];
        let moved = verdict(&fleet_with(&readings, &earlier));
        assert_eq!(moved.blocked_at, Some(after(hours(3) + mins(40))));
        assert_eq!(moved.resumes_at, Some(after(hours(4))));

        // With a 15 min cooldown the move at 10 min waits until 15 min, account 2 at 95 %, so only 5 points are lost at
        // its reset: the pool runs out 5 min later.
        let cooldown = AutoSwitchSettings {
            cooldown_seconds: 900.0,
            ..AutoSwitchSettings::default()
        };
        let waited = verdict(&fleet_under(&readings, &earlier, &cooldown));
        assert_eq!(waited.blocked_at, Some(after(hours(3) + mins(45))));
        assert_eq!(waited.resumes_at, Some(after(hours(4))));
    }

    #[test]
    fn fleet_moves_at_limit_without_waiting() {
        // At 300 an hour (account 1 rose 50 in 10 min) under a 30 min cooldown. Account 1, at the threshold, moves at
        // once to 2, whose 80 % reaches 100 % at 4 min, inside the cooldown: the at-limit move goes at once to 3, refilled
        // at 2 min, whose 100 points run out at 24 min (its proactive move at 22 min waits), then 1's last 10 at 26 min.
        // An at-limit move that waited out the cooldown would leave a gap from 4 min.
        let readings = [
            five_only(1, 90.0, hours(4)),
            five_only(2, 80.0, hours(4)),
            five_only(3, 100.0, mins(2)),
        ];
        let earlier = [
            sample(1, mins(10), 40.0, hours(4)),
            sample(2, mins(10), 80.0, hours(4)),
            sample(3, mins(10), 100.0, mins(2)),
        ];
        let cooldown = AutoSwitchSettings {
            cooldown_seconds: 1800.0,
            ..AutoSwitchSettings::default()
        };
        let judged = verdict(&fleet_under(&readings, &earlier, &cooldown));
        assert_near(judged.rate, 300.0);
        assert_eq!(judged.blocked_at, Some(after(mins(26))));
        assert_eq!(judged.resumes_at, Some(after(hours(4))));
    }

    #[test]
    fn fleet_honours_the_threshold_setting() {
        // At 60 an hour. Account 1, half used, resets at 1 h; account 2 is unused until its reset at 5 h.
        let readings = [five_only(1, 50.0, hours(1)), five_only(2, 0.0, hours(5))];
        let earlier = [
            sample(1, mins(30), 20.0, hours(1)),
            sample(2, mins(30), 0.0, hours(5)),
        ];

        // At 90 %: account 1 leaves at 40 min, 2 at 2 h 10 min for the refilled 1, which reaches 90 % at 3 h 40 min with
        // 2, the account it left, barred; 1 runs out at 3 h 50 min, 2 at 4 h, back at 2's reset at 5 h.
        let ninety = verdict(&fleet_with(&readings, &earlier));
        assert_eq!(ninety.blocked_at, Some(after(hours(4))));
        assert_eq!(ninety.resumes_at, Some(after(hours(5))));

        // At 50 %: account 1 leaves at once, and 2 takes it back when its reset refills it at 1 h; 2, left at 60 %, stays
        // barred while 1 runs out at 2 h 40 min, then 2 at 3 h 20 min.
        let fifty = AutoSwitchSettings {
            threshold: 50.0,
            ..AutoSwitchSettings::default()
        };
        let judged = verdict(&fleet_under(&readings, &earlier, &fifty));
        assert_eq!(judged.blocked_at, Some(after(hours(3) + mins(20))));
        assert_eq!(judged.resumes_at, Some(after(hours(5))));
    }

    #[test]
    fn fleet_takes_back_a_departed_account_once_it_has_recovered() {
        // At 60 an hour. Account 1, at the threshold, moves at once to 2. Account 1's reset at 30 min refills it, so when 2
        // reaches 90 % at 1 h 30 min the account the engine left has recovered and is taken back; 2's last 10 points are
        // lost at its reset at 1 h 40 min. Account 1 then leaves for the refilled 2 at 3 h, which reaches 90 % at 4 h 30
        // min with 1 barred; 2 and 1 run out of their last 10 points at 4 h 50 min, back at 1's reset at 6 h 30 min. Had
        // 1 stayed barred, 2 would have run on to its reset and the gap would start at 5 h.
        let readings = [five_only(1, 90.0, mins(30)), five_only(2, 0.0, mins(100))];
        let earlier = [
            sample(1, mins(30), 60.0, mins(30)),
            sample(2, mins(30), 0.0, mins(100)),
        ];
        let judged = verdict(&fleet_with(&readings, &earlier));
        assert_eq!(judged.blocked_at, Some(after(hours(4) + mins(50))));
        assert_eq!(judged.resumes_at, Some(after(hours(6) + mins(30))));
    }

    #[test]
    fn fleet_floors_first_reset_to_the_minute() {
        let judged = verdict(&fleet_now(&[five_only(
            1,
            0.0,
            hours(3) + SignedDuration::from_secs(59),
        )]));
        assert_eq!(judged.first_reset, Some(after(hours(3))));
    }

    #[test]
    fn fleet_pools_an_account_whose_five_hour_window_has_not_started() {
        // At 60 an hour (account 1 rose 30 in 30 min). Account 1's 7-day window at 89 % leaves 11 / 0.2 = 55 points to
        // draw; account 2's 5-hour window has not started, and its 100 points pool. Account 1's 7-day window reaches the
        // threshold, 90 %, at 5 min, and the engine moves to account 2, whose window starts with that draw. However the
        // engine moves after, the 155 points run out at 2 h 35 min, account 1 with its week spent, and account 2 is back
        // 5 h after its first draw, at 5 h 5 min, not 5 h after the judging time.
        let one = reading(
            1,
            SignedDuration::ZERO,
            Some(window(40.0, hours(4))),
            Some(window(89.0, hours(6 * 24))),
        );
        let earlier = [reading(
            1,
            -mins(30),
            Some(window(10.0, hours(4))),
            Some(window(89.0, hours(6 * 24))),
        )];
        let judged = verdict(&fleet_with(&[one, unstarted(2, 0.0, None)], &earlier));
        assert_eq!(judged.members, 2);
        assert_eq!(judged.accounts, 2);
        assert_near(judged.remaining, 160.0);
        assert_near(judged.rate, 60.0);
        assert_eq!(
            judged.first_reset,
            Some(after(hours(4))),
            "the window not yet started has no reset"
        );
        assert_eq!(judged.blocked_at, Some(after(hours(2) + mins(35))));
        assert_eq!(judged.resumes_at, Some(after(hours(5) + mins(5))));
        assert_eq!(judged.window, FleetWindow::FiveHour);
    }

    #[test]
    fn fleet_of_an_in_use_account_and_a_fresh_one_lasts_past_the_in_use_week() {
        // The screenshot's shape. Account 1, in use, rose 1.5 points of its 5-hour window and 0.5 of its 7-day one in the
        // last 30 min: 3 an hour, with its week at 90 %. Leaving account 2 out of the pool and raising account 1's week at
        // its own 1 point an hour, the old model ran dry at 10 h. Account 2 has started neither window. A 1.5-point rise
        // measures no ratio, so at 0.2 account 1's week leaves 50 points to draw and account 2's 500: at 3 an hour they
        // last past account 1's 7-day reset in 6 d 9 h, which gives it 500 more, to the end of the 7 days.
        let one = reading(
            1,
            SignedDuration::ZERO,
            Some(window(5.0, hours(4))),
            Some(window(90.0, hours(6 * 24 + 9))),
        );
        let two = unstarted(2, 0.0, Some(window(0.0, hours(6 * 24 + 12))));
        let earlier = [reading(
            1,
            -mins(30),
            Some(window(3.5, hours(4))),
            Some(window(89.5, hours(6 * 24 + 9))),
        )];
        let judged = verdict(&fleet_live(
            &[one, two],
            &earlier,
            Some(1),
            &AutoswitchState::default(),
        ));
        assert_eq!(judged.accounts, 2);
        assert_near(judged.rate, 3.0);
        assert_near(judged.ratio, 0.2);
        assert_eq!(judged.blocked_at, None);
        assert!(!judged.runs_dry);
    }

    #[test]
    fn fleet_idle_accounts_near_their_week_add_only_what_their_week_leaves() {
        // Five accounts at 95 % of their week. With no measured ratio each leaves 5 / 0.2 = 25 points to draw: account 1,
        // drawn on at 60 an hour, of the 60 left in its 5-hour window, and the four whose 5-hour windows have not started
        // of their 100. However the engine moves between them, the 125 points run out at 2 h 5 min with every week spent,
        // back at the 7-day resets in 6 d.
        let week = window(95.0, hours(6 * 24));
        let one = reading(
            1,
            SignedDuration::ZERO,
            Some(window(40.0, hours(4))),
            Some(week),
        );
        let earlier = [reading(
            1,
            -mins(30),
            Some(window(10.0, hours(4))),
            Some(week),
        )];
        let readings = [
            one,
            unstarted(2, 0.0, Some(week)),
            unstarted(3, 0.0, Some(week)),
            unstarted(4, 0.0, Some(week)),
            unstarted(5, 0.0, Some(week)),
        ];
        let judged = verdict(&fleet_with(&readings, &earlier));
        assert_eq!(judged.accounts, 5);
        assert_near(judged.remaining, 460.0);
        assert_near(judged.ratio, 0.2);
        assert_eq!(judged.blocked_at, Some(after(hours(2) + mins(5))));
        assert_eq!(judged.window, FleetWindow::SevenDay);
        assert_eq!(judged.resumes_at, Some(after(hours(6 * 24))));
    }

    #[test]
    fn fleet_leaves_an_idle_lane_week_flat() {
        // At 60 an hour (account 1 rose 30 in 30 min), starting on account 1, which has no 7-day window. Account 2's 5-hour
        // window has not started, and its week, at 80 %, rose 10 points in the 30 min before. Idle, that week stays at
        // 80 % as the ranking reads it, so its 20 points are 100 to draw at the ratio of 0.2. However the engine moves
        // between them, account 1's 70 and account 2's 100 run out at 2 h 50 min, before account 1's reset at 4 h. Had
        // account 2's week risen at its own 20 an hour while idle, it would be spent before account 1 ran out at 1 h 10 min.
        let one = five_only(1, 30.0, hours(4));
        let two = unstarted(2, 0.0, Some(window(80.0, hours(6 * 24))));
        let earlier = [
            sample(1, mins(30), 0.0, hours(4)),
            reading(2, -mins(30), None, Some(window(70.0, hours(6 * 24)))),
        ];
        let judged = verdict(&fleet_live(
            &[one, two],
            &earlier,
            Some(1),
            &AutoswitchState::default(),
        ));
        assert_eq!(judged.blocked_at, Some(after(hours(2) + mins(50))));
        assert_eq!(judged.resumes_at, Some(after(hours(4))));
        assert_eq!(judged.window, FleetWindow::FiveHour);
    }

    #[test]
    fn fleet_ratio_is_measured_from_the_in_use_account() {
        // Account 1 rose 20 points of its 5-hour window and 5 of its 7-day one in 30 min: 40 an hour, and a ratio of
        // 0.25. Its week at 97.5 % leaves 2.5 / 0.25 = 10 points to draw, gone at 15 min; at 0.2 it would leave 12.5.
        let week = |pct: f64| Some(window(pct, hours(6 * 24)));
        let one = reading(
            1,
            SignedDuration::ZERO,
            Some(window(40.0, hours(4))),
            week(97.5),
        );
        let alone = verdict(&fleet_with(
            &[one],
            &[reading(
                1,
                -mins(30),
                Some(window(20.0, hours(4))),
                week(92.5),
            )],
        ));
        assert_near(alone.ratio, 0.25);
        assert_near(alone.rate, 40.0);
        assert_eq!(alone.blocked_at, Some(after(mins(15))));
        assert_eq!(alone.window, FleetWindow::SevenDay);
        assert_eq!(alone.resumes_at, Some(after(hours(6 * 24))));

        // Account 1, in use, has spent its week with the same rises, which stay in the pool's 40 an hour; account 2, drawn
        // on from now, rose nothing, which on its own history would give no ratio. Account 1's ratio applies to it: its 2.5
        // points of week leave 10 points to draw, gone at 15 min.
        let spent = reading(
            1,
            SignedDuration::ZERO,
            Some(window(40.0, hours(4))),
            week(100.0),
        );
        let two = reading(
            2,
            SignedDuration::ZERO,
            Some(window(20.0, hours(4))),
            week(97.5),
        );
        let earlier = [
            reading(1, -mins(30), Some(window(20.0, hours(4))), week(95.0)),
            reading(2, -mins(30), Some(window(20.0, hours(4))), week(97.5)),
        ];
        let judged = verdict(&fleet_live(
            &[spent, two],
            &earlier,
            Some(1),
            &AutoswitchState::default(),
        ));
        assert_near(judged.ratio, 0.25);
        assert_near(judged.rate, 40.0);
        assert_eq!(judged.blocked_at, Some(after(mins(15))));
        assert_eq!(judged.window, FleetWindow::SevenDay);
    }

    #[test]
    fn fleet_ratio_falls_back_below_the_minimum_rises() {
        // Account 1's ratio from one earlier sample `ago` old and its current reading, each `(before, now)` in percent.
        let ratio_of = |five: (f64, f64), seven: (f64, f64), ago: SignedDuration| {
            let at = |offset: SignedDuration, five_pct: f64, seven_pct: f64| {
                reading(
                    1,
                    offset,
                    Some(window(five_pct, hours(4))),
                    Some(window(seven_pct, hours(6 * 24))),
                )
            };
            verdict(&fleet_with(
                &[at(SignedDuration::ZERO, five.1, seven.1)],
                &[at(-ago, five.0, seven.0)],
            ))
            .ratio
        };
        assert_near(ratio_of((20.0, 29.0), (90.0, 92.0), mins(30)), 0.2);
        assert_near(ratio_of((20.0, 40.0), (90.0, 91.0), mins(30)), 0.2);
        assert_near(ratio_of((20.0, 30.0), (90.0, 93.0), mins(30)), 0.3);
        assert_near(ratio_of((20.0, 40.0), (90.0, 95.0), mins(75)), 0.25);
        assert_near(ratio_of((20.0, 40.0), (90.0, 95.0), mins(76)), 0.2);
    }

    #[test]
    fn the_verdict_says_whether_its_ratio_was_measured() {
        // Account 1 rose 20 points of its 5-hour window and 5 of its 7-day one in 30 min, past both minimum rises.
        let week = |pct: f64| Some(window(pct, hours(6 * 24)));
        let one = reading(
            1,
            SignedDuration::ZERO,
            Some(window(40.0, hours(4))),
            week(97.5),
        );
        let measured = verdict(&fleet_with(
            &[one],
            &[reading(
                1,
                -mins(30),
                Some(window(20.0, hours(4))),
                week(92.5),
            )],
        ));
        assert_eq!(measured.ratio_source, RatioSource::Measured);
        assert_near(measured.ratio, 0.25);

        // With no earlier sample there is no rise to measure.
        let unmeasured = verdict(&fleet_now(&[one]));
        assert_eq!(unmeasured.ratio_source, RatioSource::Default);
        assert_near(unmeasured.ratio, 0.2);

        assert_eq!(RatioSource::Measured.name(), "measured");
        assert_eq!(RatioSource::Default.name(), "default");
    }

    #[test]
    fn fleet_ratio_skips_pairs_across_a_five_hour_window_start() {
        // Account 1's 5-hour window starts between its samples 60 and 20 min old, which loses the 5-hour points drawn
        // since it started; the 7-day window rose 3 over that pair, which counts no more than its 5-hour rise. Only the
        // pair 20 min old to now counts: 12 points of 5-hour window and 3 of 7-day, a ratio of 0.25, not 6 / 12 = 0.5.
        let week = |pct: f64| Some(window(pct, hours(6 * 24)));
        let current = reading(
            1,
            SignedDuration::ZERO,
            Some(window(24.0, hours(4) + mins(30))),
            week(96.0),
        );
        let since_start = reading(
            1,
            -mins(20),
            Some(window(12.0, hours(4) + mins(30))),
            week(93.0),
        );
        let ratio_after =
            |first: AccountReading| verdict(&fleet_with(&[current], &[first, since_start])).ratio;

        let unstarted = reading(1, -mins(60), None, week(90.0));
        assert_near(ratio_after(unstarted), 0.25);

        let window_before = reading(1, -mins(60), Some(window(80.0, -mins(10))), week(90.0));
        assert_near(ratio_after(window_before), 0.25);
    }

    #[test]
    fn fleet_of_only_unstarted_accounts_has_a_verdict_without_a_first_reset() {
        let fresh = verdict(&fleet_now(&[
            unstarted(1, 0.0, None),
            unstarted(2, 30.0, None),
        ]));
        assert_eq!(fresh.members, 2);
        assert_eq!(fresh.accounts, 2);
        assert_near(fresh.remaining, 170.0);
        assert_near(fresh.rate, 0.0);
        assert_eq!(fresh.first_reset, None);
        assert!(!fresh.runs_dry);

        // With 7-day windows, the earliest of their resets stands in.
        let weekly = verdict(&fleet_now(&[
            unstarted(1, 0.0, Some(window(10.0, hours(48)))),
            unstarted(2, 0.0, Some(window(20.0, hours(30)))),
        ]));
        assert_eq!(weekly.first_reset, Some(after(hours(30))));
    }

    #[test]
    fn fleet_starts_on_the_logged_in_account() {
        // At 60 an hour. Account 1, half used, resets at 1 h; account 2 is unused until its reset at 5 h. Started on 1,
        // the timeline blocks at 4 h (see `fleet_honours_the_threshold_setting`).
        let readings = [five_only(1, 50.0, hours(1)), five_only(2, 0.0, hours(5))];
        let earlier = [
            sample(1, mins(30), 20.0, hours(1)),
            sample(2, mins(30), 0.0, hours(5)),
        ];
        let from_one = verdict(&fleet_live(
            &readings,
            &earlier,
            None,
            &AutoswitchState::default(),
        ));
        assert_eq!(from_one.blocked_at, Some(after(hours(4))));

        // Started on 2, the logged-in account (`Rotation::active`): 1's 50 points are lost at its reset at 1 h. Account 2
        // reaches 90 % at 1 h 30 min for the refilled 1, which reaches 90 % at 3 h with 2 barred, runs out at 3 h 10 min,
        // and 2's last 10 points at 3 h 20 min, back at 2's reset at 5 h.
        let from_two = verdict(&fleet_live(
            &readings,
            &earlier,
            Some(2),
            &AutoswitchState::default(),
        ));
        assert_eq!(from_two.blocked_at, Some(after(hours(3) + mins(20))));
        assert_eq!(from_two.resumes_at, Some(after(hours(5))));
    }

    #[test]
    fn fleet_carries_the_live_cooldown() {
        // At 60 an hour. Account 1, at the threshold, resets at 30 min; account 2 is unused until its reset at 5 h. With
        // no switch recorded the engine moves to 2 at once, and 1's last 10 points are lost at its reset; the refilled 1
        // takes over at 1 h 30 min, and the pool runs out at 3 h 20 min.
        let readings = [five_only(1, 90.0, mins(30)), five_only(2, 0.0, hours(5))];
        let earlier = [
            sample(1, mins(30), 60.0, mins(30)),
            sample(2, mins(30), 0.0, hours(5)),
        ];
        let fresh = verdict(&fleet_live(
            &readings,
            &earlier,
            None,
            &AutoswitchState::default(),
        ));
        assert_eq!(fresh.blocked_at, Some(after(hours(3) + mins(20))));

        // The live state's last switch 2 min ago holds the move to 3 min, so account 1 spends 3 more points before its
        // reset and every later move comes 3 min later: the pool runs out at 3 h 23 min.
        let cooling = AutoswitchState {
            last_switch_at: Some(secs_from_timestamp(fetched()) - 120.0),
            ..AutoswitchState::default()
        };
        let judged = verdict(&fleet_live(&readings, &earlier, None, &cooling));
        assert_eq!(judged.blocked_at, Some(after(hours(3) + mins(23))));
        assert_eq!(judged.resumes_at, Some(after(hours(5))));
    }

    #[test]
    fn fleet_moves_proactively_only_on_a_tick() {
        // At 60 an hour. Account 1, 85 % used, resets at 30 min; account 2 is unused until its reset at 5 h. Ticking each
        // minute, 1 reaches the threshold and leaves at 5 min, 2 at 1 h 35 min for the refilled 1, which reaches 90 % at
        // 3 h 5 min with 2 barred; 1 runs out at 3 h 15 min and 2's last 10 points at 3 h 25 min.
        let readings = [five_only(1, 85.0, mins(30)), five_only(2, 0.0, hours(5))];
        let earlier = [
            sample(1, mins(30), 55.0, mins(30)),
            sample(2, mins(30), 0.0, hours(5)),
        ];
        let each_minute = verdict(&fleet_with(&readings, &earlier));
        assert_eq!(each_minute.blocked_at, Some(after(hours(3) + mins(25))));

        // Ticking every 10 min, the move waits for the tick at 10 min, 1 at 95 %, and every later move lands on a tick
        // 5 min later: the pool runs out at 3 h 30 min.
        let ten_minutes = AutoSwitchSettings {
            interval_seconds: 600.0,
            ..AutoSwitchSettings::default()
        };
        let judged = verdict(&fleet_under(&readings, &earlier, &ten_minutes));
        assert_eq!(judged.blocked_at, Some(after(hours(3) + mins(30))));
        assert_eq!(judged.resumes_at, Some(after(hours(5))));
    }

    #[test]
    fn fleet_rate_counts_no_rise_across_a_moved_reset() {
        // The window reset 20 min ago: 5 % 30 min ago, 15 % now, under a reset 5 h after the one before.
        let current = five_only(1, 15.0, hours(4) + mins(40));
        assert_near(
            rate_of(
                current,
                &[reading(1, -mins(30), Some(window(5.0, -mins(20))), None)],
            ),
            0.0,
        );
    }

    #[test]
    fn fleet_rate_takes_a_reset_moved_by_under_a_minute_as_the_same_window() {
        let anchor = half_hour_ago(1, 5.0, hours(3));
        let jittered = five_only(1, 15.0, hours(3) + SignedDuration::from_secs(30));
        assert_near(rate_of(jittered, &[anchor]), 20.0);

        let moved = five_only(1, 15.0, hours(3) + mins(2));
        assert_near(rate_of(moved, &[anchor]), 0.0);
    }

    #[test]
    fn fleet_holds_a_negative_pct_to_an_unused_window() {
        let judged = verdict(&fleet_now(&[five_only(1, -5.0, hours(3))]));
        assert_near(judged.remaining, 100.0);
        assert_near(judged.rate, 0.0);
        assert_eq!(judged.blocked_at, None);
    }

    #[test]
    fn fleet_rate_ignores_a_sample_older_than_the_span() {
        // The sample 31 min old is out of the span, which leaves the current reading alone: the window average, 20 an hour.
        let current = five_only(1, 40.0, hours(3));
        assert_near(
            rate_of(
                current,
                &[reading(1, -mins(31), Some(window(10.0, hours(3))), None)],
            ),
            20.0,
        );
    }

    #[test]
    fn fleet_rate_reads_the_span_it_is_given() {
        // 40 % in 2 h of 5 is 20 points an hour. The sample 40 min old is out of a 30 min span, which leaves the window
        // average; a 60 min span holds it, and its 30-point rise in 40 min is 45 an hour.
        let current = five_only(1, 40.0, hours(3));
        let earlier = [reading(1, -mins(40), Some(window(10.0, hours(3))), None)];
        let settings = AutoSwitchSettings::default();
        let state = AutoswitchState::default();
        let rate_over = |rate_span: SignedDuration| {
            let rotation = Rotation {
                active: None,
                settings: &settings,
                state: &state,
            };
            verdict(&fleet_spanning(&[current], &earlier, rotation, rate_span)).rate
        };
        assert_near(rate_over(RATE_SPAN_30), 20.0);
        assert_near(rate_over(mins(60)), 45.0);
    }

    #[test]
    fn fleet_rate_counts_the_time_since_the_newest_sample_as_no_use() {
        // 10 points between samples 30 and 20 min old, none since: 20 an hour, not 10 in 10 min.
        let newest = reading(1, -mins(20), Some(window(20.0, hours(3))), None);
        assert_near(rate_of(newest, &[half_hour_ago(1, 10.0, hours(3))]), 20.0);
    }
}
