//! Which daemon the client connects to: the local one, a host on the
//! tailnet by its `MagicDNS` name, or a host reached through `ssh`.

/// The port a host's tailnet listener serves when none is given.
pub const DEFAULT_TAILNET_PORT: u16 = 8787;

/// The daemon the client connects to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Target {
    /// The daemon on this machine, started on demand and reached over
    /// loopback with the `daemon.json` token.
    #[default]
    Local,
    /// A host's tailnet listener, dialled over TLS with this device's token
    /// for that host. Built only by [`Target::tailnet`].
    Tailnet(TailnetHost),
    /// A host reached by running `ssh` with its host string and the remote
    /// command, which bridges to that host's daemon. Built only by
    /// [`Target::parse_ssh`].
    Ssh(SshHost),
}

/// An SSH host: the string handed to `ssh` exactly as typed, and the
/// remote command when it is not the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    /// An `~/.ssh/config` alias or `user@host[:port]`, never lower-cased.
    name: String,
    remote_command: Option<String>,
}

impl SshHost {
    /// The host string handed to `ssh`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The command run on the host instead of the default bridge command.
    #[must_use]
    pub fn remote_command(&self) -> Option<&str> {
        self.remote_command.as_deref()
    }

    /// The host part: without a `user@` and without a `:port`.
    #[must_use]
    pub fn short_name(&self) -> &str {
        let host = self.name.rsplit('@').next().unwrap_or(&self.name);
        host.split(':').next().unwrap_or(host)
    }
}

/// Why an SSH host string was refused: its `Display` is the message the
/// picker shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshHostError {
    /// Nothing was typed, or a `user@` or `@host` with an empty side.
    Empty,
    /// It starts with `-`, which `ssh` would read as an option.
    LeadingDash,
    /// It holds whitespace or a control character.
    Whitespace,
}

impl std::fmt::Display for SshHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Empty => "Type an SSH host: an alias from your SSH config, or user@host.",
            Self::LeadingDash => "An SSH host cannot start with a dash.",
            Self::Whitespace => "An SSH host cannot contain spaces or control characters.",
        })
    }
}

/// A tailnet host's normalised name and its listener's port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetHost {
    /// The full `MagicDNS` name, lower-cased, without a trailing dot.
    name: String,
    port: u16,
}

impl TailnetHost {
    /// The full `MagicDNS` name, lower-cased, without a trailing dot.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The first DNS label of the name.
    #[must_use]
    pub fn short_name(&self) -> &str {
        self.name.split('.').next().unwrap_or(&self.name)
    }

    /// `name:port`.
    #[must_use]
    pub fn address(&self) -> String {
        format!("{}:{}", self.name, self.port)
    }

    /// `wss://{name}:{port}/ws`, the URL the listener is dialled at.
    #[must_use]
    pub fn url(&self) -> String {
        format!("wss://{}/ws", self.address())
    }
}

/// Why a typed host name was refused: its `Display` is the message the
/// picker shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostNameError {
    /// Nothing was typed, or only a dot.
    Empty,
    /// A single label: the listener's certificate is for the full
    /// `MagicDNS` name.
    BareLabel,
    /// A `:` asked for a port.
    Port,
    /// An address literal, in brackets or not.
    Address,
    /// Anything else a host name is not.
    NotAHostName,
}

impl std::fmt::Display for HostNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Empty => "Type a host name.",
            Self::BareLabel => "Use the full name, for example box.tail1234.ts.net.",
            Self::Port => "Put the port in the Port field, not in the name.",
            Self::Address => "Use the host's name, not its address.",
            Self::NotAHostName => "That is not a host name: use letters, digits, hyphens and dots.",
        })
    }
}

/// Whether every label of `name` is digits: an IPv4 address, not a name.
fn is_ipv4_literal(name: &str) -> bool {
    name.split('.')
        .all(|label| !label.is_empty() && label.bytes().all(|b| b.is_ascii_digit()))
}

impl Target {
    /// The tailnet host a user typed: one trailing dot trimmed, then checked
    /// as an ASCII `MagicDNS` name lower-cased label by label; `port`
    /// defaults to [`DEFAULT_TAILNET_PORT`].
    ///
    /// Surrounding whitespace is not trimmed: a name with whitespace anywhere
    /// is refused, as is one carrying a userinfo, a path, a query, a
    /// fragment, a port, a percent escape, an address literal (bracketed, an
    /// IPv4 literal or bare IPv6) or a single label.
    pub fn parse_tailnet(name: &str, port: Option<u16>) -> Result<Self, HostNameError> {
        let name = name.strip_suffix('.').unwrap_or(name);
        if name.trim().is_empty() {
            return Err(HostNameError::Empty);
        }
        if !name.is_ascii() {
            return Err(HostNameError::NotAHostName);
        }
        let name = name.to_ascii_lowercase();
        if name.contains('[') || name.contains(']') || is_ipv4_literal(&name) {
            return Err(HostNameError::Address);
        }
        if name.matches(':').count() >= 2 {
            return Err(HostNameError::Address);
        }
        if name.contains(':') {
            return Err(HostNameError::Port);
        }
        let labels: Vec<&str> = name.split('.').collect();
        let labelled = labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
        if name.len() > 253 || !labelled {
            return Err(HostNameError::NotAHostName);
        }
        if labels.len() < 2 {
            return Err(HostNameError::BareLabel);
        }
        Ok(Self::tailnet(&name, port))
    }

    /// A tailnet host by its `MagicDNS` name, with its trailing dot trimmed and
    /// lower-cased; `port` defaults to [`DEFAULT_TAILNET_PORT`].
    #[must_use]
    pub fn tailnet(name: &str, port: Option<u16>) -> Self {
        Self::Tailnet(TailnetHost {
            name: name.trim_end_matches('.').to_lowercase(),
            port: port.unwrap_or(DEFAULT_TAILNET_PORT),
        })
    }

    /// The SSH host a user typed, with an optional remote command (a blank
    /// one is none). The string is kept exactly as typed; it is refused when
    /// empty, when it starts with `-` (an `ssh` option such as
    /// `-oProxyCommand=…` would run code), when it holds whitespace or a
    /// control character, or when the user or host side of an `@` is empty.
    pub fn parse_ssh(name: &str, remote_command: Option<&str>) -> Result<Self, SshHostError> {
        if name.is_empty() {
            return Err(SshHostError::Empty);
        }
        if name.starts_with('-') {
            return Err(SshHostError::LeadingDash);
        }
        if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(SshHostError::Whitespace);
        }
        if let Some((user, host)) = name.split_once('@')
            && (user.is_empty() || host.is_empty())
        {
            return Err(SshHostError::Empty);
        }
        let remote_command = remote_command
            .filter(|command| !command.trim().is_empty())
            .map(str::to_owned);
        Ok(Self::Ssh(SshHost {
            name: name.to_owned(),
            remote_command,
        }))
    }

    /// Whether this is the local daemon.
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// The tailnet host; `None` for the local daemon and an SSH host.
    #[must_use]
    pub fn tailnet_host(&self) -> Option<&TailnetHost> {
        match self {
            Self::Tailnet(host) => Some(host),
            Self::Local | Self::Ssh(_) => None,
        }
    }

    /// The SSH host; `None` for the local daemon and a tailnet host.
    #[must_use]
    pub fn ssh_host(&self) -> Option<&SshHost> {
        match self {
            Self::Ssh(host) => Some(host),
            Self::Local | Self::Tailnet(_) => None,
        }
    }

    /// The remote host's name: the `MagicDNS` name of a tailnet host, the
    /// host string of an SSH host; `None` for the local daemon.
    #[must_use]
    pub fn remote_name(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Tailnet(host) => Some(host.name()),
            Self::Ssh(host) => Some(host.name()),
        }
    }

    /// The host's stable key: a tailnet host's `MagicDNS` name, an SSH
    /// host's string after `ssh:` so the two never share a key; `None` for
    /// the local daemon. A remote host's last session count is recorded
    /// under it.
    #[must_use]
    pub fn host_key(&self) -> Option<String> {
        match HostKey::of(self) {
            HostKey::Local => None,
            HostKey::Remote(key) => Some(key),
        }
    }

    /// The host's short name: a tailnet host's first DNS label, an SSH
    /// host's host part; `None` for the local daemon.
    #[must_use]
    pub fn short_name(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Tailnet(host) => Some(host.short_name()),
            Self::Ssh(host) => Some(host.short_name()),
        }
    }

    /// The host's address: a tailnet host's `name:port`, an SSH host's
    /// string; `None` for the local daemon.
    #[must_use]
    pub fn address(&self) -> Option<String> {
        match self {
            Self::Local => None,
            Self::Tailnet(host) => Some(host.address()),
            Self::Ssh(host) => Some(host.name().to_owned()),
        }
    }
}

/// Which host a connection, a message or a session belongs to: this PC's
/// daemon, or a remote host by its [`Target::host_key`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum HostKey {
    Local,
    Remote(String),
}

impl HostKey {
    /// The key of `target`'s host.
    #[must_use]
    pub fn of(target: &Target) -> Self {
        match target {
            Target::Local => Self::Local,
            Target::Tailnet(host) => Self::Remote(host.name().to_owned()),
            Target::Ssh(host) => Self::Remote(format!("ssh:{}", host.name())),
        }
    }

    /// The key as the wire's `host` value; `None` for this PC.
    #[must_use]
    pub fn wire(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Remote(key) => Some(key),
        }
    }
}

#[cfg(test)]
mod host_key_tests {
    use super::{HostKey, Target};

    #[test]
    fn host_key_of_local_is_local() {
        assert_eq!(HostKey::of(&Target::Local), HostKey::Local);
        assert_eq!(Target::Local.host_key(), None);
    }

    #[test]
    fn host_key_of_a_tailnet_target_is_its_name() {
        let target = Target::tailnet("box.tail1234.ts.net", Some(9443));
        assert_eq!(
            HostKey::of(&target),
            HostKey::Remote("box.tail1234.ts.net".to_owned())
        );
        assert_eq!(target.host_key().as_deref(), Some("box.tail1234.ts.net"));
        assert_eq!(HostKey::of(&target).wire(), Some("box.tail1234.ts.net"));
    }

    #[test]
    fn host_key_of_an_ssh_target_is_prefixed() {
        let target = Target::parse_ssh("me@devbox", None).ok();
        let key = target.as_ref().map(HostKey::of);
        assert_eq!(key, Some(HostKey::Remote("ssh:me@devbox".to_owned())));
        assert_eq!(key.as_ref().and_then(HostKey::wire), Some("ssh:me@devbox"));
    }

    #[test]
    fn wire_is_none_for_local() {
        assert_eq!(HostKey::Local.wire(), None);
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic,
        reason = "a name accepted or refused against the spec fails it with what happened"
    )]

    use super::{DEFAULT_TAILNET_PORT, SshHost, SshHostError, TailnetHost, Target};

    const EMPTY: &str = "Type a host name.";
    const BARE: &str = "Use the full name, for example box.tail1234.ts.net.";
    const PORT: &str = "Put the port in the Port field, not in the name.";
    const ADDRESS: &str = "Use the host's name, not its address.";
    const GENERIC: &str = "That is not a host name: use letters, digits, hyphens and dots.";

    /// The target `name` was accepted as.
    fn parsed(name: &str, port: Option<u16>) -> Target {
        let Ok(target) = Target::parse_tailnet(name, port) else {
            panic!("{name:?} was refused");
        };
        target
    }

    /// The message `name` was refused with.
    fn refusal(name: &str) -> String {
        let Err(err) = Target::parse_tailnet(name, None) else {
            panic!("{name:?} was accepted");
        };
        err.to_string()
    }

    /// A valid four-label name whose last label is `last` characters: 253
    /// characters when `last` is 61, one too many when it is 62.
    fn long_name(last: usize) -> String {
        format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(last)
        )
    }

    /// The URL a target's tailnet host is dialled at.
    fn url(target: &Target) -> Option<String> {
        target.tailnet_host().map(TailnetHost::url)
    }

    #[test]
    fn tailnet_host_is_trimmed_and_lowercased() {
        let target = Target::tailnet("Box.Tail1234.TS.net.", Some(8787));
        let host = target.tailnet_host();
        assert_eq!(host.map(TailnetHost::name), Some("box.tail1234.ts.net"));
        assert_eq!(host.map(TailnetHost::port), Some(8787));
        assert_eq!(target, Target::tailnet("box.tail1234.ts.net", Some(8787)));
    }

    #[test]
    fn tailnet_short_name_is_first_label() {
        assert_eq!(
            Target::tailnet("box.tail1234.ts.net", None).short_name(),
            Some("box")
        );
        assert_eq!(Target::tailnet("box", None).short_name(), Some("box"));
        assert_eq!(Target::Local.short_name(), None);
    }

    #[test]
    fn tailnet_url_is_wss_with_port() {
        assert_eq!(
            url(&Target::tailnet("box.tail1234.ts.net.", Some(9443))),
            Some("wss://box.tail1234.ts.net:9443/ws".to_owned())
        );
        assert_eq!(
            Target::tailnet("box.tail1234.ts.net", Some(9443)).address(),
            Some("box.tail1234.ts.net:9443".to_owned())
        );
        assert_eq!(url(&Target::Local), None);
        assert_eq!(Target::Local.address(), None);
    }

    #[test]
    fn tailnet_port_defaults_to_8787() {
        assert_eq!(DEFAULT_TAILNET_PORT, 8787);
        assert_eq!(
            url(&Target::tailnet("box.tail1234.ts.net", None)),
            Some("wss://box.tail1234.ts.net:8787/ws".to_owned())
        );
    }

    #[test]
    fn parse_tailnet_accepts_a_magicdns_name() {
        let target = parsed("box.tail1234.ts.net", None);
        assert_eq!(target, Target::tailnet("box.tail1234.ts.net", None));
        assert_eq!(
            url(&target),
            Some("wss://box.tail1234.ts.net:8787/ws".to_owned())
        );
        assert_eq!(
            parsed("box.tail1234.ts.net", Some(9443)),
            Target::tailnet("box.tail1234.ts.net", Some(9443))
        );
    }

    #[test]
    fn parse_tailnet_trims_one_trailing_dot_and_lowercases() {
        let target = parsed("Box.Tail1234.TS.net.", Some(9443));
        assert_eq!(
            target.tailnet_host().map(TailnetHost::name),
            Some("box.tail1234.ts.net")
        );
        assert_eq!(target, Target::tailnet("box.tail1234.ts.net", Some(9443)));
    }

    #[test]
    fn parse_tailnet_refuses_userinfo() {
        assert_eq!(refusal("user@box.tail1234.ts.net"), GENERIC);
        assert_eq!(refusal("box.tail1234.ts.net@evil.example"), GENERIC);
        assert_eq!(refusal("user@box"), GENERIC);
    }

    #[test]
    fn parse_tailnet_refuses_path_query_and_fragment() {
        for name in [
            "box.tail1234.ts.net/ws",
            "box.tail1234.ts.net?x=1",
            "box.tail1234.ts.net#frag",
            "box/ws",
            "box?x=1",
            "box#frag",
        ] {
            assert_eq!(refusal(name), GENERIC, "{name}");
        }
    }

    #[test]
    fn parse_tailnet_refuses_a_port() {
        assert_eq!(refusal("box.tail1234.ts.net:8787"), PORT);
        assert_eq!(refusal("box.tail1234.ts.net:443"), PORT);
    }

    #[test]
    fn parse_tailnet_refuses_whitespace_and_percent() {
        for name in [
            " box.tail1234.ts.net",
            "box.tail1234.ts.net ",
            "box .tail1234.ts.net",
            "box.tail1234.ts.net%20",
            "box.tail1234.ts.net%",
            "box ",
            "box%20",
        ] {
            assert_eq!(refusal(name), GENERIC, "{name:?}");
        }
        assert_eq!(refusal("   "), EMPTY, "whitespace alone is nothing typed");
    }

    #[test]
    fn parse_tailnet_refuses_non_ascii() {
        assert_eq!(refusal("\u{212A}ox.tail1234.ts.net"), GENERIC);
        assert_eq!(refusal("box.tail1234.ts.né"), GENERIC);
    }

    #[test]
    fn parse_tailnet_refuses_bare_ipv6_as_an_address() {
        assert_eq!(refusal("::1"), ADDRESS);
        assert_eq!(refusal("fe80::1"), ADDRESS);
        assert_eq!(refusal("2001:db8::1"), ADDRESS);
    }

    #[test]
    fn parse_tailnet_refuses_empty_and_empty_labels() {
        assert_eq!(refusal(""), EMPTY);
        assert_eq!(refusal("."), EMPTY);
        assert_eq!(refusal("box..tail1234.ts.net"), GENERIC);
        assert_eq!(refusal(".box.tail1234.ts.net"), GENERIC);
    }

    #[test]
    fn parse_tailnet_refuses_bad_label_edges() {
        assert_eq!(refusal("-box.tail1234.ts.net"), GENERIC);
        assert_eq!(refusal("box-.tail1234.ts.net"), GENERIC);
        assert_eq!(
            refusal(&format!("{}.tail1234.ts.net", "a".repeat(64))),
            GENERIC
        );
        assert_eq!(refusal(&long_name(62)), GENERIC);
        assert!(
            Target::parse_tailnet(&format!("{}.tail1234.ts.net", "a".repeat(63)), None).is_ok()
        );
        assert!(Target::parse_tailnet(&long_name(61), None).is_ok());
    }

    #[test]
    fn parse_tailnet_refuses_ip_literals() {
        for name in ["1.2.3.4", "10.0.0.1", "192.168.1.10", "[::1]", "[fe80::1]"] {
            assert_eq!(refusal(name), ADDRESS, "{name}");
        }
    }

    #[test]
    fn parse_tailnet_refuses_a_bare_label() {
        assert_eq!(refusal("box"), BARE);
        assert_eq!(refusal("localhost"), BARE);
        assert_eq!(refusal("box."), BARE);
        assert_eq!(refusal("Box"), BARE);
    }

    /// The error `name` was refused with as an SSH host.
    fn ssh_refusal(name: &str) -> SshHostError {
        let Err(err) = Target::parse_ssh(name, None) else {
            panic!("{name:?} was accepted as an SSH host");
        };
        err
    }

    #[test]
    fn parse_ssh_refuses_a_leading_dash() {
        for name in ["-oProxyCommand=x", "-V", "-"] {
            assert_eq!(ssh_refusal(name), SshHostError::LeadingDash, "{name:?}");
        }
        assert_eq!(
            SshHostError::LeadingDash.to_string(),
            "An SSH host cannot start with a dash."
        );
    }

    #[test]
    fn parse_ssh_refuses_whitespace() {
        for name in ["a b", "a\tb", " devbox", "devbox ", "a\u{a0}b"] {
            assert_eq!(ssh_refusal(name), SshHostError::Whitespace, "{name:?}");
        }
    }

    #[test]
    fn parse_ssh_refuses_control_characters() {
        for name in ["a\u{1}b", "a\nb", "a\0b", "a\u{7f}b"] {
            assert_eq!(ssh_refusal(name), SshHostError::Whitespace, "{name:?}");
        }
        assert_eq!(
            SshHostError::Whitespace.to_string(),
            "An SSH host cannot contain spaces or control characters."
        );
    }

    #[test]
    fn parse_ssh_refuses_empty() {
        for name in ["", "user@", "@host", "@"] {
            assert_eq!(ssh_refusal(name), SshHostError::Empty, "{name:?}");
        }
    }

    #[test]
    fn parse_ssh_accepts_alias_and_user_at_host_port() {
        for name in ["devbox", "me@box", "me@box:2222", "DevBox", "bôx.example"] {
            let Ok(target) = Target::parse_ssh(name, None) else {
                panic!("{name:?} was refused");
            };
            let host = target.ssh_host().map(SshHost::name);
            assert_eq!(host, Some(name), "kept exactly as typed");
            assert_eq!(target.tailnet_host(), None);
            assert_eq!(target.remote_name(), Some(name));
            assert_eq!(target.address().as_deref(), Some(name));
            assert!(!target.is_local());
        }
        let Ok(target) =
            Target::parse_ssh("me@box:2222", Some("C:\\rt\\rustling-tulipd.exe bridge"))
        else {
            panic!("a remote command was refused");
        };
        assert_eq!(target.short_name(), Some("box"));
        assert_eq!(target.host_key().as_deref(), Some("ssh:me@box:2222"));
        assert_eq!(
            target.ssh_host().and_then(SshHost::remote_command),
            Some("C:\\rt\\rustling-tulipd.exe bridge")
        );
        assert_eq!(
            Target::parse_ssh("devbox", Some("  "))
                .ok()
                .as_ref()
                .and_then(Target::ssh_host)
                .and_then(SshHost::remote_command),
            None,
            "a blank remote command is the default"
        );
        assert_ne!(
            Target::parse_ssh("devbox", None).ok(),
            Target::parse_ssh("devbox", Some("other")).ok(),
            "another remote command is another target"
        );
    }
}
