//! A pane that follows a dispatch run: a stats line over the run's recent
//! actions, kept scrolled to the end until the user scrolls up, and the last
//! state of a run that has gone. The model is plain Rust, so every rule is
//! unit-tested; `RootView` keeps the state and draws it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use alacritty_terminal::selection::SelectionType;
use gpui::{
    AnyElement, Context, Div, ElementId, HighlightStyle, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollEvent, ScrollHandle, SharedString, Stateful,
    StyledText, UnderlineStyle, Window, WindowId, div, point, prelude::*, px,
};
use protocol::{ClientMessage, DaemonMessage, DispatchRunSnapshot, DispatchRunStatus, PaneFollow};

use crate::chat_select::{self, Selection, TextPos, UnitSource, Units, transcript_keys};
use crate::dashboard_rows::{format_cost, format_tokens};
use crate::dispatch_menu::stop_tip;
use crate::driver_id::DriverId as _;
use crate::ellipsis::{ellipsized, truncating};
use crate::fonts::{DEFAULT_FAMILY, ui_px_at, ui_text_size};
use crate::grid_view::{LinkClick, LinkPick, PaneSlot, TipAt, header_tipped};
use crate::links::{self, LinkKind, TerminalLink};
use crate::mouse::{self, CopyOnSelect};
use crate::open::{self, LinkAction};
use crate::session_menu::{disabled_header_text_button, header_text_button, on_press};
use crate::spawn_view::DISABLED_OPACITY;
use crate::target::HostKey;
use crate::text_select::{SelectDraw, TextRegistry, painted_mark, select_text};
use crate::transcript_view::{LineKind, TranscriptKey, TranscriptLine, transcript_lines};
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
    /// The pane's text selection in the transcript; a pane that follows
    /// another transcript gets a fresh scroll, and no selection with it.
    select: RefCell<TranscriptSelect>,
}

impl RunScroll {
    /// A scroll of `key`'s log, at its end.
    fn of(key: (HostKey, TranscriptKey)) -> Self {
        Self {
            key,
            following: true,
            handle: ScrollHandle::new(),
            select: RefCell::new(TranscriptSelect::default()),
        }
    }
}

/// A followed pane's selection of its transcript's text, as a terminal's:
/// its ends, the word or line a double or triple click picked, the press
/// under way, and the texts the latest frame drew, for the hit test.
#[derive(Clone, Default)]
pub(crate) struct TranscriptSelect {
    selection: Option<Selection>,
    /// The (start, end) a double or triple click picked, which a drag with
    /// the button held widens from.
    picked: Option<(TextPos, TextPos)>,
    press: Option<Press>,
    /// The tool call the last click's release toggled, and its line; the
    /// next press forgets it, undoing the toggle when that press is the
    /// double or triple click the click began.
    toggled: Option<Toggled>,
    /// The link Ctrl+hover underlines, while Ctrl is held over one.
    hover: Option<HoveredLink>,
    registry: Rc<RefCell<TextRegistry>>,
}

/// A link Ctrl+hover underlines in a transcript: the (entry key, index) of
/// its line, its bytes there, its kind, and where its tip goes.
#[derive(Debug, Clone, PartialEq)]
struct HoveredLink {
    line: (String, u32),
    bytes: Range<usize>,
    kind: LinkKind,
    tip_at: Point<Pixels>,
}

/// The bytes a transcript underlines, and the (entry key, index) of their
/// line.
pub(crate) type Underline = ((String, u32), Range<usize>);

/// The link of a transcript line under a point, and what a Ctrl+hover there
/// underlines.
struct LinkHit {
    link: TerminalLink,
    hover: HoveredLink,
}

/// The bytes of `text` its characters `chars` take.
fn char_bytes(text: &str, chars: Range<usize>) -> Range<usize> {
    let byte = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(at, _)| at);
    byte(chars.start)..byte(chars.end)
}

/// The style of a link Ctrl+hover underlines: a thin line in the text's
/// own colour.
fn link_underline() -> HighlightStyle {
    HighlightStyle {
        underline: Some(UnderlineStyle {
            thickness: px(1.0),
            ..UnderlineStyle::default()
        }),
        ..HighlightStyle::default()
    }
}

/// The underline `underline` draws on the line (entry key, index) `line`
/// whose text is `text`: none for another line, nor for bytes that run past
/// the text or do not fall on its character boundaries, as a hover kept
/// from before the line changed (a tool call collapsed under it) may.
fn line_underline(
    underline: Option<&Underline>,
    line: (&str, u32),
    text: &str,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let Some(((entry, index), bytes)) = underline else {
        return Vec::new();
    };
    let here = entry == line.0 && *index == line.1;
    let fits = bytes.start <= bytes.end
        && text.is_char_boundary(bytes.start)
        && text.is_char_boundary(bytes.end);
    if here && fits {
        vec![(bytes.clone(), link_underline())]
    } else {
        Vec::new()
    }
}

/// The folders a run's relative link paths resolve against: its worktree,
/// then its main checkout, blanks left out and each folder once.
fn run_dirs(run: &DispatchRunSnapshot) -> Vec<String> {
    let mut dirs: Vec<String> = Vec::new();
    for dir in std::iter::once(&run.worktree).chain(run.main_checkout.iter()) {
        if !dir.is_empty() && !dirs.contains(dir) {
            dirs.push(dir.clone());
        }
    }
    dirs
}

/// A tool call a click toggled, by its id and the (entry key, index) of
/// the line clicked.
#[derive(Debug, Clone)]
struct Toggled {
    call: String,
    line: (String, u32),
}

impl std::fmt::Debug for TranscriptSelect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptSelect")
            .field("selection", &self.selection)
            .field("picked", &self.picked)
            .field("press", &self.press)
            .field("toggled", &self.toggled)
            .field("hover", &self.hover)
            .finish_non_exhaustive()
    }
}

/// A left press on a transcript: where it went down, and whether its
/// release there is still a click (a single press, no Shift, not moved).
#[derive(Debug, Clone, Copy)]
struct Press {
    at: Point<Pixels>,
    click: bool,
}

/// The whole line `pos` lies on, as its (start, end); `None` when no unit
/// has it.
fn line_around(units: &Units, pos: &TextPos) -> Option<(TextPos, TextPos)> {
    let end = units.clamp(&TextPos {
        byte: usize::MAX,
        ..pos.clone()
    })?;
    let start = TextPos {
        byte: 0,
        ..end.clone()
    };
    Some((start, end))
}

/// The selection a press at `pos` with `click_count` starts in `units`: a
/// character, the word around it, or its whole line, with what it picked.
fn pressed_selection(
    units: &Units,
    pos: TextPos,
    click_count: usize,
) -> (Selection, Option<(TextPos, TextPos)>) {
    let picked = match mouse::selection_type(click_count) {
        SelectionType::Semantic => units.word_around(&pos),
        SelectionType::Lines => line_around(units, &pos),
        _ => None,
    };
    let selection = match &picked {
        Some((start, end)) => Selection {
            anchor: start.clone(),
            focus: end.clone(),
        },
        None => Selection {
            anchor: pos.clone(),
            focus: pos,
        },
    };
    (selection, picked)
}

/// Whether `keystroke` is the copy key a follow pane takes: Ctrl+C (Cmd+C
/// on a Mac), Shift allowed.
pub(crate) fn is_copy_key(keystroke: &gpui::Keystroke) -> bool {
    let mods = keystroke.modifiers;
    keystroke.key.eq_ignore_ascii_case("c") && (mods.control || mods.platform) && !mods.alt
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
pub(crate) fn follow_key(follow: &PaneFollow) -> Option<TranscriptKey> {
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

/// The font, size and line height, in px, a followed pane's transcript
/// column draws at: a terminal pane's in the same tab.
#[derive(Clone)]
pub(crate) struct TranscriptFont {
    pub font: gpui::Font,
    pub size: f32,
    pub line_height: f32,
}

/// The transcript font `font` at `size` draws at, given that font's `ascent`
/// and `descent` at the size: its face, its size, and the line height
/// [`crate::fonts::line_height`] gives those metrics.
#[must_use]
pub(crate) fn transcript_font(
    font: gpui::Font,
    size: f32,
    ascent: f32,
    descent: f32,
) -> TranscriptFont {
    TranscriptFont {
        font,
        size,
        line_height: crate::fonts::line_height(ascent, descent, size),
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
            let button =
                disabled_header_text_button(&stop_id, "Stop", t.muted).opacity(DISABLED_OPACITY);
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

    /// The session followed run `run_id` of `host` was dispatched from, when
    /// the run's snapshot names one.
    pub(crate) fn run_session(&self, host: &HostKey, run_id: &str) -> Option<String> {
        match self.run_shown(host, run_id) {
            RunShown::Live(run) | RunShown::Ended(run) => run.session_id.clone(),
            RunShown::Gone => None,
        }
    }

    /// The session `key`'s transcript resolves its font from: a subagent's
    /// own session, else the dispatching session of its run when the run's
    /// snapshot names one.
    pub(crate) fn transcript_session(&self, host: &HostKey, key: &TranscriptKey) -> Option<String> {
        match key {
            TranscriptKey::Subagent { session_id, .. } => Some(session_id.clone()),
            TranscriptKey::Run(run_id) | TranscriptKey::RunSubagent { run_id, .. } => {
                self.run_session(host, run_id)
            }
        }
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
        font: TranscriptFont,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let inner = match self.run_shown(host, run_id) {
            RunShown::Live(run) => self.run_column(pane_id, host, (run, false), font, cx),
            RunShown::Ended(run) => self.run_column(pane_id, host, (run, true), font, cx),
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

    /// A run of `host` as `shown` with whether it has ended: its stats line,
    /// its log and, once it has gone, its run folder.
    fn run_column(
        &self,
        pane_id: &str,
        host: &HostKey,
        shown: (&DispatchRunSnapshot, bool),
        font: TranscriptFont,
        cx: &mut Context<Self>,
    ) -> Div {
        let t = theme(cx);
        let (run, ended) = shown;
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
            .child(self.run_log(pane_id, host, run, font, cx))
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
        font: TranscriptFont,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let key = (host.clone(), TranscriptKey::Run(run.run_id.clone()));
        let rows: Vec<AnyElement> = match self.transcripts.get(&key) {
            Some(transcript) => {
                let line_ids = format!("dispatch-line-{pane_id}");
                let lines = transcript_lines(transcript);
                let select = self.transcript_select_draw(pane_id, &lines, cx);
                let underline = self.transcript_underline(pane_id);
                Self::transcript_rows(&line_ids, lines, (select.as_ref(), underline), cx)
            }
            None => run
                .recent_actions
                .iter()
                .map(|action| div().child(action.clone()).into_any_element())
                .collect(),
        };
        self.log_column(pane_id, format!("dispatch-log-{pane_id}"), rows, font, cx)
    }

    /// Pane `pane_id`'s log column, tagged `name`, of `rows` in the
    /// terminal's font family and weight at `font`'s size and line height,
    /// scrolled to the end while the user has not scrolled up.
    pub(crate) fn log_column(
        &self,
        pane_id: &str,
        name: String,
        rows: Vec<AnyElement>,
        font: TranscriptFont,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let t = theme(cx);
        let TranscriptFont {
            font: face,
            size,
            line_height,
        } = font;
        let column = div()
            .id(ElementId::Name(SharedString::from(name.clone())))
            .driver_id(|| name)
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(8.0))
            .font(face)
            .text_size(px(size))
            .line_height(px(line_height))
            .text_color(gpui::rgb(t.text_2))
            .children(rows);
        let Some(scroll) = self.run_scrolls.get(pane_id) else {
            return column;
        };
        if scroll.following {
            scroll.handle.scroll_to_bottom();
        }
        let (registry, tip) = {
            let select = scroll.select.borrow();
            let tip = select
                .hover
                .as_ref()
                .map(|hover| crate::term_view::link_tip_at(pane_id, hover.kind, hover.tip_at, cx));
            (select.registry.clone(), tip)
        };
        let column = column.child(painted_mark(registry)).children(tip);
        let column = Self::selecting(column, pane_id, cx);
        let (pane, handle) = (pane_id.to_owned(), scroll.handle.clone());
        column.track_scroll(&scroll.handle).on_scroll(cx.listener(
            move |_, _: &ScrollEvent, _, cx| {
                cx.stop_propagation();
                let (pane, handle) = (pane.clone(), handle.clone());
                // Read once the wheel has moved the log.
                let root = cx.weak_entity();
                cx.defer(move |cx| {
                    let scrolled = -handle.offset().y / px(1.0);
                    let max_scroll = handle.max_offset().y / px(1.0);
                    let read = root.update(cx, |this, cx| {
                        if let Some(scroll) = this.run_scrolls.get_mut(&pane) {
                            scroll.following =
                                crate::chat::follows_bottom(Some((scrolled, max_scroll)));
                        }
                        cx.notify();
                    });
                    if let Err(err) = read {
                        tracing::debug!("a run log scroll's root view has gone: {err:#}");
                    }
                });
            },
        ))
    }

    /// The rows of a transcript's `lines`, each tagged `{line_ids}-{n}`
    /// and drawn through `select`, which registers each line's text and
    /// paints its selected bytes, with `underline`'s bytes underlined on its
    /// line (the link Ctrl+hover is over); a click on a row of a tool call
    /// expands or collapses the call's output ([`Self::transcript_release`]).
    pub(crate) fn transcript_rows(
        line_ids: &str,
        lines: Vec<TranscriptLine>,
        (select, underline): (Option<&SelectDraw>, Option<Underline>),
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = theme(cx);
        let keys = transcript_keys(&lines);
        let mut rows = Vec::new();
        for (at, (line, (entry, index))) in lines.into_iter().zip(keys).enumerate() {
            let colour = match line.kind {
                LineKind::Answer | LineKind::Tool => t.text,
                LineKind::Output | LineKind::Muted => t.muted,
                LineKind::Error => t.danger,
            };
            let name = format!("{line_ids}-{at}");
            let highlights =
                line_underline(underline.as_ref(), (entry.as_str(), index), &line.text);
            let source = UnitSource::TranscriptLine { line: index };
            let text = select_text(select, &entry, source, line.text, highlights);
            let row = div()
                .id(ElementId::Name(SharedString::from(name.clone())))
                .driver_id(|| name)
                .text_color(gpui::rgb(colour))
                .child(text);
            let row = if line.entry.is_some() {
                row.cursor_pointer()
            } else {
                row
            };
            rows.push(row.into_any_element());
        }
        rows
    }

    /// What pane `pane_id` draws its transcript's `lines` with: their
    /// units, its selection on them in the terminal's selection colour,
    /// and its registry, emptied first for this frame's texts. A selection
    /// whose ends no longer name a line is cleared.
    pub(crate) fn transcript_select_draw(
        &self,
        pane_id: &str,
        lines: &[TranscriptLine],
        cx: &Context<Self>,
    ) -> Option<SelectDraw> {
        let scroll = self.run_scrolls.get(pane_id)?;
        let mut select = scroll.select.borrow_mut();
        select.registry.borrow_mut().forget();
        let units = Units::from_transcript(lines);
        let ends = select
            .selection
            .as_ref()
            .and_then(|selection| selection.ordered(&units));
        if ends.is_none() {
            select.selection = None;
            select.picked = None;
        }
        let selected = ends.as_ref().and_then(|ordered| units.resolve(ordered));
        let palette = &theme(cx).terminal;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an alpha is a fraction gpui holds as an f32"
        )]
        let alpha = palette.selection_alpha as f32;
        let tint = gpui::rgb(crate::appearance::packed(palette.selection));
        let background = gpui::Hsla::from(tint).opacity(alpha);
        Some(SelectDraw {
            units: Rc::new(units),
            selected,
            ends,
            background,
            registry: select.registry.clone(),
        })
    }

    /// `column`, pane `pane_id`'s log, taking the presses, drags and
    /// releases that select its transcript's text.
    fn selecting(column: Stateful<Div>, pane_id: &str, cx: &mut Context<Self>) -> Stateful<Div> {
        let (down, up, out) = (pane_id.to_owned(), pane_id.to_owned(), pane_id.to_owned());
        column
            .child(Self::drag_follower(pane_id, cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.transcript_press(&down, event, window, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    this.transcript_release(&up, (event.position, true), cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    this.transcript_release(&out, (event.position, false), cx);
                }),
            )
    }

    /// A mark that, once painted, follows every move in the window: with
    /// the left button held, so a selection of pane `pane_id`'s transcript
    /// keeps following the pointer once it leaves the log; else for the
    /// link Ctrl+hover underlines.
    fn drag_follower(pane_id: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let root = cx.entity().downgrade();
        let pane = pane_id.to_owned();
        gpui::canvas(
            |_, _, _| (),
            move |_, (), window, _| {
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Bubble {
                        return;
                    }
                    let held = event.pressed_button == Some(MouseButton::Left);
                    // Fails only when the root view has gone.
                    root.update(cx, |this, cx| {
                        if held {
                            this.transcript_drag(&pane, event.position, cx);
                        } else {
                            let ctrl = event.modifiers.secondary(cx);
                            this.transcript_hover(&pane, event.position, ctrl, cx);
                        }
                    })
                    .ok();
                });
            },
        )
        .absolute()
        .size_0()
    }

    /// The units of the transcript pane `pane_id` follows, when one is held.
    fn pane_transcript_lines(&self, pane_id: &str) -> Option<Vec<TranscriptLine>> {
        let scroll = self.run_scrolls.get(pane_id)?;
        Some(transcript_lines(self.transcripts.get(&scroll.key)?))
    }

    /// The position in pane `pane_id`'s transcript text at window point
    /// `at`: the line under it inside the log, else the nearest line.
    fn transcript_pos(&self, pane_id: &str, at: Point<Pixels>) -> Option<TextPos> {
        let scroll = self.run_scrolls.get(pane_id)?;
        let viewport = Some(scroll.handle.bounds());
        let select = scroll.select.borrow();
        let (entry, unit, byte) = select.registry.borrow().hit(at, viewport)?;
        Some(TextPos { entry, unit, byte })
    }

    /// A left press on pane `pane_id`'s log: Ctrl (Cmd on a Mac) on a link
    /// opens it as a terminal's Ctrl+click does, the menu on Ctrl+Shift on a
    /// path, and goes no further: no selection, no tool call toggled, and
    /// only the first press of a multi-click acts. Otherwise Shift extends
    /// the selection to the press, and any other press starts one, of
    /// characters, the word or the line by its click count.
    fn transcript_press(
        &mut self,
        pane_id: &str,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.press_on_link(pane_id, event, window, cx) {
            return;
        }
        if let Some((key, call)) = self.toggle_to_undo(pane_id, event) {
            self.toggle_transcript_entry(&key, &call);
        }
        let Some(lines) = self.pane_transcript_lines(pane_id) else {
            return;
        };
        let Some(pos) = self.transcript_pos(pane_id, event.position) else {
            return;
        };
        let Some(scroll) = self.run_scrolls.get_mut(pane_id) else {
            return;
        };
        let select = scroll.select.get_mut();
        let single = event.click_count <= 1;
        let extend = event.modifiers.shift && single && select.selection.is_some();
        select.press = Some(Press {
            at: event.position,
            click: single && !extend,
        });
        if extend {
            if let Some(selection) = select.selection.as_mut() {
                selection.focus = pos;
            }
        } else {
            let units = Units::from_transcript(&lines);
            let (selection, picked) = pressed_selection(&units, pos, event.click_count);
            select.selection = Some(selection);
            select.picked = picked;
        }
        cx.notify();
    }

    /// A left press on pane `pane_id`'s log with Ctrl (Cmd on a Mac) held
    /// on a link: the press goes no further, so the pane's own press handler
    /// cannot take the keyboard back from a link menu it opens; the pane
    /// takes the keyboard here instead, and the first press of a click
    /// opens the link. `false`, doing nothing, for any other press.
    fn press_on_link(
        &mut self,
        pane_id: &str,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !event.modifiers.secondary(cx) {
            return false;
        }
        let Some(hit) = self.transcript_link_at(pane_id, event.position) else {
            return false;
        };
        // The root's own press handler, which drops what a press elsewhere
        // disarms, runs here since the press stops before reaching it.
        self.on_any_mouse_down(event, window, cx);
        cx.stop_propagation();
        if let Some(slot) = self.panes.get(pane_id) {
            let handle = slot.view().read(cx).focus_handle();
            handle.focus(window, cx);
        }
        if event.click_count == 1 {
            self.open_transcript_link(pane_id, hit.link, event, window, cx);
        }
        cx.notify();
        true
    }

    /// Ctrl (Cmd on a Mac) went down in `window` when `on`, else up: each
    /// transcript pane `window` draws underlines the link under its pointer
    /// at once rather than at the next pointer move; going up, every pane
    /// stops.
    pub(crate) fn transcript_link_mode(
        &mut self,
        on: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let here = window.window_handle().window_id();
        let panes: Vec<String> = self
            .run_scrolls
            .keys()
            .filter(|pane| !on || self.transcript_drawn_in(pane, here))
            .cloned()
            .collect();
        let at = window.mouse_position();
        for pane in panes {
            self.transcript_hover(&pane, at, on, cx);
        }
    }

    /// Whether window `here` draws pane `pane_id`: the pop-out showing it,
    /// else the main window while the pane's tab is the one on screen.
    fn transcript_drawn_in(&self, pane_id: &str, here: WindowId) -> bool {
        if let Some(popout) = self.slot_window(pane_id) {
            return popout == here;
        }
        let tab = self.panes.get(pane_id).map(PaneSlot::tab_id);
        here == self.layouts.main() && tab.is_some() && tab == self.tabs.active_id()
    }

    /// The link on the transcript line pane `pane_id` drew under window
    /// point `at`, when the point is on one of the link's own characters.
    fn transcript_link_at(&self, pane_id: &str, at: Point<Pixels>) -> Option<LinkHit> {
        let scroll = self.run_scrolls.get(pane_id)?;
        let lines = self.pane_transcript_lines(pane_id)?;
        let select = scroll.select.borrow();
        let registry = select.registry.borrow();
        let viewport = Some(scroll.handle.bounds());
        let (entry, unit, byte) = registry.hit_inside(at, viewport)?;
        let line = (entry, unit);
        let index = transcript_keys(&lines)
            .iter()
            .position(|key| *key == line)?;
        let text = &lines.get(index)?.text;
        let char_at = text.get(..byte)?.chars().count();
        let link = links::detect_links(text)
            .into_iter()
            .find(|link| link.start_index <= char_at && char_at < link.end_index)?;
        let bytes = char_bytes(text, link.start_index..link.end_index);
        // The tip sits under the line, from the link's first character.
        let layout = &registry.inside(at, viewport)?.layout;
        let left = layout
            .position_for_index(bytes.start)
            .map_or(at.x, |start| start.x);
        let hover = HoveredLink {
            line,
            bytes,
            kind: link.kind,
            tip_at: point(left, layout.bounds().bottom()),
        };
        Some(LinkHit { link, hover })
    }

    /// The pointer moved to window point `at`, with Ctrl (Cmd on a Mac)
    /// held when `ctrl`: pane `pane_id` underlines the link under it and
    /// shows its tip, or stops when it left the link or Ctrl came up.
    fn transcript_hover(
        &mut self,
        pane_id: &str,
        at: Point<Pixels>,
        ctrl: bool,
        cx: &mut Context<Self>,
    ) {
        let shown = self
            .run_scrolls
            .get(pane_id)
            .is_some_and(|scroll| scroll.select.borrow().hover.is_some());
        if !ctrl && !shown {
            return;
        }
        let hover = ctrl
            .then(|| self.transcript_link_at(pane_id, at))
            .flatten()
            .map(|hit| hit.hover);
        let Some(scroll) = self.run_scrolls.get_mut(pane_id) else {
            return;
        };
        let select = scroll.select.get_mut();
        if select.hover != hover {
            select.hover = hover;
            cx.notify();
        }
    }

    /// Whether pane `pane_id` underlines a link in its transcript.
    #[must_use]
    pub fn transcript_link_hovered(&self, pane_id: &str) -> bool {
        self.transcript_underline(pane_id).is_some()
    }

    /// The line and bytes of the link pane `pane_id` underlines, if any.
    pub(crate) fn transcript_underline(&self, pane_id: &str) -> Option<Underline> {
        let scroll = self.run_scrolls.get(pane_id)?;
        let select = scroll.select.borrow();
        let hover = select.hover.as_ref()?;
        Some((hover.line.clone(), hover.bytes.clone()))
    }

    /// Opens `link`, Ctrl+clicked by `event` in pane `pane_id`'s transcript,
    /// as a terminal's Ctrl+click does: the link menu at the click for a
    /// Ctrl+Shift+click on a path, else the link in its default app (VS Code
    /// at a `:line`) or the browser.
    fn open_transcript_link(
        &mut self,
        pane_id: &str,
        link: TerminalLink,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((host, key)) = self.run_scrolls.get(pane_id).map(|s| s.key.clone()) else {
            return;
        };
        let (base_dirs, session) = self.transcript_link_base(&host, &key);
        let pick = if matches!(link.kind, LinkKind::Path) && event.modifiers.shift {
            LinkPick::Menu(event.position)
        } else {
            LinkPick::Open {
                action: LinkAction::DefaultApp,
                at: None,
            }
        };
        let click = LinkClick {
            host: Some(host),
            link,
            base_dirs,
            session,
            run_folders: !matches!(key, TranscriptKey::Subagent { .. }),
            pick,
        };
        self.open_link_click(pane_id, click, window, cx);
    }

    /// The folders a path in `key`'s transcript on `host` resolves against,
    /// and the session they are of: a subagent's session's folders; a run's,
    /// or a run subagent's, worktree then main checkout, with the session
    /// that dispatched the run when its snapshot names one.
    fn transcript_link_base(
        &self,
        host: &HostKey,
        key: &TranscriptKey,
    ) -> (Vec<String>, Option<String>) {
        match key {
            TranscriptKey::Subagent { session_id, .. } => {
                let dirs = self
                    .sidebar
                    .sessions_of(host)
                    .iter()
                    .find(|s| s.id == *session_id)
                    .map(open::base_dirs)
                    .unwrap_or_default();
                (dirs, Some(session_id.clone()))
            }
            TranscriptKey::Run(run_id) | TranscriptKey::RunSubagent { run_id, .. } => {
                match self.run_shown(host, run_id) {
                    RunShown::Live(run) | RunShown::Ended(run) => {
                        (run_dirs(run), run.session_id.clone())
                    }
                    RunShown::Gone => (Vec::new(), None),
                }
            }
        }
    }

    /// Forgets the toggle pane `pane_id`'s last click made, and gives it
    /// back as (transcript, call) when `event` is the double or triple
    /// click that click began, on the same line: that toggle is undone, so
    /// a double or triple click leaves the call as it was.
    fn toggle_to_undo(
        &mut self,
        pane_id: &str,
        event: &MouseDownEvent,
    ) -> Option<((HostKey, TranscriptKey), String)> {
        let under = self.tool_line_at(pane_id, event.position);
        let scroll = self.run_scrolls.get_mut(pane_id)?;
        let toggled = scroll.select.get_mut().toggled.take()?;
        let same = under.is_some_and(|(_, line)| line == toggled.line);
        (event.click_count >= 2 && same).then(|| (scroll.key.clone(), toggled.call))
    }

    /// The held left button moved to window point `at` over pane
    /// `pane_id`'s log: the selection runs to the text there, from the word
    /// or line a double or triple click picked ([`chat_select::widen`]).
    fn transcript_drag(&mut self, pane_id: &str, at: Point<Pixels>, cx: &mut Context<Self>) {
        let pressed = self
            .run_scrolls
            .get(pane_id)
            .and_then(|scroll| scroll.select.borrow().press);
        let Some(press) = pressed.filter(|press| press.at != at) else {
            return;
        };
        let (Some(lines), Some(pos)) = (
            self.pane_transcript_lines(pane_id),
            self.transcript_pos(pane_id, at),
        ) else {
            return;
        };
        let Some(scroll) = self.run_scrolls.get_mut(pane_id) else {
            return;
        };
        let select = scroll.select.get_mut();
        select.press = Some(Press {
            click: false,
            ..press
        });
        let widened = match &select.picked {
            Some(picked) => chat_select::widen(&Units::from_transcript(&lines), picked, pos),
            None => select.selection.clone().map(|selection| Selection {
                focus: pos,
                ..selection
            }),
        };
        if widened.is_some() && widened != select.selection {
            select.selection = widened;
            cx.notify();
        }
    }

    /// The left button came up at window point `at`, over pane `pane_id`'s
    /// log when `inside`, ending the press there: a click (inside, where it
    /// went down) clears the selection and expands or collapses the tool
    /// call under it, which a double click undoes; any other release copies
    /// the selection while the terminal's copy-on-select setting is on.
    fn transcript_release(
        &mut self,
        pane_id: &str,
        (at, inside): (Point<Pixels>, bool),
        cx: &mut Context<Self>,
    ) {
        let Some(scroll) = self.run_scrolls.get_mut(pane_id) else {
            return;
        };
        let select = scroll.select.get_mut();
        let Some(press) = select.press.take() else {
            return;
        };
        if !(inside && press.click && press.at == at) {
            let text = self.transcript_selected_text(pane_id);
            if let Some(text) = mouse::copy_on_select(CopyOnSelect::get(cx), Some(text)) {
                self.copy_to_clipboard(&text, cx);
            }
            return;
        }
        select.selection = None;
        select.picked = None;
        let key = scroll.key.clone();
        if let Some((call, line)) = self.tool_line_at(pane_id, at) {
            self.toggle_transcript_entry(&key, &call);
            if let Some(scroll) = self.run_scrolls.get_mut(pane_id) {
                scroll.select.get_mut().toggled = Some(Toggled { call, line });
            }
        }
        cx.notify();
    }

    /// The tool call whose line pane `pane_id` drew under window point
    /// `at`, with that line's (entry key, index), if any.
    fn tool_line_at(&self, pane_id: &str, at: Point<Pixels>) -> Option<(String, (String, u32))> {
        let scroll = self.run_scrolls.get(pane_id)?;
        let lines = self.pane_transcript_lines(pane_id)?;
        let select = scroll.select.borrow();
        let registry = select.registry.borrow();
        let drawn = registry.inside(at, Some(scroll.handle.bounds()))?;
        let line = (drawn.entry.clone(), drawn.unit);
        let at = transcript_keys(&lines)
            .iter()
            .position(|key| *key == line)?;
        Some((lines.into_iter().nth(at)?.entry?, line))
    }

    /// Ctrl+C in pane `pane_id` while its transcript holds a selection:
    /// copies the selection; whether it did.
    pub(crate) fn copy_transcript_selection(
        &mut self,
        pane_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let text = self.transcript_selected_text(pane_id);
        if text.is_empty() {
            return false;
        }
        self.copy_to_clipboard(&text, cx);
        true
    }

    /// Expands the output of tool call `id` in `key`'s transcript, or
    /// collapses it again.
    fn toggle_transcript_entry(&mut self, key: &(HostKey, TranscriptKey), id: &str) {
        if let Some(transcript) = self.transcripts.get_mut(key) {
            transcript.toggle(id);
        }
    }

    /// The (entry key, unit index, byte) of the transcript text pane
    /// `pane_id` drew at window point `at`, as a press there selects from.
    #[must_use]
    pub fn transcript_hit(
        &self,
        pane_id: &str,
        at: gpui::Point<Pixels>,
    ) -> Option<(String, u32, usize)> {
        self.transcript_pos(pane_id, at)
            .map(|pos| (pos.entry, pos.unit, pos.byte))
    }

    /// The text pane `pane_id`'s transcript selection copies: the selected
    /// part of each line, prefixes included, joined by newlines; empty with
    /// none.
    #[must_use]
    pub fn transcript_selected_text(&self, pane_id: &str) -> String {
        let (Some(scroll), Some(lines)) = (
            self.run_scrolls.get(pane_id),
            self.pane_transcript_lines(pane_id),
        ) else {
            return String::new();
        };
        let units = Units::from_transcript(&lines);
        let select = scroll.select.borrow();
        select
            .selection
            .as_ref()
            .map(|selection| chat_select::selected_text(&units, selection))
            .unwrap_or_default()
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

    /// A terminal font face for these tests: Geist Mono at `weight`.
    fn face(weight: u16) -> gpui::Font {
        crate::term_view::terminal_font(SharedString::from("Geist Mono"), weight)
    }

    #[test]
    fn transcript_font_is_the_face_and_size_at_the_metrics_line_height() {
        let font = transcript_font(face(600), 18.0, 14.5, 4.2);
        assert_eq!(font.font.family.as_ref(), "Geist Mono", "the face given");
        assert_eq!(font.font.weight.0, 600.0, "at the weight given");
        assert_eq!(font.size, 18.0, "the size given");
        assert_eq!(font.line_height, 19.0, "18.7 px of metrics, ceiled");
    }

    #[test]
    fn transcript_font_falls_back_to_its_own_ratio_without_usable_metrics() {
        let font = transcript_font(face(400), 18.0, 0.0, 0.0);
        assert_eq!(font.line_height, 22.0, "18 × 1.2, ceiled");
    }
}
