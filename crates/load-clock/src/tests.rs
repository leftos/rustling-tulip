//! The clock's tests: a manual load source and ticker, no sleeping and no real clock in an assertion.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::{
    BACKSTOP_FACTOR, CpuCounters, DEFAULT_STALL_LIMIT, Deadline, Expiry, ExpiryKind, Figures,
    LoadClock, LoadSource, Sample, StallWatch, Ticker, free_share,
};

const SECOND: Duration = Duration::from_secs(1);

/// A ticker at a fixed base instant plus an offset the test moves.
struct ManualTicker {
    base: Instant,
    offset: Mutex<Duration>,
}

impl Ticker for ManualTicker {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The counters a manual source reports.
#[derive(Clone, Copy)]
struct Counters {
    machine: Option<CpuCounters>,
    own: Option<Duration>,
}

/// A source reading counters the test moves.
struct ManualSource(Arc<Mutex<Counters>>);

impl LoadSource for ManualSource {
    fn machine(&self) -> Option<CpuCounters> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .machine
    }

    fn own_cpu(&self) -> Option<Duration> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).own
    }
}

/// A clock over a manual source and ticker, one processor.
struct Rig {
    ticker: Arc<ManualTicker>,
    counters: Arc<Mutex<Counters>>,
    clock: Arc<LoadClock>,
}

impl Rig {
    fn new() -> Rig {
        Rig::over(Some(CpuCounters {
            busy: Duration::ZERO,
            total: Duration::ZERO,
        }))
    }

    fn over(machine: Option<CpuCounters>) -> Rig {
        let ticker = Arc::new(ManualTicker {
            base: Instant::now(),
            offset: Mutex::new(Duration::ZERO),
        });
        let counters = Arc::new(Mutex::new(Counters {
            machine,
            own: Some(Duration::ZERO),
        }));
        let clock = LoadClock::new(
            Box::new(ManualSource(Arc::clone(&counters))),
            ticker.clone(),
        );
        Rig {
            ticker,
            counters,
            clock,
        }
    }

    /// Moves wall time by `span` with other work leaving `share` of the machine free.
    fn advance(&self, span: Duration, share: f64) {
        *self
            .ticker
            .offset
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += span;
        let mut counters = self.counters.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(machine) = counters.machine.as_mut() {
            machine.total += span;
            machine.busy += span.mul_f64(1.0 - share);
        }
    }
}

fn counters(busy_ms: u64, total_ms: u64) -> CpuCounters {
    CpuCounters {
        busy: Duration::from_millis(busy_ms),
        total: Duration::from_millis(total_ms),
    }
}

fn sample(busy_ms: u64, total_ms: u64, own_ms: u64) -> Sample {
    Sample {
        machine: Some(counters(busy_ms, total_ms)),
        own: Some(Duration::from_millis(own_ms)),
    }
}

fn assert_near(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

fn assert_near_duration(actual: Duration, expected: Duration) {
    let gap = actual.abs_diff(expected);
    assert!(
        gap < Duration::from_micros(1),
        "expected {expected:?}, got {actual:?}"
    );
}

#[test]
fn free_share_reads_an_idle_machine_as_free() {
    assert_near(free_share(sample(0, 0, 0), sample(0, 1000, 0)), 1.0);
}

#[test]
fn free_share_subtracts_own_cpu() {
    assert_near(free_share(sample(0, 0, 0), sample(500, 1000, 0)), 0.5);
    assert_near(free_share(sample(0, 0, 0), sample(500, 1000, 250)), 0.75);
}

#[test]
fn free_share_clamps_at_the_floor() {
    assert_near(
        free_share(sample(0, 0, 0), sample(1000, 1000, 0)),
        crate::MIN_FREE,
    );
}

#[test]
fn free_share_reads_unmoved_or_missing_counters_as_free() {
    assert_near(free_share(sample(400, 1000, 0), sample(400, 1000, 0)), 1.0);
    let missing = Sample {
        machine: None,
        own: None,
    };
    assert_near(free_share(missing, sample(900, 1000, 0)), 1.0);
    assert_near(free_share(sample(0, 0, 0), missing), 1.0);
}

#[test]
fn a_loaded_machine_advances_adjusted_time_by_its_free_share() {
    let rig = Rig::new();
    for _ in 0..4 {
        rig.advance(SECOND, 0.25);
        rig.clock.read();
    }
    let reading = rig.clock.read();
    assert_eq!(reading.wall, 4 * SECOND);
    assert_near_duration(reading.adjusted, SECOND);
}

#[test]
fn an_idle_machine_keeps_wall_time() {
    let rig = Rig::new();
    for _ in 0..3 {
        rig.advance(SECOND, 1.0);
        rig.clock.read();
    }
    rig.advance(Duration::from_millis(500), 1.0);
    let reading = rig.clock.read();
    assert_eq!(reading.wall, Duration::from_millis(3500));
    assert_eq!(reading.adjusted, reading.wall);
}

#[test]
fn a_deadline_never_counts_more_adjusted_than_wall_time() {
    let rig = Rig::new();
    rig.advance(SECOND, 0.05);
    rig.clock.read();
    rig.advance(Duration::from_millis(950), 1.0);
    let deadline = Deadline::start(Arc::clone(&rig.clock), SECOND);
    // The next sample credits the whole span at 1.0, part of it from before the deadline started.
    rig.advance(Duration::from_millis(100), 1.0);
    assert_eq!(deadline.expired(), None);
    let figures = deadline.figures();
    assert!(
        figures.adjusted <= figures.wall,
        "adjusted {figures:?} passed wall"
    );
}

#[test]
fn a_waiter_following_wait_slice_keeps_wall_time_on_an_idle_machine() {
    let rig = Rig::new();
    rig.advance(SECOND, 0.05);
    rig.clock.read();
    let deadline = Deadline::start(Arc::clone(&rig.clock), Duration::from_secs(10));
    let mut expiry = None;
    for _ in 0..1000 {
        rig.advance(deadline.wait_slice(Duration::from_secs(30)), 1.0);
        expiry = deadline.expired();
        if expiry.is_some() {
            break;
        }
    }
    assert_eq!(expiry.map(|e| e.kind), Some(ExpiryKind::Ceiling));
    let wall = expiry.map_or(Duration::ZERO, |e| e.figures.wall);
    assert!(
        wall.abs_diff(Duration::from_secs(10)) <= Duration::from_millis(1),
        "ended at {wall:?} of wall"
    );
}

#[test]
fn a_deadline_ends_at_its_ceiling_in_adjusted_time() {
    let rig = Rig::new();
    let deadline = Deadline::start(Arc::clone(&rig.clock), Duration::from_secs(60));
    for _ in 0..100 {
        rig.advance(SECOND, 0.5);
    }
    assert_eq!(deadline.expired(), None);
    for _ in 0..19 {
        rig.advance(SECOND, 0.5);
        assert_eq!(deadline.expired(), None);
    }
    rig.advance(SECOND, 0.5);
    let expiry = deadline.expired().map(|e| (e.kind, e.figures.wall));
    assert_eq!(
        expiry,
        Some((ExpiryKind::Ceiling, Duration::from_secs(120)))
    );
}

#[test]
fn a_deadline_ends_at_its_backstop_before_its_ceiling() {
    let rig = Rig::new();
    let budget = Duration::from_secs(60);
    let deadline = Deadline::start(Arc::clone(&rig.clock), budget);
    let backstop = budget * BACKSTOP_FACTOR;
    while rig.clock.read().wall + SECOND < backstop {
        rig.advance(SECOND, 0.05);
        assert_eq!(deadline.expired(), None);
    }
    rig.advance(SECOND, 0.05);
    let expiry = deadline.expired();
    assert_eq!(expiry.map(|e| e.kind), Some(ExpiryKind::Backstop));
    assert_eq!(expiry.map(|e| e.figures.wall), Some(backstop));
}

#[test]
fn wait_slice_never_passes_the_nearer_end_or_max() {
    let budget = Duration::from_secs(10);
    let max = Duration::from_secs(30);
    let half = Rig::new();
    let deadline = Deadline::start(Arc::clone(&half.clock), budget);
    assert_eq!(deadline.wait_slice(max), budget);
    for _ in 0..4 {
        half.advance(SECOND, 0.5);
    }
    // 2 s of adjusted time spent: 8 s to the ceiling at a free machine, 46 s to the backstop.
    assert_near_duration(deadline.wait_slice(max), Duration::from_secs(8));
    assert_eq!(deadline.wait_slice(2 * SECOND), 2 * SECOND);

    let loaded = Rig::new();
    let deadline = Deadline::start(Arc::clone(&loaded.clock), budget);
    for _ in 0..45 {
        loaded.advance(SECOND, 0.05);
    }
    // About 2.25 s adjusted: 7.75 s to the ceiling, but only 5 s of wall to the backstop.
    assert_eq!(deadline.wait_slice(max), Duration::from_secs(5));
    for _ in 0..5 {
        loaded.advance(SECOND, 0.05);
    }
    assert_eq!(deadline.wait_slice(max), Duration::from_millis(1));
}

#[test]
fn a_stall_fires_after_the_limit_of_no_growth() {
    let rig = Rig::new();
    let mut watch = StallWatch::new(rig.ticker.clone(), DEFAULT_STALL_LIMIT);
    watch.observe(10, SECOND);
    rig.advance(Duration::from_secs(119), 1.0);
    watch.observe(10, SECOND);
    assert_eq!(watch.stalled(), None);
    rig.advance(SECOND, 1.0);
    let figures = Figures {
        budget: DEFAULT_STALL_LIMIT,
        wall: DEFAULT_STALL_LIMIT,
        adjusted: DEFAULT_STALL_LIMIT,
    };
    assert_eq!(
        watch.stalled(),
        Some(Expiry {
            kind: ExpiryKind::Stall,
            figures
        })
    );
}

#[test]
fn growth_in_log_or_cpu_resets_the_stall() {
    let rig = Rig::new();
    let limit = Duration::from_secs(120);
    let mut watch = StallWatch::new(rig.ticker.clone(), limit);
    watch.observe(10, SECOND);
    rig.advance(Duration::from_secs(100), 1.0);
    watch.observe(11, SECOND);
    rig.advance(Duration::from_secs(100), 1.0);
    watch.observe(11, 2 * SECOND);
    rig.advance(Duration::from_secs(119), 1.0);
    assert_eq!(watch.stalled(), None);
    rig.advance(SECOND, 1.0);
    assert_eq!(watch.stalled().map(|e| e.figures.wall), Some(limit));
}

#[test]
fn each_expiry_message_names_what_fired_with_its_figures() {
    let ceiling = Expiry {
        kind: ExpiryKind::Ceiling,
        figures: Figures {
            budget: Duration::from_secs(300),
            wall: Duration::from_secs(812),
            adjusted: Duration::from_millis(300_400),
        },
    };
    assert_eq!(
        ceiling.to_string(),
        "did not finish within 300 s of load-adjusted time (wall 812 s, machine free 37% on average)"
    );
    let backstop = Expiry {
        kind: ExpiryKind::Backstop,
        figures: Figures {
            budget: Duration::from_secs(60),
            wall: Duration::from_secs(300),
            adjusted: Duration::from_secs(15),
        },
    };
    assert_eq!(
        backstop.to_string(),
        "did not finish within 300 s of wall time, 5 x its 60 s ceiling (load-adjusted 15 s, machine free 5% on average)"
    );
    let stall = Expiry {
        kind: ExpiryKind::Stall,
        figures: Figures {
            budget: Duration::from_secs(120),
            wall: Duration::from_secs(125),
            adjusted: Duration::from_secs(125),
        },
    };
    assert_eq!(
        stall.to_string(),
        "stalled: no log line and no CPU for 120 s"
    );
    let short = Expiry {
        kind: ExpiryKind::Ceiling,
        figures: Figures {
            budget: Duration::from_millis(500),
            wall: Duration::from_millis(1200),
            adjusted: Duration::from_millis(300),
        },
    };
    assert_eq!(
        short.to_string(),
        "did not finish within 500 ms of load-adjusted time (wall 1200 ms, machine free 25% on average)"
    );
}

#[test]
fn the_wall_clock_reads_adjusted_equal_to_wall() {
    // `LoadClock::wall` is the no-load source over the system ticker; this drives the same source with a manual one.
    let rig = Rig::over(None);
    for millis in [300, 900, 1700, 4200] {
        rig.advance(Duration::from_millis(millis), 0.1);
        let reading = rig.clock.read();
        assert_eq!(reading.adjusted, reading.wall);
    }
}
