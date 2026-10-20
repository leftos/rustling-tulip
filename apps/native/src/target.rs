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

impl Target {
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
    use super::{DEFAULT_TAILNET_PORT, TailnetHost, Target};

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
}
