//! The host picker's model: the rows it offers (this PC, the saved tailnet
//! and SSH hosts and the owner's online tailnet peers), the sections they
//! are grouped under and the second line and state chip each one shows, the
//! typed-host checks, the add form's two modes and its Test connection
//! states, forgetting a saved host, and the target the client boots onto.
//! Plain Rust: the view draws these rows, the Tailscale scan reaches it
//! through [`PeerSource`] and Test connection through [`SshTester`].

use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tailnet::{PeerStatus, Status, TailnetError};

use crate::connection;
use crate::ssh::DEFAULT_REMOTE_COMMAND;
use crate::target::{DEFAULT_TAILNET_PORT, HostNameError, SshHost, SshHostError, Target};

/// The label of the row that connects to this PC's own daemon.
const THIS_PC: &str = "This PC";

/// How a saved host is reached: `"tailnet"` (the default when a record has
/// no `kind`) or `"ssh"` in `native-ui.json`. An unknown kind fails the
/// record's decode, so it is dropped on load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    #[default]
    Tailnet,
    Ssh,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_tailnet(kind: &HostKind) -> bool {
    *kind == HostKind::Tailnet
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_zero(port: &u16) -> bool {
    *port == 0
}

/// A host the user added, as `native-ui.json` stores it: a tailnet host's
/// full `MagicDNS` name and the port it was added with, or an SSH host's
/// string, exactly as typed, and its optional remote command, with no port.
/// A tailnet record is written without its `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedHost {
    #[serde(default, skip_serializing_if = "is_tailnet")]
    pub kind: HostKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_command: Option<String>,
    /// Whether the window connects to this host at launch; a record
    /// without the key is enabled, and an enabled host is written without
    /// it.
    #[serde(default = "enabled_by_default", skip_serializing_if = "is_enabled")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_enabled(enabled: &bool) -> bool {
    *enabled
}

impl SavedHost {
    /// The record of `target`, enabled, as a switch to it saves it; `None`
    /// for the local daemon.
    #[must_use]
    pub fn from_target(target: &Target) -> Option<Self> {
        match target {
            Target::Local => None,
            Target::Tailnet(host) => Some(Self {
                kind: HostKind::Tailnet,
                name: host.name().to_owned(),
                port: host.port(),
                remote_command: None,
                enabled: true,
            }),
            Target::Ssh(host) => Some(Self {
                kind: HostKind::Ssh,
                name: host.name().to_owned(),
                port: 0,
                remote_command: host.remote_command().map(str::to_owned),
                enabled: true,
            }),
        }
    }

    /// The target this host dials, or why its name or port is not one.
    ///
    /// # Errors
    ///
    /// For a tailnet host, [`TypedHostError::Name`] when the name fails the
    /// DNS-name check, [`TypedHostError::Port`] when the port is 0; for an
    /// SSH host, [`TypedHostError::SshName`] when its string is refused.
    pub fn target(&self) -> Result<Target, TypedHostError> {
        match self.kind {
            HostKind::Tailnet => {
                if self.port == 0 {
                    return Err(TypedHostError::Port);
                }
                Target::parse_tailnet(&self.name, Some(self.port)).map_err(TypedHostError::Name)
            }
            HostKind::Ssh => Target::parse_ssh(&self.name, self.remote_command.as_deref())
                .map_err(TypedHostError::SshName),
        }
    }

    /// Whether this record is `target`'s: the same kind and name.
    #[must_use]
    pub fn is_for(&self, target: &Target) -> bool {
        Self::from_target(target)
            .is_some_and(|host| host.kind == self.kind && host.name == self.name)
    }

    /// This host in its normal form: a tailnet name as
    /// [`Target::parse_tailnet`] keeps it (lower-cased, without a trailing
    /// dot); an SSH host as typed, without a port and without a blank
    /// remote command. Its `enabled` flag is kept.
    ///
    /// # Errors
    ///
    /// As [`SavedHost::target`].
    pub fn normalised(&self) -> Result<Self, TypedHostError> {
        let target = self.target()?;
        let normal = Self::from_target(&target).unwrap_or_else(|| self.clone());
        Ok(Self {
            enabled: self.enabled,
            ..normal
        })
    }
}

/// Where the picker's peer list comes from: `tailscale status --json` in
/// the product, a fake in tests.
pub trait PeerSource: Send + Sync {
    /// The tailnet as this node sees it. Blocks until the answer is in.
    ///
    /// # Errors
    ///
    /// Any [`TailnetError`]; the picker shows its `Display`.
    fn status(&self) -> Result<Status, TailnetError>;
}

/// The system's Tailscale: `tailnet::Cli::locate().status()`, run on a
/// one-shot thread with its own current-thread tokio runtime, so neither
/// the UI's executor nor the network thread waits on it.
pub struct SystemPeers;

impl PeerSource for SystemPeers {
    fn status(&self) -> Result<Status, TailnetError> {
        let scan = std::thread::Builder::new()
            .name("tailnet-status".to_owned())
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| {
                        TailnetError::Failed(format!("starting a runtime for status: {err}"))
                    })?;
                runtime.block_on(tailnet::Cli::locate().status())
            })
            .map_err(|err| TailnetError::Failed(format!("starting the status thread: {err}")))?;
        scan.join().unwrap_or_else(|_| {
            Err(TailnetError::Failed(
                "the status thread panicked".to_owned(),
            ))
        })
    }
}

/// Resolves when a running Test connection is cancelled: its sender sent,
/// or dropped with the form that held it.
pub type SshTestCancel = oneshot::Receiver<()>;

/// Where Test connection runs: `ssh` to the host once in the product, a
/// fake in tests.
pub trait SshTester: Send + Sync {
    /// Runs the host's remote command once and resolves to `Ok` when its
    /// daemon answers `Welcome`, else to the text that says why not. The
    /// child is killed once `cancel` resolves.
    fn test(&self, host: SshHost, cancel: SshTestCancel) -> BoxFuture<'static, Result<(), String>>;
}

/// The system's `ssh`: [`crate::ssh::test_connection`], run on a one-shot
/// thread with its own current-thread tokio runtime, so neither the UI's
/// executor nor the network thread waits on it.
pub struct SystemSsh;

impl SshTester for SystemSsh {
    fn test(&self, host: SshHost, cancel: SshTestCancel) -> BoxFuture<'static, Result<(), String>> {
        let (answer_tx, answer_rx) = oneshot::channel();
        let started = std::thread::Builder::new()
            .name("ssh-test".to_owned())
            .spawn(move || {
                let outcome = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        runtime.block_on(crate::ssh::test_connection(&host, cancel.map(|_| ())))
                    }
                    Err(err) => Err(format!("starting a runtime for the SSH test: {err}")),
                };
                if answer_tx.send(outcome).is_err() {
                    tracing::debug!("the SSH test ended after the picker stopped waiting");
                }
            });
        match started {
            Ok(_) => async move {
                answer_rx.await.unwrap_or_else(|_| {
                    Err("the SSH test thread ended without an answer".to_owned())
                })
            }
            .boxed(),
            Err(err) => {
                futures::future::ready(Err(format!("starting the SSH test thread: {err}"))).boxed()
            }
        }
    }
}

/// What the Add SSH host form's Test connection last did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SshTest {
    /// Not run since the form opened or a field changed.
    #[default]
    Idle,
    /// Running: the button reads "Cancel".
    Testing,
    /// The host's daemon answered; this is the host's short name.
    Passed(String),
    /// It did not; this is why, `ssh`'s own text unchanged.
    Failed(String),
}

/// The line Test connection shows while it runs.
pub const SSH_TESTING: &str = "Testing the connection…";

/// The "On your tailnet" header's note while the scan has not answered.
pub const SCAN_PENDING: &str = "Looking for your devices…";

impl SshTest {
    /// The state a finished test of `host` leaves.
    #[must_use]
    pub fn finished(host: &SshHost, outcome: Result<(), String>) -> Self {
        match outcome {
            Ok(()) => Self::Passed(host.short_name().to_owned()),
            Err(text) => Self::Failed(text),
        }
    }

    /// The Test connection button's label: "Cancel" while it runs.
    #[must_use]
    pub fn button_label(&self) -> &'static str {
        if *self == Self::Testing {
            "Cancel"
        } else {
            "Test connection"
        }
    }

    /// The line under the form; `None` before a test.
    #[must_use]
    pub fn text(&self) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Testing => Some(SSH_TESTING.to_owned()),
            Self::Passed(short) => Some(format!("Connected: {short}")),
            Self::Failed(text) => Some(text.clone()),
        }
    }
}

/// The two kinds of host the add form can add, one at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddMode {
    /// A tailnet host: its Name and Port.
    Tailnet,
    /// An SSH host: its Host and Remote command.
    Ssh,
}

/// One field of the add form, in the mode that shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddField {
    /// The field's label.
    pub label: &'static str,
    /// Whether the field is drawn in the mono face.
    pub mono: bool,
}

/// A button under the add form's fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddButton {
    /// Closes the form, cancelling a running test.
    Cancel,
    /// Runs Test connection; reads "Cancel" while one runs.
    Test,
    /// Adds the host and connects to it.
    Submit,
}

impl AddButton {
    /// The button's text; Test reads "Cancel" while a test runs.
    #[must_use]
    pub fn label(&self, test: &SshTest) -> &'static str {
        match self {
            Self::Cancel => "Cancel",
            Self::Test => test.button_label(),
            Self::Submit => "Add and connect",
        }
    }
}

impl AddMode {
    /// The mode's segment label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tailnet => "Tailnet",
            Self::Ssh => "SSH",
        }
    }

    /// The other mode.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Tailnet => Self::Ssh,
            Self::Ssh => Self::Tailnet,
        }
    }

    /// The mode's fields, top to bottom. Each mode keeps its own text, so a
    /// switch shows that mode's fields as they were left.
    #[must_use]
    pub const fn fields(self) -> [AddField; 2] {
        match self {
            Self::Tailnet => [
                AddField {
                    label: "Name",
                    mono: false,
                },
                AddField {
                    label: "Port",
                    mono: false,
                },
            ],
            Self::Ssh => [
                AddField {
                    label: "Host",
                    mono: false,
                },
                AddField {
                    label: "Remote command",
                    mono: true,
                },
            ],
        }
    }

    /// The form's buttons, left to right; only SSH has a test.
    #[must_use]
    pub fn buttons(self) -> Vec<AddButton> {
        match self {
            Self::Tailnet => vec![AddButton::Cancel, AddButton::Submit],
            Self::Ssh => vec![AddButton::Cancel, AddButton::Test, AddButton::Submit],
        }
    }

    /// Whether switching the form from `self` to `next` cancels a running
    /// Test connection: only leaving SSH with one running does. The view
    /// then drops it and clears the result line.
    #[must_use]
    pub fn switch_cancels_test(self, next: Self, test: &SshTest) -> bool {
        matches!(self, Self::Ssh) && matches!(next, Self::Tailnet) && *test == SshTest::Testing
    }
}

/// The SSH host the user typed into the Add SSH host form: the host string
/// exactly as typed, and the remote command, a blank one being none.
///
/// # Errors
///
/// [`TypedHostError::SshName`] when [`Target::parse_ssh`] refuses the host.
pub fn typed_ssh_target(host: &str, remote_command: &str) -> Result<Target, TypedHostError> {
    Target::parse_ssh(host, Some(remote_command)).map_err(TypedHostError::SshName)
}

/// Which kind of choice a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// This PC's own daemon.
    Local,
    /// A host the user saved.
    Saved,
    /// An online peer from the tailnet scan that is not saved.
    Peer,
}

/// One choice the picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    /// "This PC", else a tailnet host's full `MagicDNS` name or an SSH
    /// host's string.
    pub label: String,
    /// What picking the row connects to: its host's kind and name.
    pub target: Target,
}

impl Row {
    /// Whether the row is an SSH host.
    #[must_use]
    pub fn is_ssh(&self) -> bool {
        matches!(self.target, Target::Ssh(_))
    }

    /// The row's key in its selectors: an SSH host's string after `ssh:`,
    /// so it never shares one with a tailnet host of that name; else its
    /// label.
    #[must_use]
    pub fn key(&self) -> String {
        if self.is_ssh() {
            format!("ssh:{}", self.label)
        } else {
            self.label.clone()
        }
    }

    /// The section the row is listed under.
    #[must_use]
    pub const fn section(&self) -> Section {
        match self.kind {
            RowKind::Local => Section::ThisPc,
            RowKind::Saved => Section::Saved,
            RowKind::Peer => Section::Tailnet,
        }
    }

    /// The row's name as the picker draws it: a tailnet host's machine name
    /// and the greyed suffix from its first dot (`.tail1234.ts.net`); an SSH
    /// host's string and "This PC" whole.
    #[must_use]
    pub fn name_parts(&self) -> (String, Option<String>) {
        match &self.target {
            Target::Tailnet(_) => match self.label.split_once('.') {
                Some((name, suffix)) => (name.to_owned(), Some(format!(".{suffix}"))),
                None => (self.label.clone(), None),
            },
            Target::Local | Target::Ssh(_) => (self.label.clone(), None),
        }
    }
}

/// A section of the picker's list, under its own header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// This PC's own daemon.
    ThisPc,
    /// The hosts the user saved.
    Saved,
    /// The owner's online tailnet peers that are not saved.
    Tailnet,
}

impl Section {
    /// The section's header title.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::ThisPc => THIS_PC,
            Self::Saved => "Saved hosts",
            Self::Tailnet => "On your tailnet",
        }
    }
}

/// One section of the picker's list, with the rows it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerSection {
    /// Which section this is.
    pub section: Section,
    /// The header's right-side note: the Saved section's count, the Tailnet
    /// section's online count or the scan's wait or error; none for This PC.
    pub note: Option<String>,
    /// The rows it holds, as indices into [`ConnectionPicker::rows`].
    pub rows: Vec<usize>,
}

/// What kind of host a row offers, for its second line and state chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowHost {
    /// This PC's own daemon.
    ThisPc,
    /// A saved tailnet host.
    Tailnet,
    /// A saved SSH host.
    Ssh,
    /// An online tailnet peer the user has not saved.
    Peer,
}

/// The connection's live state, mirrored from [`connection::State`] so the
/// rows' lines and chips are plain values with no GPUI in them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveState {
    /// A connect is under way.
    Connecting,
    /// The daemon answered `Hello`.
    Open,
    /// The host has not approved this device.
    NotApproved,
    /// Closed, refused or failed, with the reason to show.
    Down { reason: String },
}

impl LiveState {
    /// What `state` puts on a held host's row; `None` before the first
    /// connect attempt, when there is nothing to report yet.
    #[must_use]
    pub fn from_state(state: &connection::State) -> Option<Self> {
        match state {
            connection::State::Init => None,
            connection::State::Connecting => Some(Self::Connecting),
            connection::State::Open { .. } => Some(Self::Open),
            connection::State::NotApproved { .. } => Some(Self::NotApproved),
            connection::State::Closed { reason }
            | connection::State::AuthFailed { reason }
            | connection::State::Error { reason } => Some(Self::Down {
                reason: reason.clone(),
            }),
        }
    }

    /// The chip this state puts on a held host's row.
    fn row_state(&self) -> RowState {
        match self {
            Self::Connecting => RowState::Connecting,
            Self::Open => RowState::Connected,
            Self::NotApproved => RowState::NeedsPairing,
            Self::Down { reason } => RowState::Disconnected {
                reason: reason.clone(),
            },
        }
    }
}

/// The chip at a row's trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowState {
    /// The row's daemon answered.
    Connected,
    /// The row's host is connecting.
    Connecting,
    /// The row's host has not approved this device.
    NeedsPairing,
    /// A saved tailnet host this PC holds no device token for.
    NotPaired,
    /// The row's host is disconnected, with the reason.
    Disconnected { reason: String },
    /// A saved host switched off.
    Off,
    /// A saved host whose connection reached a daemon another host already
    /// holds, named by that host's label.
    SameDaemonAs { owner: String },
}

impl RowState {
    /// The chip's text.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Connected => "Connected".to_owned(),
            Self::Connecting => "Connecting…".to_owned(),
            Self::NeedsPairing => "Needs pairing".to_owned(),
            Self::NotPaired => "Not paired".to_owned(),
            Self::Disconnected { .. } => "Disconnected".to_owned(),
            Self::Off => "Off".to_owned(),
            Self::SameDaemonAs { owner } => format!("Same daemon as {owner}"),
        }
    }
}

/// What the picker knows about one row when it draws its second line,
/// state chip and buttons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowFacts {
    /// What kind of host the row offers.
    pub host: RowHost,
    /// A tailnet row's port: the one it was saved with, or the default for a
    /// peer.
    pub port: Option<u16>,
    /// An SSH row's remote command, as saved.
    pub remote_command: Option<String>,
    /// Whether a saved host is switched on; true for every other row.
    pub enabled: bool,
    /// Whether the window holds a connection to the row's host.
    pub held: bool,
    /// The live state of the window's connection to the row's host.
    pub live: Option<LiveState>,
    /// The label of the host whose connection reached the row's daemon
    /// first, when the row's own was dropped as its duplicate.
    pub duplicate_of: Option<String>,
    /// Whether this PC holds a device token for a saved tailnet host.
    pub has_token: bool,
    /// The sessions the row's host last had, from `host_session_counts`.
    pub sessions_last_time: Option<usize>,
    /// The live session count of the row's host, while the window holds
    /// its connection.
    pub live_sessions: Option<usize>,
}

/// Whether the row draws a Pair button: a saved tailnet host the window
/// holds that has not approved this PC, or that this PC holds no device
/// token for.
#[must_use]
pub fn offers_pair(facts: &RowFacts) -> bool {
    facts.host == RowHost::Tailnet
        && facts.held
        && (facts.live == Some(LiveState::NotApproved) || !facts.has_token)
}

/// Whether the row draws Reconnect now: a saved host the window holds
/// whose connection is not open.
#[must_use]
pub fn offers_reconnect(facts: &RowFacts) -> bool {
    matches!(facts.host, RowHost::Tailnet | RowHost::Ssh)
        && facts.held
        && facts.live != Some(LiveState::Open)
}

/// A row's second line and the chip at its trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowDetail {
    /// The line under the row's name, its parts joined by ` · `.
    pub line: String,
    /// The chip at the row's trail, when it has one.
    pub state: Option<RowState>,
}

/// The second line and state chip of a row, from what the picker knows.
#[must_use]
pub fn row_detail(facts: &RowFacts) -> RowDetail {
    RowDetail {
        line: detail_line(facts),
        state: detail_state(facts),
    }
}

/// The row's second line: its kind, then what the picker knows about it.
fn detail_line(facts: &RowFacts) -> String {
    let mut parts: Vec<String> = Vec::new();
    match facts.host {
        RowHost::ThisPc => {
            parts.push("This PC's daemon".to_owned());
            parts.extend(session_count(facts.live_sessions, false));
        }
        RowHost::Tailnet => {
            parts.push("Tailnet".to_owned());
            parts.extend(port_clause(facts.port));
            parts.extend(saved_host_sessions(facts));
        }
        RowHost::Ssh => {
            parts.push("SSH".to_owned());
            parts.push(
                facts
                    .remote_command
                    .as_deref()
                    .unwrap_or(DEFAULT_REMOTE_COMMAND)
                    .to_owned(),
            );
            parts.extend(saved_host_sessions(facts));
        }
        RowHost::Peer => {
            parts.push("Online".to_owned());
            parts.extend(port_clause(Some(
                facts.port.unwrap_or(DEFAULT_TAILNET_PORT),
            )));
            parts.push("connecting saves it".to_owned());
        }
    }
    parts.join(" · ")
}

/// The "port <n>" part of a tailnet row.
fn port_clause(port: Option<u16>) -> Option<String> {
    port.map(|port| format!("port {port}"))
}

/// The sessions part of a saved host's row: its live count while the
/// window holds its connection, else the count it had last time.
fn saved_host_sessions(facts: &RowFacts) -> Option<String> {
    if facts.held {
        session_count(facts.live_sessions, false)
    } else {
        session_count(facts.sessions_last_time, true)
    }
}

/// The sessions part of a row: "3 sessions", or "3 sessions last time" for a
/// host the window does not hold. None is left out, as is zero.
fn session_count(count: Option<usize>, last_time: bool) -> Option<String> {
    let count = count.filter(|count| *count > 0)?;
    let noun = if count == 1 { "session" } else { "sessions" };
    Some(if last_time {
        format!("{count} {noun} last time")
    } else {
        format!("{count} {noun}")
    })
}

/// The chip a row shows: Off for a saved host switched off, "Same daemon
/// as" for one dropped as a duplicate, else the live state of the window's
/// connection to it, else "Not paired" for a saved tailnet host this PC
/// holds no token for.
fn detail_state(facts: &RowFacts) -> Option<RowState> {
    if !facts.enabled {
        return Some(RowState::Off);
    }
    if let Some(owner) = &facts.duplicate_of {
        return Some(RowState::SameDaemonAs {
            owner: owner.clone(),
        });
    }
    if let Some(live) = &facts.live {
        return Some(live.row_state());
    }
    match facts.host {
        RowHost::Tailnet if !facts.has_token => Some(RowState::NotPaired),
        RowHost::ThisPc | RowHost::Tailnet | RowHost::Ssh | RowHost::Peer => None,
    }
}

/// Why a typed host was refused: its `Display` is the message the picker
/// shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedHostError {
    /// The name failed the DNS-name check.
    Name(HostNameError),
    /// The SSH host string failed its check.
    SshName(SshHostError),
    /// The port is not a number from 1 to 65535.
    Port,
}

impl std::fmt::Display for TypedHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name(err) => err.fmt(f),
            Self::SshName(err) => err.fmt(f),
            Self::Port => f.write_str("The port is a number from 1 to 65535."),
        }
    }
}

/// The host the user typed into the name and Port fields. The name is
/// checked first; the port's surrounding whitespace is ignored.
///
/// # Errors
///
/// [`TypedHostError::Name`] when the name fails [`Target::parse_tailnet`],
/// else [`TypedHostError::Port`] when the port is not 1 to 65535.
pub fn typed_target(name: &str, port: &str) -> Result<Target, TypedHostError> {
    Target::parse_tailnet(name, None).map_err(TypedHostError::Name)?;
    let port = port
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(TypedHostError::Port)?;
    Target::parse_tailnet(name, Some(port)).map_err(TypedHostError::Name)
}

/// The last tailnet scan's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scan {
    /// No scan has answered yet.
    Pending,
    /// The offered peers' full names, sorted.
    Peers(Vec<String>),
    /// The scan failed with this text.
    Failed(String),
}

/// The picker's state: the saved hosts and the last scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionPicker {
    saved: Vec<SavedHost>,
    scan: Scan,
}

impl ConnectionPicker {
    /// A picker over `saved`, in saved order, before any scan.
    #[must_use]
    pub fn new(saved: Vec<SavedHost>) -> Self {
        Self {
            saved,
            scan: Scan::Pending,
        }
    }

    #[must_use]
    pub fn saved(&self) -> &[SavedHost] {
        &self.saved
    }

    /// Takes a scan's answer: the owner's online, untagged, unshared peers,
    /// or the error's text.
    pub fn apply_scan(&mut self, scan: Result<Status, TailnetError>) {
        self.scan = match scan {
            Ok(status) => Scan::Peers(offered_peers(&status)),
            Err(err) => Scan::Failed(err.to_string()),
        };
    }

    /// The last scan's error text; `None` when it worked or has not answered.
    #[must_use]
    pub fn scan_error(&self) -> Option<&str> {
        match &self.scan {
            Scan::Failed(text) => Some(text),
            Scan::Pending | Scan::Peers(_) => None,
        }
    }

    /// This PC, then the saved hosts in saved order, then the scanned peers
    /// not already saved as tailnet hosts, by name. A peer dials
    /// [`DEFAULT_TAILNET_PORT`]; a saved tailnet host of the same name is
    /// listed once, with its own port.
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = vec![Row {
            kind: RowKind::Local,
            label: THIS_PC.to_owned(),
            target: Target::Local,
        }];
        rows.extend(self.saved.iter().filter_map(|host| {
            let target = host.target().ok()?;
            Some(Row {
                kind: RowKind::Saved,
                label: host.name.clone(),
                target,
            })
        }));
        if let Scan::Peers(peers) = &self.scan {
            rows.extend(
                peers
                    .iter()
                    .filter(|name| {
                        !self
                            .saved
                            .iter()
                            .any(|host| host.kind == HostKind::Tailnet && &host.name == *name)
                    })
                    .map(|name| Row {
                        kind: RowKind::Peer,
                        label: name.clone(),
                        target: Target::tailnet(name, Some(DEFAULT_TAILNET_PORT)),
                    }),
            );
        }
        rows
    }

    /// The rows grouped under their sections, in draw order: This PC, the
    /// saved hosts, then the peers. A section with no rows is left out,
    /// apart from "On your tailnet", which stays while the scan has not
    /// answered or has failed.
    #[must_use]
    pub fn sections(&self) -> Vec<PickerSection> {
        let rows = self.rows();
        let mut this_pc = Vec::new();
        let mut saved = Vec::new();
        let mut tailnet = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            match row.section() {
                Section::ThisPc => this_pc.push(index),
                Section::Saved => saved.push(index),
                Section::Tailnet => tailnet.push(index),
            }
        }
        let mut sections = vec![PickerSection {
            section: Section::ThisPc,
            note: None,
            rows: this_pc,
        }];
        if !saved.is_empty() {
            sections.push(PickerSection {
                section: Section::Saved,
                note: Some(saved.len().to_string()),
                rows: saved,
            });
        }
        if let Some(note) = self.tailnet_note(tailnet.len()) {
            sections.push(PickerSection {
                section: Section::Tailnet,
                note: Some(note),
                rows: tailnet,
            });
        }
        sections
    }

    /// The "On your tailnet" header's note, or `None` when the section is
    /// left out because the scan answered with no peer to offer.
    fn tailnet_note(&self, online: usize) -> Option<String> {
        match &self.scan {
            Scan::Pending => Some(SCAN_PENDING.to_owned()),
            Scan::Failed(text) => Some(text.clone()),
            Scan::Peers(_) if online == 0 => None,
            Scan::Peers(_) => Some(format!("{online} online")),
        }
    }

    /// Whether the scan has not answered yet.
    #[must_use]
    pub fn scan_pending(&self) -> bool {
        self.scan == Scan::Pending
    }

    /// Saves `host` after the others; a saved host of its kind and name is
    /// replaced by it, in its place, so a new port or remote command is
    /// kept, and so is the saved record's `enabled` flag. A host of the
    /// other kind and the same name is left alone.
    pub fn remember(&mut self, host: SavedHost) {
        match self
            .saved
            .iter_mut()
            .find(|saved| saved.kind == host.kind && saved.name == host.name)
        {
            Some(saved) => {
                *saved = SavedHost {
                    enabled: saved.enabled,
                    ..host
                };
            }
            None => self.saved.push(host),
        }
    }

    /// Whether the saved host `target` dials is switched on; true for a
    /// target no saved host dials.
    #[must_use]
    pub fn enabled(&self, target: &Target) -> bool {
        self.saved
            .iter()
            .find(|host| host.is_for(target))
            .is_none_or(|host| host.enabled)
    }

    /// Switches the saved host `target` dials on or off. Returns whether a
    /// saved host matched.
    pub fn set_enabled(&mut self, target: &Target, enabled: bool) -> bool {
        let mut matched = false;
        for host in self.saved.iter_mut().filter(|host| host.is_for(target)) {
            host.enabled = enabled;
            matched = true;
        }
        matched
    }

    /// Forgets the saved host `target` dials.
    /// A tailnet name matches in its normal form, an SSH string as typed; a
    /// host of the other kind and the same name is left alone.
    pub fn forget(&mut self, target: &Target) {
        self.saved.retain(|host| !host.is_for(target));
    }
}

/// The full names of the peers the picker offers from `status`: online,
/// untagged, not shared in, and logged in as this node's own user, each
/// through the DNS-name check. Without this node's own entry the owner is
/// unknown and none is offered.
fn offered_peers(status: &Status) -> Vec<String> {
    let Some(me) = &status.self_node else {
        tracing::warn!("tailscale status has no entry for this PC; no peer is offered");
        return Vec::new();
    };
    let mut names: Vec<String> = status
        .peer
        .values()
        .filter(|peer| is_owner_device(peer, me))
        .filter_map(|peer| match Target::parse_tailnet(&peer.dns_name, None) {
            Ok(target) => target.tailnet_host().map(|host| host.name().to_owned()),
            Err(err) => {
                tracing::warn!(dns_name = %peer.dns_name, "a peer's DNS name is refused ({err})");
                None
            }
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Whether `peer` is one of the owner's own devices that can serve: online,
/// untagged, not shared in, and logged in as `me`'s user.
fn is_owner_device(peer: &PeerStatus, me: &PeerStatus) -> bool {
    peer.online && peer.tags.is_empty() && !peer.sharee_node && peer.user_id == me.user_id
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        AddButton, AddMode, ConnectionPicker, HostKind, LiveState, PeerSource, Row, RowFacts,
        RowHost, RowKind, RowState, SCAN_PENDING, SSH_TESTING, SavedHost, Section, SshTest,
        offers_pair, offers_reconnect, row_detail, typed_ssh_target, typed_target,
    };
    use crate::target::{DEFAULT_TAILNET_PORT, Target};
    use tailnet::{PeerStatus, Status, TailnetError};

    const OWNER: i64 = 42;
    const GENERIC: &str = "That is not a host name: use letters, digits, hyphens and dots.";
    const BARE: &str = "Use the full name, for example box.tail1234.ts.net.";
    const PORT_IN_NAME: &str = "Put the port in the Port field, not in the name.";
    const PORT_RANGE: &str = "The port is a number from 1 to 65535.";

    /// This PC's own node.
    fn me() -> PeerStatus {
        PeerStatus {
            dns_name: "this-pc.tail1234.ts.net.".to_owned(),
            user_id: OWNER,
            online: true,
            ..PeerStatus::default()
        }
    }

    /// An online, untagged, unshared peer of the owner named `name`, with
    /// the trailing dot `status --json` gives it.
    fn peer(name: &str) -> PeerStatus {
        PeerStatus {
            dns_name: format!("{name}."),
            user_id: OWNER,
            online: true,
            ..PeerStatus::default()
        }
    }

    fn status(peers: Vec<PeerStatus>) -> Status {
        Status {
            self_node: Some(me()),
            peer: peers
                .into_iter()
                .enumerate()
                .map(|(i, peer)| (format!("nodekey:{i}"), peer))
                .collect(),
        }
    }

    fn saved(name: &str, port: u16) -> SavedHost {
        SavedHost {
            kind: HostKind::Tailnet,
            name: name.to_owned(),
            port,
            remote_command: None,
            enabled: true,
        }
    }

    fn row(kind: RowKind, name: &str, port: u16) -> Row {
        Row {
            kind,
            label: name.to_owned(),
            target: Target::tailnet(name, Some(port)),
        }
    }

    fn this_pc() -> Row {
        Row {
            kind: RowKind::Local,
            label: "This PC".to_owned(),
            target: Target::Local,
        }
    }

    /// The names of the peer rows `peers` give.
    fn peer_names(peers: Vec<PeerStatus>) -> Vec<String> {
        let mut picker = ConnectionPicker::new(Vec::new());
        picker.apply_scan(Ok(status(peers)));
        picker
            .rows()
            .into_iter()
            .filter(|row| row.kind == RowKind::Peer)
            .map(|row| row.label)
            .collect()
    }

    struct FailingSource(TailnetError);

    impl PeerSource for FailingSource {
        fn status(&self) -> Result<Status, TailnetError> {
            Err(self.0.clone())
        }
    }

    #[test]
    fn rows_list_local_then_saved_then_online_peers() {
        let mut picker = ConnectionPicker::new(vec![
            saved("zed.tail1234.ts.net", 8787),
            saved("alpha.tail1234.ts.net", 9443),
        ]);
        assert_eq!(
            picker.rows(),
            [
                this_pc(),
                row(RowKind::Saved, "zed.tail1234.ts.net", 8787),
                row(RowKind::Saved, "alpha.tail1234.ts.net", 9443),
            ],
            "before the scan: this PC and the saved hosts in saved order"
        );

        picker.apply_scan(Ok(status(vec![
            peer("mid.tail1234.ts.net"),
            peer("beta.tail1234.ts.net"),
            peer("zed.tail1234.ts.net"),
        ])));
        assert_eq!(
            picker.rows(),
            [
                this_pc(),
                row(RowKind::Saved, "zed.tail1234.ts.net", 8787),
                row(RowKind::Saved, "alpha.tail1234.ts.net", 9443),
                row(RowKind::Peer, "beta.tail1234.ts.net", 8787),
                row(RowKind::Peer, "mid.tail1234.ts.net", 8787),
            ],
            "then the peers not already saved, by DNS name"
        );
        assert_eq!(picker.scan_error(), None);
    }

    #[test]
    fn peer_rows_use_the_dns_name_without_the_trailing_dot() {
        let mut picker = ConnectionPicker::new(Vec::new());
        picker.apply_scan(Ok(status(vec![peer("Box.Tail1234.ts.net")])));
        assert_eq!(
            picker.rows(),
            [this_pc(), row(RowKind::Peer, "box.tail1234.ts.net", 8787)]
        );
        assert_eq!(
            peer_names(vec![peer(""), peer("box/ws.tail1234.ts.net")]),
            [] as [String; 0],
            "a peer whose DNS name fails the check is not offered"
        );
    }

    #[test]
    fn offline_tagged_and_shared_peers_are_not_offered() {
        let offline = PeerStatus {
            online: false,
            ..peer("offline.tail1234.ts.net")
        };
        let tagged = PeerStatus {
            tags: vec!["tag:server".to_owned()],
            ..peer("tagged.tail1234.ts.net")
        };
        let shared = PeerStatus {
            sharee_node: true,
            ..peer("shared.tail1234.ts.net")
        };
        let stranger = PeerStatus {
            user_id: OWNER + 1,
            ..peer("stranger.tail1234.ts.net")
        };
        assert_eq!(
            peer_names(vec![
                offline,
                tagged,
                shared,
                stranger,
                peer("ok.tail1234.ts.net")
            ]),
            ["ok.tail1234.ts.net"]
        );

        let mut picker = ConnectionPicker::new(Vec::new());
        picker.apply_scan(Ok(Status {
            self_node: None,
            ..status(vec![peer("ok.tail1234.ts.net")])
        }));
        assert_eq!(
            picker.rows(),
            [this_pc()],
            "without this PC's own node the owner is unknown: no peer is offered"
        );
    }

    #[test]
    fn a_saved_port_wins_for_a_listed_peer_of_that_name() {
        let mut picker = ConnectionPicker::new(vec![saved("box.tail1234.ts.net", 9443)]);
        picker.apply_scan(Ok(status(vec![peer("Box.tail1234.ts.net")])));
        assert_eq!(
            picker.rows(),
            [this_pc(), row(RowKind::Saved, "box.tail1234.ts.net", 9443)],
            "one row for the host, dialling the port it was saved with"
        );
    }

    #[test]
    fn typed_name_goes_through_the_dns_check_and_names_the_refusal() {
        let refusal = |name: &str| typed_target(name, "8787").map_err(|err| err.to_string());
        assert_eq!(refusal("box.tail1234.ts.net/ws"), Err(GENERIC.to_owned()));
        assert_eq!(refusal("user@box.tail1234.ts.net"), Err(GENERIC.to_owned()));
        assert_eq!(refusal("box"), Err(BARE.to_owned()));
        assert_eq!(
            refusal("box.tail1234.ts.net:8787"),
            Err(PORT_IN_NAME.to_owned())
        );
        assert_eq!(
            typed_target("Box.Tail1234.ts.net.", "9443"),
            Ok(Target::tailnet("box.tail1234.ts.net", Some(9443)))
        );
        assert_eq!(
            typed_target("box", "0").map_err(|err| err.to_string()),
            Err(BARE.to_owned()),
            "the name is checked first"
        );
    }

    #[test]
    fn typed_port_outside_1_to_65535_is_refused() {
        for port in ["0", "65536", "", "port", "-1", "8787.5"] {
            assert_eq!(
                typed_target("box.tail1234.ts.net", port).map_err(|err| err.to_string()),
                Err(PORT_RANGE.to_owned()),
                "{port:?}"
            );
        }
        assert_eq!(
            typed_target("box.tail1234.ts.net", "1"),
            Ok(Target::tailnet("box.tail1234.ts.net", Some(1)))
        );
        assert_eq!(
            typed_target("box.tail1234.ts.net", " 65535 "),
            Ok(Target::tailnet("box.tail1234.ts.net", Some(65535)))
        );
    }

    #[test]
    fn forget_removes_a_saved_host() {
        let mut picker = ConnectionPicker::new(vec![
            saved("a.tail1234.ts.net", 8787),
            saved("b.tail1234.ts.net", 9443),
        ]);
        picker.forget(&Target::tailnet("a.tail1234.ts.net", Some(8787)));
        assert_eq!(picker.saved(), [saved("b.tail1234.ts.net", 9443)]);
        assert_eq!(
            picker.rows(),
            [this_pc(), row(RowKind::Saved, "b.tail1234.ts.net", 9443)]
        );

        picker.forget(&Target::tailnet("c.tail1234.ts.net", Some(8787)));
        assert_eq!(
            picker.saved(),
            [saved("b.tail1234.ts.net", 9443)],
            "an unknown host forgets nothing"
        );

        picker.forget(&Target::tailnet("b.tail1234.ts.net", Some(9443)));
        assert_eq!(picker.saved(), [] as [SavedHost; 0]);
    }

    #[test]
    fn forget_matches_a_normalised_name() {
        let mut picker = ConnectionPicker::new(vec![saved("box.tail1234.ts.net", 9443)]);
        picker.forget(&Target::tailnet("Box.Tail1234.ts.net.", Some(9443)));
        assert_eq!(
            picker.saved(),
            [] as [SavedHost; 0],
            "the host was forgotten under another spelling of its name"
        );
    }

    #[test]
    fn remember_adds_a_host_once_and_updates_its_port() {
        let mut picker = ConnectionPicker::new(vec![saved("a.tail1234.ts.net", 8787)]);
        picker.remember(saved("b.tail1234.ts.net", 9443));
        assert_eq!(
            picker.saved(),
            [
                saved("a.tail1234.ts.net", 8787),
                saved("b.tail1234.ts.net", 9443)
            ],
            "a new host goes last"
        );

        picker.remember(saved("b.tail1234.ts.net", 9443));
        assert_eq!(
            picker.saved(),
            [
                saved("a.tail1234.ts.net", 8787),
                saved("b.tail1234.ts.net", 9443)
            ],
            "the same name and port changes nothing"
        );

        picker.remember(saved("a.tail1234.ts.net", 1234));
        assert_eq!(
            picker.saved(),
            [
                saved("a.tail1234.ts.net", 1234),
                saved("b.tail1234.ts.net", 9443)
            ],
            "a saved name takes the new port, in its place"
        );
    }

    #[test]
    fn tailscale_errors_show_their_display_text() {
        let mut picker = ConnectionPicker::new(vec![saved("box.tail1234.ts.net", 8787)]);
        picker.apply_scan(FailingSource(TailnetError::NotRunning).status());
        assert_eq!(
            picker.scan_error(),
            Some("the tailscale CLI could not reach tailscaled (not running?)")
        );
        assert_eq!(
            picker.rows(),
            [this_pc(), row(RowKind::Saved, "box.tail1234.ts.net", 8787)],
            "this PC and the saved hosts are still offered"
        );

        picker.apply_scan(Ok(status(Vec::new())));
        assert_eq!(picker.scan_error(), None, "a scan that works clears it");
    }

    const BOX: &str = "box.tail1234.ts.net";

    fn ssh_saved(name: &str, remote_command: Option<&str>) -> SavedHost {
        SavedHost {
            kind: HostKind::Ssh,
            name: name.to_owned(),
            port: 0,
            remote_command: remote_command.map(str::to_owned),
            enabled: true,
        }
    }

    fn ssh(name: &str) -> Target {
        Target::parse_ssh(name, None).expect("an SSH host string")
    }

    fn ssh_row(name: &str) -> Row {
        Row {
            kind: RowKind::Saved,
            label: name.to_owned(),
            target: ssh(name),
        }
    }

    #[test]
    fn rows_list_a_saved_ssh_host_in_saved_order_keyed_apart() {
        let picker = ConnectionPicker::new(vec![
            ssh_saved("me@devbox", None),
            saved("a.tail1234.ts.net", 8787),
        ]);
        let rows = picker.rows();
        assert_eq!(
            rows,
            [
                this_pc(),
                ssh_row("me@devbox"),
                row(RowKind::Saved, "a.tail1234.ts.net", 8787),
            ]
        );
        assert_eq!(
            rows.iter().map(Row::key).collect::<Vec<_>>(),
            ["This PC", "ssh:me@devbox", "a.tail1234.ts.net"]
        );
    }

    #[test]
    fn an_ssh_host_does_not_hide_a_tailnet_peer_of_the_same_name() {
        let mut picker = ConnectionPicker::new(vec![ssh_saved(BOX, None)]);
        picker.apply_scan(Ok(status(vec![peer(BOX)])));
        assert_eq!(
            picker.rows(),
            [this_pc(), ssh_row(BOX), row(RowKind::Peer, BOX, 8787)]
        );
    }

    #[test]
    fn a_saved_host_without_enabled_loads_enabled() {
        let host: SavedHost =
            serde_json::from_value(serde_json::json!({ "name": BOX, "port": 8787 }))
                .expect("a host record");
        assert!(host.enabled);
        let off: SavedHost = serde_json::from_value(
            serde_json::json!({ "name": BOX, "port": 8787, "enabled": false }),
        )
        .expect("a host record");
        assert!(!off.enabled);
    }

    #[test]
    fn remembering_a_known_host_keeps_its_enabled_flag() {
        let mut picker = ConnectionPicker::new(vec![SavedHost {
            enabled: false,
            ..saved(BOX, 8787)
        }]);
        picker.remember(saved(BOX, 9443));
        assert_eq!(
            picker.saved(),
            [SavedHost {
                enabled: false,
                ..saved(BOX, 9443)
            }],
            "the new port is kept, and the host stays off"
        );
        picker.remember(saved("new.tail1234.ts.net", 8787));
        assert!(picker.saved()[1].enabled, "a new host is enabled");
    }

    #[test]
    fn remember_replaces_the_record_of_the_same_kind_and_name() {
        let mut picker = ConnectionPicker::new(vec![ssh_saved("devbox", Some("old.cmd"))]);
        picker.remember(ssh_saved("devbox", Some("new.cmd")));
        assert_eq!(
            picker.saved(),
            [ssh_saved("devbox", Some("new.cmd"))],
            "a changed remote command is kept"
        );
        picker.remember(ssh_saved("devbox", None));
        assert_eq!(picker.saved(), [ssh_saved("devbox", None)]);
    }

    #[test]
    fn remember_keeps_an_ssh_and_a_tailnet_host_of_one_name_apart() {
        let mut picker = ConnectionPicker::new(vec![saved(BOX, 8787)]);
        picker.remember(ssh_saved(BOX, None));
        assert_eq!(picker.saved(), [saved(BOX, 8787), ssh_saved(BOX, None)]);
        picker.remember(saved(BOX, 9443));
        assert_eq!(
            picker.saved(),
            [saved(BOX, 9443), ssh_saved(BOX, None)],
            "the tailnet record takes the port; the SSH one is untouched"
        );
    }

    #[test]
    fn forget_an_ssh_host_matches_its_string_as_typed() {
        let mut picker = ConnectionPicker::new(vec![
            ssh_saved("Me@DevBox", None),
            ssh_saved("me@devbox", None),
        ]);
        picker.forget(&ssh("Me@DevBox"));
        assert_eq!(picker.saved(), [ssh_saved("me@devbox", None)]);
        picker.forget(&ssh("me@devbox"));
        assert_eq!(picker.saved(), [] as [SavedHost; 0]);
    }

    #[test]
    fn forget_leaves_the_other_kind_of_one_name() {
        let tailnet = Target::tailnet(BOX, Some(8787));
        let mut picker = ConnectionPicker::new(vec![saved(BOX, 8787), ssh_saved(BOX, None)]);
        picker.forget(&ssh(BOX));
        assert_eq!(picker.saved(), [saved(BOX, 8787)]);

        let mut picker = ConnectionPicker::new(vec![saved(BOX, 8787), ssh_saved(BOX, None)]);
        picker.forget(&tailnet);
        assert_eq!(picker.saved(), [ssh_saved(BOX, None)]);
    }

    #[test]
    fn ssh_test_states_say_what_the_form_shows() {
        let target = ssh("me@devbox:2222");
        let host = target.ssh_host().expect("an SSH target");
        assert_eq!(SshTest::Idle.text(), None);
        assert_eq!(SshTest::Idle.button_label(), "Test connection");
        assert_eq!(SshTest::Testing.text().as_deref(), Some(SSH_TESTING));
        assert_eq!(SshTest::Testing.button_label(), "Cancel");
        let passed = SshTest::finished(host, Ok(()));
        assert_eq!(passed, SshTest::Passed("devbox".to_owned()));
        assert_eq!(passed.text().as_deref(), Some("Connected: devbox"));
        assert_eq!(passed.button_label(), "Test connection");
        let failed = SshTest::finished(host, Err("Permission denied (publickey).".to_owned()));
        assert_eq!(
            failed.text().as_deref(),
            Some("Permission denied (publickey)."),
            "ssh's text unchanged"
        );
    }

    #[test]
    fn typed_ssh_target_keeps_the_host_as_typed_and_a_blank_command_as_none() {
        assert_eq!(typed_ssh_target("Me@Box", "  "), Ok(ssh("Me@Box")));
        assert_eq!(
            typed_ssh_target("devbox", "rtd.cmd").ok(),
            Target::parse_ssh("devbox", Some("rtd.cmd")).ok()
        );
        let refusal = |host: &str| typed_ssh_target(host, "").map_err(|err| err.to_string());
        assert_eq!(
            refusal(""),
            Err("Type an SSH host: an alias from your SSH config, or user@host.".to_owned())
        );
        assert_eq!(
            refusal("-oProxyCommand=x"),
            Err("An SSH host cannot start with a dash.".to_owned())
        );
    }

    /// Row facts with nothing known about the row beyond its kind.
    fn facts(host: RowHost) -> RowFacts {
        RowFacts {
            host,
            port: None,
            remote_command: None,
            enabled: true,
            held: false,
            live: None,
            duplicate_of: None,
            has_token: true,
            sessions_last_time: None,
            live_sessions: None,
        }
    }

    #[test]
    fn a_held_saved_host_shows_its_live_session_count() {
        let tailnet = RowFacts {
            port: Some(8787),
            held: true,
            sessions_last_time: Some(3),
            live_sessions: Some(4),
            ..facts(RowHost::Tailnet)
        };
        assert_eq!(
            row_detail(&tailnet).line,
            "Tailnet · port 8787 · 4 sessions"
        );
        let ssh = RowFacts {
            held: true,
            sessions_last_time: Some(2),
            live_sessions: Some(1),
            ..facts(RowHost::Ssh)
        };
        assert_eq!(
            row_detail(&ssh).line,
            "SSH · rustling-tulipd bridge · 1 session"
        );
    }

    #[test]
    fn every_held_rows_live_state_wins_over_not_paired() {
        let held = |live: LiveState| RowFacts {
            port: Some(8787),
            held: true,
            live: Some(live),
            has_token: false,
            ..facts(RowHost::Tailnet)
        };
        assert_eq!(
            row_detail(&held(LiveState::Open)).state,
            Some(RowState::Connected)
        );
        assert_eq!(
            row_detail(&held(LiveState::Connecting)).state,
            Some(RowState::Connecting)
        );
        assert_eq!(
            row_detail(&held(LiveState::NotApproved)).state,
            Some(RowState::NeedsPairing)
        );
        let down = row_detail(&held(LiveState::Down {
            reason: "connection reset".to_owned(),
        }));
        assert_eq!(
            down.state,
            Some(RowState::Disconnected {
                reason: "connection reset".to_owned()
            })
        );
        assert_eq!(RowState::Connected.label(), "Connected");
        assert_eq!(RowState::Connecting.label(), "Connecting…");
        assert_eq!(RowState::NeedsPairing.label(), "Needs pairing");
        assert_eq!(RowState::NotPaired.label(), "Not paired");
        assert_eq!(
            down.state.map(|state| state.label()).as_deref(),
            Some("Disconnected")
        );
    }

    #[test]
    fn a_switched_off_host_shows_off_over_everything_else() {
        let off = RowFacts {
            port: Some(8787),
            enabled: false,
            has_token: false,
            sessions_last_time: Some(3),
            duplicate_of: Some("box".to_owned()),
            ..facts(RowHost::Tailnet)
        };
        let detail = row_detail(&off);
        assert_eq!(detail.state, Some(RowState::Off));
        assert_eq!(detail.line, "Tailnet · port 8787 · 3 sessions last time");
        assert_eq!(RowState::Off.label(), "Off");
    }

    #[test]
    fn a_duplicate_shows_same_daemon_as_its_owner() {
        let duplicate = RowFacts {
            duplicate_of: Some("box".to_owned()),
            ..facts(RowHost::Ssh)
        };
        let state = row_detail(&duplicate).state;
        assert_eq!(
            state,
            Some(RowState::SameDaemonAs {
                owner: "box".to_owned()
            })
        );
        assert_eq!(
            state.map(|state| state.label()).as_deref(),
            Some("Same daemon as box")
        );
    }

    #[test]
    fn pair_and_reconnect_are_offered_only_on_held_saved_hosts() {
        let held = |host: RowHost, live: Option<LiveState>, has_token: bool| RowFacts {
            held: true,
            live,
            has_token,
            ..facts(host)
        };
        let down = || {
            Some(LiveState::Down {
                reason: "connection reset".to_owned(),
            })
        };
        assert!(offers_pair(&held(
            RowHost::Tailnet,
            Some(LiveState::NotApproved),
            true
        )));
        assert!(
            offers_pair(&held(RowHost::Tailnet, None, false)),
            "no token"
        );
        assert!(!offers_pair(&held(
            RowHost::Tailnet,
            Some(LiveState::Open),
            true
        )));
        assert!(
            !offers_pair(&held(RowHost::Ssh, Some(LiveState::NotApproved), false)),
            "an SSH host never pairs"
        );
        assert!(!offers_pair(&held(RowHost::ThisPc, None, false)));
        let not_held = RowFacts {
            has_token: false,
            ..facts(RowHost::Tailnet)
        };
        assert!(!offers_pair(&not_held), "a host the window does not hold");

        assert!(offers_reconnect(&held(RowHost::Tailnet, down(), true)));
        assert!(offers_reconnect(&held(
            RowHost::Ssh,
            Some(LiveState::Connecting),
            true
        )));
        assert!(!offers_reconnect(&held(
            RowHost::Ssh,
            Some(LiveState::Open),
            true
        )));
        assert!(!offers_reconnect(&held(RowHost::ThisPc, down(), true)));
        assert!(!offers_reconnect(&facts(RowHost::Tailnet)), "not held");
    }

    #[test]
    fn set_enabled_switches_a_saved_host() {
        let mut picker = ConnectionPicker::new(vec![saved(BOX, 8787)]);
        let target = Target::tailnet(BOX, Some(8787));
        assert!(picker.enabled(&target));
        assert!(picker.set_enabled(&target, false));
        assert!(!picker.saved()[0].enabled);
        assert!(!picker.enabled(&target));
        assert!(
            !picker.set_enabled(&Target::tailnet("other.tail1234.ts.net", None), true),
            "no saved host matched"
        );
        assert!(
            picker.enabled(&Target::Local),
            "no saved host dials This PC"
        );
    }

    /// The sections the picker draws, in draw order.
    fn section_ids(picker: &ConnectionPicker) -> Vec<Section> {
        picker
            .sections()
            .iter()
            .map(|group| group.section)
            .collect()
    }

    /// A section's header note.
    fn section_note(picker: &ConnectionPicker, section: Section) -> Option<String> {
        picker
            .sections()
            .into_iter()
            .find(|group| group.section == section)
            .and_then(|group| group.note)
    }

    /// A section's rows, as indices into `rows()`.
    fn section_rows(picker: &ConnectionPicker, section: Section) -> Vec<usize> {
        picker
            .sections()
            .into_iter()
            .find(|group| group.section == section)
            .map_or_else(Vec::new, |group| group.rows)
    }

    #[test]
    fn sections_title_and_order() {
        let picker = ConnectionPicker::new(vec![saved(BOX, 8787), ssh_saved("me@devbox", None)]);
        let sections = picker.sections();
        assert_eq!(
            sections
                .iter()
                .map(|group| group.section)
                .collect::<Vec<_>>(),
            [Section::ThisPc, Section::Saved, Section::Tailnet],
            "the scan has not answered, so its section stays to say it is looking"
        );
        assert_eq!(
            sections
                .iter()
                .map(|group| group.section.title())
                .collect::<Vec<_>>(),
            ["This PC", "Saved hosts", "On your tailnet"]
        );
        assert_eq!(
            sections
                .iter()
                .map(|group| group.rows.clone())
                .collect::<Vec<_>>(),
            [vec![0], vec![1, 2], Vec::new()]
        );
    }

    #[test]
    fn saved_section_is_hidden_when_empty() {
        let mut picker = ConnectionPicker::new(Vec::new());
        assert_eq!(
            section_ids(&picker),
            [Section::ThisPc, Section::Tailnet],
            "the scan is pending, so its section stays"
        );
        picker.apply_scan(Ok(status(Vec::new())));
        assert_eq!(
            section_ids(&picker),
            [Section::ThisPc],
            "no saved host and no peer to offer"
        );
    }

    #[test]
    fn tailnet_section_note_counts_online_peers() {
        let mut picker = ConnectionPicker::new(vec![
            saved("a.tail1234.ts.net", 8787),
            saved("b.tail1234.ts.net", 9443),
        ]);
        picker.apply_scan(Ok(status(vec![peer("laptop.tail1234.ts.net")])));
        assert_eq!(section_note(&picker, Section::Saved).as_deref(), Some("2"));
        assert_eq!(
            section_note(&picker, Section::Tailnet).as_deref(),
            Some("1 online")
        );
        assert_eq!(section_rows(&picker, Section::Tailnet), [3]);

        picker.apply_scan(Ok(status(vec![
            peer("laptop.tail1234.ts.net"),
            peer("phone.tail1234.ts.net"),
        ])));
        assert_eq!(
            section_note(&picker, Section::Tailnet).as_deref(),
            Some("2 online")
        );
        assert_eq!(section_rows(&picker, Section::Tailnet), [3, 4]);
    }

    #[test]
    fn tailnet_section_shows_pending_and_error_notes() {
        let mut picker = ConnectionPicker::new(Vec::new());
        assert_eq!(
            section_note(&picker, Section::Tailnet).as_deref(),
            Some(SCAN_PENDING)
        );
        picker.apply_scan(FailingSource(TailnetError::NotRunning).status());
        assert_eq!(
            section_note(&picker, Section::Tailnet).as_deref(),
            Some("the tailscale CLI could not reach tailscaled (not running?)")
        );
    }

    #[test]
    fn tailnet_section_is_hidden_when_the_scan_found_no_unsaved_peer() {
        let mut picker = ConnectionPicker::new(vec![saved(BOX, 8787)]);
        picker.apply_scan(Ok(status(Vec::new())));
        assert_eq!(section_ids(&picker), [Section::ThisPc, Section::Saved]);
        picker.apply_scan(Ok(status(vec![peer(BOX)])));
        assert_eq!(
            section_ids(&picker),
            [Section::ThisPc, Section::Saved],
            "its only online peer is the saved host"
        );
    }

    #[test]
    fn name_parts_greys_the_tailnet_suffix() {
        assert_eq!(
            row(RowKind::Peer, BOX, 8787).name_parts(),
            ("box".to_owned(), Some(".tail1234.ts.net".to_owned()))
        );
        assert_eq!(
            row(RowKind::Saved, BOX, 9443).name_parts(),
            ("box".to_owned(), Some(".tail1234.ts.net".to_owned()))
        );
        assert_eq!(this_pc().name_parts(), ("This PC".to_owned(), None));
    }

    #[test]
    fn name_parts_keeps_an_ssh_host_whole() {
        assert_eq!(
            ssh_row("me@devbox").name_parts(),
            ("me@devbox".to_owned(), None)
        );
        assert_eq!(
            ssh_row("me@box.example.com").name_parts(),
            ("me@box.example.com".to_owned(), None),
            "a dotted SSH host is not split"
        );
    }

    #[test]
    fn this_pc_detail_line() {
        let held = RowFacts {
            held: true,
            live: Some(LiveState::Open),
            live_sessions: Some(7),
            ..facts(RowHost::ThisPc)
        };
        let detail = row_detail(&held);
        assert_eq!(detail.line, "This PC's daemon · 7 sessions");
        assert_eq!(detail.state, Some(RowState::Connected));
    }

    #[test]
    fn saved_tailnet_detail_line() {
        let unpaired = RowFacts {
            port: Some(8787),
            sessions_last_time: Some(3),
            has_token: false,
            ..facts(RowHost::Tailnet)
        };
        let detail = row_detail(&unpaired);
        assert_eq!(detail.line, "Tailnet · port 8787 · 3 sessions last time");
        assert_eq!(detail.state, Some(RowState::NotPaired));

        let paired = RowFacts {
            port: Some(9443),
            ..facts(RowHost::Tailnet)
        };
        let detail = row_detail(&paired);
        assert_eq!(detail.line, "Tailnet · port 9443", "none last time");
        assert_eq!(
            detail.state, None,
            "a saved host this PC holds a token for has no chip"
        );
    }

    #[test]
    fn ssh_detail_line_falls_back_to_the_bridge_command() {
        let plain = RowFacts {
            sessions_last_time: Some(2),
            ..facts(RowHost::Ssh)
        };
        assert_eq!(
            row_detail(&plain).line,
            "SSH · rustling-tulipd bridge · 2 sessions last time"
        );

        let custom = RowFacts {
            remote_command: Some("rtd.cmd".to_owned()),
            has_token: false,
            ..facts(RowHost::Ssh)
        };
        let detail = row_detail(&custom);
        assert_eq!(detail.line, "SSH · rtd.cmd");
        assert_eq!(detail.state, None, "an SSH host is never not paired");
    }

    #[test]
    fn peer_detail_line() {
        let online = RowFacts {
            port: Some(DEFAULT_TAILNET_PORT),
            ..facts(RowHost::Peer)
        };
        let detail = row_detail(&online);
        assert_eq!(detail.line, "Online · port 8787 · connecting saves it");
        assert_eq!(detail.state, None);
    }

    #[test]
    fn session_counts_are_singular_at_one_and_hidden_at_zero() {
        let tailnet = |sessions_last_time: Option<usize>| RowFacts {
            port: Some(8787),
            sessions_last_time,
            ..facts(RowHost::Tailnet)
        };
        assert_eq!(
            row_detail(&tailnet(Some(1))).line,
            "Tailnet · port 8787 · 1 session last time"
        );
        assert_eq!(
            row_detail(&tailnet(Some(0))).line,
            "Tailnet · port 8787",
            "none last time"
        );
        assert_eq!(
            row_detail(&tailnet(None)).line,
            "Tailnet · port 8787",
            "unknown last time"
        );

        let this_pc_row = |live_sessions: Option<usize>| RowFacts {
            held: true,
            live: Some(LiveState::Open),
            live_sessions,
            ..facts(RowHost::ThisPc)
        };
        assert_eq!(
            row_detail(&this_pc_row(Some(1))).line,
            "This PC's daemon · 1 session"
        );
        assert_eq!(row_detail(&this_pc_row(Some(0))).line, "This PC's daemon");
        assert_eq!(row_detail(&this_pc_row(None)).line, "This PC's daemon");
    }

    #[test]
    fn add_mode_buttons_per_mode() {
        assert_eq!(
            AddMode::Tailnet.buttons(),
            [AddButton::Cancel, AddButton::Submit]
        );
        assert_eq!(
            AddMode::Ssh.buttons(),
            [AddButton::Cancel, AddButton::Test, AddButton::Submit]
        );
        let labels = |mode: AddMode, test: &SshTest| {
            mode.buttons()
                .iter()
                .map(|button| button.label(test))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(AddMode::Tailnet, &SshTest::Idle),
            ["Cancel", "Add and connect"]
        );
        assert_eq!(
            labels(AddMode::Ssh, &SshTest::Idle),
            ["Cancel", "Test connection", "Add and connect"]
        );
        assert_eq!(
            labels(AddMode::Ssh, &SshTest::Testing),
            ["Cancel", "Cancel", "Add and connect"],
            "the test button cancels its own run"
        );
        assert_eq!(AddMode::Tailnet.label(), "Tailnet");
        assert_eq!(AddMode::Ssh.label(), "SSH");
        assert_eq!(AddMode::Tailnet.other(), AddMode::Ssh);
    }

    #[test]
    fn switching_mode_keeps_each_modes_fields_and_cancels_a_running_test() {
        assert!(AddMode::Ssh.switch_cancels_test(AddMode::Tailnet, &SshTest::Testing));
        for test in [
            SshTest::Idle,
            SshTest::Passed("devbox".to_owned()),
            SshTest::Failed("Permission denied (publickey).".to_owned()),
        ] {
            assert!(
                !AddMode::Ssh.switch_cancels_test(AddMode::Tailnet, &test),
                "{test:?}"
            );
        }
        assert!(
            !AddMode::Ssh.switch_cancels_test(AddMode::Ssh, &SshTest::Testing),
            "pressing the mode it is already on"
        );
        assert!(
            !AddMode::Tailnet.switch_cancels_test(AddMode::Ssh, &SshTest::Testing),
            "SSH holds the only test"
        );

        let tailnet = AddMode::Tailnet.fields();
        let ssh = AddMode::Ssh.fields();
        assert_eq!(
            tailnet.iter().map(|field| field.label).collect::<Vec<_>>(),
            ["Name", "Port"]
        );
        assert_eq!(
            ssh.iter().map(|field| field.label).collect::<Vec<_>>(),
            ["Host", "Remote command"]
        );
        assert!(
            ssh.iter().any(|field| field.mono),
            "the remote command is drawn in the mono face"
        );
        assert_ne!(
            tailnet.to_vec(),
            ssh.to_vec(),
            "a switch swaps the field set, it does not share it"
        );
    }
}
