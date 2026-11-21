//! [`PhoneRoot`], the phone's root view, and its push effect plumbing.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use gpui::{
    App, ClickEvent, Context, Div, Edges, Entity, FontWeight, IntoElement, Pixels, Render,
    Stateful, Subscription, Task, Window, WindowInsets, WindowVisibility, div, prelude::*, px,
};
use protocol::{ClientMessage, PathKind};

use super::browser::FolderEnd;
use super::fetched::{self, Entry, Freshness, HostStat, Index};
use super::home::Offer;
use super::home_view::{STATUS_TEXT_SIZE, TOUCH_TARGET};
use super::hosts_view::phone_button;
use super::key_bar::KeyBar;
use super::open_tap::{self, PendingOpen};
use super::pasteboard::{LoggingPasteboard, PhonePasteboard};
use super::photos::{LoggingPhotos, PhotoPicker};
use super::prompt_sheet::SheetState;
use super::push::{self, Authorization, Effect, PushPlatform, PushState};
use super::quick_replies::{ReplyGroup, ReplySet};
use super::session::{Page, SessionPage};
use super::share::{LoggingShare, ShareSheet};
use super::sheet::{
    CANCEL_LABEL, NO_RESUME_TEXT, OFFLINE_TEXT, PhoneReply, Sheet, SheetStep, SheetTap,
};
use super::sweep::{self, HostNow};
use super::terminal_view::TerminalTap;
use super::{PhoneLayout, content_padding, home, hosts_view};
use crate::buttons::ButtonKind;
use crate::connection::State;
use crate::driver_id::DriverId as _;
use crate::file_fetch::{self, FetchRequest};
use crate::open::LinkAction;
use crate::target::HostKey;
use crate::ui_theme::theme;
use crate::{PhoneFetched, PhoneStatAnswer, RootView};

/// The phone's index of kept copies, in the UI folder.
const FETCHED_FILE: &str = "fetched.json";

/// How long the host may take to answer whether a kept copy changed before
/// the copy opens as possibly out of date.
const KEPT_STAT_TIMEOUT: Duration = Duration::from_secs(5);

/// A stat out to a host about a kept copy, until its answer, its timeout
/// or its host's reset, whichever comes first.
struct KeptStat {
    /// The stat's id, as the host echoes it.
    id: String,
    /// The host asked.
    host: HostKey,
    /// The copy's entry when the stat went out.
    entry: Entry,
    /// The timeout's timer; dropping it cancels the timeout.
    timeout: Task<()>,
}

/// The wall-clock time in milliseconds since the Unix epoch; 0 before it.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The phone's root view: the saved hosts or the first-run form, or while
/// a saved host is open the phone's Home list at the window's width, or the
/// session a row's tap opened, and the Pair dialog over them, inside the
/// safe area. It draws only those of a phone-shape [`RootView`], whose
/// sidebar has no This PC group, so This PC's dormant slot never shows as a
/// connection.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a state of its own, set and read separately"
)]
pub struct PhoneRoot {
    pub(super) root: Entity<RootView>,
    /// The add form's fields, its refusal and whether it shows.
    pub(super) hosts: hosts_view::PhoneHosts,
    /// The phone's push: permission, device token and install keys.
    pub(super) platform: Arc<dyn PushPlatform>,
    /// The session shown in place of Home: its host and id.
    pub(super) shown: Option<(HostKey, String)>,
    /// The page the shown session is on, while a session is shown.
    pub(super) page: Option<SessionPage>,
    /// The composer's open dock on the shown session's page, if any.
    pub(super) composer_dock: super::composer::Dock,
    /// The shown session's prompt sheet: the prompt put away and the drag.
    pub(super) prompt_sheet: SheetState,
    /// The session whose terminal the Terminal page opened, while open.
    pub(super) terminal_open: Option<(HostKey, String)>,
    /// How much of the open terminal the raised keyboard hides, in points.
    pub(super) terminal_hidden: f32,
    /// How far the open terminal is panned to show the rows the keyboard
    /// hides.
    pub(super) terminal_pan: super::terminal_fit::Pan,
    /// The pan the open terminal's cursor row last needed, so its redraw
    /// redraws the page only when that changes.
    pub(super) terminal_need: f32,
    /// What a tap on the open terminal does, as the key bar last drew it.
    pub(super) terminal_tap: TerminalTap,
    /// The open terminal's redraws, watched for its cursor's row.
    pub(super) terminal_watch: Option<gpui::Subscription>,
    /// The viewport size the open terminal was last laid out at.
    pub(super) terminal_viewport: Option<gpui::Size<gpui::Pixels>>,
    /// The Terminal page's key bar, shared with the terminal's typed text
    /// so a sticky Ctrl reaches the soft keyboard's next character.
    pub(super) key_bar: Rc<RefCell<KeyBar>>,
    /// Whether the Terminal page's Quick replies are open.
    pub(super) terminal_quick: bool,
    /// Home's Needs you tray: the card it shows and the swipe in progress.
    pub(super) tray: super::tray::Tray,
    /// The sheet a long press on a Home row opened, while it is open.
    sheet: Option<Sheet>,
    /// Whether Home's sort sheet is open.
    pub(super) sort_menu: bool,
    /// Settings, drawn in place of Home or the hosts screen while open.
    pub(super) settings: Option<super::settings::Settings>,
    /// The Quick replies title or reply editor over the open editor
    /// sub-page, while it is open.
    pub(super) reply_editor: Option<super::quick_replies_view::TextEditor>,
    /// The key editor and the picker over it, while one is open.
    pub(super) key_editor: Option<super::keys_view::KeyEditor>,
    /// The reply set whose Restore defaults asks for confirmation.
    pub(super) restore_confirm: Option<ReplySet>,
    /// What the Keys page's Restore defaults draws.
    pub(super) keys_restore: super::keys_view::KeysRestore,
    /// The session whose composer draft's Clear asks for confirmation.
    pub(super) draft_clear_asking: Option<String>,
    /// The drag reordering a Settings list, while a long press holds one.
    pub(super) reorder_drag: Option<super::reorder::ReorderDrag>,
    /// The boxes the open page's lists recorded in its last frame, which a
    /// long press on the page hit-tests against.
    pub(super) list_boxes: super::reorder::ListBoxes,
    /// The sheet's resumes out, each until the reply its request id gets.
    /// The phone's own record: the desktop's `Duplicates` never sees them.
    resumes: Vec<home::PendingResume>,
    /// The insets drawn around the content in place of the window's, when
    /// set: a window with none of its own (a shot) draws a phone's.
    insets_override: Option<WindowInsets>,
    /// The layout the last frame picked from the window's usable size, which
    /// the views draw their panes by.
    pub(super) layout: PhoneLayout,
    /// The page the shown session returns to when the iPhone turns upright:
    /// recorded when turning it on its side moved the session off it, and
    /// forgotten once the user picks a page or another session shows.
    pub(super) landscape_return: Option<Page>,
    /// Whether the first render has logged the app's ready line.
    logged_ready: bool,
    /// Whether the window is in front, so delivered notifications are swept.
    in_front: bool,
    /// The hosts the last sweep ran over; an unchanged state sweeps again
    /// only on a return to the foreground.
    swept: Option<Vec<HostNow>>,
    /// How many sweeps have started; a sweep whose delivered list returns
    /// after a newer one started does nothing.
    sweeps: u64,
    /// A tap on a notification waiting for its host to list the session it
    /// names, replacing any tap before it.
    pub(super) pending_open: Option<PendingOpen>,
    /// Why the last tap could not open its session, until the next
    /// navigation or tap.
    pub(super) open_note: Option<String>,
    /// Why the last tapped path did not open (`Not on {host}: {path}`), with
    /// the session (host and id) whose page it was tapped on, until a tap
    /// opens something or another page shows.
    path_note: Option<((HostKey, String), String)>,
    /// The copies the phone keeps of fetched files.
    pub(super) fetched: Index,
    /// Where the index is saved; `None` keeps it in memory only.
    fetched_path: Option<PathBuf>,
    /// Why the Storage page's last Clear left copies behind, until the page
    /// is left.
    pub(super) storage_error: Option<String>,
    /// The viewer page, drawn over the page shown while open.
    pub(super) viewer: Option<super::viewer_view::PhoneViewer>,
    /// The platform's share sheet, which the viewer's Share hands its copy.
    share: Arc<dyn ShareSheet>,
    /// The platform's pasteboard, whose image the composer's Paste uploads.
    pub(super) pasteboard: Arc<dyn PhonePasteboard>,
    /// The platform's photo picker, which the composer's Photos opens.
    pub(super) photos: Arc<dyn PhotoPicker>,
    /// The host browser, drawn over the page shown while open, under the
    /// viewer.
    pub(super) browser: Option<super::browser_view::BrowserPage>,
    /// The stats out about kept copies, at most one per host and host path.
    kept_stats: Vec<KeptStat>,
    /// The modified time a changed copy's host answered with, by the copy's
    /// host folder and host path, until its fetch again is saved.
    refetching: HashMap<(String, String), Option<i64>>,
    /// Redraws the phone whenever the root view changes.
    _root_changed: Subscription,
    /// Redials the dropped saved hosts when the window comes back to the
    /// foreground.
    _visibility: Subscription,
}

impl PhoneRoot {
    /// The phone's root over `root`, its push through `platform`. gpui
    /// refreshes the window when its insets change, and `render` reads them
    /// afresh, so the insets need no subscription of their own.
    pub fn new(
        root: Entity<RootView>,
        platform: Arc<dyn PushPlatform>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let root_changed = cx.observe(&root, |this, root, cx| this.on_root_changed(&root, cx));
        if platform.supported() {
            Self::learn_token(&platform, cx);
        }
        let mut taps = platform.taps();
        cx.spawn(async move |this, cx| {
            while let Some(tap) = taps.next().await {
                if this.update(cx, |this, cx| this.tap(tap, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        let visibility = cx.observe_window_visibility(window, |this, visibility, window, cx| {
            this.in_front = visibility == WindowVisibility::Visible;
            if this.in_front {
                this.root.update(cx, |root, cx| {
                    root.returned_to_foreground();
                    root.redial_saved_hosts(window, cx);
                    for host in open_saved_hosts(root) {
                        root.send_to(&host, ClientMessage::ListSessions);
                    }
                });
                this.swept = None;
                this.sweep_delivered(cx);
                this.sweep_fetched(cx);
            }
        });
        let fetched_path = root.read(cx).ui_dir().map(|dir| dir.join(FETCHED_FILE));
        let fetched = fetched_path.as_deref().map(Index::read).unwrap_or_default();
        let mut this = Self {
            hosts: hosts_view::PhoneHosts::new(&root, cx),
            root,
            platform,
            shown: None,
            page: None,
            composer_dock: super::composer::Dock::None,
            prompt_sheet: SheetState::default(),
            terminal_open: None,
            terminal_hidden: 0.0,
            terminal_pan: super::terminal_fit::Pan::default(),
            terminal_need: 0.0,
            terminal_tap: TerminalTap::RaisesKeyboard,
            terminal_watch: None,
            terminal_viewport: None,
            key_bar: Rc::new(RefCell::new(KeyBar::new())),
            terminal_quick: false,
            tray: super::tray::Tray::default(),
            sheet: None,
            sort_menu: false,
            settings: None,
            reply_editor: None,
            key_editor: None,
            restore_confirm: None,
            keys_restore: super::keys_view::KeysRestore::Idle,
            draft_clear_asking: None,
            reorder_drag: None,
            list_boxes: super::reorder::ListBoxes::default(),
            resumes: Vec::new(),
            insets_override: None,
            layout: PhoneLayout::Single,
            landscape_return: None,
            logged_ready: false,
            in_front: window.visibility() == WindowVisibility::Visible,
            swept: None,
            sweeps: 0,
            pending_open: None,
            open_note: None,
            path_note: None,
            fetched,
            fetched_path,
            storage_error: None,
            viewer: None,
            share: Arc::new(LoggingShare),
            pasteboard: Arc::new(LoggingPasteboard),
            photos: Arc::new(LoggingPhotos),
            browser: None,
            kept_stats: Vec::new(),
            refetching: HashMap::new(),
            _root_changed: root_changed,
            _visibility: visibility,
        };
        this.sweep_fetched(cx);
        this
    }

    /// The root view changed: the phone takes what it queued and keeps its
    /// pages in step with it, then redraws.
    fn on_root_changed(&mut self, root: &Entity<RootView>, cx: &mut Context<Self>) {
        self.hosts.root_changed(root.read(cx));
        self.keep_the_add_form(hosts_view::any_host_open(root.read(cx)), cx);
        self.try_open_pending(cx);
        self.forget_gone_session(cx);
        self.keep_draft_clear_confirm(cx);
        self.take_replies(cx);
        self.sync_sheet(cx);
        self.push_changed(cx);
        self.sweep_delivered(cx);
        self.take_kept_answers(cx);
        self.take_browse_answers(cx);
        self.take_folder_fetches(cx);
        self.take_fetched(cx);
        self.take_phone_opens(cx);
        cx.notify();
    }

    /// Records the root's queued saved fetches in the index, opened now, and
    /// writes it once; the last one opens in the viewer page.
    fn take_fetched(&mut self, cx: &mut Context<Self>) {
        let saved = self.root.update(cx, |root, _| root.take_phone_fetched());
        let Some(last) = saved.last().cloned() else {
            return;
        };
        // A copy opening is a tap that opened something.
        self.path_note = None;
        let now = now_ms();
        for saved in saved {
            // A copy fetched again because it changed keeps the modified
            // time its host answered with.
            let modified = self
                .refetching
                .remove(&(saved.host.clone(), saved.host_path.clone()))
                .flatten();
            self.fetched.upsert(Entry {
                host: saved.host,
                host_path: saved.host_path,
                local_path: saved.local_path,
                size: saved.size,
                modified,
                fetched_at: now,
                last_opened: now,
                folder: false,
            });
        }
        self.write_fetched();
        self.open_viewer(last, cx);
    }

    /// Takes the phone's folder fetches that ended: a saved folder is kept
    /// in the index as one folder, opened now, and the index written once;
    /// the open browser's result line says how each ended. A folder opens
    /// nothing.
    fn take_folder_fetches(&mut self, cx: &mut Context<Self>) {
        let ended = self
            .root
            .update(cx, |root, _| root.phone_browse.take_folders());
        if ended.is_empty() {
            return;
        }
        let now = now_ms();
        let mut kept = false;
        for end in ended {
            match end {
                FolderEnd::Saved { host, asked, saved } => {
                    tracing::debug!(path = %saved.local_path, "phone: keeping a fetched folder");
                    let size = saved.size;
                    self.fetched.upsert(Entry {
                        host: saved.host,
                        host_path: saved.host_path,
                        local_path: saved.local_path,
                        size,
                        modified: None,
                        fetched_at: now,
                        last_opened: now,
                        folder: true,
                    });
                    kept = true;
                    self.browser_fetch_ended(&host, &asked, Ok(size));
                }
                FolderEnd::Failed {
                    host,
                    asked,
                    reason,
                } => self.browser_fetch_ended(&host, &asked, Err(reason)),
            }
        }
        if kept {
            self.write_fetched();
        }
        cx.notify();
    }

    /// The open browser on `host` hears how the fetch of `asked` ended: its
    /// size once saved, else why it failed.
    fn browser_fetch_ended(&mut self, host: &HostKey, asked: &str, outcome: Result<u64, String>) {
        let Some(page) = self
            .browser
            .as_mut()
            .filter(|page| page.state.host() == host)
        else {
            return;
        };
        match outcome {
            Ok(size) => page.state.fetch_done(asked, size),
            Err(reason) => page.state.fetch_failed(asked, reason),
        }
    }

    /// Fetches `host`'s folder `host_path` whole by its host path into the
    /// copies folder, where a file from that path would land.
    ///
    /// # Errors
    /// Why the fetch is refused, as one line.
    pub(super) fn fetch_host_folder(
        &mut self,
        host: HostKey,
        host_path: String,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let request = FetchRequest {
            open_when_done: false,
            ..kept_request(host, host_path)
        };
        let started = self
            .root
            .update(cx, |root, cx| root.phone_fetch_folder(request, cx));
        started.map(|id| tracing::debug!(%id, "phone: fetching a host folder"))
    }

    /// Deletes the copies unopened for the retention time and writes the
    /// index when that changed it.
    fn sweep_fetched(&mut self, cx: &App) {
        let root = match self.root.read(cx).remote_files_root() {
            Ok(root) => root.to_path_buf(),
            Err(err) => {
                tracing::warn!(error = %err, "phone: no copies folder; fetched copies not swept");
                return;
            }
        };
        let before = self.fetched.entries.len();
        if let Err(err) = self.fetched.sweep(&root, now_ms()) {
            tracing::warn!(error = %err, "phone: sweeping the fetched copies");
        }
        if self.fetched.entries.len() != before {
            self.write_fetched();
        }
    }

    /// Writes the index to the UI folder; with none it stays in memory.
    pub(super) fn write_fetched(&self) {
        let Some(path) = self.fetched_path.as_deref() else {
            return;
        };
        if let Err(err) = self.fetched.write_atomic(path) {
            tracing::warn!(path = %path.display(), error = %err, "phone: writing the fetched index");
        }
    }

    /// Opens the kept copy of `host`'s `host_path` again. With no copy, the
    /// file is fetched by its host path and opens once saved. With one, a
    /// host that is open and serves host paths is asked whether its file
    /// changed: the answer opens the copy or fetches it again, and no answer
    /// within [`KEPT_STAT_TIMEOUT`] opens the copy as possibly out of date,
    /// as a host that cannot be asked does at once. While a stat about the
    /// same copy is out, nothing happens.
    pub fn open_kept(&mut self, host: HostKey, host_path: String, cx: &mut Context<Self>) {
        let Some(entry) = self.kept_entry(&host, &host_path, cx) else {
            tracing::debug!(?host, %host_path, "phone: no kept copy; fetching the file");
            self.fetch_kept(host, host_path, cx);
            return;
        };
        if self
            .kept_stats
            .iter()
            .any(|stat| stat.host == host && stat.entry.host_path == host_path)
        {
            tracing::debug!(?host, %host_path, "phone: a kept copy's stat is already out");
            return;
        }
        let asks = {
            let root = self.root.read(cx);
            hosts_view::host_open(root, &host) && root.host_serves_paths(&host)
        };
        if !asks {
            tracing::debug!(?host, %host_path, "phone: the host cannot be asked; opening the kept copy");
            self.open_kept_copy(&host, &entry, true, cx);
            return;
        }
        self.send_kept_stat(host, entry, cx);
    }

    /// The latest kept copy of `host`'s `host_path`, if any.
    fn kept_entry(&self, host: &HostKey, host_path: &str, cx: &App) -> Option<Entry> {
        let folder = self
            .root
            .read(cx)
            .conn_of(host)
            .and_then(|conn| file_fetch::host_folder(conn.target()))?;
        // A fetched folder opens nothing, so it is never a file's copy.
        self.fetched
            .entries
            .iter()
            .filter(|entry| !entry.folder && entry.host == folder && entry.host_path == host_path)
            .max_by_key(|entry| entry.fetched_at)
            .cloned()
    }

    /// Asks `host` whether `entry`'s file changed, under a fresh id the
    /// desktop's link stats never see, and starts its timeout.
    fn send_kept_stat(&mut self, host: HostKey, entry: Entry, cx: &mut Context<Self>) {
        let id = crate::new_request_id();
        let request = kept_request(host.clone(), entry.host_path.clone());
        tracing::debug!(%id, ?host, path = %entry.host_path, "phone: asking whether a kept copy changed");
        self.root.update(cx, |root, _| {
            root.phone_stats.register(&host, &id);
            root.send_to(&host, file_fetch::stat_message(id.clone(), &request));
        });
        let timed_out = id.clone();
        let timeout = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(KEPT_STAT_TIMEOUT).await;
            // Fails only when the phone's root is gone.
            this.update(cx, |this, cx| this.kept_stat_timed_out(&timed_out, cx))
                .ok();
        });
        self.kept_stats.push(KeptStat {
            id,
            host,
            entry,
            timeout,
        });
    }

    /// The stat `id` went unanswered for [`KEPT_STAT_TIMEOUT`]: its copy
    /// opens as possibly out of date, and a later answer is dropped.
    fn kept_stat_timed_out(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(at) = self.kept_stats.iter().position(|stat| stat.id == id) else {
            return;
        };
        let stat = self.kept_stats.remove(at);
        // This runs inside the timer's own task, which ends on its own.
        stat.timeout.detach();
        tracing::debug!(%id, "phone: a kept copy's stat went unanswered");
        self.open_kept_copy(&stat.host, &stat.entry, true, cx);
    }

    /// Takes the answers the root view kept for the phone's stats: each one
    /// still waited for decides its copy; any other is dropped.
    fn take_kept_answers(&mut self, cx: &mut Context<Self>) {
        let answers = self.root.update(cx, |root, _| root.phone_stats.drain());
        for (id, answer) in answers {
            let Some(at) = self.kept_stats.iter().position(|stat| stat.id == id) else {
                tracing::debug!(%id, "phone: dropping a stat answer no longer waited for");
                continue;
            };
            // Dropping the stat cancels its timeout.
            let stat = self.kept_stats.remove(at);
            self.on_kept_answer(&stat, answer, cx);
        }
    }

    /// `answer` came for `stat`: a file that has not changed opens its copy,
    /// one that changed is fetched again, and anything else (another kind,
    /// a host that started over) opens the copy as possibly out of date.
    fn on_kept_answer(&mut self, stat: &KeptStat, answer: PhoneStatAnswer, cx: &mut Context<Self>) {
        let PhoneStatAnswer::Stat {
            kind: PathKind::File,
            size,
            modified,
        } = answer
        else {
            tracing::debug!(id = %stat.id, ?answer, "phone: no file answer; opening the kept copy");
            self.open_kept_copy(&stat.host, &stat.entry, true, cx);
            return;
        };
        match fetched::freshness(&stat.entry, Some(HostStat { size, modified })) {
            Freshness::Fresh => self.open_kept_copy(&stat.host, &stat.entry, false, cx),
            Freshness::Changed => {
                tracing::debug!(id = %stat.id, "phone: the kept copy changed; fetching it again");
                let key = (stat.entry.host.clone(), stat.entry.host_path.clone());
                self.refetching.insert(key.clone(), modified);
                if !self.fetch_kept(stat.host.clone(), stat.entry.host_path.clone(), cx) {
                    // The tap still shows something: the old copy, marked.
                    self.refetching.remove(&key);
                    self.open_kept_copy(&stat.host, &stat.entry, true, cx);
                }
            }
            Freshness::Unknown => self.open_kept_copy(&stat.host, &stat.entry, true, cx),
        }
    }

    /// Fetches `host`'s `host_path` by its host path; the saved copy is
    /// recorded and opens as every phone fetch does. Returns whether the
    /// fetch started.
    fn fetch_kept(&mut self, host: HostKey, host_path: String, cx: &mut Context<Self>) -> bool {
        let request = kept_request(host, host_path);
        let started = self.root.update(cx, |root, cx| {
            root.try_start_fetch(request, None, false, cx)
        });
        match started {
            Ok(_) => true,
            Err(refusal) => {
                tracing::info!(
                    reason = refusal.message(),
                    "phone: a kept copy's fetch was refused"
                );
                false
            }
        }
    }

    /// Opens `entry`'s copy from `host` in the viewer page, marked possibly
    /// out of date when `stale`, and records it opened now.
    fn open_kept_copy(
        &mut self,
        host: &HostKey,
        entry: &Entry,
        stale: bool,
        cx: &mut Context<Self>,
    ) {
        let host_label = self.root.read(cx).host_label_of(host);
        self.fetched
            .touch_opened(&entry.host, &entry.local_path, now_ms());
        self.write_fetched();
        self.open_viewer(
            PhoneFetched {
                host: entry.host.clone(),
                host_path: entry.host_path.clone(),
                local_path: entry.local_path.clone(),
                size: entry.size,
                host_label,
            },
            cx,
        );
        if let Some(viewer) = self.viewer.as_mut() {
            viewer.stale = stale;
        }
        cx.notify();
    }

    /// A host that connects while the hosts screen's add form is open would
    /// take the form off screen with that screen: Settings' Hosts page shows
    /// it in its place, with the text typed so far and its focus.
    fn keep_the_add_form(&mut self, open: bool, cx: &mut Context<Self>) {
        if !self.hosts.form_left_by_a_connection(open) {
            return;
        }
        if self.settings.is_none() && self.shown.is_none() {
            self.open_settings_on(super::settings::Page::Hosts, cx);
        }
    }

    /// The same root drawing `insets` around its content in place of the
    /// window's, while `insets` is set.
    #[must_use]
    pub fn with_insets_override(mut self, insets: Option<WindowInsets>) -> Self {
        self.insets_override = insets;
        self
    }

    /// Draws `insets` around the content in place of the window's from now
    /// on, while `insets` is set.
    pub fn set_insets_override(&mut self, insets: Option<WindowInsets>, cx: &mut Context<Self>) {
        self.insets_override = insets;
        cx.notify();
    }

    /// The same root sharing through `share`; it shares through a sheet that
    /// only logs until then.
    #[must_use]
    pub fn with_share(mut self, share: Arc<dyn ShareSheet>) -> Self {
        self.share = share;
        self
    }

    /// Shares through `share` from now on.
    pub fn set_share(&mut self, share: Arc<dyn ShareSheet>) {
        self.share = share;
    }

    /// The same root reading images off `pasteboard`; its Paste pastes only
    /// text until then.
    #[must_use]
    pub fn with_pasteboard(mut self, pasteboard: Arc<dyn PhonePasteboard>) -> Self {
        self.pasteboard = pasteboard;
        self
    }

    /// Reads images off `pasteboard` from now on.
    pub fn set_pasteboard(&mut self, pasteboard: Arc<dyn PhonePasteboard>) {
        self.pasteboard = pasteboard;
    }

    /// The same root picking photos through `photos`; its Photos picks
    /// nothing until then.
    #[must_use]
    pub fn with_photos(mut self, photos: Arc<dyn PhotoPicker>) -> Self {
        self.photos = photos;
        self
    }

    /// Picks photos through `photos` from now on.
    pub fn set_photos(&mut self, photos: Arc<dyn PhotoPicker>) {
        self.photos = photos;
    }

    /// Share on the viewer page: hands the open copy, as it is, to the share
    /// sheet; nothing while the page is closed.
    pub(super) fn share_viewer(&self) {
        if let Some(viewer) = &self.viewer {
            self.share.present(&viewer.path);
        }
    }

    /// The root view the phone reads and drives.
    #[must_use]
    pub fn root(&self) -> &Entity<RootView> {
        &self.root
    }

    /// The layout the last frame picked from the window's usable size.
    #[must_use]
    pub fn layout(&self) -> PhoneLayout {
        self.layout
    }

    /// Why the add form refused the typed host, while it says so.
    #[must_use]
    pub fn host_error(&self) -> Option<&str> {
        self.hosts.error()
    }

    /// Why the phone could not open the session a notification named, while
    /// it says so.
    #[must_use]
    pub fn open_note(&self) -> Option<&str> {
        self.open_note.as_deref()
    }

    /// Why the last path tapped in the session's chat or terminal did not
    /// open, while the session page says so.
    #[must_use]
    pub fn path_note(&self) -> Option<&str> {
        self.path_note
            .as_ref()
            .filter(|(session, _)| self.shown.as_ref() == Some(session))
            .map(|(_, note)| note.as_str())
    }

    /// What the root's tapped paths opened, each acted on only while the
    /// page of the session it was tapped in shows: a folder opens the
    /// browser at it, and a path not on its host leaves that page's line.
    fn take_phone_opens(&mut self, cx: &mut Context<Self>) {
        let opens = self.root.update(cx, |root, _| root.take_phone_opens());
        for open in opens {
            let (crate::PhoneOpen::Folder { host, session, .. }
            | crate::PhoneOpen::Missing { host, session, .. }) = &open;
            let Some(shown) = self
                .shown
                .clone()
                .filter(|(shown_host, id)| shown_host == host && session.as_ref() == Some(id))
            else {
                tracing::debug!(
                    ?session,
                    "phone: a tapped path's session is not shown; dropped"
                );
                continue;
            };
            match open {
                crate::PhoneOpen::Folder { host, path, .. } => {
                    self.path_note = None;
                    self.open_browser(host, path, cx);
                }
                crate::PhoneOpen::Missing { note, .. } => {
                    self.path_note = Some((shown, note));
                    cx.notify();
                }
            }
        }
    }

    /// A tap on a notification for its user info's session: remembered until
    /// that host lists it, replacing any tap before it.
    fn tap(&mut self, tap: push::Tap, cx: &mut Context<Self>) {
        // One token store read per tap, so deciding on the root changes that
        // follow reads the set this keeps and never the store.
        self.hosts.read_tokens(self.root.read(cx));
        let host = open_tap::target_key(&tap.host);
        tracing::debug!(
            ?host,
            session_id = tap.session_id.as_str(),
            "phone: a notification was tapped"
        );
        self.pending_open = Some(PendingOpen {
            host,
            session_id: tap.session_id,
        });
        self.open_note = None;
        self.try_open_pending(cx);
        cx.notify();
    }

    /// Decides the tap waiting for its host's session list: opens the
    /// session, gives up with a line, or leaves it waiting for the next
    /// root change.
    fn try_open_pending(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_open.clone() else {
            return;
        };
        let next = {
            let root = self.root.read(cx);
            open_tap::decide(
                pending.host_name(),
                open_facts(root, self.hosts.not_paired(), &pending),
            )
        };
        match next {
            open_tap::Next::Wait => {}
            open_tap::Next::Open => self.open_session(pending.host, &pending.session_id, cx),
            open_tap::Next::Note(note) => {
                tracing::debug!(
                    note = note.as_str(),
                    "phone: a tap's session could not be opened"
                );
                self.pending_open = None;
                // On one pane the viewer, a session page or Settings hides
                // the line, so the note returns Home; in two panes the list
                // shows it beside them, and only Settings covers it.
                if self.one_pane(cx) {
                    self.close_viewer(cx);
                    self.close_browser(cx);
                    if self.shown.is_some() {
                        self.close_session(cx);
                    }
                }
                self.settings = None;
                self.keep_reply_editor();
                self.keep_key_editor();
                self.keep_restore_confirm();
                self.keep_keys_restore_confirm();
                self.open_note = Some(note);
                cx.notify();
            }
        }
    }

    /// Any navigation the user makes clears the tap's wait and its line.
    pub(super) fn forget_pending_open(&mut self) {
        self.pending_open = None;
        self.open_note = None;
    }

    /// The session shown in place of Home, or beside it in two panes, as its
    /// host and id.
    #[must_use]
    pub fn shown_session(&self) -> Option<(&HostKey, &str)> {
        self.shown.as_ref().map(|(host, id)| (host, id.as_str()))
    }

    /// Whether `host`'s session `session_id` is the one selected beside
    /// Home's list in two panes.
    pub(super) fn is_selected(&self, host: &HostKey, session_id: &str) -> bool {
        self.layout.two_pane()
            && self
                .shown
                .as_ref()
                .is_some_and(|(shown_host, id)| shown_host == host && id == session_id)
    }
    /// A tap on the composer's Quick button: the Quick dock opens over it,
    /// or the open one closes.
    pub fn tap_composer_quick(&mut self, cx: &mut Context<Self>) {
        self.toggle_dock(super::composer::DockButton::Quick, cx);
    }

    /// A tap on the composer's Keys button: the Keys dock opens over it, or
    /// the open one closes. Opening it disarms Ctrl, so a dock never opens
    /// with a Ctrl armed by an earlier one.
    pub fn tap_composer_keys(&mut self, cx: &mut Context<Self>) {
        self.toggle_dock(super::composer::DockButton::Keys, cx);
        if self.composer_dock == super::composer::Dock::Keys {
            *self.key_bar.borrow_mut() = KeyBar::new();
        }
    }

    /// The shown session's Quick dock replies, group by group: an agent's
    /// groups for a session with an agent, the shell pair otherwise.
    #[must_use]
    pub fn quick_reply_items(&self, cx: &App) -> Vec<String> {
        let set = ReplySet::of(self.shown_has_agent(cx));
        let saved = self.saved_replies(set, cx);
        super::quick_replies::groups(saved.as_deref(), set)
            .into_iter()
            .flat_map(|group| group.replies)
            .collect()
    }

    /// The saved groups of `set`, or `None` while the built-in set shows.
    pub(super) fn saved_replies(&self, set: ReplySet, cx: &App) -> Option<Vec<ReplyGroup>> {
        let state = self.root.read(cx).sidebar.ui_state();
        match set {
            ReplySet::Agent => state.phone_agent_replies.clone(),
            ReplySet::Shell => state.phone_shell_replies.clone(),
        }
    }

    /// Whether the shown session has an agent.
    pub(super) fn shown_has_agent(&self, cx: &App) -> bool {
        let Some((host, session_id)) = self.shown.as_ref() else {
            return false;
        };
        self.root
            .read(cx)
            .sidebar
            .session_on(host, session_id)
            .is_some_and(|session| session.agent.is_some())
    }

    /// A tap on one of the composer's dock buttons: its dock opens, or the
    /// open one closes.
    fn toggle_dock(&mut self, button: super::composer::DockButton, cx: &mut Context<Self>) {
        self.composer_dock = self.composer_dock.tap(button);
        cx.notify();
    }

    /// How many session rows Home lists now.
    #[must_use]
    pub fn home_row_count(&self, cx: &App) -> usize {
        self.home(cx).rows().count()
    }

    /// The headings of Home's groups now, in list order.
    #[must_use]
    pub fn home_group_names(&self, cx: &App) -> Vec<String> {
        self.home(cx)
            .hosts
            .into_iter()
            .flat_map(|host| host.groups)
            .map(|group| group.name)
            .collect()
    }

    /// Shows `host`'s session `session_id` in place of Home, on its chat
    /// when the chat is mirrored, else on its terminal, and loads its chat
    /// while it can show. It opens no tab and lays out no pane, and closes
    /// the viewer, the browser and Settings.
    pub(crate) fn open_session(&mut self, host: HostKey, session_id: &str, cx: &mut Context<Self>) {
        tracing::debug!(?host, session_id, "phone: showing a session");
        self.forget_pending_open();
        self.path_note = None;
        self.close_viewer(cx);
        self.close_browser(cx);
        self.settings = None;
        self.keep_reply_editor();
        self.keep_key_editor();
        self.keep_restore_confirm();
        self.keep_keys_restore_confirm();
        self.draft_clear_asking = None;
        self.landscape_return = None;
        self.page = Some(SessionPage::opened(
            self.chat_mirrored(&host, session_id, cx),
        ));
        self.composer_dock = super::composer::Dock::None;
        let shown = (host, session_id.to_owned());
        self.shown = Some(shown.clone());
        self.prompt_sheet = SheetState::default();
        self.sheet = None;
        self.sort_menu = false;
        // The page's context label reads the host's Dashboard facts.
        self.root.update(cx, |root, cx| {
            root.show_phone_chat(Some(shown), cx);
            root.sync_dashboard_watch();
        });
        cx.notify();
    }

    /// Opens the sheet of `host`'s session `session_id` over Home; nothing
    /// while Settings shows, or a session page in Home's place. It sends
    /// nothing.
    pub(super) fn open_sheet(&mut self, host: HostKey, session_id: &str, cx: &mut Context<Self>) {
        if (self.shown.is_some() && self.one_pane(cx)) || self.settings.is_some() {
            return;
        }
        let Some(offer) = self.offer_of(&host, session_id, cx) else {
            return;
        };
        self.forget_pending_open();
        tracing::debug!(?host, session_id, "phone: opening a session's sheet");
        let mut sheet = Sheet::new(host, session_id, offer);
        // A resume already out for the session keeps its row inert.
        if self
            .resumes
            .iter()
            .any(|pending| sheet.is_for(&pending.host, &pending.original))
        {
            sheet.resume_sent();
        }
        self.sheet = Some(sheet);
        cx.notify();
    }

    /// What `host`'s session `session_id`'s sheet offers now: nothing while
    /// its host is closed; `None` once the host no longer lists it.
    fn offer_of(&self, host: &HostKey, session_id: &str, cx: &App) -> Option<Offer> {
        let root = self.root.read(cx);
        let session = root.sidebar.session_on(host, session_id)?;
        Some(home::offer_on(session, hosts_view::host_open(root, host)))
    }

    /// Opens the sheet of the session Home lists as `session_id`, as a long
    /// press on its row does; returns whether Home lists it.
    pub fn open_sheet_on(&mut self, session_id: &str, cx: &mut Context<Self>) -> bool {
        let host = self
            .home(cx)
            .rows()
            .find(|row| row.session_id == session_id)
            .map(|row| row.host.clone());
        let Some(host) = host else {
            return false;
        };
        self.open_sheet(host, session_id, cx);
        true
    }

    /// The id of the session whose sheet is open.
    #[must_use]
    pub fn sheet_session(&self) -> Option<&str> {
        self.sheet.as_ref().map(|sheet| sheet.session_id.as_str())
    }

    /// What the open sheet's Resume row reads, while it has one.
    #[must_use]
    pub fn sheet_resume_label(&self) -> Option<&'static str> {
        self.sheet.as_ref().map(Sheet::resume_label)
    }

    /// Why the daemon refused the open sheet's last resume.
    #[must_use]
    pub fn sheet_refusal(&self) -> Option<&str> {
        self.sheet.as_ref().and_then(Sheet::refusal)
    }

    /// Closes the sheet; a resume out still gets its follow-up.
    pub(super) fn close_sheet(&mut self, cx: &mut Context<Self>) {
        self.sheet = None;
        cx.notify();
    }

    /// A tap in the sheet: Stop's second tap sends a bare stop and closes
    /// it; Resume sends the resume and keeps it open for the reply.
    pub(super) fn sheet_tap(&mut self, tap: SheetTap, cx: &mut Context<Self>) {
        let Some(sheet) = self.sheet.as_mut() else {
            return;
        };
        match sheet.tap(tap) {
            SheetStep::Nothing => {}
            SheetStep::SendStop => {
                let (host, session_id) = (sheet.host.clone(), sheet.session_id.clone());
                self.sheet = None;
                self.send(&host, home::stop(&session_id), cx);
            }
            SheetStep::SendResume => self.send_resume(cx),
        }
        cx.notify();
    }

    /// Sends the open sheet's session its resume, keeping the record its
    /// reply is matched against; a `ResumeAbandoned`, which no reply names,
    /// closes the sheet.
    fn send_resume(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = self.sheet.as_mut() else {
            return;
        };
        // One resume per session at a time, whatever sheet sent it.
        if self
            .resumes
            .iter()
            .any(|pending| sheet.is_for(&pending.host, &pending.original))
        {
            sheet.resume_sent();
            return;
        }
        let root = self.root.read(cx);
        if !hosts_view::host_open(root, &sheet.host) {
            return;
        }
        let Some(session) = root.sidebar.session_on(&sheet.host, &sheet.session_id) else {
            return;
        };
        let resume_session = root
            .conn_of(&sheet.host)
            .is_some_and(|conn| conn.supports(protocol::FEATURE_RESUME_SESSION));
        let (msg, pending) = home::resume(
            session,
            &sheet.host,
            crate::new_request_id(),
            resume_session,
        );
        let host = sheet.host.clone();
        let Some(pending) = pending else {
            self.sheet = None;
            self.send(&host, msg, cx);
            return;
        };
        sheet.resume_sent();
        self.root.update(cx, |root, _| {
            root.phone_replies.register(&host, &pending.request_id);
            root.send_to(&host, msg);
        });
        self.resumes.push(pending);
    }

    /// Takes the replies to the sheet's requests: a new session's discards
    /// the original it reran and closes that session's sheet; a refusal
    /// shows in the sheet, or on nothing once it is closed. A resume whose
    /// host closed is dropped with its id, as is one whose id its host
    /// forgot (it reconnected).
    fn take_replies(&mut self, cx: &mut Context<Self>) {
        if self.resumes.is_empty() {
            return;
        }
        let replies = self.root.update(cx, |root, _| root.phone_replies.drain());
        for (request_id, reply) in replies {
            self.on_reply(&request_id, reply, cx);
        }
        let closed: Vec<HostKey> = {
            let root = self.root.read(cx);
            self.resumes
                .iter()
                .map(|pending| &pending.host)
                .filter(|host| !hosts_view::host_open(root, host))
                .cloned()
                .collect()
        };
        if !closed.is_empty() {
            tracing::debug!(?closed, "phone: dropping the resumes out to closed hosts");
            self.root.update(cx, |root, _| {
                for host in &closed {
                    root.phone_replies.forget_host(host);
                }
            });
        }
        let root = self.root.read(cx);
        self.resumes
            .retain(|pending| root.phone_replies.is_out(&pending.request_id));
        if let Some(sheet) = self.sheet.as_mut()
            && sheet.resuming()
            && !self
                .resumes
                .iter()
                .any(|pending| sheet.is_for(&pending.host, &pending.original))
        {
            sheet.resume_dropped();
        }
    }

    /// `reply` came for the resume out under `request_id`.
    fn on_reply(&mut self, request_id: &str, reply: PhoneReply, cx: &mut Context<Self>) {
        let Some(at) = self
            .resumes
            .iter()
            .position(|pending| pending.request_id == request_id)
        else {
            return;
        };
        let pending = self.resumes.remove(at);
        let sheet = self
            .sheet
            .as_mut()
            .filter(|sheet| sheet.is_for(&pending.host, &pending.original));
        match reply {
            PhoneReply::Created => {
                if sheet.is_some() {
                    self.sheet = None;
                }
                if let Some(discard) = pending.follow_up() {
                    self.send(&pending.host, discard, cx);
                }
            }
            PhoneReply::Refused(message) => {
                if let Some(sheet) = sheet {
                    sheet.refused(message);
                } else {
                    tracing::debug!(
                        session_id = %pending.original,
                        %message,
                        "phone: a refused resume whose sheet is closed"
                    );
                }
            }
        }
    }

    /// The sheet's session left Home's list: the sheet closes. Else the
    /// sheet takes what the session offers now, so an armed Stop goes when
    /// the session stops on its own or its host closes.
    fn sync_sheet(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = self.sheet.as_ref() else {
            return;
        };
        let listed = self
            .home(cx)
            .label_of(&sheet.host, &sheet.session_id)
            .is_some();
        let offer = self.offer_of(&sheet.host, &sheet.session_id, cx);
        let Some(offer) = offer.filter(|_| listed) else {
            tracing::debug!("phone: the sheet's session is gone; closing it");
            self.sheet = None;
            return;
        };
        if let Some(sheet) = self.sheet.as_mut() {
            sheet.see(offer);
        }
    }

    /// Back from the shown session to Home; its chat unloads.
    pub(super) fn close_session(&mut self, cx: &mut Context<Self>) {
        self.forget_pending_open();
        self.path_note = None;
        self.shown = None;
        self.page = None;
        self.landscape_return = None;
        self.draft_clear_asking = None;
        self.composer_dock = super::composer::Dock::None;
        self.prompt_sheet = SheetState::default();
        self.root.update(cx, |root, cx| {
            root.show_phone_chat(None, cx);
            root.sync_dashboard_watch();
        });
        cx.notify();
    }

    /// What Home lists now: every host's groups in the order the owner
    /// picked, Manual unless another is saved.
    pub(super) fn home(&self, cx: &App) -> home::Home {
        let root = self.root.read(cx);
        super::order::apply(
            home::from_sidebar(&root.sidebar),
            root.sidebar.ui_state().phone_order,
            &root.phone_layouts,
        )
    }

    /// The shown session left its host's list: Home shows again.
    fn forget_gone_session(&mut self, cx: &mut Context<Self>) {
        let root = self.root.read(cx);
        let gone = self
            .shown
            .as_ref()
            .is_some_and(|(host, id)| root.sidebar.session_on(host, id).is_none());
        if gone {
            tracing::debug!("phone: the shown session is gone; back to Home");
            self.shown = None;
            self.path_note = None;
            self.page = None;
            self.landscape_return = None;
            self.composer_dock = super::composer::Dock::None;
            self.prompt_sheet = SheetState::default();
            self.root.update(cx, |root, cx| {
                root.show_phone_chat(None, cx);
                root.sync_dashboard_watch();
            });
        }
    }

    /// While the window is in front and the saved hosts' Needs you sets or
    /// loaded state changed since the last sweep, removes the delivered
    /// notifications whose wait ended and sets the badge ([`sweep::sweep`]).
    /// Nothing is asked before any host has loaded.
    fn sweep_delivered(&mut self, cx: &mut Context<Self>) {
        if !self.in_front || !self.platform.supported() {
            return;
        }
        let hosts = sweep_hosts(self.root.read(cx));
        if self.swept.as_ref() == Some(&hosts) {
            return;
        }
        self.swept = Some(hosts.clone());
        if !hosts.iter().any(|host| host.loaded) {
            return;
        }
        self.sweeps += 1;
        let started = self.sweeps;
        let delivered = self.platform.delivered();
        let platform = Arc::clone(&self.platform);
        cx.spawn(async move |this, cx| {
            let delivered = delivered.await;
            // A root gone, or a newer sweep started, leaves this one's answer unused.
            let newest = this
                .read_with(cx, |this, _| this.sweeps == started)
                .unwrap_or(false);
            if !newest {
                tracing::debug!("phone: a newer sweep replaces this one");
                return;
            }
            let result = sweep::sweep(&delivered, &hosts);
            if !result.remove.is_empty() {
                tracing::debug!(
                    count = result.remove.len(),
                    "phone: removing handled notifications"
                );
                platform.remove_delivered(result.remove);
            }
            if let Some(badge) = result.badge {
                platform.set_badge(badge);
            }
        })
        .detach();
    }

    /// With the permission already granted, reads this phone's device token
    /// so each host's push state compares against it.
    fn learn_token(platform: &Arc<dyn PushPlatform>, cx: &mut Context<Self>) {
        let platform = Arc::clone(platform);
        cx.spawn(async move |this, cx| {
            if platform.authorization().await != Authorization::Granted {
                return;
            }
            match platform.device_token().await {
                Ok(token) => {
                    // Fails only when the phone's root is gone.
                    this.update(cx, |this, cx| this.token_read(&token, cx)).ok();
                }
                Err(err) => tracing::warn!(error = %err, "phone: no device token"),
            }
        })
        .detach();
    }

    /// This phone's device token is `token`: a host on for another token
    /// re-registers.
    fn token_read(&mut self, token: &str, cx: &mut Context<Self>) {
        self.root.update(cx, |root, cx| {
            root.push_hosts_mut().set_current(token);
            cx.notify();
        });
    }

    /// The root view changed: the queued effects run, then the saved
    /// tailnet hosts open and taking relay pushes, with their connection
    /// generations, go to the switches' model, and the effects it makes run.
    fn push_changed(&mut self, cx: &mut Context<Self>) {
        if !self.platform.supported() {
            return;
        }
        let open = hosts_view::relay_push_hosts(self.root.read(cx));
        let effects = self.root.update(cx, |root, _| {
            let push = root.push_hosts_mut();
            let mut effects = push.take_effects();
            effects.extend(push.hosts_open(&open));
            effects
        });
        self.run_effects(effects, cx);
    }

    /// Runs `step` on `host`'s switch, then the effects it and earlier steps
    /// queued, redrawing the switch.
    fn step_push(
        &mut self,
        host: &HostKey,
        step: impl FnOnce(&mut PushState, Option<&str>) -> Vec<Effect>,
        cx: &mut Context<Self>,
    ) {
        let effects = self.root.update(cx, |root, cx| {
            let push = root.push_hosts_mut();
            push.step(host, step);
            cx.notify();
            push.take_effects()
        });
        self.run_effects(effects, cx);
    }

    /// Runs each of `effects` for its host.
    fn run_effects(&mut self, effects: Vec<(HostKey, Effect)>, cx: &mut Context<Self>) {
        for (host, effect) in effects {
            match effect {
                Effect::SendGetStatus { request_id } => {
                    self.send(&host, ClientMessage::GetPushStatus { request_id }, cx);
                }
                Effect::Unregister { request_id } => {
                    self.send(&host, ClientMessage::UnregisterPush { request_id }, cx);
                }
                Effect::RequestAuthorization => self.request_authorization(host, cx),
                Effect::Register { request_id } => self.register(host, request_id, cx),
            }
        }
    }

    /// Sends `msg` to `host`'s daemon.
    fn send(&self, host: &HostKey, msg: ClientMessage, cx: &mut Context<Self>) {
        self.root.read(cx).send_to(host, msg);
    }

    /// Asks the owner for the notification permission, then folds the
    /// answer into `host`'s switch.
    fn request_authorization(&self, host: HostKey, cx: &mut Context<Self>) {
        let asked = self.platform.request_authorization();
        cx.spawn(async move |this, cx| {
            let answer = asked.await;
            // Fails only when the phone's root is gone.
            this.update(cx, |this, cx| {
                this.step_push(&host, |state, _| state.authorization_answered(answer), cx);
            })
            .ok();
        })
        .detach();
    }

    /// Reads the device token, then registers this phone with `host` for
    /// all three kinds of push.
    fn register(&self, host: HostKey, request_id: String, cx: &mut Context<Self>) {
        let token = self.platform.device_token();
        cx.spawn(async move |this, cx| {
            let token = token.await;
            // Fails only when the phone's root is gone.
            this.update(cx, |this, cx| {
                this.send_registration(&host, request_id, token, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Sends `host` the registration for `token`, or, when the platform gave
    /// no token or keys, logs why and marks the switch failed; either only
    /// while `request_id` is still the switch's registration out, else the
    /// completion is dropped.
    fn send_registration(
        &mut self,
        host: &HostKey,
        request_id: String,
        token: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        if !self
            .root
            .read(cx)
            .push_hosts()
            .token_ready(host, &request_id)
        {
            tracing::debug!(
                ?host,
                %request_id,
                "phone: dropping a push registration no longer out"
            );
            return;
        }
        let registration = token.and_then(|token| {
            push::apns_registration(self.platform.as_ref(), token.clone()).map(|reg| (token, reg))
        });
        match registration {
            Ok((token, registration)) => self.root.update(cx, |root, cx| {
                root.push_hosts_mut().set_current(&token);
                root.send_to(
                    host,
                    ClientMessage::RegisterPush {
                        registration,
                        needs_you: true,
                        finished: true,
                        hold_at_desk: true,
                        request_id,
                    },
                );
                cx.notify();
            }),
            Err(err) => {
                tracing::warn!(error = %err, ?host, "phone: push registration failed");
                self.root.update(cx, |root, cx| {
                    if root.push_hosts_mut().failed(host, &request_id) {
                        cx.notify();
                    }
                });
            }
        }
    }

    /// `host`'s Notifications switch tapped: while a request is out nothing
    /// happens; on turns it off; anything else reads the permission and
    /// turns it on.
    pub(super) fn toggle_push(&mut self, host: &HostKey, cx: &mut Context<Self>) {
        let Some((busy, on)) = self
            .root
            .read(cx)
            .push_hosts()
            .state(host)
            .map(|state| (state.busy(), state.switched_on()))
        else {
            return;
        };
        if busy {
            return;
        }
        if on {
            self.step_push(host, |state, _| state.switch_off(), cx);
            return;
        }
        let asked = self.platform.authorization();
        let host = host.clone();
        cx.spawn(async move |this, cx| {
            let auth = asked.await;
            // Fails only when the phone's root is gone.
            this.update(cx, |this, cx| {
                this.step_push(&host, |state, _| state.switch_on(auth), cx);
            })
            .ok();
        })
        .detach();
    }
}

/// The fetch of `host`'s file at `host_path` by its host path, opening in
/// the viewer once saved.
fn kept_request(host: HostKey, host_path: String) -> FetchRequest {
    FetchRequest {
        host,
        repo_id: String::new(),
        repo_name: String::new(),
        worktree_path: None,
        rel_path: host_path,
        host_path: true,
        line: None,
        column: None,
        open_when_done: true,
        action: LinkAction::Viewer,
    }
}

/// The saved hosts whose connection is open, in saved order.
fn open_saved_hosts(root: &RootView) -> Vec<HostKey> {
    root.sidebar
        .ui_state()
        .saved_hosts
        .iter()
        .filter_map(|saved| Some(HostKey::of(&saved.target().ok()?)))
        .filter(|key| hosts_view::host_open(root, key))
        .collect()
}

/// Every saved host the window can name on the wire, as the sweep sees it:
/// loaded when its connection is open and has sent its session list both
/// since its `Welcome` and since the app last came to the foreground, with
/// the ids of its sessions waiting on the user.
fn sweep_hosts(root: &RootView) -> Vec<HostNow> {
    let rows = crate::needs_you::rows(&root.sidebar);
    root.sidebar
        .ui_state()
        .saved_hosts
        .iter()
        .filter_map(|saved| {
            let key = HostKey::of(&saved.target().ok()?);
            let host = key.wire()?.to_owned();
            let loaded = hosts_view::host_open(root, &key)
                && root.listed_since_welcome(&key)
                && root.listed_since_foreground(&key);
            let needs_you = sweep::needs_you_ids(&rows, &key);
            Some(HostNow {
                host,
                loaded,
                needs_you,
            })
        })
        .collect()
}

/// What the phone knows about `pending`'s host: whether a saved host has
/// its key, whether that host is paired with this phone (from the not-paired
/// set `PhoneHosts` keeps, so no token store is read here), whether it is
/// loaded, and whether the loaded list holds the session.
fn open_facts(
    root: &RootView,
    not_paired: &HashSet<String>,
    pending: &PendingOpen,
) -> open_tap::Facts {
    let saved_hosts = &root.sidebar.ui_state().saved_hosts;
    let saved = saved_hosts.iter().find(|saved| {
        saved
            .target()
            .is_ok_and(|target| HostKey::of(&target) == pending.host)
    });
    let refused = root
        .conn_of(&pending.host)
        .is_some_and(|conn| matches!(conn.state(), State::NotApproved { .. }));
    open_tap::Facts {
        saved: saved.is_some(),
        paired: saved.is_some_and(|saved| !refused && !not_paired.contains(&saved.name)),
        loaded: hosts_view::host_open(root, &pending.host)
            && root.listed_since_welcome(&pending.host)
            && root.listed_since_foreground(&pending.host),
        listed: root
            .sidebar
            .session_on(&pending.host, &pending.session_id)
            .is_some(),
    }
}

/// One line of the sheet's `text`, tagged `id`, in `color`.
fn sheet_line(id: &'static str, text: String, color: u32, cx: &App) -> Div {
    div()
        .driver_id(move || id.to_owned())
        .px(px(4.0))
        .text_size(crate::fonts::ui_px(cx, STATUS_TEXT_SIZE))
        .text_color(gpui::rgb(color))
        .child(text)
}

impl PhoneRoot {
    /// The open sheet over a scrim: the session's label, the rows its
    /// state offers, a refusal, then Cancel, its bottom clear of `bottom`'s
    /// home indicator. `None` while no sheet is open or its session is gone.
    fn sheet_layer(&self, bottom: Pixels, cx: &mut Context<Self>) -> Option<Div> {
        let sheet = self.sheet.as_ref()?;
        let offer = self.offer_of(&sheet.host, &sheet.session_id, cx)?;
        let title = self
            .home(cx)
            .label_of(&sheet.host, &sheet.session_id)
            .map_or_else(|| sheet.session_id.clone(), str::to_owned);
        let t = theme(cx);
        let note = match offer {
            Offer::Nothing => Some(("phone-sheet-note", NO_RESUME_TEXT)),
            Offer::Offline => Some(("phone-sheet-offline", OFFLINE_TEXT)),
            Offer::Stop | Offer::Resume => None,
        }
        .map(|(id, text)| sheet_line(id, text.to_owned(), t.muted, cx));
        let refusal = sheet
            .refusal()
            .map(|text| sheet_line("phone-sheet-error", text.to_owned(), t.danger_text, cx));
        let scrim = div()
            .id("phone-sheet-scrim")
            .driver_id(|| "phone-sheet-scrim".to_owned())
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(gpui::rgba(t.scrim))
            .occlude()
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.close_sheet(cx)));
        let panel = div()
            .id("phone-sheet")
            .driver_id(|| "phone-sheet".to_owned())
            .occlude()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(16.0))
            .pt(px(16.0))
            .pb(bottom + px(16.0))
            .rounded_t(px(14.0))
            .border_t_1()
            .border_color(gpui::rgb(t.line))
            .bg(gpui::rgb(t.raised))
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.sheet_tap(SheetTap::Elsewhere, cx);
            }))
            .child(
                div()
                    .driver_id(|| "phone-sheet-title".to_owned())
                    .flex()
                    .items_center()
                    .min_h(px(TOUCH_TARGET))
                    .px(px(4.0))
                    .truncate()
                    .font_weight(crate::fonts::ui_weight(cx, FontWeight::SEMIBOLD))
                    .child(title),
            )
            .children(Self::sheet_action(sheet, offer, cx))
            .children(note)
            .children(refusal)
            .child(
                Self::sheet_button("phone-sheet-cancel", ButtonKind::Outlined, cx)
                    .child(CANCEL_LABEL)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.close_sheet(cx);
                    })),
            );
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .justify_end()
                .child(scrim)
                .child(panel),
        )
    }

    /// The row `offer` gives: Stop in the danger colour, or Resume.
    fn sheet_action(sheet: &Sheet, offer: Offer, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let (id, kind, label, tap) = match offer {
            Offer::Stop => (
                "phone-sheet-stop",
                ButtonKind::Danger,
                sheet.stop_label(),
                SheetTap::Stop,
            ),
            Offer::Resume => (
                "phone-sheet-resume",
                ButtonKind::Outlined,
                sheet.resume_label(),
                SheetTap::Resume,
            ),
            Offer::Nothing | Offer::Offline => return None,
        };
        Some(
            Self::sheet_button(id, kind, cx)
                .child(label)
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.sheet_tap(tap, cx);
                })),
        )
    }

    /// A full-width sheet row of `kind`, at least a touch target tall.
    pub(super) fn sheet_button(id: &str, kind: ButtonKind, cx: &Context<Self>) -> Stateful<Div> {
        phone_button(id, kind, cx).w_full().min_h(px(TOUCH_TARGET))
    }
}

/// How far below the safe area's top the copied chip sits: under the
/// session header.
const CHIP_TOP: f32 = 60.0;

impl PhoneRoot {
    /// The "copied" chip, centred across the screen `top` (the safe area's
    /// top inset) plus [`CHIP_TOP`] down, while a copy is recent; it takes
    /// no input.
    fn copied_chip_layer(&self, top: Pixels, cx: &App) -> Option<Div> {
        let t = crate::ui_theme::theme(cx);
        let offset = crate::fonts::ui_text_size(cx);
        let chip = self.root.read(cx).copied_chip_element(offset, &t)?;
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(top + px(CHIP_TOP))
                .flex()
                .justify_center()
                .child(chip),
        )
    }
}

/// The list pane's width, in points, in two panes.
pub(super) const LIST_PANE_WIDTH: f32 = 320.0;

/// The width, in points, of the line between the two panes.
const PANE_LINE_WIDTH: f32 = 1.0;

/// What the session pane says while no session is selected.
const PICK_SESSION_TEXT: &str = "Pick a session";

/// A pane of the two, which a sheet over it covers alone.
#[derive(Clone, Copy)]
enum Pane {
    /// Home's list, on the left.
    List,
    /// The selected session's page, on the right.
    Session,
}

impl PhoneRoot {
    /// Whether the window draws Home's list beside the session pane: a
    /// two-pane layout with no full-screen terminal, Settings closed and
    /// [`Self::panes_shown`].
    fn split(&self, host_open: bool, cx: &App) -> bool {
        !self.one_pane(cx) && self.settings.is_none() && self.panes_shown(host_open)
    }

    /// Whether the shown session's terminal fills the screen: the iPhone on
    /// its side, with the session on its Terminal page.
    pub(super) fn terminal_full(&self, cx: &App) -> bool {
        self.layout == PhoneLayout::TerminalFull
            && self
                .session_header(cx)
                .is_some_and(|header| self.shown_page(header.chat_mirrored) == Page::Terminal)
    }

    /// Whether a page covers the shown session's: Settings, the viewer or
    /// the browser is open.
    pub(super) fn page_covered(&self) -> bool {
        self.settings.is_some() || self.viewer.is_some() || self.browser.is_some()
    }

    /// Whether the window shows one page at a time: a one-pane layout, or
    /// a terminal filling the screen.
    fn one_pane(&self, cx: &App) -> bool {
        !self.layout.two_pane() || self.terminal_full(cx)
    }

    /// The window turned from layout `previous` to the one this frame
    /// picked: on its side, [`Self::turn_on_side`]; upright again,
    /// [`Self::turn_upright`].
    fn turn_terminal(&mut self, previous: PhoneLayout, cx: &App) {
        let full = PhoneLayout::TerminalFull;
        if previous != full && self.layout == full {
            self.turn_on_side(cx);
        } else if previous == full && self.layout != full {
            self.turn_upright();
        }
    }

    /// The iPhone turned on its side: a shown session on its Chat page that
    /// has a terminal goes to its Terminal page, which fills the screen, and
    /// the Chat page is recorded for the turn back.
    fn turn_on_side(&mut self, cx: &App) {
        let Some(header) = self.session_header(cx) else {
            return;
        };
        if let Some(page) = self.page.as_mut()
            && header.toggle
            && page.shown(header.chat_mirrored) == Page::Chat
        {
            page.set(Page::Terminal);
            self.landscape_return = Some(Page::Chat);
        }
    }

    /// The iPhone turned upright: the shown session returns to the page its
    /// turn on its side recorded, if any.
    fn turn_upright(&mut self) {
        let recorded = self.landscape_return.take();
        if let (Some(page), Some(recorded)) = (self.page.as_mut(), recorded) {
            page.set(recorded);
        }
    }

    /// Whether two panes draw rather than the hosts screen: a saved host is
    /// open, or a session, the viewer or the browser is up (they stay while
    /// their host is disconnected, as one pane keeps its page).
    fn panes_shown(&self, host_open: bool) -> bool {
        host_open || self.shown.is_some() || self.viewer.is_some() || self.browser.is_some()
    }

    /// The page the window draws inside its safe area.
    fn page_body(
        &mut self,
        host_open: bool,
        keyboard_up: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        // A terminal filling the screen draws as one pane does, under
        // Settings, the viewer and the browser.
        if self.one_pane(cx) {
            return self.single_body(host_open, keyboard_up, window, cx);
        }
        // Settings and the hosts screen cover the whole window in two panes.
        if let Some(page) = self.settings_page(window, cx) {
            page
        } else if self.panes_shown(host_open) {
            self.two_pane_body(keyboard_up, cx)
        } else {
            self.hosts_body(cx)
        }
    }

    /// One pane at a time: Settings, the viewer, the browser, a session's
    /// page, Home, else the hosts screen. Settings is on top, over whatever
    /// two panes left open in the session pane.
    fn single_body(
        &mut self,
        host_open: bool,
        keyboard_up: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        if let Some(page) = self.settings_page(window, cx) {
            page
        } else if let Some(page) = self.viewer_page(cx) {
            page
        } else if let Some(page) = self.browser_page(cx) {
            page
        } else if let Some(page) = self.session_page(keyboard_up, cx) {
            page
        } else if host_open {
            self.list_body(cx)
        } else {
            self.hosts_body(cx)
        }
    }

    /// Home's list in its pane, a line, then the session pane.
    fn two_pane_body(&mut self, keyboard_up: bool, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        let list = div()
            .driver_id(|| "phone-list-pane".to_owned())
            .flex_none()
            .w(px(LIST_PANE_WIDTH))
            .h_full()
            .child(self.list_body(cx));
        let line = div()
            .flex_none()
            .w(px(PANE_LINE_WIDTH))
            .h_full()
            .bg(gpui::rgb(t.line));
        let pane = self.session_pane(keyboard_up, cx);
        div()
            .size_full()
            .flex()
            .flex_row()
            .child(list)
            .child(line)
            .child(div().flex_1().min_w_0().h_full().child(pane))
    }

    /// The session pane: the viewer, the browser, the selected session's
    /// page, else "Pick a session".
    fn session_pane(&mut self, keyboard_up: bool, cx: &mut Context<Self>) -> Div {
        if let Some(page) = self.viewer_page(cx) {
            page
        } else if let Some(page) = self.browser_page(cx) {
            page
        } else if let Some(page) = self.session_page(keyboard_up, cx) {
            page
        } else {
            let t = theme(cx);
            div()
                .driver_id(|| "phone-pane-empty".to_owned())
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(gpui::rgb(t.muted))
                .child(PICK_SESSION_TEXT)
        }
    }

    /// The sheets over the page, in order: Home's long-press sheet, the
    /// browser's folder sheet, then the prompt sheet; in two panes each
    /// covers its own pane only.
    fn sheet_layers(
        &mut self,
        host_open: bool,
        padding: Edges<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<Div> {
        let split = self.split(host_open, cx);
        let boxed =
            |pane: Pane| move |layer: Div| Self::boxed(layer, split.then_some(pane), &padding);
        let sheet = self
            .home_sheet_over(host_open, cx)
            .then(|| self.sheet_layer(padding.bottom, cx))
            .flatten()
            .map(boxed(Pane::List));
        let sort_sheet = self
            .home_sheet_over(host_open, cx)
            .then(|| self.sort_sheet_layer(padding.bottom, cx))
            .flatten()
            .map(boxed(Pane::List));
        let browser_sheet = self
            .browser_sheet_shown()
            .then(|| self.browser_sheet_layer(padding.bottom, cx))
            .flatten()
            .map(boxed(Pane::Session));
        self.sync_prompt_sheet(cx);
        let screen_height = window.viewport_size().height;
        let prompt_sheet = self
            .prompt_sheet_shown()
            .then(|| self.prompt_sheet_layer(padding, screen_height, cx))
            .flatten()
            .map(boxed(Pane::Session));
        sheet
            .into_iter()
            .chain(sort_sheet)
            .chain(browser_sheet)
            .chain(prompt_sheet)
            .collect()
    }

    /// Whether Home's long-press sheet may draw: in two panes over the
    /// list while it shows; on one pane only over Home itself, with no
    /// viewer, browser, session page, Settings or hosts screen under it. A
    /// terminal filling the screen counts as one pane.
    fn home_sheet_over(&self, host_open: bool, cx: &App) -> bool {
        if !self.one_pane(cx) {
            return self.split(host_open, cx);
        }
        let page_over_home = self.viewer.is_some()
            || self.browser.is_some()
            || self.shown.is_some()
            || self.settings.is_some();
        host_open && !page_over_home
    }

    /// Whether the browser's folder sheet may draw: the browser is the page
    /// on top, under no viewer and no Settings.
    fn browser_sheet_shown(&self) -> bool {
        self.viewer.is_none() && self.settings.is_none()
    }

    /// Whether the prompt sheet may draw: a session's page is shown with no
    /// viewer, browser or Settings over it.
    fn prompt_sheet_shown(&self) -> bool {
        let page_over = self.viewer.is_some() || self.browser.is_some() || self.settings.is_some();
        self.shown.is_some() && !page_over
    }

    /// `layer` over the whole window, or over `pane` alone while one is
    /// given, inside `padding`'s side insets.
    fn boxed(layer: Div, pane: Option<Pane>, padding: &Edges<Pixels>) -> Div {
        let Some(pane) = pane else {
            return layer;
        };
        let frame = div().absolute().top_0().bottom_0();
        let frame = match pane {
            Pane::List => frame.left(padding.left).w(px(LIST_PANE_WIDTH)),
            Pane::Session => frame
                .left(padding.left + px(LIST_PANE_WIDTH + PANE_LINE_WIDTH))
                .right(padding.right),
        };
        frame.child(layer)
    }
}

impl Render for PhoneRoot {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The ready line follows the first frame, not the window's opening.
        if !self.logged_ready {
            self.logged_ready = true;
            tracing::info!("phone app started");
        }
        let t = crate::ui_theme::theme(cx);
        let insets = self
            .insets_override
            .clone()
            .unwrap_or_else(|| window.insets());
        let previous = self.layout;
        self.layout = PhoneLayout::of(window.viewport_size(), &insets);
        self.turn_terminal(previous, cx);
        let padding = content_padding(&insets);
        let keyboard_up = crate::keyboard::is_up(&insets);
        let hidden = super::terminal_fit::hidden_height(
            f32::from(insets.safe_area.bottom),
            f32::from(insets.ime.bottom),
        );
        self.sync_terminal(keyboard_up, hidden, window, cx);
        self.browser_next_page(cx);
        let host_open = hosts_view::any_host_open(self.root.read(cx));
        let body = self.page_body(host_open, keyboard_up, window, cx);
        let pairing = self
            .root
            .update(cx, |root, cx| root.device_pairing_layer(cx));
        let layers = self.sheet_layers(host_open, padding, window, cx);
        let padded = || {
            div()
                .pt(padding.top)
                .pr(padding.right)
                .pb(padding.bottom)
                .pl(padding.left)
        };
        // The whole window takes the viewer's, the browser's, a session
        // page's or Settings' back gesture, so a swipe that starts in the
        // safe area's bands goes back too.
        let goes_back = self.viewer.is_some()
            || self.browser.is_some()
            || self.shown.is_some()
            || self.settings.is_some();
        div()
            .relative()
            .size_full()
            .when(goes_back, |root| {
                root.on_back_gesture(cx.listener(Self::browser_back_gesture))
            })
            .bg(gpui::rgb(t.ground))
            .text_color(gpui::rgb(t.text))
            // The guard registers first, so its capture runs before any
            // layer's in the body.
            .child(Self::pan_guard(cx))
            .child(padded().size_full().child(body))
            .children(self.copied_chip_layer(padding.top, cx))
            .children(layers)
            .children(pairing.map(|layer| {
                padded()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .child(div().relative().size_full().child(layer))
            }))
    }
}
