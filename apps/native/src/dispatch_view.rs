//! A pane that follows a dispatch run: a stats line over the run's recent
//! actions, kept scrolled to the end until the user scrolls up, and the last
//! state of a run that has gone. The model is plain Rust, so every rule is
//! unit-tested; `RootView` keeps the state and draws it.

use std::collections::HashMap;

use gpui::{
    Context, Div, ElementId, ScrollHandle, ScrollWheelEvent, SharedString, Stateful, div,
    prelude::*, px,
};
use protocol::{DaemonMessage, DispatchRunSnapshot, DispatchRunStatus, PaneFollow};

use crate::dashboard_rows::{format_cost, format_tokens};
use crate::driver_id::DriverId as _;
use crate::fonts::{DEFAULT_FAMILY, ui_px_at, ui_text_size};
use crate::target::HostKey;
use crate::ui_theme::theme;
use crate::{RootView, UI_TEXT_SIZE, tabs};

/// The word the stats line shows for a run's `status`.
pub(crate) fn status_word(status: DispatchRunStatus) -> &'static str {
    match status {
        DispatchRunStatus::Starting => "starting",
        DispatchRunStatus::Working => "working",
        DispatchRunStatus::Stopping => "stopping",
        DispatchRunStatus::Ended => "ended",
        DispatchRunStatus::Unknown => "unknown",
    }
}

/// The run's stats line: its status (`ended` once the run has gone), cost
/// and input and output tokens.
pub(crate) fn stats_line(run: &DispatchRunSnapshot, ended: bool) -> String {
    let status = if ended {
        "ended"
    } else {
        status_word(run.status)
    };
    format!(
        "{status} · {cost} · {inp} in · {out} out",
        cost = format_cost(Some(run.metrics.cost_usd)),
        inp = format_tokens(run.metrics.input_tokens),
        out = format_tokens(run.metrics.output_tokens),
    )
}

/// The last snapshot a client saw of a followed dispatch run, and whether
/// the run has gone since.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RunKeep {
    pub last: DispatchRunSnapshot,
    pub ended: bool,
}

/// The kept snapshots of followed dispatch runs, by host and run id.
#[derive(Debug, Default)]
pub(crate) struct KeptRuns(HashMap<(HostKey, String), RunKeep>);

impl KeptRuns {
    /// Stores or refreshes `run` of `host`, as not ended.
    pub(crate) fn remember(&mut self, host: &HostKey, run: &DispatchRunSnapshot) {
        let keep = RunKeep {
            last: run.clone(),
            ended: false,
        };
        self.0.insert((host.clone(), run.run_id.clone()), keep);
    }

    /// Stores or refreshes, as not ended, each run of `host` in `run_ids`
    /// that `lookup` finds a snapshot of.
    pub(crate) fn remember_each<'a>(
        &mut self,
        host: &HostKey,
        run_ids: &[String],
        lookup: impl Fn(&str) -> Option<&'a DispatchRunSnapshot>,
    ) {
        for run in run_ids.iter().filter_map(|run_id| lookup(run_id)) {
            self.remember(host, run);
        }
    }

    /// Marks run `run_id` of `host` as ended; nothing when it is not kept.
    pub(crate) fn end(&mut self, host: &HostKey, run_id: &str) {
        if let Some(keep) = self.0.get_mut(&(host.clone(), run_id.to_owned())) {
            keep.ended = true;
        }
    }

    /// Keeps only the runs `followed` says a pane still follows.
    pub(crate) fn retain_followed(&mut self, mut followed: impl FnMut(&HostKey, &str) -> bool) {
        self.0.retain(|(host, run_id), _| followed(host, run_id));
    }

    /// The kept state of run `run_id` of `host`.
    pub(crate) fn get(&self, host: &HostKey, run_id: &str) -> Option<&RunKeep> {
        self.0.get(&(host.clone(), run_id.to_owned()))
    }

    /// The ids of `host`'s kept runs that have not ended.
    fn unended_on(&self, host: &HostKey) -> Vec<String> {
        self.0
            .iter()
            .filter(|((on, _), keep)| on == host && !keep.ended)
            .map(|((_, run_id), _)| run_id.clone())
            .collect()
    }
}

/// What a pane that follows a dispatch run draws.
#[derive(Debug, PartialEq)]
pub(crate) enum RunShown<'a> {
    /// The run, still working or not yet re-listed.
    Live(&'a DispatchRunSnapshot),
    /// The run's last snapshot, the run having gone.
    Ended(&'a DispatchRunSnapshot),
    /// Nothing known of the run.
    Gone,
}

/// What to draw for a followed run: the live run wins; else a kept snapshot,
/// ended once marked so, and live until then (between a reconnect's Welcome
/// and its re-list); else nothing.
pub(crate) fn shown<'a>(
    live: Option<&'a DispatchRunSnapshot>,
    kept: Option<&'a RunKeep>,
) -> RunShown<'a> {
    match (live, kept) {
        (Some(run), _) => RunShown::Live(run),
        (None, Some(keep)) if keep.ended => RunShown::Ended(&keep.last),
        (None, Some(keep)) => RunShown::Live(&keep.last),
        (None, None) => RunShown::Gone,
    }
}

/// A followed run's log scroll: the run it scrolls, the handle and whether
/// it stays at the end.
#[derive(Debug, Clone)]
pub(crate) struct RunScroll {
    run: (HostKey, String),
    following: bool,
    handle: ScrollHandle,
}

impl RunScroll {
    /// A scroll of `run`'s log, at its end.
    fn of(run: (HostKey, String)) -> Self {
        Self {
            run,
            following: true,
            handle: ScrollHandle::new(),
        }
    }
}

/// A pane that follows a dispatch run, with the run's host and id.
struct FollowedRun {
    pane_id: String,
    host: HostKey,
    run_id: String,
}

impl RootView {
    /// Every pane of every tab that follows a dispatch run.
    fn followed_runs(&self) -> Vec<FollowedRun> {
        self.tabs
            .tabs()
            .iter()
            .filter_map(|tab| tab.grid())
            .flat_map(tabs::collect_panes)
            .filter_map(|pane| match pane.follow {
                Some(PaneFollow::DispatchRun { run_id }) => Some(FollowedRun {
                    pane_id: pane.id.to_owned(),
                    host: pane.host.clone(),
                    run_id: run_id.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// Whether a pane follows run `run_id` of `host`.
    fn follows_run(&self, host: &HostKey, run_id: &str) -> bool {
        tabs::find_tab_following(self.tabs.tabs(), host, run_id).is_some()
    }

    /// Keeps the snapshot of each followed run `msg` lists, updates or
    /// removes, before the sidebar folds it; a run that goes is marked ended.
    pub(crate) fn keep_followed_runs(&mut self, host: &HostKey, msg: &DaemonMessage) {
        match msg {
            DaemonMessage::DispatchRunUpdated { run } if self.follows_run(host, &run.run_id) => {
                self.kept_runs.remember(host, run);
            }
            DaemonMessage::DispatchRuns { runs } => self.keep_listed_runs(host, runs),
            DaemonMessage::DispatchRunRemoved { run_id } if self.follows_run(host, run_id) => {
                self.end_kept_run(host, run_id);
            }
            _ => {}
        }
    }

    /// A whole list: each followed run in it is kept, and each followed or
    /// kept run of `host` it leaves out has gone.
    fn keep_listed_runs(&mut self, host: &HostKey, runs: &[DispatchRunSnapshot]) {
        for run in runs {
            if self.follows_run(host, &run.run_id) {
                self.kept_runs.remember(host, run);
            }
        }
        let listed = |run_id: &str| runs.iter().any(|run| run.run_id == run_id);
        let mut gone: Vec<String> = self
            .followed_runs()
            .into_iter()
            .filter(|followed| followed.host == *host)
            .map(|followed| followed.run_id)
            .chain(self.kept_runs.unended_on(host))
            .filter(|run_id| !listed(run_id))
            .collect();
        gone.sort();
        gone.dedup();
        for run_id in gone {
            self.end_kept_run(host, &run_id);
        }
    }

    /// Keeps the sidebar's current snapshot of run `run_id` of `host`, when
    /// it has one, and marks the run ended.
    fn end_kept_run(&mut self, host: &HostKey, run_id: &str) {
        if let Some(run) = self.sidebar.run_on(host, run_id) {
            self.kept_runs.remember(host, run);
        }
        self.kept_runs.end(host, run_id);
    }

    /// The ids of the runs of `host` that a pane follows.
    fn followed_on(&self, host: &HostKey) -> Vec<String> {
        self.followed_runs()
            .into_iter()
            .filter(|followed| followed.host == *host)
            .map(|followed| followed.run_id)
            .collect()
    }

    /// Keeps the sidebar's current snapshot of every followed run of
    /// `host`, before something forgets the host's runs.
    pub(crate) fn keep_followed_snapshots(&mut self, host: &HostKey) {
        let run_ids = self.followed_on(host);
        let sidebar = &self.sidebar;
        self.kept_runs
            .remember_each(host, &run_ids, |run_id| sidebar.run_on(host, run_id));
    }

    /// Before a new connection to `host` forgets its runs: keeps every
    /// followed run of `host`, so it draws until the re-list decides. A
    /// connection that lists no dispatch runs never re-lists, so its runs
    /// end at once.
    pub(crate) fn keep_runs_before_welcome(&mut self, host: &HostKey) {
        self.keep_followed_snapshots(host);
        let lists_runs = self
            .conn_of(host)
            .is_some_and(|conn| conn.supports(protocol::FEATURE_DISPATCH_RUNS));
        if !lists_runs {
            for run_id in self.followed_on(host) {
                self.kept_runs.end(host, &run_id);
            }
        }
    }

    /// Keeps only the kept runs and log scrolls of panes that still follow
    /// a run, and gives each such pane a log scroll.
    pub(crate) fn prune_kept_runs(&mut self) {
        let followed = self.followed_runs();
        self.kept_runs.retain_followed(|host, run_id| {
            followed
                .iter()
                .any(|f| f.host == *host && f.run_id == run_id)
        });
        self.run_scrolls
            .retain(|pane_id, _| followed.iter().any(|f| f.pane_id == *pane_id));
        for f in followed {
            let run = (f.host, f.run_id);
            let scroll = self
                .run_scrolls
                .entry(f.pane_id)
                .or_insert_with(|| RunScroll::of(run.clone()));
            // A pane that now follows another run starts at its end.
            if scroll.run != run {
                *scroll = RunScroll::of(run);
            }
        }
    }

    /// What run `run_id` of `host` draws as.
    fn run_shown(&self, host: &HostKey, run_id: &str) -> RunShown<'_> {
        shown(
            self.sidebar.run_on(host, run_id),
            self.kept_runs.get(host, run_id),
        )
    }

    /// The host and run pane `pane_id` follows.
    fn pane_run(&self, pane_id: &str) -> Option<(HostKey, String)> {
        self.followed_runs()
            .into_iter()
            .find(|f| f.pane_id == pane_id)
            .map(|f| (f.host, f.run_id))
    }

    /// The body of pane `pane_id`, which follows run `run_id` of `host`: the
    /// stats line over the run's recent actions, the run folder under them
    /// once the run has gone, or a note when nothing is known of the run.
    pub(crate) fn dispatch_body(
        &self,
        pane_id: &str,
        host: &HostKey,
        run_id: &str,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let inner = match self.run_shown(host, run_id) {
            RunShown::Live(run) => self.run_column(pane_id, run, false, cx),
            RunShown::Ended(run) => self.run_column(pane_id, run, true, cx),
            RunShown::Gone => crate::grid_view::muted_note(
                ui_text_size(cx),
                format!("This run has ended: {run_id}"),
                &t,
            )
            .driver_id(|| format!("dispatch-gone-{pane_id}")),
        };
        div()
            .driver_id(|| format!("pane-follow-{pane_id}"))
            .flex()
            .flex_col()
            .flex_1()
            .size_full()
            .min_h(px(0.0))
            .child(inner)
    }

    /// A run's stats line, its log and, once it has gone, its run folder.
    fn run_column(
        &self,
        pane_id: &str,
        run: &DispatchRunSnapshot,
        ended: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let size = ui_px_at(ui_text_size(cx), UI_TEXT_SIZE);
        let line = |id: String, text: String| {
            div()
                .driver_id(|| id)
                .px(px(8.0))
                .py(px(4.0))
                .text_size(size)
                .text_color(gpui::rgb(t.muted))
                .child(text)
        };
        let stats = line(format!("dispatch-stats-{pane_id}"), stats_line(run, ended));
        let note = ended.then(|| {
            line(
                format!("dispatch-ended-{pane_id}"),
                format!("Ended. Run folder: {}", run.run_dir),
            )
        });
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(stats)
            .child(self.run_log(pane_id, run, cx))
            .children(note)
    }

    /// The run's recent actions, oldest first, in the terminal's font,
    /// scrolled to the end while the user has not scrolled up.
    fn run_log(
        &self,
        pane_id: &str,
        run: &DispatchRunSnapshot,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let t = theme(cx);
        let family = self.sidebar.ui_state().terminal_font.family.as_deref();
        let family = SharedString::from(family.unwrap_or(DEFAULT_FAMILY).to_owned());
        let name = format!("dispatch-log-{pane_id}");
        let column = div()
            .id(ElementId::Name(SharedString::from(name.clone())))
            .driver_id(|| name)
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(8.0))
            .font_family(family)
            .font_weight(crate::fonts::mono_weight())
            .text_size(ui_px_at(ui_text_size(cx), UI_TEXT_SIZE))
            .text_color(gpui::rgb(t.text_2))
            .children(
                run.recent_actions
                    .iter()
                    .map(|action| div().child(action.clone())),
            );
        let Some(scroll) = self.run_scrolls.get(pane_id) else {
            return column;
        };
        if scroll.following {
            scroll.handle.scroll_to_bottom();
        }
        let (pane, handle) = (pane_id.to_owned(), scroll.handle.clone());
        column
            .track_scroll(&scroll.handle)
            .on_scroll_wheel(cx.listener(move |_, _: &ScrollWheelEvent, window, cx| {
                cx.stop_propagation();
                let (pane, handle) = (pane.clone(), handle.clone());
                // Read once the wheel has moved the log.
                cx.defer_in(window, move |this, _, cx| {
                    let scrolled = -handle.offset().y / px(1.0);
                    let max_scroll = handle.max_offset().height / px(1.0);
                    if let Some(scroll) = this.run_scrolls.get_mut(&pane) {
                        scroll.following =
                            crate::chat::follows_bottom(Some((scrolled, max_scroll)));
                    }
                    cx.notify();
                });
            }))
    }

    /// The stats line pane `pane_id` draws for the run it follows.
    #[must_use]
    pub fn dispatch_view_stats(&self, pane_id: &str) -> Option<String> {
        let (host, run_id) = self.pane_run(pane_id)?;
        match self.run_shown(&host, &run_id) {
            RunShown::Live(run) => Some(stats_line(run, false)),
            RunShown::Ended(run) => Some(stats_line(run, true)),
            RunShown::Gone => None,
        }
    }

    /// The log lines pane `pane_id` draws for the run it follows, oldest
    /// first.
    #[must_use]
    pub fn dispatch_view_lines(&self, pane_id: &str) -> Vec<String> {
        let Some((host, run_id)) = self.pane_run(pane_id) else {
            return Vec::new();
        };
        match self.run_shown(&host, &run_id) {
            RunShown::Live(run) | RunShown::Ended(run) => run.recent_actions.clone(),
            RunShown::Gone => Vec::new(),
        }
    }

    /// The note pane `pane_id` draws for the run it follows: the run folder
    /// line once the run has gone, or the note for a run never seen.
    #[must_use]
    pub fn dispatch_view_note(&self, pane_id: &str) -> Option<String> {
        let (host, run_id) = self.pane_run(pane_id)?;
        match self.run_shown(&host, &run_id) {
            RunShown::Live(_) => None,
            RunShown::Ended(run) => Some(format!("Ended. Run folder: {}", run.run_dir)),
            RunShown::Gone => Some(format!("This run has ended: {run_id}")),
        }
    }

    /// Whether pane `pane_id`'s run log stays scrolled to the end.
    #[must_use]
    pub fn dispatch_view_following(&self, pane_id: &str) -> Option<bool> {
        self.run_scrolls.get(pane_id).map(|scroll| scroll.following)
    }

    /// Whether a snapshot of run `run_id` of `host` is kept.
    #[must_use]
    pub fn kept_run(&self, host: &HostKey, run_id: &str) -> bool {
        self.kept_runs.get(host, run_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::tests::run;

    fn working(id: &str) -> DispatchRunSnapshot {
        run(id, "C:\\wt", None, 0, DispatchRunStatus::Working)
    }

    #[test]
    fn status_word_names_each_status() {
        for (status, want) in [
            (DispatchRunStatus::Starting, "starting"),
            (DispatchRunStatus::Working, "working"),
            (DispatchRunStatus::Stopping, "stopping"),
            (DispatchRunStatus::Ended, "ended"),
            (DispatchRunStatus::Unknown, "unknown"),
        ] {
            assert_eq!(status_word(status), want, "{status:?}");
        }
    }

    fn metered(id: &str) -> DispatchRunSnapshot {
        let mut run = working(id);
        run.metrics.cost_usd = 0.46;
        run.metrics.input_tokens = 842_000;
        run.metrics.output_tokens = 3_100;
        run
    }

    #[test]
    fn stats_line_reads_status_cost_and_tokens() {
        assert_eq!(
            stats_line(&metered("r1"), false),
            "working · $0.46 · 842k in · 3k out"
        );
    }

    #[test]
    fn stats_line_says_ended_once_ended() {
        assert_eq!(
            stats_line(&metered("r1"), true),
            "ended · $0.46 · 842k in · 3k out"
        );
    }

    #[test]
    fn kept_runs_remember_end_and_prune() {
        let mut kept = KeptRuns::default();
        let host = HostKey::Local;
        kept.remember(&host, &working("r1"));
        kept.remember(&host, &working("r2"));
        assert_eq!(kept.get(&host, "r1").map(|k| k.ended), Some(false));

        kept.end(&host, "r1");
        kept.end(&host, "absent");
        assert_eq!(kept.get(&host, "r1").map(|k| k.ended), Some(true));
        assert!(kept.get(&host, "absent").is_none(), "ending adds nothing");

        kept.retain_followed(|_, run_id| run_id != "r2");
        assert!(kept.get(&host, "r1").is_some());
        assert!(kept.get(&host, "r2").is_none(), "an unfollowed run goes");
    }

    #[test]
    fn remember_each_keeps_every_run_it_finds() {
        let mut kept = KeptRuns::default();
        let host = HostKey::Local;
        let listed = [working("r1")];
        let lookup = |run_id: &str| listed.iter().find(|run| run.run_id == run_id);
        kept.remember(&host, &working("r1"));
        kept.end(&host, "r1");

        kept.remember_each(&host, &["r1".to_owned(), "r2".to_owned()], lookup);

        assert_eq!(
            kept.get(&host, "r1").map(|k| k.ended),
            Some(false),
            "a found run is kept as not ended"
        );
        assert!(
            kept.get(&host, "r2").is_none(),
            "a run not found is not kept"
        );
    }

    #[test]
    fn shown_prefers_the_live_run() {
        let live = working("r1");
        let mut old = working("r1");
        old.recent_actions = vec!["old".to_owned()];
        let keep = RunKeep {
            last: old,
            ended: true,
        };
        assert_eq!(shown(Some(&live), Some(&keep)), RunShown::Live(&live));
    }

    #[test]
    fn shown_uses_the_kept_snapshot_before_the_relist() {
        let keep = RunKeep {
            last: working("r1"),
            ended: false,
        };
        assert_eq!(shown(None, Some(&keep)), RunShown::Live(&keep.last));
    }

    #[test]
    fn shown_is_ended_once_marked() {
        let keep = RunKeep {
            last: working("r1"),
            ended: true,
        };
        assert_eq!(shown(None, Some(&keep)), RunShown::Ended(&keep.last));
    }

    #[test]
    fn shown_is_gone_without_either() {
        assert_eq!(shown(None, None), RunShown::Gone);
    }
}
