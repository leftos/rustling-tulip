//! The live-run runtime's tests: the snapshot's status rule, and the watcher,
//! tailer and liveness check against a scratch folder.

use super::*;

#[test]
fn snapshot_status_maps_the_stream_status() {
    assert_eq!(status_of(None), DispatchRunStatus::Starting);
    assert_eq!(
        status_of(Some(SessionStatus::Working)),
        DispatchRunStatus::Working
    );
    assert_eq!(
        status_of(Some(SessionStatus::Idle)),
        DispatchRunStatus::Working
    );
    assert_eq!(
        status_of(Some(SessionStatus::Stopped)),
        DispatchRunStatus::Ended
    );
    assert_eq!(
        status_of(Some(SessionStatus::Error)),
        DispatchRunStatus::Ended
    );
}

#[cfg(windows)]
#[expect(
    clippy::expect_used,
    reason = "tests set up their scratch folder, child and files with expect"
)]
mod runtime {
    use super::*;
    use std::io::Write as _;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};

    /// The backstop for a wait the product should end; reaching it means a hang.
    const HANG_GUARD: Duration = Duration::from_secs(60);

    /// The stream's `init` line: `Working`, and the run's own conversation id.
    const INIT_LINE: &str = r#"{"type":"system","subtype":"init","session_id":"s1"}"#;
    /// An assistant line with one tool call, which records `tool: Read`.
    const TOOL_LINE: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}"#;
    /// The action [`TOOL_LINE`] records.
    const TOOL_ACTION: &str = "tool: Read";

    fn timing() -> Timing {
        Timing {
            scan: Duration::from_secs(1),
            liveness: Duration::from_millis(200),
            tail: Duration::from_millis(50),
            debounce: Duration::from_millis(50),
        }
    }

    /// A scratch folder under the temp dir, private to this process and case,
    /// removed on drop. The live-run folder is `live` inside it.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(case: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("rt-dispatch-runs-{case}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("the scratch folder is created");
            Self(root)
        }

        fn live(&self) -> PathBuf {
            self.0.join("live")
        }

        fn stream(&self) -> PathBuf {
            self.0.join("stream.jsonl")
        }

        fn run_file(&self, run_id: &str) -> PathBuf {
            self.live().join(format!("{run_id}.json"))
        }

        /// Write `<run_id>.json` as the tool does: a temporary file, then a
        /// rename over the real one.
        fn write_run(&self, run_id: &str, text: &str) -> PathBuf {
            std::fs::create_dir_all(self.live()).expect("the live folder is created");
            let path = self.run_file(run_id);
            let tmp = self.live().join(format!("{run_id}.json.tmp"));
            std::fs::write(&tmp, text).expect("the temporary run file is written");
            std::fs::rename(&tmp, &path).expect("the run file is replaced");
            path
        }

        /// A live-run file's text for `pid` with the start `child_start`.
        fn run_text(&self, run_id: &str, pid: u32, child_start: Option<u64>) -> String {
            serde_json::json!({
                "schema": 1,
                "run_id": run_id,
                "title": "a run",
                "backend": "a backend",
                "worktree": self.0,
                "run_dir": self.0,
                "stream": self.stream(),
                "child_pid": pid,
                "child_start": child_start,
                "started_at": Utc::now(),
            })
            .to_string()
        }

        fn append_stream(&self, text: &str) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.stream())
                .expect("the stream file opens");
            file.write_all(text.as_bytes())
                .expect("the stream text is appended");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A long-lived child, killed and waited on when dropped.
    struct Pinger(Child);

    impl Pinger {
        fn spawn() -> Self {
            let child = Command::new("cmd")
                .args(["/C", "ping", "-n", "300", "127.0.0.1"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("the ping child starts");
            Self(child)
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        fn start(&self) -> u64 {
            guard::creation_filetime(self.pid()).expect("the child's start is read")
        }
    }

    impl Drop for Pinger {
        /// Kill the whole tree, `ping` included, so no grandchild outlives
        /// the test holding its inherited handles; then reap the child.
        fn drop(&mut self) {
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &self.pid().to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A child that has exited and been waited on: its pid and its start.
    fn exited_child() -> (u32, u64) {
        let child = Pinger::spawn();
        let ids = (child.pid(), child.start());
        drop(child);
        ids
    }

    /// The runtime under test, stopped when dropped.
    struct Runtime {
        runs: Arc<DispatchRuns>,
        rx: broadcast::Receiver<DispatchRunEvent>,
        shutdown: CancellationToken,
    }

    impl Runtime {
        fn start(live: &Path) -> Self {
            let runs = DispatchRuns::new();
            let rx = runs.subscribe();
            let shutdown = CancellationToken::new();
            spawn(
                live.to_path_buf(),
                Arc::clone(&runs),
                timing(),
                shutdown.clone(),
            )
            .expect("the watcher starts");
            Self { runs, rx, shutdown }
        }

        /// Wait for the first event `matches` accepts, failing as hung at the
        /// guard.
        async fn wait_for(
            &mut self,
            what: &str,
            matches: impl Fn(&DispatchRunEvent) -> bool,
        ) -> DispatchRunEvent {
            let rx = &mut self.rx;
            let wait = async {
                loop {
                    match rx.recv().await {
                        Ok(event) if matches(&event) => return event,
                        Ok(_) | Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => unreachable!("the event channel closed"),
                    }
                }
            };
            tokio::time::timeout(HANG_GUARD, wait)
                .await
                .unwrap_or_else(|_| unreachable!("hung waiting for {what}"))
        }

        /// Wait for an `Updated` of `run_id` that `matches` accepts.
        async fn wait_updated(
            &mut self,
            run_id: &str,
            what: &str,
            matches: impl Fn(&DispatchRunSnapshot) -> bool,
        ) -> DispatchRunSnapshot {
            let event = self
                .wait_for(what, |event| {
                    matches!(event, DispatchRunEvent::Updated(run) if run.run_id == run_id && matches(run))
                })
                .await;
            match event {
                DispatchRunEvent::Updated(run) => run,
                DispatchRunEvent::Removed(_) => unreachable!("the wait accepts only an update"),
            }
        }

        /// The events sent so far and not yet received.
        fn drain(&mut self) -> Vec<DispatchRunEvent> {
            let mut events = Vec::new();
            loop {
                match self.rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Lagged(_)) => {}
                    Err(TryRecvError::Empty | TryRecvError::Closed) => return events,
                }
            }
        }
    }

    impl Drop for Runtime {
        fn drop(&mut self) {
            self.shutdown.cancel();
        }
    }

    /// Wait until `done` holds, polling the observation, failing as hung at
    /// the guard.
    async fn wait_until(what: &str, done: impl Fn() -> bool) {
        let wait = async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(HANG_GUARD, wait)
            .await
            .unwrap_or_else(|_| unreachable!("hung waiting until {what}"));
    }

    /// Wait for one full scan to finish: an orphan stop marker is deleted
    /// only by a scan, after its runs were read and removed.
    async fn wait_a_scan(live: &Path) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let marker = live.join(format!(
            "zz-scan-{}.stop",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&marker, b"").expect("the scan marker is written");
        wait_until("a scan deletes its marker", || !marker.exists()).await;
    }

    fn is_removed(event: &DispatchRunEvent, run_id: &str) -> bool {
        matches!(event, DispatchRunEvent::Removed(id) if id == run_id)
    }

    fn is_update_of(event: &DispatchRunEvent, run_id: &str) -> bool {
        matches!(event, DispatchRunEvent::Updated(run) if run.run_id == run_id)
    }

    #[tokio::test]
    async fn a_run_file_appears_and_its_stream_lines_show_as_actions() {
        let scratch = Scratch::new("appears");
        let child = Pinger::spawn();
        let mut runtime = Runtime::start(&scratch.live());
        scratch.write_run(
            "run-a",
            &scratch.run_text("run-a", child.pid(), Some(child.start())),
        );
        runtime
            .wait_updated("run-a", "the run's first update", |run| {
                run.status == DispatchRunStatus::Starting
            })
            .await;

        scratch.append_stream(&format!("{INIT_LINE}\n{TOOL_LINE}\n"));
        let run = runtime
            .wait_updated("run-a", "the tool action", |run| {
                run.recent_actions
                    .iter()
                    .any(|action| action == TOOL_ACTION)
            })
            .await;
        assert_eq!(run.status, DispatchRunStatus::Working);
        assert_eq!(run.agent_session_id.as_deref(), Some("s1"));
        assert_eq!(runtime.runs.snapshots().len(), 1);
    }

    #[tokio::test]
    async fn a_partial_stream_line_waits_for_its_newline() {
        let scratch = Scratch::new("partial");
        let child = Pinger::spawn();
        let mut runtime = Runtime::start(&scratch.live());
        scratch.write_run(
            "run-p",
            &scratch.run_text("run-p", child.pid(), Some(child.start())),
        );

        scratch.append_stream(&format!("{INIT_LINE}\n{TOOL_LINE}"));
        let run = runtime
            .wait_updated("run-p", "the init line", |run| {
                run.agent_session_id.as_deref() == Some("s1")
            })
            .await;
        assert!(
            !run.recent_actions
                .iter()
                .any(|action| action == TOOL_ACTION),
            "a line without its newline is not read: {:?}",
            run.recent_actions
        );

        scratch.append_stream("\n");
        runtime
            .wait_updated("run-p", "the completed line", |run| {
                run.recent_actions
                    .iter()
                    .any(|action| action == TOOL_ACTION)
            })
            .await;
    }

    #[tokio::test]
    async fn killing_the_child_removes_the_run_and_deletes_its_file() {
        let scratch = Scratch::new("killed");
        let child = Pinger::spawn();
        let mut runtime = Runtime::start(&scratch.live());
        let path = scratch.write_run(
            "run-k",
            &scratch.run_text("run-k", child.pid(), Some(child.start())),
        );
        runtime
            .wait_updated("run-k", "the run's listing", |_| true)
            .await;

        drop(child);
        runtime
            .wait_for("the run's removal", |event| is_removed(event, "run-k"))
            .await;
        assert!(!path.exists(), "the dead run's file is deleted");
        assert_eq!(runtime.runs.snapshots().len(), 0);

        wait_a_scan(&scratch.live()).await;
        let later = runtime.drain();
        assert!(
            !later.iter().any(|event| is_removed(event, "run-k")),
            "one removal per run: {later:?}"
        );
    }

    #[tokio::test]
    async fn a_dead_pid_file_is_dropped_at_startup() {
        let scratch = Scratch::new("dead");
        let (pid, start) = exited_child();
        let path = scratch.write_run("run-d", &scratch.run_text("run-d", pid, Some(start)));
        let mut runtime = Runtime::start(&scratch.live());

        wait_until("the dead run's file is deleted", || !path.exists()).await;
        assert_eq!(runtime.runs.snapshots().len(), 0);
        let events = runtime.drain();
        assert!(
            !events.iter().any(|event| is_update_of(event, "run-d")),
            "a dead run is never listed: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_file_whose_start_does_not_match_is_rejected() {
        let scratch = Scratch::new("mismatch");
        let child = Pinger::spawn();
        let mut runtime = Runtime::start(&scratch.live());
        let path = scratch.write_run(
            "run-m",
            &scratch.run_text("run-m", child.pid(), Some(child.start() + 1)),
        );

        wait_until("the mismatched run's file is deleted", || !path.exists()).await;
        assert_eq!(runtime.runs.snapshots().len(), 0);
        let events = runtime.drain();
        assert!(
            !events.iter().any(|event| is_update_of(event, "run-m")),
            "a run whose start does not match is never listed: {events:?}"
        );
    }

    #[tokio::test]
    async fn deleting_the_file_removes_the_run_once() {
        let scratch = Scratch::new("deleted");
        let child = Pinger::spawn();
        let mut runtime = Runtime::start(&scratch.live());
        let path = scratch.write_run(
            "run-x",
            &scratch.run_text("run-x", child.pid(), Some(child.start())),
        );
        runtime
            .wait_updated("run-x", "the run's listing", |_| true)
            .await;

        std::fs::remove_file(&path).expect("the run file is deleted");
        runtime
            .wait_for("the run's removal", |event| is_removed(event, "run-x"))
            .await;
        wait_a_scan(&scratch.live()).await;
        let later = runtime.drain();
        assert!(
            !later.iter().any(|event| is_removed(event, "run-x")),
            "a scan after the removal sends none: {later:?}"
        );
        assert_eq!(runtime.runs.snapshots().len(), 0);
    }

    #[tokio::test]
    async fn an_unknown_schema_file_is_skipped_and_kept() {
        let scratch = Scratch::new("unknown");
        let path = scratch.write_run("run-u", r#"{"schema": 2, "run_id": "run-u"}"#);
        let mut runtime = Runtime::start(&scratch.live());

        wait_a_scan(&scratch.live()).await;
        assert_eq!(runtime.runs.snapshots().len(), 0);
        assert!(path.exists(), "an unknown schema's file is kept");
        let events = runtime.drain();
        assert!(
            !events.iter().any(|event| is_update_of(event, "run-u")),
            "an unknown schema's run is never listed: {events:?}"
        );
    }

    #[tokio::test]
    async fn an_orphan_stop_marker_is_deleted() {
        let scratch = Scratch::new("orphan");
        scratch.write_run("kept", r#"{"schema": 2}"#);
        let kept = scratch.live().join("kept.stop");
        let orphan = scratch.live().join("orphan.stop");
        std::fs::write(&kept, b"").expect("the kept marker is written");
        std::fs::write(&orphan, b"").expect("the orphan marker is written");
        let _runtime = Runtime::start(&scratch.live());

        wait_until("the orphan marker is deleted", || !orphan.exists()).await;
        wait_a_scan(&scratch.live()).await;
        assert!(kept.exists(), "a marker beside its run file is kept");
    }

    #[tokio::test]
    async fn a_shrunk_stream_replays_without_duplicates() {
        const GREP_LINE: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Grep","input":{}}]}}"#;
        let scratch = Scratch::new("shrunk");
        let child = Pinger::spawn();
        let mut runtime = Runtime::start(&scratch.live());
        scratch.append_stream(&format!("{INIT_LINE}\n{TOOL_LINE}\n"));
        scratch.write_run(
            "run-s",
            &scratch.run_text("run-s", child.pid(), Some(child.start())),
        );
        runtime
            .wait_updated("run-s", "the first stream's tool action", |run| {
                run.recent_actions
                    .iter()
                    .any(|action| action == TOOL_ACTION)
            })
            .await;

        std::fs::write(scratch.stream(), format!("{GREP_LINE}\n"))
            .expect("the stream is rewritten shorter");
        runtime
            .wait_updated("run-s", "the rewritten stream's one action", |run| {
                run.recent_actions == ["tool: Grep"]
            })
            .await;
    }

    #[tokio::test]
    async fn spawn_returns_with_the_existing_runs_listed() {
        let scratch = Scratch::new("startup");
        let child = Pinger::spawn();
        scratch.write_run(
            "run-e",
            &scratch.run_text("run-e", child.pid(), Some(child.start())),
        );
        let runtime = Runtime::start(&scratch.live());
        let listed: Vec<String> = runtime
            .runs
            .snapshots()
            .into_iter()
            .map(|run| run.run_id)
            .collect();
        assert_eq!(
            listed,
            ["run-e"],
            "the startup scan ran before spawn returned"
        );
    }

    #[tokio::test]
    async fn a_broken_rewrite_keeps_the_listed_run() {
        let scratch = Scratch::new("broken");
        let child = Pinger::spawn();
        let mut runtime = Runtime::start(&scratch.live());
        let path = scratch.write_run(
            "run-b",
            &scratch.run_text("run-b", child.pid(), Some(child.start())),
        );
        runtime
            .wait_updated("run-b", "the run's listing", |_| true)
            .await;

        scratch.write_run("run-b", "{ \"schema\": 1, \"run_id\": ");
        wait_a_scan(&scratch.live()).await;
        assert_eq!(
            runtime.runs.snapshots().len(),
            1,
            "a broken file keeps its run"
        );
        assert!(path.exists(), "a broken file is kept for the next read");
        let later = runtime.drain();
        assert!(
            !later.iter().any(|event| is_removed(event, "run-b")),
            "a parse failure never removes a listed run: {later:?}"
        );
    }

    #[tokio::test]
    async fn a_stream_written_before_the_run_file_replays_from_the_start() {
        let scratch = Scratch::new("replay");
        let child = Pinger::spawn();
        scratch.append_stream(&format!("{INIT_LINE}\n{TOOL_LINE}\n"));
        let mut runtime = Runtime::start(&scratch.live());
        scratch.write_run(
            "run-r",
            &scratch.run_text("run-r", child.pid(), Some(child.start())),
        );
        let run = runtime
            .wait_updated("run-r", "the replayed tool action", |run| {
                run.recent_actions
                    .iter()
                    .any(|action| action == TOOL_ACTION)
            })
            .await;
        assert_eq!(run.recent_actions, ["system: init", TOOL_ACTION]);
        assert_eq!(run.agent_session_id.as_deref(), Some("s1"));
    }
}
