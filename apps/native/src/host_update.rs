//! This PC's daemon counting down to an update another device asked for:
//! the seconds left, and the toast's title, text and key.

use std::time::{Duration, Instant};

/// The countdown toast's Cancel button.
pub(crate) const HOST_UPDATE_CANCEL: &str = "host-update-cancel";

/// The Cancel button of the countdown for an update a host asked this app to
/// run; pressing it ends the countdown here and sends nothing.
pub(crate) const CLIENT_UPDATE_CANCEL: &str = "client-update-cancel";

/// What every self-update countdown toast's key starts with.
pub(crate) const CLIENT_UPDATE_TOAST_PREFIX: &str = "client-update-";

/// Whole seconds from `now` to `ends_at`, rounded up; 0 once it passed.
fn seconds_left(ends_at: Instant, now: Instant) -> u32 {
    let left = ends_at.saturating_duration_since(now);
    let whole = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    u32::try_from(whole).unwrap_or(u32::MAX)
}

/// An update This PC's daemon starts when its countdown ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostUpdateCountdown {
    pub(crate) update_id: String,
    requested_by: String,
    version: String,
    ends_at: Instant,
}

impl HostUpdateCountdown {
    /// The countdown `update_id`, `seconds` long from `now`.
    pub(crate) fn scheduled(
        update_id: String,
        requested_by: String,
        version: String,
        seconds: u32,
        now: Instant,
    ) -> Self {
        Self {
            update_id,
            requested_by,
            version,
            ends_at: now
                .checked_add(Duration::from_secs(u64::from(seconds)))
                .unwrap_or(now),
        }
    }

    /// Whole seconds left at `now`, rounded up; 0 once the countdown ended.
    pub(crate) fn seconds_left(&self, now: Instant) -> u32 {
        seconds_left(self.ends_at, now)
    }

    /// The toast's title at `now`.
    pub(crate) fn title(&self, now: Instant) -> String {
        format!("Updating in {} s", self.seconds_left(now))
    }

    /// The toast's text: who asked and the version it updates to.
    pub(crate) fn body(&self) -> String {
        format!(
            "Asked by {}. Updates to {}; your sessions keep running.",
            self.requested_by, self.version
        )
    }
}

/// What every countdown toast's key starts with.
pub(crate) const TOAST_KEY_PREFIX: &str = "host-update-";

/// The key of update `update_id`'s toast.
pub(crate) fn toast_key(update_id: &str) -> String {
    format!("{TOAST_KEY_PREFIX}{update_id}")
}

/// This app's own update, counting down because a host asked it to update
/// itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientUpdateCountdown {
    pub(crate) update_id: String,
    /// The host that asked, as its Connections row names it.
    asked_by: String,
    ends_at: Instant,
}

impl ClientUpdateCountdown {
    /// The countdown `update_id`, `asked_by` naming the host that asked,
    /// `seconds` long from `now`.
    pub(crate) fn scheduled(
        update_id: String,
        asked_by: String,
        seconds: u32,
        now: Instant,
    ) -> Self {
        Self {
            update_id,
            asked_by,
            ends_at: now
                .checked_add(Duration::from_secs(u64::from(seconds)))
                .unwrap_or(now),
        }
    }

    /// Whole seconds left at `now`, rounded up; 0 once the countdown ended.
    pub(crate) fn seconds_left(&self, now: Instant) -> u32 {
        seconds_left(self.ends_at, now)
    }

    /// The toast's title at `now`.
    pub(crate) fn title(&self, now: Instant) -> String {
        format!("Updating this app in {} s", self.seconds_left(now))
    }

    /// The toast's text: who asked, and what the update does.
    pub(crate) fn body(&self) -> String {
        format!(
            "{} asked this app to update. It downloads the newest release and reopens; your sessions keep running.",
            self.asked_by
        )
    }
}

/// The key of this app's self-update toast for `update_id`.
pub(crate) fn client_update_toast_key(update_id: &str) -> String {
    format!("{CLIENT_UPDATE_TOAST_PREFIX}{update_id}")
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{HostUpdateCountdown, toast_key};

    fn countdown(now: Instant) -> HostUpdateCountdown {
        HostUpdateCountdown::scheduled(
            "u1".to_owned(),
            "leftos-macbook".to_owned(),
            "0.12.0-alpha".to_owned(),
            10,
            now,
        )
    }

    #[test]
    fn seconds_left_counts_down_and_stops_at_zero() {
        let t0 = Instant::now();
        let update = countdown(t0);
        assert_eq!(update.seconds_left(t0), 10);
        assert_eq!(update.seconds_left(t0 + Duration::from_secs(3)), 7);
        assert_eq!(update.seconds_left(t0 + Duration::from_millis(9200)), 1);
        assert_eq!(update.seconds_left(t0 + Duration::from_secs(12)), 0);
    }

    #[test]
    fn the_title_and_body_name_the_seconds_who_asked_and_the_version() {
        let t0 = Instant::now();
        let update = countdown(t0);
        assert_eq!(update.title(t0), "Updating in 10 s");
        assert_eq!(update.title(t0 + Duration::from_secs(4)), "Updating in 6 s");
        assert_eq!(
            update.body(),
            "Asked by leftos-macbook. Updates to 0.12.0-alpha; your sessions keep running."
        );
    }

    #[test]
    fn a_toast_key_is_per_update() {
        assert_eq!(toast_key("u1"), "host-update-u1");
        assert_ne!(toast_key("u1"), toast_key("u2"));
    }
}
