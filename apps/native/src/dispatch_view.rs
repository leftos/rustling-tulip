//! A pane that follows a dispatch run: a stats line over the run's recent
//! actions, kept scrolled to the end until the user scrolls up, and the last
//! state of a run that has gone. The model is plain Rust, so every rule is
//! unit-tested; `RootView` keeps the state and draws it.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, Context, Div, ElementId, Pixels, ScrollHandle, ScrollWheelEvent, SharedString,
    Stateful, StyledText, div, prelude::*, px,
};
use protocol::{ClientMessage, DaemonMessage, DispatchRunSnapshot, DispatchRunStatus, PaneFollow};

use crate::dashboard_rows::{format_cost, format_tokens};
use crate::dispatch_menu::stop_tip;
use crate::driver_id::DriverId as _;
use crate::ellipsis::{ellipsized, truncating};
use crate::fonts::{DEFAULT_FAMILY, ui_px_at, ui_text_size};
use crate::grid_view::{TipAt, header_tipped};
use crate::session_menu::{header_text_button, on_press};
use crate::spawn_view::DISABLED_OPACITY;
use crate::target::HostKey;
use crate::transcript_view::{LineKind, Transcript, TranscriptKey, transcript_lines};
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

/// What a pane header draws for the dispatch run its pane follows: the
/// run's title, its `DS · backend · tier n` chip and its status word.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RunHeader {
    pub title: String,
    pub chip: String,
    pub status: &'static str,
}

/// The header of a followed run as `shown` draws it; `None` when nothing is
/// known of the run.
pub(crate) fn run_header(shown: &RunShown<'_>) -> Option<RunHeader> {
    let (run, ended) = match *shown {
        RunShown::Live(run) => (run, false),
        RunShown::Ended(run) => (run, true),
        RunShown::Gone => return None,
    };
    let tier = run
        .tier
        .map(|tier| format!(" · tier {tier}"))
        .unwrap_or_default();
    let status = if ended {
        "ended"
    } else {
        status_word(run.status)
    };
    Some(RunHeader {
        title: run.title.clone(),
        chip: format!("DS · {}{tier}", run.backend),
        status,
    })
}

/// The status a followed run's Stop goes by: the run's own, `Ended` once it
/// has gone; `None` when nothing is known of the run.
fn run_status(shown: &RunShown<'_>) -> Option<DispatchRunStatus> {
    match *shown {
        RunShown::Live(run) => Some(run.status),
        RunShown::Ended(_) => Some(DispatchRunStatus::Ended),
        RunShown::Gone => None,
    }
}

/// The left of the header of pane `pane_id`, which follows a dispatch run:
/// `title`, the header's styled title box, holding the run's title cut to
/// fit, then the run's chip, which gives way with `…` down to `chip_floor`
/// as a session's chips do, and its status word, kept whole.
pub(crate) fn run_title(
    pane_id: &str,
    header: &RunHeader,
    title: Stateful<Div>,
    chip_floor: Option<Pixels>,
    cx: &mut Context<RootView>,
) -> Vec<AnyElement> {
    let t = theme(cx);
    let title_id = format!("dispatch-title-{pane_id}");
    let text = SharedString::from(header.title.clone());
    let name = div()
        .driver_id(|| title_id.clone())
        .relative()
        .min_w(px(0.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .children(ellipsized(
            &title_id,
            text.clone(),
            StyledText::new(text.clone()),
        ));
    let chip_id = format!("dispatch-chip-{pane_id}");
    let chip = div()
        .id(ElementId::Name(SharedString::from(chip_id.clone())))
        .driver_id(|| chip_id.clone())
        .flex()
        .flex_none()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .font_family(DEFAULT_FAMILY)
        .font_weight(crate::fonts::mono_weight())
        .text_size(ui_px_at(ui_text_size(cx), CHIP_TEXT_SIZE))
        .text_color(gpui::rgb(t.subtle))
        .child(truncating(&chip_id, StyledText::new(header.chip.clone())));
    let chip = crate::grid_view::give_way(chip, chip_floor);
    let status = div()
        .driver_id(|| format!("dispatch-status-{pane_id}"))
        .flex_none()
        .whitespace_nowrap()
        .text_color(gpui::rgb(t.muted))
        .child(header.status);
    vec![
        title.child(name).into_any_element(),
        chip.into_any_element(),
        status.into_any_element(),
    ]
}

/// The size of a followed run's chip in its pane header, as the sidebar's
/// dispatch tag.
pub(crate) const CHIP_TEXT_SIZE: f32 = 10.0;

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
#[derive(Debug, Clone, Copy, PartialEq)]
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

/// A followed run's or subagent's log scroll: the transcript it scrolls,
/// the handle and whether it stays at the end.
#[derive(Debug, Clone)]
pub(crate) struct RunScroll {
    key: (HostKey, TranscriptKey),
    following: bool,
    handle: ScrollHandle,
}

impl RunScroll {
    /// A scroll of `key`'s log, at its end.
    fn of(key: (HostKey, TranscriptKey)) -> Self {
        Self {
            key,
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

/// A pane that follows a transcript, with its host and key.
pub(crate) struct FollowedTranscript {
    pub pane_id: String,
    pub host: HostKey,
    pub key: TranscriptKey,
}

/// The transcript a pane `follow` follows; `None` for a follow this build
/// does not know.
fn follow_key(follow: &PaneFollow) -> Option<TranscriptKey> {
    match follow {
        PaneFollow::DispatchRun { run_id } => Some(TranscriptKey::Run(run_id.clone())),
        PaneFollow::Subagent {
            session_id,
            agent_id,
        } => Some(TranscriptKey::Subagent {
            session_id: session_id.clone(),
            agent_id: agent_id.clone(),
        }),
        PaneFollow::RunSubagent { run_id, agent_id } => Some(TranscriptKey::RunSubagent {
            run_id: run_id.clone(),
            agent_id: agent_id.clone(),
        }),
        PaneFollow::Unknown => None,
    }
}

impl RootView {
    /// Every pane of every tab that follows a dispatch run or a subagent.
    pub(crate) fn followed_transcripts(&self) -> Vec<FollowedTranscript> {
        self.tabs
            .tabs()
            .iter()
            .filter_map(|tab| tab.grid())
            .flat_map(tabs::collect_panes)
            .filter_map(|pane| {
                let key = follow_key(pane.follow?)?;
                Some(FollowedTranscript {
                    pane_id: pane.id.to_owned(),
                    host: pane.host.clone(),
                    key,
                })
            })
            .collect()
    }

    /// The host and key of every transcript some pane follows.
    pub(crate) fn followed_transcript_keys(&self) -> HashSet<(HostKey, TranscriptKey)> {
        self.followed_transcripts()
            .into_iter()
            .map(|followed| (followed.host, followed.key))
            .collect()
    }

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

    /// Keeps only the kept runs of panes that still follow a run and the
    /// log scrolls of panes that still follow a transcript, and gives each
    /// such pane a log scroll.
    pub(crate) fn prune_kept_runs(&mut self) {
        let followed = self.followed_runs();
        self.kept_runs.retain_followed(|host, run_id| {
            followed
                .iter()
                .any(|f| f.host == *host && f.run_id == run_id)
        });
        let followed = self.followed_transcripts();
        self.run_scrolls
            .retain(|pane_id, _| followed.iter().any(|f| f.pane_id == *pane_id));
        for f in followed {
            let key = (f.host, f.key);
            let scroll = self
                .run_scrolls
                .entry(f.pane_id)
                .or_insert_with(|| RunScroll::of(key.clone()));
            // A pane that now follows another transcript starts at its end.
            if scroll.key != key {
                *scroll = RunScroll::of(key);
            }
        }
    }

    /// What pane `pane_id` of tab `tab_id` shows of the dispatch run it
    /// follows; `None` when it follows no dispatch run.
    pub(crate) fn pane_run_shown(&self, tab_id: &str, pane_id: &str) -> Option<RunShown<'_>> {
        let Some(PaneFollow::DispatchRun { run_id }) = self.tabs.pane_follow(tab_id, pane_id)
        else {
            return None;
        };
        let host = self.tabs.pane_host(tab_id, pane_id);
        Some(self.run_shown(&host, run_id))
    }

    /// The Stop of pane `pane_id` of tab `tab_id` when it follows a
    /// dispatch run, its tip shown `at`; nothing otherwise.
    pub(crate) fn header_run_stop(
        &self,
        tab_id: &str,
        pane_id: &str,
        at: TipAt,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(PaneFollow::DispatchRun { run_id }) = self.tabs.pane_follow(tab_id, pane_id)
        else {
            return Vec::new();
        };
        let host = self.tabs.pane_host(tab_id, pane_id);
        self.run_stop_controls(pane_id, host, run_id, at, cx)
    }

    /// Pane `pane_id`'s Stop for run `run_id` of `host`, then, once
    /// pressed, its Confirm stop and Cancel. Stop is drawn disabled, saying
    /// why, while the run is stopping or has ended, and not at all when
    /// nothing is known of the run. The buttons act on the press.
    pub(crate) fn run_stop_controls(
        &self,
        pane_id: &str,
        host: HostKey,
        run_id: &str,
        at: TipAt,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(status) = run_status(&self.run_shown(&host, run_id)) else {
            return Vec::new();
        };
        let t = theme(cx);
        let stop_id = format!("dispatch-stop-{pane_id}");
        if let Some(tip) = stop_tip(status) {
            let button = header_text_button(&stop_id, "Stop", t.muted, &t)
                .opacity(DISABLED_OPACITY)
                .cursor_default()
                .hover(|style| style);
            return vec![header_tipped(button, &stop_id, pane_id, tip, at, cx).into_any_element()];
        }
        let key = (host, run_id.to_owned());
        if self.run_stop_armed.as_ref() != Some(&key) {
            let button = header_text_button(&stop_id, "Stop", t.muted, &t);
            return vec![
                on_press(button, cx, move |this, _, _| this.arm_run_stop(key.clone()))
                    .into_any_element(),
            ];
        }
        let confirm = format!("dispatch-stop-confirm-{pane_id}");
        let confirm = header_text_button(&confirm, "Confirm stop", t.danger, &t);
        let cancel = format!("dispatch-stop-cancel-{pane_id}");
        let cancel = header_text_button(&cancel, "Cancel", t.text, &t);
        vec![
            on_press(confirm, cx, move |this, _, _| this.confirm_run_stop(&key)).into_any_element(),
            on_press(cancel, cx, |this, _, _| {
                this.disarm_run_stop();
            })
            .into_any_element(),
        ]
    }

    /// Arms the Stop of `run`, dropping a session's armed Stop.
    fn arm_run_stop(&mut self, run: (HostKey, String)) {
        self.confirm.disarm();
        self.run_stop_armed = Some(run);
    }

    /// The confirm of `run`'s Stop: disarms, and asks the run's host to
    /// stop it unless the run is now stopping or has ended.
    fn confirm_run_stop(&mut self, run: &(HostKey, String)) {
        self.disarm_run_stop();
        let (host, run_id) = run;
        let stoppable = run_status(&self.run_shown(host, run_id))
            .is_some_and(|status| stop_tip(status).is_none());
        if stoppable {
            self.send_to(
                host,
                ClientMessage::StopDispatchRun {
                    run_id: run_id.clone(),
                    request_id: Some(crate::new_request_id()),
                },
            );
        }
    }

    /// Drops an armed dispatch run Stop; returns whether one was armed for
    /// a run that can still be stopped. An arm whose run has since begun
    /// stopping, ended or gone draws no Confirm stop, so it goes silently.
    pub(crate) fn disarm_run_stop(&mut self) -> bool {
        let Some((host, run_id)) = self.run_stop_armed.take() else {
            return false;
        };
        run_status(&self.run_shown(&host, &run_id)).is_some_and(|status| stop_tip(status).is_none())
    }

    /// The labels of the Stop pane `pane_id` of tab `tab_id` draws for the
    /// dispatch run it follows, as [`Self::run_stop_controls`] draws them:
    /// none when it follows no known run, Confirm stop and Cancel while
    /// armed, else Stop.
    pub(crate) fn run_stop_labels(&self, tab_id: &str, pane_id: &str) -> &'static [&'static str] {
        let Some(PaneFollow::DispatchRun { run_id }) = self.tabs.pane_follow(tab_id, pane_id)
        else {
            return &[];
        };
        let host = self.tabs.pane_host(tab_id, pane_id);
        let Some(status) = run_status(&self.run_shown(&host, run_id)) else {
            return &[];
        };
        let armed = self
            .run_stop_armed
            .as_ref()
            .is_some_and(|(on, id)| *on == host && id == run_id);
        if armed && stop_tip(status).is_none() {
            &["Confirm stop", "Cancel"]
        } else {
            &["Stop"]
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
            RunShown::Live(run) => self.run_column(pane_id, host, run, false, cx),
            RunShown::Ended(run) => self.run_column(pane_id, host, run, true, cx),
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

    /// A run of `host`: its stats line, its log and, once it has gone, its
    /// run folder.
    fn run_column(
        &self,
        pane_id: &str,
        host: &HostKey,
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
            .child(self.run_log(pane_id, host, run, cx))
            .children(note)
    }

    /// The run's transcript once one is held, else its recent actions,
    /// oldest first, in the terminal's font, scrolled to the end while the
    /// user has not scrolled up.
    fn run_log(
        &self,
        pane_id: &str,
        host: &HostKey,
        run: &DispatchRunSnapshot,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let key = (host.clone(), TranscriptKey::Run(run.run_id.clone()));
        let rows: Vec<AnyElement> = match self.transcripts.get(&key) {
            Some(transcript) => {
                let line_ids = format!("dispatch-line-{pane_id}");
                Self::transcript_rows(&line_ids, &key, transcript, cx)
            }
            None => run
                .recent_actions
                .iter()
                .map(|action| div().child(action.clone()).into_any_element())
                .collect(),
        };
        self.log_column(pane_id, format!("dispatch-log-{pane_id}"), rows, cx)
    }

    /// Pane `pane_id`'s log column, tagged `name`, of `rows` in the
    /// terminal's font, scrolled to the end while the user has not scrolled
    /// up.
    pub(crate) fn log_column(
        &self,
        pane_id: &str,
        name: String,
        rows: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let t = theme(cx);
        let family = self.sidebar.ui_state().terminal_font.family.as_deref();
        let family = SharedString::from(family.unwrap_or(DEFAULT_FAMILY).to_owned());
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
            .children(rows);
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

    /// The rows of `transcript`, held for `key`, each tagged
    /// `{line_ids}-{n}`; a row of a tool call expands or collapses the
    /// call's output on a press.
    pub(crate) fn transcript_rows(
        line_ids: &str,
        key: &(HostKey, TranscriptKey),
        transcript: &Transcript,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = theme(cx);
        let mut rows = Vec::new();
        for (at, line) in transcript_lines(transcript).into_iter().enumerate() {
            let colour = match line.kind {
                LineKind::Answer | LineKind::Tool => t.text,
                LineKind::Output | LineKind::Muted => t.muted,
                LineKind::Error => t.danger,
            };
            let name = format!("{line_ids}-{at}");
            let row = div()
                .id(ElementId::Name(SharedString::from(name.clone())))
                .driver_id(|| name)
                .text_color(gpui::rgb(colour))
                .child(line.text);
            let Some(entry) = line.entry else {
                rows.push(row.into_any_element());
                continue;
            };
            let key = key.clone();
            let row = on_press(row.cursor_pointer(), cx, move |this, _, _| {
                this.toggle_transcript_entry(&key, &entry);
            });
            rows.push(row.into_any_element());
        }
        rows
    }

    /// Expands the output of tool call `id` in `key`'s transcript, or
    /// collapses it again.
    fn toggle_transcript_entry(&mut self, key: &(HostKey, TranscriptKey), id: &str) {
        if let Some(transcript) = self.transcripts.get_mut(key) {
            transcript.toggle(id);
        }
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
    /// first: the transcript's once one is held, else the recent actions.
    #[must_use]
    pub fn dispatch_view_lines(&self, pane_id: &str) -> Vec<String> {
        let Some((host, run_id)) = self.pane_run(pane_id) else {
            return Vec::new();
        };
        let held = self
            .transcripts
            .get(&(host.clone(), TranscriptKey::Run(run_id.clone())));
        match (self.run_shown(&host, &run_id), held) {
            (RunShown::Gone, _) => Vec::new(),
            (_, Some(transcript)) => transcript_lines(transcript)
                .into_iter()
                .map(|line| line.text)
                .collect(),
            (RunShown::Live(run) | RunShown::Ended(run), None) => run.recent_actions.clone(),
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

    /// The title, chip and status pane `pane_id`'s header draws for the run
    /// it follows; `None` when nothing is known of the run.
    #[must_use]
    pub fn dispatch_view_header(&self, pane_id: &str) -> Option<(String, String, String)> {
        let (host, run_id) = self.pane_run(pane_id)?;
        let header = run_header(&self.run_shown(&host, &run_id))?;
        Some((header.title, header.chip, header.status.to_owned()))
    }

    /// Whether pane `pane_id`'s Stop waits for its confirm.
    #[must_use]
    pub fn dispatch_stop_armed(&self, pane_id: &str) -> bool {
        self.pane_run(pane_id)
            .is_some_and(|run| self.run_stop_armed.as_ref() == Some(&run))
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
    fn run_header_reads_title_chip_and_status() {
        let mut tiered = working("r1");
        tiered.tier = Some(2);
        assert_eq!(
            run_header(&RunShown::Live(&tiered)),
            Some(RunHeader {
                title: "title r1".to_owned(),
                chip: "DS · backend-x · tier 2".to_owned(),
                status: "working",
            })
        );
        let plain = working("r1");
        assert_eq!(
            run_header(&RunShown::Live(&plain)).map(|header| header.chip),
            Some("DS · backend-x".to_owned()),
            "no tier, no tier part"
        );
        assert_eq!(
            run_header(&RunShown::Ended(&plain)).map(|header| header.status),
            Some("ended")
        );
        assert_eq!(run_header(&RunShown::Gone), None);
    }

    #[test]
    fn shown_is_gone_without_either() {
        assert_eq!(shown(None, None), RunShown::Gone);
    }
}
