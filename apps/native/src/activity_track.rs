//! The two-hour activity track of a Dashboard session, as plain data: the
//! bar's runs, its hold ticks and its captions, read from the wire segments
//! of `DaemonMessage::ActivityTracks`.

use chrono::{DateTime, Duration, Local, Utc};
use protocol::{ActivitySegment, SegmentActivity, SegmentKind};

use crate::dashboard_rows::format_age;

/// A stretch of time, start first.
type Span = (DateTime<Utc>, DateTime<Utc>);

/// What a stretch of the bar shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    Working,
    Asking,
    Idle,
    Finished,
    Gap,
}

/// One stretch of the bar, placed as fractions of the bar's width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarRun {
    pub start: f32,
    pub width: f32,
    pub kind: RunKind,
}

/// A gate hold's mark on the bar, at a fraction of the bar's width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoldTick {
    pub at: f32,
    /// Whether the hold is still running at the window's end.
    pub open: bool,
}

/// A session's track, as the view draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub runs: Vec<BarRun>,
    pub ticks: Vec<HoldTick>,
    /// The caption under the bar: `busy 46m · in gate`.
    pub short: String,
    /// The lane caption: `busy 46m of 2h · gate held now`.
    pub long: String,
    /// The bar's tooltip when it holds a gap.
    pub note: Option<String>,
}

/// What a legend entry shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegendMark {
    Run(RunKind),
    Hold,
}

/// One entry of the track's legend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegendEntry {
    pub label: &'static str,
    pub mark: LegendMark,
}

/// The stretch of time a bar covers: the `hours` up to `end`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TrackWindow {
    end: DateTime<Utc>,
    hours: u32,
}

impl TrackWindow {
    pub(crate) fn new(end: DateTime<Utc>, hours: u32) -> Self {
        Self { end, hours }
    }

    fn start(self) -> DateTime<Utc> {
        self.end - Duration::hours(i64::from(self.hours))
    }

    /// The part of `segment` inside the window; an open stretch runs to the
    /// window's end.
    fn clip(self, segment: &ActivitySegment) -> Option<Span> {
        let from = segment.from.max(self.start());
        let to = segment.to.unwrap_or(self.end).min(self.end);
        (from < to).then_some((from, to))
    }

    /// How far across the window `at` falls, from 0 to 1.
    fn fraction(self, at: DateTime<Utc>) -> f32 {
        seconds(at - self.start()) / seconds(self.end - self.start())
    }
}

/// A span in seconds; a negative one is no time at all.
fn seconds(span: Duration) -> f32 {
    span.to_std().map_or(0.0, |span| span.as_secs_f32())
}

/// How much of the window the session spent working and asking.
#[derive(Clone, Copy)]
struct Spent {
    busy: Duration,
    asking: Duration,
}

/// What the captions say about the gate.
enum Gate {
    Never,
    Held,
    LastEnded(DateTime<Utc>),
}

/// `key`'s track inside `window`, `None` when no segment names `key` or the
/// window is empty. `unseen` draws a last `Idle` stretch as finished.
pub(crate) fn session_track(
    segments: &[ActivitySegment],
    key: &str,
    window: TrackWindow,
    unseen: bool,
) -> Option<Track> {
    let own: Vec<&ActivitySegment> = segments.iter().filter(|s| s.key == key).collect();
    if own.is_empty() || window.hours == 0 {
        return None;
    }
    let holds: Vec<&ActivitySegment> = of_kind(&own, SegmentKind::Hold);
    let mut gaps: Vec<Span> = of_kind(&own, SegmentKind::Gap)
        .into_iter()
        .filter_map(|gap| window.clip(gap))
        .collect();
    gaps.sort_unstable();
    let first_gap = gaps.first().copied();
    let since = first_gap
        .filter(|(from, _)| *from == window.start())
        .map(|(_, to)| hhmm(to));
    let spent = Spent {
        busy: stretch_span(&own, window, SegmentActivity::Working),
        asking: stretch_span(&own, window, SegmentActivity::Asking),
    };
    let (short, long) = captions(spent, &holds, since, window);
    Some(Track {
        runs: bar_runs(&own, window, unseen),
        ticks: hold_ticks(&holds, window),
        short,
        long,
        note: first_gap
            .map(|(_, to)| format!("no record before {} · the daemon restarted", hhmm(to))),
    })
}

fn of_kind<'a>(segments: &[&'a ActivitySegment], kind: SegmentKind) -> Vec<&'a ActivitySegment> {
    segments
        .iter()
        .copied()
        .filter(|segment| segment.kind == kind)
        .collect()
}

/// A time as the local clock read it.
fn hhmm(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%H:%M").to_string()
}

/// The stretches of `activity` in the window, overlaps merged, earliest first.
fn stretch_union(
    segments: &[&ActivitySegment],
    window: TrackWindow,
    activity: SegmentActivity,
) -> Vec<Span> {
    let mut spans: Vec<Span> = of_kind(segments, SegmentKind::Session)
        .into_iter()
        .filter(|stretch| stretch.activity == Some(activity))
        .filter_map(|stretch| window.clip(stretch))
        .collect();
    spans.sort_unstable();
    let mut merged: Vec<Span> = Vec::new();
    for (from, to) in spans {
        match merged.last_mut() {
            Some((_, end)) if from <= *end => *end = to.max(*end),
            _ => merged.push((from, to)),
        }
    }
    merged
}

fn stretch_span(
    segments: &[&ActivitySegment],
    window: TrackWindow,
    activity: SegmentActivity,
) -> Duration {
    stretch_union(segments, window, activity)
        .into_iter()
        .fold(Duration::zero(), |total, (from, to)| total + (to - from))
}

/// The bar's runs, left to right. A segment this build cannot name draws
/// nothing, so its stretch of the bar stays empty.
fn bar_runs(segments: &[&ActivitySegment], window: TrackWindow, unseen: bool) -> Vec<BarRun> {
    let last = of_kind(segments, SegmentKind::Session)
        .into_iter()
        .max_by_key(|stretch| stretch.from);
    let mut runs: Vec<BarRun> = segments
        .iter()
        .filter_map(|segment| {
            let finished = unseen && last.is_some_and(|last| std::ptr::eq(last, *segment));
            let kind = run_kind(segment, finished)?;
            let (from, to) = window.clip(segment)?;
            let start = window.fraction(from);
            Some(BarRun {
                start,
                width: window.fraction(to) - start,
                kind,
            })
        })
        .collect();
    runs.sort_unstable_by(|a, b| a.start.total_cmp(&b.start));
    runs
}

fn run_kind(segment: &ActivitySegment, finished: bool) -> Option<RunKind> {
    match (segment.kind, segment.activity) {
        (SegmentKind::Gap, _) => Some(RunKind::Gap),
        (SegmentKind::Session, Some(SegmentActivity::Working)) => Some(RunKind::Working),
        (SegmentKind::Session, Some(SegmentActivity::Asking)) => Some(RunKind::Asking),
        (SegmentKind::Session, Some(SegmentActivity::Idle)) if finished => Some(RunKind::Finished),
        (SegmentKind::Session, Some(SegmentActivity::Idle)) => Some(RunKind::Idle),
        _ => None,
    }
}

/// Whether `hold` still runs at the window's end.
fn hold_is_open(hold: &ActivitySegment, window: TrackWindow) -> bool {
    hold.to.is_none_or(|to| to > window.end)
}

fn hold_ticks(holds: &[&ActivitySegment], window: TrackWindow) -> Vec<HoldTick> {
    holds
        .iter()
        .filter_map(|hold| {
            let (from, _) = window.clip(hold)?;
            Some(HoldTick {
                at: window.fraction(from),
                open: hold_is_open(hold, window),
            })
        })
        .collect()
}

fn gate(holds: &[&ActivitySegment], window: TrackWindow) -> Gate {
    let shown = holds.iter().filter(|hold| window.clip(hold).is_some());
    if shown.clone().any(|hold| hold_is_open(hold, window)) {
        return Gate::Held;
    }
    shown
        .filter_map(|hold| hold.to)
        .max()
        .map_or(Gate::Never, Gate::LastEnded)
}

/// The opening words of the short and the long caption: what the session
/// spent the window on, `None` when it neither ran nor asked.
fn lead(spent: Spent, window: TrackWindow) -> Option<(String, String)> {
    let span = |total| format_age(window.end - total, window.end);
    if spent.busy > Duration::zero() {
        let busy = span(spent.busy);
        return Some((
            format!("busy {busy}"),
            format!("busy {busy} of {}h", window.hours),
        ));
    }
    (spent.asking > Duration::zero()).then(|| {
        (
            "asking".to_owned(),
            format!("asking for {}", span(spent.asking)),
        )
    })
}

/// The short and the long caption. Nothing ran, nothing asked and no gate:
/// the idle wording, alone.
fn captions(
    spent: Spent,
    holds: &[&ActivitySegment],
    since: Option<String>,
    window: TrackWindow,
) -> (String, String) {
    let (short_gate, long_gate) = match gate(holds, window) {
        Gate::Never => (None, None),
        Gate::Held => (Some("in gate".to_owned()), Some("gate held now".to_owned())),
        Gate::LastEnded(at) => (
            None,
            Some(format!("last gate {} ago", format_age(at, window.end))),
        ),
    };
    let since = since.map_or_else(String::new, |start| format!("since {start} · "));
    let joined = |first: String, second: Option<String>| match second {
        Some(second) => format!("{first} · {second}"),
        None => first,
    };
    match (lead(spent, window), long_gate) {
        (Some((short, long)), long_gate) => (
            joined(short, short_gate),
            format!("{since}{}", joined(long, long_gate)),
        ),
        (None, Some(gate_words)) => (
            short_gate.unwrap_or_else(|| gate_words.clone()),
            format!("{since}{gate_words}"),
        ),
        (None, None) => ("idle".to_owned(), format!("idle for {}h", window.hours)),
    }
}

/// The track's legend, one entry per thing a bar can show.
#[must_use]
pub fn legend() -> Vec<LegendEntry> {
    let run = |label, kind| LegendEntry {
        label,
        mark: LegendMark::Run(kind),
    };
    vec![
        run("working", RunKind::Working),
        run("asking", RunKind::Asking),
        run("idle", RunKind::Idle),
        run("finished", RunKind::Finished),
        run("no record", RunKind::Gap),
        LegendEntry {
            label: "gate hold",
            mark: LegendMark::Hold,
        },
    ]
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    const KEY: &str = "session-s1";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("a valid time")
    }

    fn ago(minutes: i64) -> DateTime<Utc> {
        now() - Duration::minutes(minutes)
    }

    fn segment(
        kind: SegmentKind,
        activity: Option<SegmentActivity>,
        from: i64,
        to: Option<i64>,
    ) -> ActivitySegment {
        ActivitySegment {
            repo: "r1".to_owned(),
            key: KEY.to_owned(),
            kind,
            label: "s1".to_owned(),
            activity,
            from: ago(from),
            to: to.map(ago),
        }
    }

    fn stretch(activity: SegmentActivity, from: i64, to: Option<i64>) -> ActivitySegment {
        segment(SegmentKind::Session, Some(activity), from, to)
    }

    fn work(from: i64, to: Option<i64>) -> ActivitySegment {
        stretch(SegmentActivity::Working, from, to)
    }

    fn hold(from: i64, to: Option<i64>) -> ActivitySegment {
        segment(SegmentKind::Hold, None, from, to)
    }

    fn gap(from: i64, to: i64) -> ActivitySegment {
        segment(SegmentKind::Gap, None, from, Some(to))
    }

    fn track(segments: &[ActivitySegment]) -> Track {
        session_track(segments, KEY, TrackWindow::new(now(), 2), false).expect("the key is listed")
    }

    fn assert_close(actual: f32, expected: f32, what: &str) {
        assert!(
            (actual - expected).abs() < 1e-3,
            "{what}: {actual} is not {expected}"
        );
    }

    #[test]
    fn busy_union_merges_overlaps() {
        let drawn = track(&[work(100, Some(60)), work(80, Some(50)), work(30, Some(20))]);
        assert_eq!(drawn.short, "busy 1h00m", "50 merged minutes, then 10");
        assert_eq!(drawn.long, "busy 1h00m of 2h");
    }

    #[test]
    fn an_open_stretch_runs_to_now() {
        let drawn = track(&[work(10, None)]);
        assert_eq!(drawn.short, "busy 10m");
        let run = drawn.runs[0];
        assert_eq!(run.kind, RunKind::Working);
        assert_close(run.start, 110.0 / 120.0, "start");
        assert_close(run.width, 10.0 / 120.0, "width");
    }

    #[test]
    fn a_stretch_before_the_window_is_clipped() {
        let drawn = track(&[work(200, Some(130)), work(150, Some(110))]);
        assert_eq!(drawn.short, "busy 10m", "only what falls in the window");
        assert_eq!(drawn.runs.len(), 1, "a stretch wholly before it is dropped");
        assert_close(drawn.runs[0].start, 0.0, "start");
        assert_close(drawn.runs[0].width, 10.0 / 120.0, "width");
    }

    #[test]
    fn gate_held_now_caption() {
        let drawn = track(&[work(46, None), hold(5, None)]);
        assert_eq!(drawn.short, "busy 46m · in gate");
        assert_eq!(drawn.long, "busy 46m of 2h · gate held now");
        assert_eq!(drawn.ticks.len(), 1);
        assert!(drawn.ticks[0].open);
        assert_close(drawn.ticks[0].at, 115.0 / 120.0, "tick");
    }

    #[test]
    fn last_gate_n_ago_caption() {
        let drawn = track(&[work(46, None), hold(20, Some(12))]);
        assert_eq!(drawn.short, "busy 46m");
        assert_eq!(drawn.long, "busy 46m of 2h · last gate 12m ago");
        assert!(!drawn.ticks[0].open);
    }

    #[test]
    fn since_hhmm_after_a_gap() {
        let drawn = track(&[gap(130, 90), work(60, Some(14))]);
        let start = ago(90).with_timezone(&Local).format("%H:%M");
        assert_eq!(drawn.long, format!("since {start} · busy 46m of 2h"));
        assert_eq!(
            drawn.note,
            Some(format!("no record before {start} · the daemon restarted"))
        );
        assert_eq!(drawn.runs[0].kind, RunKind::Gap);
        assert_close(drawn.runs[0].width, 30.0 / 120.0, "gap width");
    }

    #[test]
    fn idle_for_2h() {
        let drawn = track(&[stretch(SegmentActivity::Idle, 130, None)]);
        assert_eq!(drawn.long, "idle for 2h");
        assert_eq!(drawn.short, "idle");
        assert_eq!(drawn.runs[0].kind, RunKind::Idle);
    }

    #[test]
    fn a_long_wait_on_a_prompt_reads_asking() {
        let asking = |from, to| stretch(SegmentActivity::Asking, from, to);
        let drawn = track(&[asking(130, None)]);
        assert_eq!(drawn.short, "asking");
        assert_eq!(drawn.long, "asking for 2h00m");
        assert_eq!(drawn.runs[0].kind, RunKind::Asking);

        let merged = track(&[asking(60, Some(30)), asking(40, Some(20))]);
        assert_eq!(merged.long, "asking for 40m", "overlaps merge");

        let gated = track(&[asking(30, None), hold(5, None)]);
        assert_eq!(gated.short, "asking · in gate");
        assert_eq!(gated.long, "asking for 30m · gate held now");

        let restarted = track(&[gap(130, 30), asking(30, None)]);
        let start = ago(30).with_timezone(&Local).format("%H:%M");
        assert_eq!(restarted.long, format!("since {start} · asking for 30m"));
    }

    #[test]
    fn an_open_hold_still_shows_with_no_busy_time() {
        let idle = stretch(SegmentActivity::Idle, 60, None);
        let drawn = track(&[idle.clone(), hold(5, None)]);
        assert_eq!(drawn.short, "in gate");
        assert_eq!(drawn.long, "gate held now");

        let past = track(&[idle, hold(20, Some(12))]);
        assert_eq!(past.short, "last gate 12m ago");
        assert_eq!(past.long, "last gate 12m ago");
    }

    #[test]
    fn an_unknown_activity_draws_nothing() {
        let drawn = track(&[
            stretch(SegmentActivity::Unknown, 30, Some(20)),
            segment(SegmentKind::Unknown, None, 20, Some(10)),
        ]);
        assert_eq!(drawn.runs, []);
        assert_eq!(drawn.ticks, []);
        assert_eq!(drawn.short, "idle");
    }

    #[test]
    fn legend_names_each_kind() {
        let labels: Vec<&str> = legend().iter().map(|entry| entry.label).collect();
        assert_eq!(
            labels,
            [
                "working",
                "asking",
                "idle",
                "finished",
                "no record",
                "gate hold"
            ]
        );
    }
}
