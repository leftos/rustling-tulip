//! Short-code pairing window for LAN discovery.
//!
//! When the host clicks "Pair a device" the daemon opens a time-boxed window
//! holding a freshly-generated numeric code. A discovering laptop — which has
//! already pinned the cert fingerprint from the mDNS TXT record — submits that
//! code to the daemon's `/pair` endpoint over the pinned-TLS LAN channel; a
//! match hands back the auth token, so the long token never has to be typed.
//!
//! Two bounds keep the short code from being brute-forced: a short TTL and a
//! hard cap on wrong attempts (after which the window self-invalidates and the
//! host must generate a new code). With a six-digit space, a five-attempt cap,
//! and a three-minute TTL, the odds of a blind guess landing are negligible.

use crate::secret::constant_time_eq;
use protocol::PairingEndReason;
use rand::Rng as _;
use std::time::{Duration, Instant};

/// How long a pairing code stays valid after [`StartPairing`] before the host
/// must regenerate it.
///
/// [`StartPairing`]: protocol::ClientMessage::StartPairing
pub const PAIRING_TTL: Duration = Duration::from_mins(3);

/// Wrong-code submissions allowed before the window self-invalidates.
const MAX_ATTEMPTS: u8 = 5;

/// Number of decimal digits in a pairing code.
const CODE_DIGITS: u32 = 6;

/// The live pairing window: the code to match, when it expires, and how many
/// wrong attempts have been made so far.
#[derive(Debug)]
pub struct PairingSession {
    code: String,
    expires_at: Instant,
    attempts: u8,
}

impl PairingSession {
    /// Open a new window valid for [`PAIRING_TTL`] from `now`.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            code: generate_code(),
            expires_at: now + PAIRING_TTL,
            attempts: 0,
        }
    }

    /// The code to display to the host user.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }
}

/// Outcome of validating a submitted code against the active window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairOutcome {
    /// The code matched. The caller should hand back the auth token and clear
    /// the window (it is single-use).
    Matched,
    /// No pairing window is open.
    NoSession,
    /// The window's TTL elapsed. The caller should clear it.
    Expired,
    /// Wrong code, attempts remain. The window stays open.
    Wrong,
    /// Wrong code and the attempt cap was reached. The caller should clear the
    /// window.
    TooManyAttempts,
}

impl PairOutcome {
    /// The `PairingEndReason` to broadcast for a terminal outcome, or `None`
    /// when the window stays open (`Wrong`) or never existed (`NoSession`).
    #[must_use]
    pub fn end_reason(self) -> Option<PairingEndReason> {
        match self {
            PairOutcome::Matched => Some(PairingEndReason::Paired),
            PairOutcome::Expired => Some(PairingEndReason::Expired),
            PairOutcome::TooManyAttempts => Some(PairingEndReason::TooManyAttempts),
            PairOutcome::Wrong | PairOutcome::NoSession => None,
        }
    }
}

/// Validate `code` against the optional active window at time `now`, mutating
/// it in place. The window is cleared (`*slot = None`) on every terminal
/// outcome — match, expiry, or attempt-cap — so it is single-use and a guessing
/// client can't keep hammering a stale code.
pub fn validate(slot: &mut Option<PairingSession>, code: &str, now: Instant) -> PairOutcome {
    let Some(session) = slot.as_mut() else {
        return PairOutcome::NoSession;
    };
    if now >= session.expires_at {
        *slot = None;
        return PairOutcome::Expired;
    }
    if constant_time_eq(session.code.as_bytes(), code.trim().as_bytes()) {
        *slot = None;
        return PairOutcome::Matched;
    }
    session.attempts += 1;
    if session.attempts >= MAX_ATTEMPTS {
        *slot = None;
        return PairOutcome::TooManyAttempts;
    }
    PairOutcome::Wrong
}

/// Generate a zero-padded `CODE_DIGITS`-digit decimal code.
fn generate_code() -> String {
    let max = 10u32.pow(CODE_DIGITS);
    let n = rand::thread_rng().gen_range(0..max);
    format!("{n:0width$}", width = CODE_DIGITS as usize)
}

#[cfg(test)]
mod tests {
    use super::{
        CODE_DIGITS, MAX_ATTEMPTS, PAIRING_TTL, PairOutcome, PairingSession, generate_code,
        validate,
    };
    use protocol::PairingEndReason;
    use std::time::{Duration, Instant};

    #[test]
    fn generated_code_is_six_digits() {
        for _ in 0..100 {
            let code = generate_code();
            assert_eq!(code.len(), CODE_DIGITS as usize);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[test]
    fn matching_code_consumes_the_window() {
        let now = Instant::now();
        let session = PairingSession::new(now);
        let code = session.code().to_string();
        let mut slot = Some(session);
        assert_eq!(validate(&mut slot, &code, now), PairOutcome::Matched);
        assert!(slot.is_none(), "a matched window is single-use");
        // A second submit of the same code now finds nothing.
        assert_eq!(validate(&mut slot, &code, now), PairOutcome::NoSession);
    }

    #[test]
    fn matching_code_tolerates_surrounding_whitespace() {
        let now = Instant::now();
        let session = PairingSession::new(now);
        let code = format!("  {}\n", session.code());
        let mut slot = Some(session);
        assert_eq!(validate(&mut slot, &code, now), PairOutcome::Matched);
    }

    #[test]
    fn expired_window_is_cleared() {
        let now = Instant::now();
        let session = PairingSession::new(now);
        let code = session.code().to_string();
        let mut slot = Some(session);
        let later = now + PAIRING_TTL + Duration::from_secs(1);
        assert_eq!(validate(&mut slot, &code, later), PairOutcome::Expired);
        assert!(slot.is_none());
    }

    #[test]
    fn wrong_codes_lock_out_after_the_cap() {
        let now = Instant::now();
        let mut slot = Some(PairingSession::new(now));
        // The real code is six digits; "wrong" can never collide.
        for _ in 0..(MAX_ATTEMPTS - 1) {
            assert_eq!(validate(&mut slot, "wrong", now), PairOutcome::Wrong);
            assert!(slot.is_some(), "window stays open while attempts remain");
        }
        assert_eq!(
            validate(&mut slot, "wrong", now),
            PairOutcome::TooManyAttempts
        );
        assert!(slot.is_none(), "the window self-invalidates at the cap");
    }

    #[test]
    fn end_reason_maps_terminal_outcomes() {
        assert_eq!(
            PairOutcome::Matched.end_reason(),
            Some(PairingEndReason::Paired)
        );
        assert_eq!(
            PairOutcome::Expired.end_reason(),
            Some(PairingEndReason::Expired)
        );
        assert_eq!(
            PairOutcome::TooManyAttempts.end_reason(),
            Some(PairingEndReason::TooManyAttempts)
        );
        assert_eq!(PairOutcome::Wrong.end_reason(), None);
        assert_eq!(PairOutcome::NoSession.end_reason(), None);
    }
}
