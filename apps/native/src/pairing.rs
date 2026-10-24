//! The pairing model as plain data: the devices waiting for remote access,
//! the approval dialog with the one-time code it shows, the toast a new
//! request raises, and the QR matrix that code encodes to. Nothing here
//! draws anything or touches the daemon; the clock is passed in, so every
//! rule is a function of its inputs. The one thing it does beyond computing
//! is log a code that cannot be drawn as a QR code.

use std::collections::HashSet;
use std::fmt;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use protocol::{ClientMessage, PendingDevice, Secret};
use qrcodegen::{QrCode, QrCodeEcc};

/// How long an unapproved request stays listed after the device asked.
const REQUEST_LIFETIME_SECS: i64 = 300;

/// The title every pairing toast carries.
const TOAST_TITLE: &str = "A device asks for remote access";

/// A payload too long for a QR code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QrError;

impl fmt::Display for QrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("The code is too long for a QR code.")
    }
}

impl std::error::Error for QrError {}

/// A QR code's modules, row-major, without a quiet zone.
#[derive(Clone, PartialEq, Eq)]
pub struct QrMatrix {
    /// Modules per side.
    pub width: usize,
    modules: Vec<bool>,
}

impl QrMatrix {
    /// Whether the module at column `x`, row `y` is dark. A module outside
    /// the matrix is not.
    #[must_use]
    pub fn dark(&self, x: usize, y: usize) -> bool {
        x < self.width && y < self.width && self.modules[y * self.width + x]
    }
}

/// The matrix prints its width only: its modules decode back to the code,
/// which never prints in `{:?}`.
impl fmt::Debug for QrMatrix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QrMatrix")
            .field("width", &self.width)
            .finish_non_exhaustive()
    }
}

/// What the QR code carries: the bare one-time code, since the phone already
/// knows the host it is asking, and a scanned URL would open a browser
/// outside the app.
#[must_use]
pub fn qr_payload(code: &str) -> String {
    code.to_owned()
}

/// The QR matrix `payload` encodes to, at medium error correction.
///
/// # Errors
///
/// [`QrError`] when the payload is too long for a QR code.
pub fn qr_matrix(payload: &str) -> Result<QrMatrix, QrError> {
    let code = QrCode::encode_text(payload, QrCodeEcc::Medium).map_err(|_| QrError)?;
    let side = code.size();
    let width = usize::try_from(side).map_err(|_| QrError)?;
    let mut modules = Vec::with_capacity(width * width);
    for y in 0..side {
        for x in 0..side {
            modules.push(code.get_module(x, y));
        }
    }
    Ok(QrMatrix { width, modules })
}

/// What a press on the pairing toast means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingAction {
    /// Approve the request `request_id`: opens the approval dialog, whose
    /// own Approve button is what approves; the toast never does.
    Approve { request_id: String },
    /// Decline the request `request_id`.
    Deny { request_id: String },
    /// A press on the toast body: bring the app forward.
    Open,
}

impl PairingAction {
    /// The action a toast activation carries: `approve:<id>` or `deny:<id>`
    /// from a button, no argument at all from a press on the toast body, and
    /// nothing from any other argument.
    #[must_use]
    pub fn from_activation(arg: Option<&str>) -> Option<Self> {
        let Some(arg) = arg else {
            return Some(Self::Open);
        };
        let (kind, request_id) = arg.split_once(':')?;
        if request_id.is_empty() || request_id.contains(':') {
            return None;
        }
        match kind {
            "approve" => Some(Self::Approve {
                request_id: request_id.to_owned(),
            }),
            "deny" => Some(Self::Deny {
                request_id: request_id.to_owned(),
            }),
            _ => None,
        }
    }

    /// The argument the Approve button carries for `request_id`.
    #[must_use]
    pub fn approve_arg(request_id: &str) -> String {
        format!("approve:{request_id}")
    }

    /// The argument the Deny button carries for `request_id`.
    #[must_use]
    pub fn deny_arg(request_id: &str) -> String {
        format!("deny:{request_id}")
    }
}

/// The toast a waiting request raises, once per request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingToast {
    pub request_id: String,
    pub title: String,
    pub body: String,
}

impl PairingToast {
    /// The toast for `request`, naming the device and the Tailscale login
    /// the daemon's `whois` read for it.
    #[must_use]
    pub fn for_request(request: &PendingDevice) -> Self {
        Self {
            request_id: request.request_id.clone(),
            title: TOAST_TITLE.to_owned(),
            body: identity_line(&request.name, &request.login),
        }
    }
}

/// Who is asking, and as whom they signed in to Tailscale.
fn identity_line(name: &str, login: &str) -> String {
    format!("{name} · signed in to Tailscale as {login}")
}

/// One waiting request, as the Remote access page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingRow {
    pub request_id: String,
    pub name: String,
    pub login: String,
    pub approved: bool,
    /// Seconds until the request lapses, rounded up; zero for an approved
    /// request, which waits on the daemon instead of this clock.
    pub remaining_secs: u64,
}

impl WaitingRow {
    /// The device and its login on one line: `name (login)`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} ({})", self.name, self.login)
    }
}

/// Which phase the approval dialog is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogPhase {
    /// The owner has not approved yet: no code, no QR, and an Approve button
    /// that sends [`approve_message`].
    Confirm,
    /// The code the owner's approval minted, while it lasts.
    Code {
        /// The one-time code; [`Secret`] keeps it out of a `{:?}`.
        code: Secret,
        /// When the code stops being good.
        deadline: Instant,
        /// The code as a QR matrix, absent when it could not be encoded.
        qr: Option<QrMatrix>,
    },
}

/// The approval dialog for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogView {
    pub request_id: String,
    pub name: String,
    pub login: String,
    pub phase: DialogPhase,
}

impl DialogView {
    /// The dialog's title, in both phases.
    #[must_use]
    pub fn title(&self) -> String {
        format!("Approve {}", self.name)
    }

    /// Who is asking, as the Confirm phase says it.
    #[must_use]
    pub fn device_line(&self) -> String {
        identity_line(&self.name, &self.login)
    }

    /// The dialog's button labels, in order.
    #[must_use]
    pub fn buttons(&self) -> &'static [&'static str] {
        match self.phase {
            DialogPhase::Confirm => &["Approve", "Deny", "Close"],
            DialogPhase::Code { .. } => &["Deny", "Close"],
        }
    }

    /// The code in two groups of three, once one is minted.
    #[must_use]
    pub fn code_groups(&self) -> Option<String> {
        match &self.phase {
            DialogPhase::Confirm => None,
            DialogPhase::Code { code, .. } => Some(groups(&code.0)),
        }
    }

    /// Seconds left on the code at `now`, rounded up; `None` before a code
    /// exists.
    #[must_use]
    pub fn remaining_secs(&self, now: Instant) -> Option<u64> {
        match &self.phase {
            DialogPhase::Confirm => None,
            DialogPhase::Code { deadline, .. } => {
                Some(secs_ceil(deadline.saturating_duration_since(now)))
            }
        }
    }

    /// The line under the code, while it lasts.
    #[must_use]
    pub fn timer_text(&self, now: Instant) -> Option<String> {
        self.remaining_secs(now)
            .filter(|secs| *secs > 0)
            .map(|secs| format!("expires in {} · this request only", mmss(secs)))
    }

    /// The code as a QR matrix, once one is minted and could be drawn.
    #[must_use]
    pub fn qr(&self) -> Option<&QrMatrix> {
        match &self.phase {
            DialogPhase::Confirm => None,
            DialogPhase::Code { qr, .. } => qr.as_ref(),
        }
    }
}

/// `code` in groups of three characters.
fn groups(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    chars
        .chunks(3)
        .map(|group| group.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `secs` as `m:ss`.
pub(crate) fn mmss(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Whole seconds in `left`, rounded up.
fn secs_ceil(left: Duration) -> u64 {
    let millis = left.as_millis();
    if millis == 0 {
        return 0;
    }
    u64::try_from(millis.div_ceil(1000)).unwrap_or(u64::MAX)
}

/// The message that approves `request_id`.
#[must_use]
pub fn approve_message(request_id: &str) -> ClientMessage {
    ClientMessage::ApproveDevice {
        request_id: request_id.to_owned(),
    }
}

/// The message that declines `request_id`.
#[must_use]
pub fn deny_message(request_id: &str) -> ClientMessage {
    ClientMessage::DenyDevice {
        request_id: request_id.to_owned(),
    }
}

/// The pairing model: the waiting requests, the approval dialog the owner
/// opened, and which requests have raised their toast already.
#[derive(Debug, Clone, Default)]
pub struct PairingState {
    /// The waiting requests as last listed, without the ones the list
    /// carries no longer.
    pending: Vec<PendingDevice>,
    /// The requests that have raised their toast already.
    toasted: HashSet<String>,
    /// The approval dialog that is open, with its code once minted.
    open: Option<DialogView>,
}

impl PairingState {
    /// A model that knows nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds the daemon's waiting list in at `now`: the requests the list
    /// no longer carries are dropped, along with an open dialog for one of
    /// them and its code, and the ids of the unapproved requests not seen
    /// before come back, so each request raises its toast once.
    pub fn on_pending(&mut self, requests: &[PendingDevice], now: DateTime<Utc>) -> Vec<String> {
        self.pending = requests
            .iter()
            .filter(|request| waiting(request, now))
            .cloned()
            .collect();
        let mut fresh = Vec::new();
        for request in &self.pending {
            let first_sighting = self.toasted.insert(request.request_id.clone());
            if first_sighting && !request.approved {
                fresh.push(request.request_id.clone());
            }
        }
        let listed: HashSet<&str> = self
            .pending
            .iter()
            .map(|request| request.request_id.as_str())
            .collect();
        self.toasted.retain(|id| listed.contains(id.as_str()));
        if self
            .open
            .as_ref()
            .is_some_and(|dialog| !listed.contains(dialog.request_id.as_str()))
        {
            self.open = None;
        }
        fresh
    }

    /// Folds in the one-time code the daemon minted and switches the dialog
    /// to it: `true` whenever the client shows the dialog, and `false` only
    /// for a CLI-minted code, which names no request.
    pub fn on_code(
        &mut self,
        request_id: Option<&str>,
        code: &Secret,
        expires_in_secs: u64,
        now: Instant,
    ) -> bool {
        let Some(request_id) = request_id else {
            return false;
        };
        let qr = match qr_matrix(&qr_payload(&code.0)) {
            Ok(qr) => Some(qr),
            Err(err) => {
                tracing::warn!("the pairing code could not be drawn as a QR code: {err}");
                None
            }
        };
        let (name, login) = self.identity(request_id);
        self.open = Some(DialogView {
            request_id: request_id.to_owned(),
            name,
            login,
            phase: DialogPhase::Code {
                code: code.clone(),
                deadline: now + Duration::from_secs(expires_in_secs),
                qr,
            },
        });
        true
    }

    /// The device name and login `request_id` is known by, from its open
    /// dialog or else from its waiting row.
    fn identity(&self, request_id: &str) -> (String, String) {
        if let Some(dialog) = self
            .open
            .as_ref()
            .filter(|dialog| dialog.request_id == request_id)
        {
            return (dialog.name.clone(), dialog.login.clone());
        }
        self.pending
            .iter()
            .find(|request| request.request_id == request_id)
            .map_or_else(
                || (request_id.to_owned(), String::new()),
                |row| (row.name.clone(), row.login.clone()),
            )
    }

    /// The waiting requests at `now`, the lapsed ones left out.
    #[must_use]
    pub fn rows(&self, now: DateTime<Utc>) -> Vec<WaitingRow> {
        self.pending
            .iter()
            .filter(|request| waiting(request, now))
            .map(|request| WaitingRow {
                request_id: request.request_id.clone(),
                name: request.name.clone(),
                login: request.login.clone(),
                approved: request.approved,
                remaining_secs: remaining_secs(request, now),
            })
            .collect()
    }

    /// How many requests are waiting at `now`, for the Remote access tab's
    /// badge: the same requests [`Self::rows`] lists.
    #[must_use]
    pub fn count(&self, now: DateTime<Utc>) -> usize {
        self.pending
            .iter()
            .filter(|request| waiting(request, now))
            .count()
    }

    /// Opens the approval dialog for `request_id` in its Confirm phase.
    pub fn open_dialog(&mut self, request_id: &str) {
        let (name, login) = self.identity(request_id);
        self.open = Some(DialogView {
            request_id: request_id.to_owned(),
            name,
            login,
            phase: DialogPhase::Confirm,
        });
    }

    /// Closes the approval dialog and drops its code.
    pub fn close_dialog(&mut self) {
        self.open = None;
    }

    /// The dialog at `now`, while its request is listed and its code has
    /// time left.
    #[must_use]
    pub fn dialog(&self, now: Instant) -> Option<DialogView> {
        let dialog = self.open.as_ref()?;
        match &dialog.phase {
            DialogPhase::Confirm => Some(dialog.clone()),
            DialogPhase::Code { deadline, .. } => {
                (!deadline.saturating_duration_since(now).is_zero()).then(|| dialog.clone())
            }
        }
    }

    /// Forgets everything: another host, or a target this client no longer
    /// holds.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.toasted.clear();
        self.open = None;
    }
}

/// When `request` lapses, unless it is approved.
fn lapse_at(request: &PendingDevice) -> DateTime<Utc> {
    request.asked_at + TimeDelta::seconds(REQUEST_LIFETIME_SECS)
}

/// Whether `request` is still waiting at `now`. An approved request never
/// lapses on this clock: the daemon keeps it until its code has run out, so
/// it lives until the daemon's list drops it.
fn waiting(request: &PendingDevice, now: DateTime<Utc>) -> bool {
    request.approved || remaining_secs(request, now) > 0
}

/// Seconds `request` has left at `now`, rounded up; zero for an approved
/// request and for one past its lifetime.
fn remaining_secs(request: &PendingDevice, now: DateTime<Utc>) -> u64 {
    if request.approved {
        return 0;
    }
    (lapse_at(request) - now).to_std().map_or(0, secs_ceil)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::time::{Duration, Instant};

    use chrono::{DateTime, TimeDelta, Utc};
    use protocol::{ClientMessage, PendingDevice, Secret};

    use super::{
        DialogPhase, PairingAction, PairingState, PairingToast, approve_message, deny_message,
        qr_matrix, qr_payload,
    };

    /// The instant every fixture request asked at.
    fn asked_at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).expect("a valid instant")
    }

    fn request(id: &str) -> PendingDevice {
        PendingDevice {
            request_id: id.to_owned(),
            name: "phone".to_owned(),
            login: "alice@example.com".to_owned(),
            asked_at: asked_at(),
            approved: false,
        }
    }

    fn approved_request(id: &str) -> PendingDevice {
        PendingDevice {
            approved: true,
            ..request(id)
        }
    }

    fn code() -> Secret {
        Secret("735160".to_owned())
    }

    /// A state listing one request and showing the code minted for it.
    fn showing_code(at: Instant) -> PairingState {
        let mut state = PairingState::new();
        state.on_pending(&[request("r1")], asked_at());
        assert!(state.on_code(Some("r1"), &code(), 120, at));
        state
    }

    #[test]
    fn a_request_row_names_the_device_and_its_login() {
        let mut state = PairingState::new();
        state.on_pending(&[request("r1")], asked_at());
        let rows = state.rows(asked_at());
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.request_id, "r1");
        assert_eq!(row.name, "phone");
        assert_eq!(row.login, "alice@example.com");
        assert_eq!(row.label(), "phone (alice@example.com)");
        assert!(!row.approved);
        assert_eq!(row.remaining_secs, 300);
        assert_eq!(state.count(asked_at()), 1);
    }

    #[test]
    fn request_lapses_after_its_expiry() {
        let mut state = PairingState::new();
        let at = asked_at();
        state.on_pending(&[request("r1")], at);
        assert_eq!(state.rows(at + TimeDelta::seconds(299)).len(), 1);
        assert!(
            state.rows(at + TimeDelta::seconds(300)).is_empty(),
            "five minutes"
        );
        state.on_pending(&[request("r2")], at + TimeDelta::seconds(301));
        assert!(
            state.rows(at + TimeDelta::seconds(301)).is_empty(),
            "a request already past its lifetime is never listed"
        );
    }

    #[test]
    fn an_approved_request_stays_until_the_daemon_drops_it() {
        let mut state = PairingState::new();
        let at = asked_at();
        state.on_pending(&[approved_request("r1")], at);
        assert_eq!(state.rows(at + TimeDelta::seconds(270)).len(), 1, "4:30");
        assert_eq!(
            state.rows(at + TimeDelta::seconds(330)).len(),
            1,
            "5:30, past the client's five minutes"
        );
        assert_eq!(state.count(at + TimeDelta::seconds(330)), 1);
        assert_eq!(
            state.rows(at + TimeDelta::seconds(330))[0].remaining_secs,
            0,
            "no countdown runs on an approved request"
        );
        state.on_pending(&[], at + TimeDelta::seconds(331));
        assert!(
            state.rows(at + TimeDelta::seconds(331)).is_empty(),
            "the daemon dropped it"
        );
    }

    #[test]
    fn a_request_missing_from_the_list_is_dropped() {
        let mut state = PairingState::new();
        state.on_pending(&[request("r1"), request("r2")], asked_at());
        assert_eq!(state.count(asked_at()), 2);
        state.on_pending(&[request("r1")], asked_at());
        let rows = state.rows(asked_at());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, "r1");
        assert_eq!(state.count(asked_at()), 1);
    }

    #[test]
    fn only_new_requests_are_reported_for_a_toast() {
        let mut state = PairingState::new();
        assert_eq!(state.on_pending(&[request("r1")], asked_at()), ["r1"]);
        assert_eq!(
            state.on_pending(&[request("r1")], asked_at()),
            Vec::<String>::new(),
            "a request seen before raises no second toast"
        );
        assert_eq!(
            state.on_pending(&[request("r1"), request("r2")], asked_at()),
            ["r2"]
        );
        assert_eq!(
            state.on_pending(&[request("r3")], asked_at()),
            ["r3"],
            "a superseding request is new"
        );
    }

    #[test]
    fn an_approved_request_raises_no_toast() {
        let mut state = PairingState::new();
        assert_eq!(
            state.on_pending(&[approved_request("r1")], asked_at()),
            Vec::<String>::new(),
            "a request that arrives approved asks for nothing"
        );
        assert_eq!(state.count(asked_at()), 1, "it is still a row");
    }

    #[test]
    fn qr_matrix_encodes_the_payload() {
        let matrix = qr_matrix("735160").expect("six digits fit a QR code");
        assert_eq!(matrix.width, 21, "version 1 at medium correction");
        assert!(
            !matrix.dark(21, 0),
            "a module outside the matrix is not dark"
        );
        assert!(
            !matrix.dark(0, 21),
            "a module outside the matrix is not dark"
        );
        for y in 0..7 {
            for x in 0..7 {
                let border = x == 0 || y == 0 || x == 6 || y == 6;
                let centre = (2..=4).contains(&x) && (2..=4).contains(&y);
                assert_eq!(matrix.dark(x, y), border || centre, "({x}, {y})");
            }
        }
        let other = qr_matrix("735161").expect("six digits fit a QR code");
        assert_ne!(matrix, other, "a different payload is a different matrix");
    }

    #[test]
    fn qr_payload_is_the_bare_code() {
        assert_eq!(qr_payload("735160"), "735160");
    }

    #[test]
    fn approve_message_carries_only_the_request_id() {
        assert!(matches!(
            approve_message("r1"),
            ClientMessage::ApproveDevice { request_id } if request_id == "r1"
        ));
        assert!(matches!(
            deny_message("r1"),
            ClientMessage::DenyDevice { request_id } if request_id == "r1"
        ));
    }

    #[test]
    fn opening_the_dialog_sends_nothing() {
        let now = Instant::now();
        let mut state = PairingState::new();
        state.on_pending(&[request("r1")], asked_at());
        state.open_dialog("r1");
        let dialog = state.dialog(now).expect("the Confirm phase shows at once");
        assert_eq!(dialog.phase, DialogPhase::Confirm);
        assert_eq!(dialog.title(), "Approve phone");
        assert_eq!(
            dialog.device_line(),
            "phone · signed in to Tailscale as alice@example.com"
        );
        assert_eq!(dialog.buttons(), ["Approve", "Deny", "Close"].as_slice());
        assert!(dialog.code_groups().is_none(), "no code yet");
        assert!(dialog.qr().is_none(), "no QR yet");
        assert!(dialog.timer_text(now).is_none(), "nothing to count down");
        assert_eq!(state.count(asked_at()), 1, "the request is still waiting");
        assert!(!state.rows(asked_at())[0].approved, "nothing was approved");
    }

    #[test]
    fn a_code_switches_the_open_dialog_to_the_code_phase() {
        let now = Instant::now();
        let mut state = PairingState::new();
        state.on_pending(&[request("r1")], asked_at());
        state.open_dialog("r1");
        assert!(state.on_code(Some("r1"), &code(), 120, now));
        let dialog = state.dialog(now).expect("the code shows at once");
        assert!(matches!(dialog.phase, DialogPhase::Code { .. }));
        assert_eq!(dialog.code_groups().as_deref(), Some("735 160"));
        assert_eq!(dialog.buttons(), ["Deny", "Close"].as_slice());
        assert_eq!(dialog.title(), "Approve phone");
    }

    #[test]
    fn a_request_that_leaves_the_list_closes_its_dialog() {
        let now = Instant::now();
        let mut state = showing_code(now);
        assert!(state.dialog(now).is_some(), "the code shows");
        state.on_pending(&[], asked_at());
        assert!(
            state.dialog(now).is_none(),
            "the dialog closes with its request"
        );
        state.open_dialog("r1");
        assert_eq!(
            state.dialog(now).expect("reopened").phase,
            DialogPhase::Confirm,
            "the code went with the dialog"
        );
    }

    #[test]
    fn code_countdown_ends_at_two_minutes() {
        let now = Instant::now();
        let state = showing_code(now);
        let dialog = state.dialog(now).expect("the code shows at once");
        assert_eq!(dialog.remaining_secs(now), Some(120));
        assert_eq!(
            dialog.timer_text(now).as_deref(),
            Some("expires in 2:00 · this request only")
        );
        assert_eq!(
            dialog.timer_text(now + Duration::from_secs(2)).as_deref(),
            Some("expires in 1:58 · this request only")
        );
        assert!(state.dialog(now + Duration::from_secs(119)).is_some());
        assert!(
            state.dialog(now + Duration::from_secs(120)).is_none(),
            "the dialog ends with the code"
        );
    }

    #[test]
    fn countdowns_round_up_and_lapse_only_at_zero() {
        let now = Instant::now();
        let state = showing_code(now);
        let just_minted = now + Duration::from_millis(1);
        let dialog = state.dialog(just_minted).expect("still showing");
        assert_eq!(
            dialog.remaining_secs(just_minted),
            Some(120),
            "a millisecond in still reads 2:00"
        );
        let half_left = now + Duration::from_millis(119_500);
        assert!(
            state.dialog(half_left).is_some(),
            "half a second left still shows"
        );
        assert_eq!(
            state
                .dialog(half_left)
                .and_then(|dialog| dialog.timer_text(half_left))
                .as_deref(),
            Some("expires in 0:01 · this request only")
        );
        assert!(
            state.dialog(now + Duration::from_secs(120)).is_none(),
            "expiry is at zero, not when whole seconds truncate"
        );

        let mut waiting_state = PairingState::new();
        let at = asked_at();
        waiting_state.on_pending(&[request("r1")], at);
        assert_eq!(
            waiting_state
                .rows(at + TimeDelta::milliseconds(299_500))
                .len(),
            1,
            "half a second left is still a row"
        );
        assert!(
            waiting_state.rows(at + TimeDelta::seconds(300)).is_empty(),
            "five minutes"
        );
    }

    #[test]
    fn a_code_that_cannot_be_drawn_still_shows() {
        let now = Instant::now();
        let mut state = PairingState::new();
        state.on_pending(&[request("r1")], asked_at());
        let long = Secret("7".repeat(9_000));
        assert!(
            state.on_code(Some("r1"), &long, 120, now),
            "only a CLI-minted code is not shown"
        );
        let dialog = state.dialog(now).expect("the code shows without a QR");
        assert!(dialog.qr().is_none(), "no matrix for a code that big");
        assert!(dialog.code_groups().is_some(), "the code still reads out");
    }

    #[test]
    fn cli_minted_code_opens_no_dialog() {
        let mut state = PairingState::new();
        assert!(
            !state.on_code(None, &code(), 120, Instant::now()),
            "a code nobody asked for shows nothing"
        );
        assert!(state.dialog(Instant::now()).is_none());
    }

    #[test]
    fn dialog_text_follows_the_mock() {
        let now = Instant::now();
        let state = showing_code(now);
        let dialog = state.dialog(now).expect("the code shows at once");
        assert_eq!(dialog.title(), "Approve phone");
        assert_eq!(dialog.code_groups().as_deref(), Some("735 160"));
        assert_eq!(
            dialog.timer_text(now).as_deref(),
            Some("expires in 2:00 · this request only")
        );
        assert_eq!(
            dialog.qr(),
            Some(&qr_matrix("735160").expect("six digits fit"))
        );
    }

    #[test]
    fn code_never_prints_in_debug() {
        let now = Instant::now();
        let state = showing_code(now);
        let model = format!("{state:?}");
        assert!(!model.contains("735160"), "{model}");
        assert!(model.contains("Secret(<redacted>)"), "{model}");
        assert!(!model.contains("true,"), "no module list in {model}");
        let dialog = format!("{:?}", state.dialog(now).expect("the code shows"));
        assert!(!dialog.contains("735160"), "{dialog}");
        assert!(!dialog.contains("true,"), "no module list in {dialog}");
        assert!(dialog.contains("width: 21"), "{dialog}");
    }

    #[test]
    fn toast_text_names_the_device_and_login() {
        let toast = PairingToast::for_request(&request("r1"));
        assert_eq!(toast.request_id, "r1");
        assert_eq!(toast.title, "A device asks for remote access");
        assert_eq!(
            toast.body,
            "phone · signed in to Tailscale as alice@example.com"
        );
    }

    #[test]
    fn toast_actions_parse_approve_deny_and_body_clicks() {
        assert_eq!(
            PairingAction::from_activation(None),
            Some(PairingAction::Open)
        );
        assert_eq!(
            PairingAction::from_activation(Some("approve:r1")),
            Some(PairingAction::Approve {
                request_id: "r1".to_owned()
            })
        );
        assert_eq!(
            PairingAction::from_activation(Some("deny:r1")),
            Some(PairingAction::Deny {
                request_id: "r1".to_owned()
            })
        );
        assert_eq!(PairingAction::approve_arg("r1"), "approve:r1");
        assert_eq!(PairingAction::deny_arg("r1"), "deny:r1");
        for garbage in [
            "",
            "approve",
            "approve:",
            "approve:r1:extra",
            "deny:",
            "open",
            "approve  r1",
            ":r1",
        ] {
            assert_eq!(
                PairingAction::from_activation(Some(garbage)),
                None,
                "{garbage:?}"
            );
        }
    }
}
