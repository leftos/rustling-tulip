//! The Settings modal's Remote access page as plain data: the lines it
//! shows for the daemon's last `RemoteStatus`, the port field's check, the
//! `ConfigureRemote` each control sends, and the seam over the daemon's
//! login launch.

use std::fmt;

use protocol::{ClientMessage, RemoteState};

/// Seconds in a day, for the certificate's age.
const DAY_SECS: u64 = 86_400;

/// The daemon's last `RemoteStatus`: the persisted switch, the configured
/// port and what the tailnet listener is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteStatusView {
    pub enabled: bool,
    pub port: u16,
    pub state: RemoteState,
}

/// The line under the switch that says what remote access is doing. A
/// `reason` arm prints the daemon's fixed text as it is.
#[must_use]
pub fn state_line(state: &RemoteState, port: u16) -> String {
    match state {
        RemoteState::Off => "Remote access is off.".to_owned(),
        RemoteState::Starting => "Starting…".to_owned(),
        RemoteState::NotInstalled => "Tailscale is not installed on this PC.".to_owned(),
        RemoteState::NotRunning => {
            "Tailscale is not running. Start it and remote access connects on its own.".to_owned()
        }
        RemoteState::OtherWindowsUser => {
            "Tailscale belongs to another Windows user or runs in server mode.".to_owned()
        }
        RemoteState::NotConnected => {
            "Tailscale is signed out or stopped. Sign in to Tailscale to turn remote access on."
                .to_owned()
        }
        RemoteState::TailscaleError { reason }
        | RemoteState::CertFailed { reason }
        | RemoteState::BindFailed { reason } => reason.clone(),
        RemoteState::HttpsDisabled => "HTTPS certificates are off for your tailnet. Enable HTTPS \
                                       in the Tailscale admin console (DNS page)."
            .to_owned(),
        RemoteState::HostTagged => "This PC is tagged in Tailscale, so it has no owner to admit. \
                                    Remote access stays down."
            .to_owned(),
        RemoteState::PortInUse => {
            format!("Port {port} is in use on this PC's tailnet address. Choose another port.")
        }
        RemoteState::Serving { .. } => "Serving on your tailnet.".to_owned(),
        RemoteState::Unknown => {
            "Remote access reported a state this version does not know.".to_owned()
        }
    }
}

/// The address other devices reach this PC at, while it serves.
#[must_use]
pub fn address(state: &RemoteState) -> Option<String> {
    match state {
        RemoteState::Serving { name, port, .. } => Some(format!("https://{name}:{port}")),
        _ => None,
    }
}

/// How long the serving certificate has left at `now_unix`, in whole days;
/// `None` when its expiry is not known.
#[must_use]
pub fn cert_line(expires_at: Option<u64>, now_unix: u64) -> Option<String> {
    let expires_at = expires_at?;
    if expires_at <= now_unix {
        return Some("Certificate expired".to_owned());
    }
    let line = match (expires_at - now_unix) / DAY_SECS {
        0 => "Certificate expires today".to_owned(),
        1 => "Certificate expires in 1 day".to_owned(),
        days => format!("Certificate expires in {days} days"),
    };
    Some(line)
}

/// The port field holds no port from 1 to 65535.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortError;

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Enter a port from 1 to 65535.")
    }
}

impl std::error::Error for PortError {}

/// The port in `text`, trimmed: ASCII digits only, from 1 to 65535.
///
/// # Errors
///
/// [`PortError`] for an empty field, a sign, any other character, zero, or
/// a number past 65535.
pub fn parse_port(text: &str) -> Result<u16, PortError> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PortError);
    }
    match text.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(PortError),
    }
}

/// The switch's press: the opposite of the persisted switch, the port kept.
#[must_use]
pub fn toggle_message(status: &RemoteStatusView) -> ClientMessage {
    ClientMessage::ConfigureRemote {
        enabled: !status.enabled,
        port: None,
    }
}

/// Apply's press: the port in `draft` with the switch as it is, only when
/// the draft is a valid port other than the configured one.
#[must_use]
pub fn apply_port_message(status: &RemoteStatusView, draft: &str) -> Option<ClientMessage> {
    let port = parse_port(draft).ok()?;
    (port != status.port).then_some(ClientMessage::ConfigureRemote {
        enabled: status.enabled,
        port: Some(port),
    })
}

/// Whether the switch and the port field act: only once the daemon has
/// reported a status on this connection.
#[must_use]
pub fn controls_enabled(status: Option<&RemoteStatusView>) -> bool {
    status.is_some()
}

/// The line shown when turning the login launch on or off failed.
#[must_use]
pub fn autostart_failure_line(error: &str) -> String {
    format!("Could not change start on login: {error}.")
}

/// The line shown when reading whether the daemon starts on login failed.
#[must_use]
pub fn autostart_read_failure_line(error: &str) -> String {
    format!("Could not read start on login: {error}.")
}

/// Whether this PC starts the daemon when the user logs in, and the switch
/// for it. The client runs with [`SystemAutostart`]; specs use a recorder.
pub trait Autostart {
    /// Whether the login launch is registered.
    ///
    /// # Errors
    ///
    /// The reason the registration could not be read.
    fn get(&self) -> Result<bool, String>;

    /// Registers (`true`) or removes the login launch.
    ///
    /// # Errors
    ///
    /// The reason the registration could not be written or removed.
    fn set(&self, enabled: bool) -> Result<(), String>;
}

/// The login launch of this PC's daemon, through `daemon_client::autostart`.
pub struct SystemAutostart;

impl Autostart for SystemAutostart {
    fn get(&self) -> Result<bool, String> {
        Ok(daemon_client::autostart::get())
    }

    fn set(&self, enabled: bool) -> Result<(), String> {
        daemon_client::autostart::set(enabled).map_err(|err| format!("{err:#}"))
    }
}

#[cfg(test)]
mod tests {
    use protocol::{ClientMessage, RemoteState};

    use super::{
        PortError, RemoteStatusView, address, apply_port_message, cert_line, controls_enabled,
        parse_port, state_line, toggle_message,
    };

    const DAY: u64 = 86_400;
    const NOW: u64 = 1_800_000_000;

    fn status(enabled: bool, port: u16, state: RemoteState) -> RemoteStatusView {
        RemoteStatusView {
            enabled,
            port,
            state,
        }
    }

    #[test]
    fn state_line_for_every_remote_state() {
        let reason = |text: &str| text.to_owned();
        let cases = [
            (RemoteState::Off, "Remote access is off.".to_owned()),
            (RemoteState::Starting, "Starting…".to_owned()),
            (
                RemoteState::NotInstalled,
                "Tailscale is not installed on this PC.".to_owned(),
            ),
            (
                RemoteState::NotRunning,
                "Tailscale is not running. Start it and remote access connects on its own."
                    .to_owned(),
            ),
            (
                RemoteState::OtherWindowsUser,
                "Tailscale belongs to another Windows user or runs in server mode.".to_owned(),
            ),
            (
                RemoteState::NotConnected,
                "Tailscale is signed out or stopped. Sign in to Tailscale to turn remote access on."
                    .to_owned(),
            ),
            (
                RemoteState::TailscaleError {
                    reason: reason("Tailscale timed out; see the daemon log."),
                },
                reason("Tailscale timed out; see the daemon log."),
            ),
            (
                RemoteState::HttpsDisabled,
                "HTTPS certificates are off for your tailnet. Enable HTTPS in the Tailscale \
                 admin console (DNS page)."
                    .to_owned(),
            ),
            (
                RemoteState::HostTagged,
                "This PC is tagged in Tailscale, so it has no owner to admit. Remote access \
                 stays down."
                    .to_owned(),
            ),
            (
                RemoteState::CertFailed {
                    reason: reason("The certificate could not be fetched."),
                },
                reason("The certificate could not be fetched."),
            ),
            (
                RemoteState::PortInUse,
                "Port 9000 is in use on this PC's tailnet address. Choose another port.".to_owned(),
            ),
            (
                RemoteState::BindFailed {
                    reason: reason("Binding the tailnet address failed."),
                },
                reason("Binding the tailnet address failed."),
            ),
            (
                RemoteState::Serving {
                    name: "box.tail1234.ts.net".to_owned(),
                    port: 9000,
                    cert_expires_at: None,
                },
                "Serving on your tailnet.".to_owned(),
            ),
            (
                RemoteState::Unknown,
                "Remote access reported a state this version does not know.".to_owned(),
            ),
        ];
        assert_eq!(cases.len(), 14, "one case per RemoteState arm");
        for (state, want) in cases {
            assert_eq!(state_line(&state, 9000), want, "{state:?}");
        }
    }

    #[test]
    fn serving_names_the_address_and_certificate_age() {
        let serving = RemoteState::Serving {
            name: "box.tail1234.ts.net".to_owned(),
            port: 8787,
            cert_expires_at: Some(NOW + 63 * DAY),
        };
        assert_eq!(
            address(&serving).as_deref(),
            Some("https://box.tail1234.ts.net:8787")
        );
        assert_eq!(address(&RemoteState::Off), None);
        assert_eq!(address(&RemoteState::PortInUse), None);
        assert_eq!(
            cert_line(Some(NOW + 63 * DAY), NOW).as_deref(),
            Some("Certificate expires in 63 days")
        );
        assert_eq!(
            cert_line(Some(NOW + DAY), NOW).as_deref(),
            Some("Certificate expires in 1 day")
        );
        assert_eq!(
            cert_line(Some(NOW + DAY - 1), NOW).as_deref(),
            Some("Certificate expires today")
        );
        assert_eq!(
            cert_line(Some(NOW), NOW).as_deref(),
            Some("Certificate expired")
        );
        assert_eq!(
            cert_line(Some(NOW - DAY), NOW).as_deref(),
            Some("Certificate expired")
        );
        assert_eq!(cert_line(None, NOW), None);
    }

    #[test]
    fn parse_port_accepts_only_1_to_65535() {
        assert_eq!(parse_port("8787"), Ok(8787));
        assert_eq!(parse_port(" 9000 "), Ok(9000));
        assert_eq!(parse_port("1"), Ok(1));
        assert_eq!(parse_port("65535"), Ok(65535));
        for bad in [
            "",
            "   ",
            "0",
            "65536",
            "-1",
            "+80",
            "80a",
            "８０",
            "99999999999",
        ] {
            assert_eq!(parse_port(bad), Err(PortError), "{bad:?}");
        }
        assert_eq!(PortError.to_string(), "Enter a port from 1 to 65535.");
    }

    /// The switch and port a `ConfigureRemote` carries; `None` for any
    /// other message or none.
    fn configure(msg: Option<ClientMessage>) -> Option<(bool, Option<u16>)> {
        match msg? {
            ClientMessage::ConfigureRemote { enabled, port } => Some((enabled, port)),
            _ => None,
        }
    }

    #[test]
    fn toggle_sends_the_opposite_switch_and_keeps_the_port() {
        let off = status(false, 9000, RemoteState::Off);
        assert_eq!(configure(Some(toggle_message(&off))), Some((true, None)));
        let on = status(true, 9000, RemoteState::NotInstalled);
        assert_eq!(configure(Some(toggle_message(&on))), Some((false, None)));
    }

    #[test]
    fn apply_port_keeps_the_switch_and_needs_a_changed_valid_port() {
        let off = status(false, 8787, RemoteState::Off);
        assert_eq!(
            configure(apply_port_message(&off, "9000")),
            Some((false, Some(9000)))
        );
        let on = status(true, 8787, RemoteState::Starting);
        assert_eq!(
            configure(apply_port_message(&on, " 9000 ")),
            Some((true, Some(9000)))
        );
        assert!(apply_port_message(&off, "8787").is_none(), "unchanged");
        assert!(apply_port_message(&off, "0").is_none(), "invalid");
        assert!(apply_port_message(&off, "").is_none(), "empty");
    }

    #[test]
    fn controls_are_disabled_before_the_first_status() {
        assert!(!controls_enabled(None));
        let off = status(false, 8787, RemoteState::Off);
        assert!(controls_enabled(Some(&off)));
    }
}
