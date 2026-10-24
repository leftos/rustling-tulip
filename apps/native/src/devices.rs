//! The approved devices on the Remote access page: the daemon's list in the
//! order the page shows it, each row's status, Block and Forget, and the
//! block confirm with Cancel focused first. The screen is
//! [`crate::devices_view`].

use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use protocol::{ClientMessage, DeviceInfo};

use crate::spawn_form::human_relative_time;

/// What the block confirm says blocking does.
pub(crate) const CONFIRM_BODY: &str =
    "Its live connections end now, and it cannot pair again until you forget it.";
/// A blocked device's tag, in place of its status.
pub(crate) const BLOCKED_TAG: &str = "Blocked";
/// The section's line while no device is approved.
pub(crate) const NO_DEVICES: &str = "No device has paired yet.";

/// The button a device row offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowAction {
    Block,
    Forget,
}

impl RowAction {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Block => "Block",
            Self::Forget => "Forget",
        }
    }

    pub(crate) fn selector(self, device_id: &str) -> String {
        match self {
            Self::Block => format!("settings-device-block-{device_id}"),
            Self::Forget => format!("settings-device-forget-{device_id}"),
        }
    }
}

/// One approved device as its row shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceRow {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) login: String,
    pub(crate) blocked: bool,
    /// How it is connected or when it last was; the Blocked tag for a
    /// blocked device.
    pub(crate) status: String,
}

impl DeviceRow {
    /// Block for a device that may connect, Forget for a blocked one.
    pub(crate) fn action(&self) -> RowAction {
        if self.blocked {
            RowAction::Forget
        } else {
            RowAction::Block
        }
    }
}

/// A button of the block confirm, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmButton {
    Cancel,
    Block,
}

impl ConfirmButton {
    pub(crate) const ALL: [Self; 2] = [Self::Cancel, Self::Block];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Cancel => "Cancel",
            Self::Block => "Block",
        }
    }

    pub(crate) fn selector(self) -> &'static str {
        match self {
            Self::Cancel => "device-block-confirm-cancel",
            Self::Block => "device-block-confirm-block",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Cancel => Self::Block,
            Self::Block => Self::Cancel,
        }
    }
}

/// The open block confirm: the device it would block and the focused
/// button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockConfirm {
    device_id: String,
    name: String,
    focused: ConfirmButton,
}

impl BlockConfirm {
    /// `Block <name>?`
    pub(crate) fn title(&self) -> String {
        format!("Block {}?", self.name)
    }

    pub(crate) fn focused(&self) -> ConfirmButton {
        self.focused
    }

    /// A key while the confirm is open; returns the button it presses. Esc
    /// presses Cancel, Enter the focused button, and Tab and Shift+Tab move
    /// the focus to the other button.
    fn key(&mut self, key: &str) -> Option<ConfirmButton> {
        match key {
            "escape" => Some(ConfirmButton::Cancel),
            "enter" => Some(self.focused),
            "tab" => {
                self.focused = self.focused.other();
                None
            }
            _ => None,
        }
    }
}

/// The approved devices as the daemon last listed them, and the block
/// confirm the owner opened.
#[derive(Debug, Clone, Default)]
pub(crate) struct DevicesState {
    devices: Vec<DeviceInfo>,
    confirm: Option<BlockConfirm>,
}

impl DevicesState {
    /// Folds in the daemon's list, which replaces the last one whole; a
    /// confirm for a device the list no longer carries closes. Returns
    /// whether it did.
    pub(crate) fn on_devices(&mut self, devices: Vec<DeviceInfo>) -> bool {
        self.devices = devices;
        let gone = self.confirm.as_ref().is_some_and(|confirm| {
            !self
                .devices
                .iter()
                .any(|device| device.device_id == confirm.device_id)
        });
        if gone {
            self.confirm = None;
        }
        gone
    }

    /// The rows at `now`: connected devices, then the others by when they
    /// were last seen, newest first and never-seen last, then the blocked
    /// ones; ties by name.
    pub(crate) fn rows(&self, now: DateTime<Utc>) -> Vec<DeviceRow> {
        let mut devices: Vec<&DeviceInfo> = self.devices.iter().collect();
        devices.sort_by(|a, b| row_order(a, b));
        devices
            .into_iter()
            .map(|device| DeviceRow {
                device_id: device.device_id.clone(),
                name: device.name.clone(),
                login: device.login.clone(),
                blocked: device.revoked,
                status: if device.revoked {
                    BLOCKED_TAG.to_owned()
                } else {
                    status_text(device, now)
                },
            })
            .collect()
    }

    /// Block on the row of `device_id`: opens its confirm with Cancel
    /// focused. A device not listed, or already blocked, opens nothing;
    /// returns whether the confirm opened.
    pub(crate) fn open_confirm(&mut self, device_id: &str) -> bool {
        let Some(device) = self
            .devices
            .iter()
            .find(|device| device.device_id == device_id && !device.revoked)
        else {
            return false;
        };
        self.confirm = Some(BlockConfirm {
            device_id: device.device_id.clone(),
            name: device.name.clone(),
            focused: ConfirmButton::Cancel,
        });
        true
    }

    pub(crate) fn confirm(&self) -> Option<&BlockConfirm> {
        self.confirm.as_ref()
    }

    /// A key while the confirm is open; returns the button it presses.
    pub(crate) fn confirm_key(&mut self, key: &str) -> Option<ConfirmButton> {
        self.confirm.as_mut().and_then(|confirm| confirm.key(key))
    }

    /// Presses `button`: the confirm closes, and Block yields the message
    /// that blocks its device. With no confirm open nothing happens.
    pub(crate) fn press(&mut self, button: ConfirmButton) -> Option<ClientMessage> {
        let confirm = self.confirm.take()?;
        (button == ConfirmButton::Block).then_some(ClientMessage::RevokeDevice {
            device_id: confirm.device_id,
        })
    }

    /// Closes the confirm, sending nothing; returns whether one was open.
    pub(crate) fn close_confirm(&mut self) -> bool {
        self.confirm.take().is_some()
    }

    /// Forgets the list and the confirm: another daemon, or a lost one.
    pub(crate) fn reset(&mut self) {
        self.devices.clear();
        self.confirm = None;
    }
}

/// The message that forgets `device_id`.
pub(crate) fn forget_message(device_id: &str) -> ClientMessage {
    ClientMessage::ForgetDevice {
        device_id: device_id.to_owned(),
    }
}

/// A device's status at `now`: its live connections, else when it was last
/// seen, else that it never connected.
fn status_text(device: &DeviceInfo, now: DateTime<Utc>) -> String {
    match (device.connected, device.last_seen) {
        (1, _) => "Connected".to_owned(),
        (count, _) if count > 1 => format!("Connected ({count})"),
        (_, Some(seen)) => format!(
            "Last seen {}",
            human_relative_time(now.timestamp(), seen.timestamp())
        ),
        (_, None) => "Never connected".to_owned(),
    }
}

/// Where a device's row falls on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    Connected,
    Seen,
    Blocked,
}

fn group(device: &DeviceInfo) -> Group {
    if device.revoked {
        Group::Blocked
    } else if device.connected > 0 {
        Group::Connected
    } else {
        Group::Seen
    }
}

fn row_order(a: &DeviceInfo, b: &DeviceInfo) -> Ordering {
    let by_seen = if group(a) == Group::Seen && group(b) == Group::Seen {
        b.last_seen.cmp(&a.last_seen)
    } else {
        Ordering::Equal
    };
    group(a)
        .cmp(&group(b))
        .then(by_seen)
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.device_id.cmp(&b.device_id))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use chrono::{DateTime, TimeDelta, Utc};
    use protocol::{ClientMessage, DeviceInfo};

    use super::{ConfirmButton, DevicesState, RowAction};

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).expect("a valid instant")
    }

    /// A device `id` named `name`, seen `seen_secs_ago` (never when none).
    fn device(id: &str, name: &str, seen_secs_ago: Option<i64>) -> DeviceInfo {
        DeviceInfo {
            device_id: id.to_owned(),
            name: name.to_owned(),
            login: "alice@example.com".to_owned(),
            created_at: now() - TimeDelta::days(30),
            last_seen: seen_secs_ago.map(|secs| now() - TimeDelta::seconds(secs)),
            revoked: false,
            connected: 0,
        }
    }

    fn connected(id: &str, name: &str, count: u32) -> DeviceInfo {
        DeviceInfo {
            connected: count,
            ..device(id, name, Some(5))
        }
    }

    fn blocked(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            revoked: true,
            ..device(id, name, Some(60))
        }
    }

    fn ids(state: &DevicesState) -> Vec<String> {
        state
            .rows(now())
            .into_iter()
            .map(|row| row.device_id)
            .collect()
    }

    fn listing(devices: Vec<DeviceInfo>) -> DevicesState {
        let mut state = DevicesState::default();
        state.on_devices(devices);
        state
    }

    #[test]
    fn rows_order_connected_then_last_seen_then_blocked() {
        let state = listing(vec![
            blocked("b2", "zeta"),
            device("never", "alpha", None),
            device("old", "beta", Some(7_200)),
            blocked("b1", "eta"),
            connected("c2", "tablet", 1),
            device("new", "gamma", Some(120)),
            connected("c1", "phone", 2),
            device("tie-b", "omega", Some(600)),
            device("tie-a", "delta", Some(600)),
        ]);
        assert_eq!(
            ids(&state),
            [
                "c1", "c2", "new", "tie-a", "tie-b", "old", "never", "b1", "b2"
            ]
        );
    }

    #[test]
    fn status_text_counts_connections_and_last_seen() {
        let state = listing(vec![
            connected("one", "a", 1),
            connected("two", "b", 3),
            device("seen", "c", Some(7_200)),
            device("just", "d", Some(10)),
            device("never", "e", None),
        ]);
        let statuses: Vec<(String, String)> = state
            .rows(now())
            .into_iter()
            .map(|row| (row.device_id, row.status))
            .collect();
        let expected = [
            ("one", "Connected"),
            ("two", "Connected (3)"),
            ("just", "Last seen just now"),
            ("seen", "Last seen 2h ago"),
            ("never", "Never connected"),
        ];
        let mut want: Vec<(String, String)> = expected
            .iter()
            .map(|(id, status)| ((*id).to_owned(), (*status).to_owned()))
            .collect();
        want.sort();
        let mut got = statuses;
        got.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn blocked_row_offers_forget_not_block() {
        let state = listing(vec![blocked("b1", "phone"), device("d1", "laptop", None)]);
        let rows = state.rows(now());
        let blocked_row = rows
            .iter()
            .find(|row| row.device_id == "b1")
            .expect("the blocked row");
        assert!(blocked_row.blocked);
        assert_eq!(blocked_row.status, "Blocked", "the tag in place of status");
        assert_eq!(blocked_row.action(), RowAction::Forget);
        let open_row = rows
            .iter()
            .find(|row| row.device_id == "d1")
            .expect("the open row");
        assert!(!open_row.blocked);
        assert_eq!(open_row.action(), RowAction::Block);

        let mut state = state;
        assert!(!state.open_confirm("b1"), "a blocked device has no Block");
        assert!(state.confirm().is_none());
        assert!(!state.open_confirm("gone"), "a device not listed");
        assert!(matches!(
            super::forget_message("b1"),
            ClientMessage::ForgetDevice { device_id } if device_id == "b1"
        ));
    }

    #[test]
    fn confirm_block_yields_revoke_device_for_the_row() {
        let mut state = listing(vec![device("d1", "phone", None), device("d2", "tab", None)]);
        assert!(state.open_confirm("d1"));
        let confirm = state.confirm().expect("the confirm opened");
        assert_eq!(confirm.title(), "Block phone?");
        assert_eq!(confirm.focused(), ConfirmButton::Cancel, "Cancel first");
        assert_eq!(state.confirm_key("tab"), None);
        assert_eq!(
            state.confirm().expect("still open").focused(),
            ConfirmButton::Block
        );
        let pressed = state.confirm_key("enter").expect("Enter presses Block");
        assert_eq!(pressed, ConfirmButton::Block);
        let sent = state.press(pressed);
        assert!(
            matches!(&sent, Some(ClientMessage::RevokeDevice { device_id }) if device_id == "d1"),
            "{sent:?}"
        );
        assert!(state.confirm().is_none(), "the confirm closes");
        assert!(state.press(ConfirmButton::Block).is_none(), "nothing open");
    }

    #[test]
    fn cancel_yields_nothing() {
        let mut state = listing(vec![device("d1", "phone", None)]);
        assert!(state.open_confirm("d1"));
        assert!(state.press(ConfirmButton::Cancel).is_none());
        assert!(state.confirm().is_none());

        assert!(state.open_confirm("d1"));
        assert_eq!(state.confirm_key("escape"), Some(ConfirmButton::Cancel));
        assert_eq!(state.confirm_key("tab"), None);
        assert_eq!(state.confirm_key("tab"), None);
        assert_eq!(
            state.confirm_key("enter"),
            Some(ConfirmButton::Cancel),
            "Tab twice is back on Cancel"
        );
    }

    #[test]
    fn devices_reply_without_the_device_closes_the_confirm() {
        let mut state = listing(vec![device("d1", "phone", None), device("d2", "tab", None)]);
        assert!(state.open_confirm("d1"));
        assert!(
            !state.on_devices(vec![
                device("d1", "phone", Some(3)),
                device("d2", "tab", None)
            ]),
            "still listed"
        );
        assert!(state.confirm().is_some());
        assert!(state.on_devices(vec![device("d2", "tab", None)]));
        assert!(state.confirm().is_none());
    }

    #[test]
    fn fold_replaces_the_list() {
        let mut state = listing(vec![device("d1", "phone", None), device("d2", "tab", None)]);
        assert_eq!(ids(&state), ["d1", "d2"]);
        state.on_devices(vec![device("d3", "laptop", None)]);
        assert_eq!(ids(&state), ["d3"]);
        state.on_devices(Vec::new());
        assert_eq!(ids(&state), Vec::<String>::new());
    }
}
