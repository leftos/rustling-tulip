//! This PC pairing itself with a tailnet host, as the device: the dialog's
//! phases from the first ask through the code the owner reads on the host,
//! with their lines, buttons and countdowns. The network thread stores the
//! token the host issues; nothing of it reaches this model. The screen is
//! [`crate::device_pairing_view`].

use std::fmt;
use std::time::{Duration, Instant};

use protocol::{DEVICE_NOT_APPROVED, DaemonMessage, DevicePairOutcome};

use crate::pairing::mmss;

/// How many digits a pairing code has.
pub(crate) const CODE_LEN: usize = 6;
/// How long an approval's code stays good on the host.
const CODE_LIFETIME: Duration = Duration::from_secs(120);

/// Which phase the dialog is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhaseKind {
    Intro,
    Asking,
    Waiting,
    CodeEntry,
    Ended,
}

/// A button of the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceButton {
    Ask,
    AskAgain,
    Pair,
    Cancel,
    Close,
}

impl DeviceButton {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask to pair",
            Self::AskAgain => "Ask again",
            Self::Pair => "Pair",
            Self::Cancel => "Cancel",
            Self::Close => "Close",
        }
    }

    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Ask => "device-pairing-ask",
            Self::AskAgain => "device-pairing-ask-again",
            Self::Pair => "device-pairing-pair",
            Self::Cancel => "device-pairing-cancel",
            Self::Close => "device-pairing-close",
        }
    }

    /// Whether it is its phase's main action, drawn as primary.
    pub(crate) fn is_primary(self) -> bool {
        matches!(self, Self::Ask | Self::AskAgain | Self::Pair)
    }
}

/// Why the request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EndReason {
    Denied,
    /// The host refused the request as soon as it went out.
    NotAccepted,
    Expired,
    Superseded,
    Unknown,
    Lost,
    TokenSaveFailed,
    /// The host refused a pairing step with this reason.
    Failed(String),
}

#[derive(Clone)]
enum Phase {
    Intro,
    /// The request is asked for; `sent` once it is on the host's socket.
    Asking {
        sent: bool,
    },
    Waiting {
        deadline: Instant,
    },
    CodeEntry {
        deadline: Instant,
        /// The digits typed so far; never printed.
        code: String,
        /// The host's last refusal of a code, until the next keystroke.
        refusal: Option<String>,
    },
    Ended(EndReason),
}

/// The pairing dialog's state for one host.
#[derive(Clone)]
pub(crate) struct DevicePairing {
    host: String,
    had_token: bool,
    phase: Phase,
}

impl fmt::Debug for DevicePairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DevicePairing")
            .field("host", &self.host)
            .field("had_token", &self.had_token)
            .field("phase", &self.kind())
            .finish_non_exhaustive()
    }
}

impl DevicePairing {
    /// The dialog for `host` in its Intro phase; `had_token` says whether
    /// the host turned down a token this PC held.
    pub(crate) fn new(host: &str, had_token: bool) -> Self {
        Self {
            host: host.to_owned(),
            had_token,
            phase: Phase::Intro,
        }
    }

    pub(crate) fn kind(&self) -> PhaseKind {
        match self.phase {
            Phase::Intro => PhaseKind::Intro,
            Phase::Asking { .. } => PhaseKind::Asking,
            Phase::Waiting { .. } => PhaseKind::Waiting,
            Phase::CodeEntry { .. } => PhaseKind::CodeEntry,
            Phase::Ended(_) => PhaseKind::Ended,
        }
    }

    pub(crate) fn title(&self) -> String {
        format!("Pair with {}", self.host)
    }

    /// The phase's sentence.
    pub(crate) fn line(&self) -> String {
        let host = &self.host;
        match &self.phase {
            Phase::Intro if self.had_token => {
                format!("{host} no longer accepts this PC's pairing.")
            }
            Phase::Intro => format!("{host} does not know this PC."),
            Phase::Asking { .. } => format!("Asking {host}…"),
            Phase::Waiting { .. } => format!("Approve this PC on {host}."),
            Phase::CodeEntry { .. } => format!("Type the code {host} shows."),
            Phase::Ended(reason) => self.end_line(reason),
        }
    }

    fn end_line(&self, reason: &EndReason) -> String {
        let host = &self.host;
        match reason {
            EndReason::Denied => format!("{host} turned this PC down."),
            EndReason::NotAccepted => format!("{host} did not accept this PC's request."),
            EndReason::Expired => "The request expired.".to_owned(),
            EndReason::Superseded => "A newer request from this PC replaced this one.".to_owned(),
            EndReason::Unknown => "The request ended.".to_owned(),
            EndReason::Lost => {
                format!("The connection to {host} dropped, and the request with it.")
            }
            EndReason::TokenSaveFailed => {
                "Could not save this device's token; pair again.".to_owned()
            }
            EndReason::Failed(reason) => reason.clone(),
        }
    }

    /// The countdown the phase shows at `now`: the request's while it
    /// waits, the code's once approved.
    pub(crate) fn countdown(&self, now: Instant) -> Option<String> {
        let left = |deadline: Instant| mmss(deadline.saturating_duration_since(now).as_secs());
        match &self.phase {
            Phase::Waiting { deadline } => Some(format!("expires in {}", left(*deadline))),
            Phase::CodeEntry { deadline, .. } => {
                Some(format!("code expires in {}", left(*deadline)))
            }
            Phase::Intro | Phase::Asking { .. } | Phase::Ended(_) => None,
        }
    }

    /// The host's last refusal of a typed code.
    pub(crate) fn refusal(&self) -> Option<&str> {
        match &self.phase {
            Phase::CodeEntry { refusal, .. } => refusal.as_deref(),
            _ => None,
        }
    }

    /// The digits in the code field, while it shows.
    pub(crate) fn code(&self) -> Option<&str> {
        match &self.phase {
            Phase::CodeEntry { code, .. } => Some(code),
            _ => None,
        }
    }

    pub(crate) fn buttons(&self) -> &'static [DeviceButton] {
        match self.phase {
            Phase::Intro => &[DeviceButton::Ask, DeviceButton::Close],
            Phase::Asking { .. } | Phase::Waiting { .. } => &[DeviceButton::Cancel],
            Phase::CodeEntry { .. } => &[DeviceButton::Pair, DeviceButton::Cancel],
            Phase::Ended(_) => &[DeviceButton::AskAgain, DeviceButton::Close],
        }
    }

    /// Whether the typed code can be sent: exactly six digits.
    pub(crate) fn can_pair(&self) -> bool {
        self.code().is_some_and(|code| code.len() == CODE_LEN)
    }

    /// The button Enter presses, when the phase has one to press.
    pub(crate) fn primary(&self) -> Option<DeviceButton> {
        match self.phase {
            Phase::Intro => Some(DeviceButton::Ask),
            Phase::Ended(_) => Some(DeviceButton::AskAgain),
            Phase::CodeEntry { .. } if self.can_pair() => Some(DeviceButton::Pair),
            Phase::Asking { .. } | Phase::Waiting { .. } | Phase::CodeEntry { .. } => None,
        }
    }

    /// Whether a request is out, so closing the dialog withdraws it.
    pub(crate) fn cancels_request(&self) -> bool {
        matches!(
            self.phase,
            Phase::Asking { .. } | Phase::Waiting { .. } | Phase::CodeEntry { .. }
        )
    }

    /// Ask to pair, or Ask again: returns whether a request goes out.
    pub(crate) fn ask(&mut self) -> bool {
        if matches!(self.phase, Phase::Intro | Phase::Ended(_)) {
            self.phase = Phase::Asking { sent: false };
            true
        } else {
            false
        }
    }

    /// Folds in a pairing message from the host, at `now`. The "not
    /// approved" signal ends the request only once it is on the host's
    /// socket ([`Self::on_request_sent`]); before that it is the pairing
    /// connection's greeting and changes nothing.
    pub(crate) fn on_message(&mut self, msg: &DaemonMessage, now: Instant) {
        match msg {
            DaemonMessage::DevicePairPending {
                expires_in_secs, ..
            } => {
                if matches!(self.phase, Phase::Asking { .. } | Phase::Waiting { .. }) {
                    let deadline = now + Duration::from_secs(*expires_in_secs);
                    self.phase = Phase::Waiting { deadline };
                }
            }
            DaemonMessage::DevicePairDecided { outcome, .. } => self.on_decided(*outcome, now),
            DaemonMessage::DevicePairFailed { reason } if reason != DEVICE_NOT_APPROVED => {
                self.on_failed(reason);
            }
            DaemonMessage::DevicePairFailed { .. } => {
                if matches!(self.phase, Phase::Asking { sent: true }) {
                    self.phase = Phase::Ended(EndReason::NotAccepted);
                }
            }
            _ => {}
        }
    }

    fn on_decided(&mut self, outcome: DevicePairOutcome, now: Instant) {
        if !self.cancels_request() {
            return;
        }
        let reason = match outcome {
            DevicePairOutcome::Approved => {
                self.on_approved(now);
                return;
            }
            DevicePairOutcome::Denied => EndReason::Denied,
            DevicePairOutcome::Expired => EndReason::Expired,
            DevicePairOutcome::Superseded => EndReason::Superseded,
            DevicePairOutcome::Unknown => EndReason::Unknown,
        };
        self.phase = Phase::Ended(reason);
    }

    /// The owner approved: the code field shows, its countdown restarted;
    /// digits typed under an earlier approval stay.
    fn on_approved(&mut self, now: Instant) {
        let code = match &mut self.phase {
            Phase::CodeEntry { code, .. } => std::mem::take(code),
            _ => String::new(),
        };
        self.phase = Phase::CodeEntry {
            deadline: now + CODE_LIFETIME,
            code,
            refusal: None,
        };
    }

    fn on_failed(&mut self, reason: &str) {
        match &mut self.phase {
            Phase::CodeEntry { refusal, .. } => *refusal = Some(reason.to_owned()),
            Phase::Asking { .. } | Phase::Waiting { .. } => {
                self.phase = Phase::Ended(EndReason::Failed(reason.to_owned()));
            }
            Phase::Intro | Phase::Ended(_) => {}
        }
    }

    /// The request is on the host's socket: a "not approved" from now on
    /// is the host refusing it, not the redial's greeting.
    pub(crate) fn on_request_sent(&mut self) {
        if let Phase::Asking { sent } = &mut self.phase {
            *sent = true;
        }
    }

    /// The pairing socket closed while a request was out.
    pub(crate) fn on_lost(&mut self) {
        if self.cancels_request() {
            self.phase = Phase::Ended(EndReason::Lost);
        }
    }

    /// The host issued a token that could not be stored.
    pub(crate) fn on_token_save_failed(&mut self) {
        self.phase = Phase::Ended(EndReason::TokenSaveFailed);
    }

    /// The code field's new text: its digits, at most six, kept; returns
    /// what the field holds now. Any keystroke clears the refusal. Before
    /// the approval there is no field and nothing is kept.
    pub(crate) fn type_code(&mut self, text: &str) -> String {
        let Phase::CodeEntry { code, refusal, .. } = &mut self.phase else {
            return String::new();
        };
        *code = text
            .chars()
            .filter(char::is_ascii_digit)
            .take(CODE_LEN)
            .collect();
        *refusal = None;
        code.clone()
    }

    /// Pair: the six typed digits, the field cleared; `None` while the code
    /// is not complete.
    pub(crate) fn take_code(&mut self) -> Option<String> {
        if !self.can_pair() {
            return None;
        }
        match &mut self.phase {
            Phase::CodeEntry { code, .. } => Some(std::mem::take(code)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use protocol::{DEVICE_NOT_APPROVED, DaemonMessage, DevicePairOutcome};

    use super::{DeviceButton, DevicePairing, PhaseKind};

    const HOST: &str = "box.tail1234.ts.net";

    fn pending(expires_in_secs: u64) -> DaemonMessage {
        DaemonMessage::DevicePairPending {
            request_id: "r1".to_owned(),
            expires_in_secs,
        }
    }

    fn decided(outcome: DevicePairOutcome) -> DaemonMessage {
        DaemonMessage::DevicePairDecided {
            request_id: "r1".to_owned(),
            outcome,
        }
    }

    fn failed(reason: &str) -> DaemonMessage {
        DaemonMessage::DevicePairFailed {
            reason: reason.to_owned(),
        }
    }

    fn waiting(now: Instant) -> DevicePairing {
        let mut pairing = DevicePairing::new(HOST, false);
        assert!(pairing.ask());
        pairing.on_message(&pending(300), now);
        pairing
    }

    fn code_entry(now: Instant) -> DevicePairing {
        let mut pairing = waiting(now);
        pairing.on_message(&decided(DevicePairOutcome::Approved), now);
        pairing
    }

    #[test]
    fn ask_moves_intro_to_asking() {
        let mut pairing = DevicePairing::new(HOST, false);
        assert_eq!(pairing.kind(), PhaseKind::Intro);
        assert_eq!(pairing.title(), "Pair with box.tail1234.ts.net");
        assert_eq!(pairing.line(), "box.tail1234.ts.net does not know this PC.");
        assert_eq!(pairing.buttons(), [DeviceButton::Ask, DeviceButton::Close]);
        assert_eq!(pairing.primary(), Some(DeviceButton::Ask));
        assert!(!pairing.cancels_request());

        assert!(pairing.ask());
        assert_eq!(pairing.kind(), PhaseKind::Asking);
        assert_eq!(pairing.line(), "Asking box.tail1234.ts.net…");
        assert_eq!(pairing.buttons(), [DeviceButton::Cancel]);
        assert_eq!(pairing.primary(), None);
        assert!(pairing.cancels_request());
        assert!(!pairing.ask(), "a request already out is not asked again");

        let stale = DevicePairing::new(HOST, true);
        assert_eq!(
            stale.line(),
            "box.tail1234.ts.net no longer accepts this PC's pairing."
        );
    }

    #[test]
    fn pending_moves_asking_to_waiting_with_its_countdown() {
        let now = Instant::now();
        let mut intro = DevicePairing::new(HOST, false);
        intro.on_message(&pending(300), now);
        assert_eq!(intro.kind(), PhaseKind::Intro, "nothing was asked");

        let pairing = waiting(now);
        assert_eq!(pairing.kind(), PhaseKind::Waiting);
        assert_eq!(pairing.line(), "Approve this PC on box.tail1234.ts.net.");
        assert_eq!(pairing.countdown(now).as_deref(), Some("expires in 5:00"));
        assert_eq!(
            pairing.countdown(now + Duration::from_secs(61)).as_deref(),
            Some("expires in 3:59")
        );
        assert_eq!(pairing.buttons(), [DeviceButton::Cancel]);
        assert!(pairing.cancels_request());

        let late = now + Duration::from_secs(400);
        assert_eq!(pairing.countdown(late).as_deref(), Some("expires in 0:00"));
        assert_eq!(
            pairing.kind(),
            PhaseKind::Waiting,
            "a countdown at zero ends nothing by itself"
        );
    }

    #[test]
    fn approved_enables_code_entry_and_restarts_the_code_countdown() {
        let now = Instant::now();
        let mut pairing = code_entry(now);
        assert_eq!(pairing.kind(), PhaseKind::CodeEntry);
        assert_eq!(pairing.line(), "Type the code box.tail1234.ts.net shows.");
        assert_eq!(
            pairing.countdown(now).as_deref(),
            Some("code expires in 2:00")
        );
        assert_eq!(
            pairing.countdown(now + Duration::from_secs(30)).as_deref(),
            Some("code expires in 1:30")
        );
        assert_eq!(
            pairing.buttons(),
            [DeviceButton::Pair, DeviceButton::Cancel]
        );
        assert!(!pairing.can_pair());
        assert_eq!(pairing.code(), Some(""));
        assert!(pairing.cancels_request());

        let later = now + Duration::from_secs(90);
        pairing.type_code("12");
        pairing.on_message(&decided(DevicePairOutcome::Approved), later);
        assert_eq!(pairing.kind(), PhaseKind::CodeEntry);
        assert_eq!(
            pairing.countdown(later).as_deref(),
            Some("code expires in 2:00"),
            "a later approval restarts the code's countdown"
        );
        assert_eq!(pairing.code(), Some("12"), "and keeps what was typed");
    }

    #[test]
    fn code_must_be_six_digits() {
        let now = Instant::now();
        let mut pairing = code_entry(now);
        assert_eq!(pairing.type_code("12a3"), "123");
        assert_eq!(pairing.code(), Some("123"));
        assert!(!pairing.can_pair());
        assert_eq!(pairing.primary(), None);
        assert_eq!(pairing.take_code(), None, "three digits are not a code");
        assert_eq!(pairing.code(), Some("123"));

        assert_eq!(pairing.type_code("12345678"), "123456");
        assert!(pairing.can_pair());
        assert_eq!(pairing.primary(), Some(DeviceButton::Pair));
        assert!(
            !format!("{pairing:?}").contains("123456"),
            "the typed code never prints"
        );
        assert_eq!(pairing.take_code().as_deref(), Some("123456"));
        assert_eq!(pairing.code(), Some(""), "the field clears on Pair");
        assert!(!pairing.can_pair());

        let mut asking = DevicePairing::new(HOST, false);
        assert!(asking.ask());
        assert_eq!(
            asking.type_code("123456"),
            "",
            "no field before the approval"
        );
        assert_eq!(asking.take_code(), None);
    }

    #[test]
    fn wrong_code_keeps_entry_and_shows_reason() {
        let now = Instant::now();
        let mut pairing = code_entry(now);
        pairing.type_code("123456");
        assert!(pairing.take_code().is_some());
        pairing.on_message(&failed("That code is not right."), now);
        assert_eq!(pairing.kind(), PhaseKind::CodeEntry);
        assert_eq!(pairing.refusal(), Some("That code is not right."));

        pairing.on_message(
            &failed("Too many wrong codes; ask again from the device."),
            now,
        );
        assert_eq!(
            pairing.refusal(),
            Some("Too many wrong codes; ask again from the device."),
            "the last refusal shows"
        );
        pairing.type_code("1");
        assert_eq!(pairing.refusal(), None, "a keystroke clears the refusal");
        assert_eq!(pairing.kind(), PhaseKind::CodeEntry);
    }

    #[test]
    fn denied_expired_superseded_end_with_their_lines() {
        let now = Instant::now();
        let cases = [
            (
                DevicePairOutcome::Denied,
                "box.tail1234.ts.net turned this PC down.",
            ),
            (DevicePairOutcome::Expired, "The request expired."),
            (
                DevicePairOutcome::Superseded,
                "A newer request from this PC replaced this one.",
            ),
            (DevicePairOutcome::Unknown, "The request ended."),
        ];
        for (outcome, line) in cases {
            for mut pairing in [waiting(now), code_entry(now)] {
                pairing.on_message(&decided(outcome), now);
                assert_eq!(pairing.kind(), PhaseKind::Ended, "{outcome:?}");
                assert_eq!(pairing.line(), line);
                assert_eq!(pairing.countdown(now), None);
                assert_eq!(
                    pairing.buttons(),
                    [DeviceButton::AskAgain, DeviceButton::Close]
                );
                assert_eq!(pairing.primary(), Some(DeviceButton::AskAgain));
                assert!(!pairing.cancels_request());
            }
        }

        let mut asking = DevicePairing::new(HOST, false);
        assert!(asking.ask());
        asking.on_message(&failed("remote access is off"), now);
        assert_eq!(asking.kind(), PhaseKind::Ended);
        assert_eq!(asking.line(), "remote access is off");

        let mut pairing = waiting(now);
        pairing.on_message(&failed("too many requests on this connection"), now);
        assert_eq!(pairing.kind(), PhaseKind::Ended);
        assert_eq!(pairing.line(), "too many requests on this connection");
    }

    #[test]
    fn pairing_lost_and_token_save_failed_end_with_their_lines() {
        let now = Instant::now();
        let mut lost = code_entry(now);
        lost.on_lost();
        assert_eq!(lost.kind(), PhaseKind::Ended);
        assert_eq!(
            lost.line(),
            "The connection to box.tail1234.ts.net dropped, and the request with it."
        );
        assert_eq!(
            lost.buttons(),
            [DeviceButton::AskAgain, DeviceButton::Close]
        );

        let mut waiting_lost = waiting(now);
        waiting_lost.on_lost();
        assert_eq!(waiting_lost.kind(), PhaseKind::Ended);

        let mut intro = DevicePairing::new(HOST, false);
        intro.on_lost();
        assert_eq!(intro.kind(), PhaseKind::Intro, "no request was out to lose");

        let mut unsaved = code_entry(now);
        unsaved.on_token_save_failed();
        assert_eq!(unsaved.kind(), PhaseKind::Ended);
        assert_eq!(
            unsaved.line(),
            "Could not save this device's token; pair again."
        );
        assert_eq!(
            unsaved.buttons(),
            [DeviceButton::AskAgain, DeviceButton::Close]
        );
    }

    #[test]
    fn the_not_approved_signal_is_ignored() {
        let now = Instant::now();
        let signal = failed(DEVICE_NOT_APPROVED);

        let mut intro = DevicePairing::new(HOST, false);
        intro.on_message(&signal, now);
        assert_eq!(intro.kind(), PhaseKind::Intro);

        let mut asking = DevicePairing::new(HOST, false);
        assert!(asking.ask());
        asking.on_message(&signal, now);
        assert_eq!(asking.kind(), PhaseKind::Asking);

        let mut pairing = waiting(now);
        pairing.on_message(&signal, now);
        assert_eq!(pairing.kind(), PhaseKind::Waiting);

        let mut entry = code_entry(now);
        entry.on_message(&signal, now);
        assert_eq!(entry.kind(), PhaseKind::CodeEntry);
        assert_eq!(entry.refusal(), None);
    }

    #[test]
    fn not_approved_after_the_request_went_out_ends_as_not_accepted() {
        let now = Instant::now();
        let mut pairing = DevicePairing::new(HOST, false);
        assert!(pairing.ask());
        pairing.on_request_sent();
        pairing.on_message(&failed(DEVICE_NOT_APPROVED), now);
        assert_eq!(pairing.kind(), PhaseKind::Ended);
        assert_eq!(
            pairing.line(),
            "box.tail1234.ts.net did not accept this PC's request."
        );
        assert_eq!(
            pairing.buttons(),
            [DeviceButton::AskAgain, DeviceButton::Close]
        );

        assert!(pairing.ask(), "Ask again");
        pairing.on_message(&failed(DEVICE_NOT_APPROVED), now);
        assert_eq!(
            pairing.kind(),
            PhaseKind::Asking,
            "a fresh ask is not sent until the net thread says so"
        );
    }

    #[test]
    fn not_approved_before_the_request_went_out_changes_nothing() {
        let now = Instant::now();
        let mut pairing = DevicePairing::new(HOST, false);
        assert!(pairing.ask());
        pairing.on_message(&failed(DEVICE_NOT_APPROVED), now);
        assert_eq!(pairing.kind(), PhaseKind::Asking, "the redial's greeting");
        assert_eq!(pairing.line(), "Asking box.tail1234.ts.net…");
    }

    #[test]
    fn not_approved_outside_asking_changes_nothing() {
        let now = Instant::now();
        let signal = failed(DEVICE_NOT_APPROVED);

        let mut intro = DevicePairing::new(HOST, false);
        intro.on_request_sent();
        intro.on_message(&signal, now);
        assert_eq!(intro.kind(), PhaseKind::Intro);

        let mut pairing = waiting(now);
        pairing.on_request_sent();
        pairing.on_message(&signal, now);
        assert_eq!(pairing.kind(), PhaseKind::Waiting);

        let mut entry = code_entry(now);
        entry.on_request_sent();
        entry.on_message(&signal, now);
        assert_eq!(entry.kind(), PhaseKind::CodeEntry);
        assert_eq!(entry.refusal(), None);

        let mut ended = waiting(now);
        ended.on_message(&decided(DevicePairOutcome::Denied), now);
        ended.on_request_sent();
        ended.on_message(&signal, now);
        assert_eq!(ended.line(), "box.tail1234.ts.net turned this PC down.");
    }

    #[test]
    fn ask_again_from_ended_asks() {
        let now = Instant::now();
        let mut pairing = waiting(now);
        pairing.on_message(&decided(DevicePairOutcome::Denied), now);
        assert_eq!(pairing.kind(), PhaseKind::Ended);

        assert!(pairing.ask());
        assert_eq!(pairing.kind(), PhaseKind::Asking);
        assert_eq!(pairing.line(), "Asking box.tail1234.ts.net…");
        pairing.on_message(&pending(300), now);
        let entry = {
            pairing.on_message(&decided(DevicePairOutcome::Approved), now);
            pairing
        };
        assert_eq!(entry.kind(), PhaseKind::CodeEntry);
        assert_eq!(entry.refusal(), None);
    }
}
