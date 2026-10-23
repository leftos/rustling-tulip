//! Which daemon the client connects to: the local one, or a host on the
//! tailnet by its `MagicDNS` name.

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

    /// Whether this is the local daemon.
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// The tailnet host; `None` for the local daemon.
    #[must_use]
    pub fn tailnet_host(&self) -> Option<&TailnetHost> {
        match self {
            Self::Local => None,
            Self::Tailnet(host) => Some(host),
        }
    }

    /// The host's short name, its first DNS label; `None` for the local
    /// daemon.
    #[must_use]
    pub fn short_name(&self) -> Option<&str> {
        self.tailnet_host().map(TailnetHost::short_name)
    }

    /// The host's full name and port, `name:port`; `None` for the local
    /// daemon.
    #[must_use]
    pub fn address(&self) -> Option<String> {
        self.tailnet_host().map(TailnetHost::address)
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic,
        reason = "a name accepted or refused against the spec fails it with what happened"
    )]

    use super::{DEFAULT_TAILNET_PORT, TailnetHost, Target};

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
}
