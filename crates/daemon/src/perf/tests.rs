//! The sampling service over a scripted source, on the paused clock.
#![expect(clippy::expect_used, reason = "tests assert preconditions with expect")]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protocol::{DaemonMessage, PerfSample};
use tokio::sync::mpsc;

use super::sample::{CpuTimes, RawTable};
use super::{INTERVAL, Perf, PerfSource};
use crate::sync::lock;

/// How long a test waits on an event before it fails as hung; on the
/// paused clock it passes only when nothing else can run.
const GUARD: Duration = Duration::from_secs(120);

/// A source answering its queue in order, then its last entry again; each
/// read is counted.
struct Scripted {
    queue: Mutex<VecDeque<Option<RawTable>>>,
    last: Mutex<Option<RawTable>>,
    reads: Arc<AtomicUsize>,
}

impl PerfSource for Scripted {
    fn read(&self) -> Option<RawTable> {
        let next = lock(&self.queue).pop_front();
        let table = match next {
            Some(entry) => {
                lock(&self.last).clone_from(&entry);
                entry
            }
            None => lock(&self.last).clone(),
        };
        self.reads.fetch_add(1, Ordering::SeqCst);
        table
    }
}

/// The service over a scripted source, its ticker running.
struct Rig {
    perf: Perf,
    reads: Arc<AtomicUsize>,
    ticker: tokio::task::JoinHandle<()>,
}

impl Rig {
    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.ticker.abort();
    }
}

fn rig(script: Vec<Option<RawTable>>) -> Rig {
    let reads = Arc::new(AtomicUsize::new(0));
    let source = Scripted {
        queue: Mutex::new(VecDeque::from(script)),
        last: Mutex::new(None),
        reads: Arc::clone(&reads),
    };
    let perf = Perf::new(Some(Arc::new(source)));
    let ticker = perf.spawn_ticker().expect("a source ticks");
    Rig {
        perf,
        reads,
        ticker,
    }
}

/// A table whose machine has run `busy` of `total`.
fn table(busy: u64, total: u64) -> RawTable {
    RawTable {
        machine_cpu: Some(CpuTimes { busy, total }),
        mem_used_bytes: 4_000,
        mem_total_bytes: 8_000,
    }
}

/// A rising script: each table one more step of machine time, half busy.
fn rising(steps: u64) -> Vec<Option<RawTable>> {
    (0..steps)
        .map(|step| Some(table(step * 500, step * 1_000)))
        .collect()
}

type Watcher = (
    u64,
    mpsc::UnboundedSender<DaemonMessage>,
    mpsc::UnboundedReceiver<DaemonMessage>,
);

fn watcher(id: u64) -> Watcher {
    let (tx, rx) = mpsc::unbounded_channel();
    (id, tx, rx)
}

/// The next sample sent to `rx`.
async fn next_sample(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) -> PerfSample {
    let message = tokio::time::timeout(GUARD, rx.recv())
        .await
        .expect("hung: no sample came")
        .expect("the outbox is open");
    match message {
        DaemonMessage::Perf { sample } => Some(sample),
        _ => None,
    }
    .expect("a perf sample")
}

#[tokio::test(start_paused = true)]
async fn no_read_happens_without_a_watcher() {
    let rig = rig(rising(4));
    tokio::time::sleep(INTERVAL * 10).await;
    assert_eq!(rig.reads(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_subscribe_reads_at_once_with_no_cpu() {
    let rig = rig(rising(4));
    let (id, tx, mut rx) = watcher(1);
    let started = tokio::time::Instant::now();
    rig.perf.subscribe(id, tx);

    let sample = next_sample(&mut rx).await;

    assert!(started.elapsed() < INTERVAL, "read at once, not on a tick");
    assert_eq!(rig.reads(), 1);
    assert_eq!(sample.machine.cpu_percent, None);
    assert_eq!(sample.machine.mem_used_bytes, 4_000);
    assert_eq!(sample.machine.mem_total_bytes, 8_000);
    assert_eq!(sample.interval_ms, 2000);
}

#[tokio::test(start_paused = true)]
async fn each_tick_sends_one_sample_to_every_watcher() {
    let rig = rig(rising(6));
    let (id1, tx1, mut rx1) = watcher(1);
    let (id2, tx2, mut rx2) = watcher(2);
    let (_, tx1_again, mut rx1_again) = watcher(1);
    rig.perf.subscribe(id1, tx1);
    rig.perf.subscribe(id2, tx2);
    // A second subscribe on the same connection is the same subscription.
    rig.perf.subscribe(id1, tx1_again);
    assert_eq!(*rig.perf.watcher_count().borrow(), 2);

    let first = next_sample(&mut rx2).await;
    assert_eq!(first, next_sample(&mut rx1_again).await);
    assert!(rx1.try_recv().is_err(), "the replaced outbox gets nothing");

    let second = next_sample(&mut rx2).await;
    assert_eq!(second, next_sample(&mut rx1_again).await);
    assert_eq!(rig.reads(), 2, "one read serves both watchers");
    assert_eq!(second.machine.cpu_percent, Some(50.0));
    assert!(rx2.try_recv().is_err(), "one sample per tick");
    assert!(rx1_again.try_recv().is_err(), "one sample per tick");
}

#[tokio::test(start_paused = true)]
async fn unsubscribing_the_last_watcher_stops_reading() {
    let rig = rig(rising(4));
    let (id, tx, mut rx) = watcher(1);
    rig.perf.subscribe(id, tx);
    let _ = next_sample(&mut rx).await;

    rig.perf.unsubscribe(id);
    rig.perf.unsubscribe(id);
    tokio::time::sleep(INTERVAL * 10).await;

    assert_eq!(rig.reads(), 1, "no read runs with no watcher");
    assert!(rx.try_recv().is_err());
    assert_eq!(*rig.perf.watcher_count().borrow(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_closed_connection_drops_its_watch() {
    let rig = rig(rising(4));
    let (id, tx, mut rx) = watcher(1);
    let guard = rig.perf.watch_guard(id);
    rig.perf.subscribe(id, tx);
    let _ = next_sample(&mut rx).await;

    drop(guard);
    assert_eq!(*rig.perf.watcher_count().borrow(), 0);
    rig.perf.unsubscribe(id);
    tokio::time::sleep(INTERVAL * 10).await;
    assert_eq!(rig.reads(), 1, "no read after the connection closed");

    // A watcher whose outbox closed without a guard is dropped at its next
    // send.
    let (id, tx, rx) = watcher(2);
    rig.perf.subscribe(id, tx);
    drop(rx);
    let mut count = rig.perf.watcher_count();
    tokio::time::timeout(GUARD, count.wait_for(|watchers| *watchers == 0))
        .await
        .expect("hung: the closed outbox stayed a watcher")
        .expect("the service is alive");
}

#[tokio::test(start_paused = true)]
async fn a_failed_read_sends_nothing() {
    let script = vec![Some(table(0, 0)), None, Some(table(1_000, 4_000))];
    let rig = rig(script);
    let (id, tx, mut rx) = watcher(1);
    rig.perf.subscribe(id, tx);
    let _ = next_sample(&mut rx).await;

    let after = next_sample(&mut rx).await;

    assert_eq!(rig.reads(), 3, "the failed read sent nothing");
    assert_eq!(
        after.machine.cpu_percent,
        Some(25.0),
        "measured against the read before the failed one"
    );
}

#[tokio::test(start_paused = true)]
async fn a_resubscribe_after_zero_watchers_starts_without_cpu() {
    let rig = rig(rising(8));
    let (id, tx, mut rx) = watcher(1);
    rig.perf.subscribe(id, tx);
    let _ = next_sample(&mut rx).await;
    let second = next_sample(&mut rx).await;
    assert!(second.machine.cpu_percent.is_some());

    rig.perf.unsubscribe(id);
    let (id, tx, mut rx) = watcher(1);
    rig.perf.subscribe(id, tx);
    let again = next_sample(&mut rx).await;

    assert_eq!(
        again.machine.cpu_percent, None,
        "no delta spans the idle gap"
    );
}
