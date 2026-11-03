//! Which process owns the TCP listener on a loopback port: a retiring or
//! stopping client force-terminates the pid `daemon.json` names only when
//! that pid owns the listener on the port that just proved it holds the
//! token, so a `daemon.json` cannot aim the kill at an unrelated process.
//!
//! The read of the system's listener table mirrors the daemon crate's
//! `integrity.rs` (which this crate cannot call), asking for listening rows
//! (`TCP_TABLE_OWNER_PID_LISTENER`) instead of connections.

#[cfg(any(windows, test))]
use std::net::{Ipv4Addr, SocketAddrV4};

/// How a retire or stop asks which pid owns the listener on a port:
/// [`listener_owner`] in production, a stand-in in tests whose listener lives
/// in the test process.
pub(crate) type OwnerLookup = dyn Fn(u16) -> anyhow::Result<Option<u32>> + Sync;

/// `MIB_TCP_STATE_LISTEN`: a listening socket.
#[cfg(any(windows, test))]
const MIB_TCP_STATE_LISTEN: u32 = 2;

/// `MIB_TCP_STATE_ESTAB`: an established connection.
#[cfg(test)]
const MIB_TCP_STATE_ESTAB: u32 = 5;

/// One row of the IPv4 TCP listener table: its local end, owning pid and
/// state.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListenRow {
    local: SocketAddrV4,
    pid: u32,
    state: u32,
}

/// The one pid that owns every listening socket on `port` that a connection
/// to `127.0.0.1:port` could reach: of the listening rows on `port` bound to
/// `127.0.0.1` or `0.0.0.0`, at least one is a `127.0.0.1` row and all of
/// them have the same pid, other than 0. Two owners (a second socket bound
/// beside the first with `SO_REUSEADDR`, or a wildcard listener of another
/// process) leave the proof's answerer unknown, so they name none; a lone
/// wildcard row is not the daemon, which binds only `127.0.0.1`. Rows of
/// other states, ports or addresses are ignored.
#[cfg(any(windows, test))]
fn listener_owner_of(rows: &[ListenRow], port: u16) -> Option<u32> {
    let mut owner = None;
    let mut on_loopback = false;
    let reachable = rows.iter().filter(|row| {
        row.state == MIB_TCP_STATE_LISTEN
            && row.local.port() == port
            && (*row.local.ip() == Ipv4Addr::LOCALHOST || *row.local.ip() == Ipv4Addr::UNSPECIFIED)
    });
    for row in reachable {
        if row.pid == 0 || owner.is_some_and(|pid| pid != row.pid) {
            return None;
        }
        owner = Some(row.pid);
        on_loopback |= *row.local.ip() == Ipv4Addr::LOCALHOST;
    }
    owner.filter(|_| on_loopback)
}

/// The sole pid listening on `127.0.0.1:port`, as [`listener_owner_of`]
/// judges it; `None` when none does or another process listens there too.
///
/// # Errors
///
/// Fails when the system's TCP listener table cannot be read.
#[cfg(windows)]
pub(crate) fn listener_owner(port: u16) -> anyhow::Result<Option<u32>> {
    Ok(listener_owner_of(&imp::listen_rows()?, port))
}

/// Stand-in so callers compile off Windows: the listener's owner is only
/// consulted on Windows, where the pid is pinned, so this is never consulted.
///
/// # Errors
///
/// Never fails.
#[cfg(not(windows))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "matches the Windows lookup's signature"
)]
pub(crate) fn listener_owner(port: u16) -> anyhow::Result<Option<u32>> {
    let _ = port;
    Ok(None)
}

#[cfg(windows)]
mod imp {
    use super::ListenRow;
    use anyhow::anyhow;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
    };

    /// `AF_INET`, the address family of the loopback listener.
    const AF_INET: u32 = 2;
    /// `NO_ERROR`.
    const NO_ERROR: u32 = 0;
    /// `ERROR_INSUFFICIENT_BUFFER`: the table grew between the size query and
    /// the read.
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    /// How many times the table read is retried when it grew in between.
    const TABLE_ATTEMPTS: u32 = 4;

    fn row_end(addr: u32, port: u32) -> SocketAddrV4 {
        let port = u16::from_be(u16::try_from(port & 0xFFFF).unwrap_or_default());
        SocketAddrV4::new(Ipv4Addr::from(addr.to_ne_bytes()), port)
    }

    /// Every IPv4 TCP listener with its owning pid.
    pub(super) fn listen_rows() -> anyhow::Result<Vec<ListenRow>> {
        let mut size = 0u32;
        for _ in 0..TABLE_ATTEMPTS {
            // u32 words keep the buffer aligned for MIB_TCPTABLE_OWNER_PID.
            let words = usize::try_from(size)
                .unwrap_or(usize::MAX)
                .div_ceil(4)
                .max(1);
            let mut buffer = vec![0u32; words];
            let mut capacity = u32::try_from(words * 4).unwrap_or(u32::MAX);
            // SAFETY: `buffer` holds `capacity` bytes and outlives the call.
            let status = unsafe {
                GetExtendedTcpTable(
                    Some(buffer.as_mut_ptr().cast()),
                    &raw mut capacity,
                    false,
                    AF_INET,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            match status {
                NO_ERROR => return Ok(rows_of(&buffer)),
                ERROR_INSUFFICIENT_BUFFER => size = capacity,
                other => {
                    return Err(anyhow!(
                        "reading the TCP listener table failed with error {other}"
                    ));
                }
            }
        }
        Err(anyhow!(
            "the TCP listener table kept growing while it was read"
        ))
    }

    /// The rows of the `MIB_TCPTABLE_OWNER_PID` in `buffer`: a u32 count
    /// followed by that many rows.
    fn rows_of(buffer: &[u32]) -> Vec<ListenRow> {
        // A row is six u32 fields.
        const _: () = assert!(size_of::<MIB_TCPROW_OWNER_PID>() == 6 * 4);
        let count = buffer
            .first()
            .map_or(0, |&n| usize::try_from(n).unwrap_or(0));
        buffer
            .get(1..)
            .unwrap_or_default()
            .as_chunks::<6>()
            .0
            .iter()
            .take(count)
            .map(
                |&[
                    state,
                    local_addr,
                    local_port,
                    _remote_addr,
                    _remote_port,
                    pid,
                ]| ListenRow {
                    local: row_end(local_addr, local_port),
                    pid,
                    state,
                },
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{ListenRow, MIB_TCP_STATE_ESTAB, MIB_TCP_STATE_LISTEN, listener_owner_of};
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn row(addr: Ipv4Addr, port: u16, pid: u32, state: u32) -> ListenRow {
        ListenRow {
            local: SocketAddrV4::new(addr, port),
            pid,
            state,
        }
    }

    const P: u16 = 40123;
    const A: u32 = 4242;
    const B: u32 = 5151;
    const OTHER_HOST: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 5);

    fn listen(addr: Ipv4Addr, pid: u32) -> ListenRow {
        row(addr, P, pid, MIB_TCP_STATE_LISTEN)
    }

    #[test]
    fn a_listen_row_on_the_port_names_its_pid() {
        let rows = [listen(Ipv4Addr::LOCALHOST, A)];

        assert_eq!(listener_owner_of(&rows, P), Some(A));
    }

    #[test]
    fn two_loopback_listeners_on_the_port_name_no_owner() {
        let rows = [
            listen(Ipv4Addr::LOCALHOST, A),
            listen(Ipv4Addr::LOCALHOST, B),
        ];

        assert_eq!(listener_owner_of(&rows, P), None);
    }

    #[test]
    fn a_wildcard_listener_of_another_pid_names_no_owner() {
        let rows = [
            listen(Ipv4Addr::LOCALHOST, A),
            listen(Ipv4Addr::UNSPECIFIED, B),
        ];

        assert_eq!(listener_owner_of(&rows, P), None);
    }

    #[test]
    fn a_wildcard_listener_alone_never_matches() {
        let rows = [listen(Ipv4Addr::UNSPECIFIED, A)];

        assert_eq!(listener_owner_of(&rows, P), None);
    }

    #[test]
    fn a_wildcard_listener_of_the_same_pid_keeps_its_owner() {
        let rows = [
            listen(Ipv4Addr::LOCALHOST, A),
            listen(Ipv4Addr::UNSPECIFIED, A),
        ];

        assert_eq!(listener_owner_of(&rows, P), Some(A));
    }

    #[test]
    fn an_established_row_on_the_port_is_ignored() {
        let rows = [
            listen(Ipv4Addr::LOCALHOST, A),
            row(Ipv4Addr::LOCALHOST, P, B, MIB_TCP_STATE_ESTAB),
        ];

        assert_eq!(listener_owner_of(&rows, P), Some(A));
    }

    #[test]
    fn a_non_loopback_listener_is_ignored() {
        let rows = [listen(Ipv4Addr::LOCALHOST, A), listen(OTHER_HOST, B)];

        assert_eq!(listener_owner_of(&rows, P), Some(A));
    }

    #[test]
    fn a_pid_zero_row_never_matches() {
        let rows = [listen(Ipv4Addr::LOCALHOST, 0)];

        assert_eq!(listener_owner_of(&rows, P), None);
    }

    #[test]
    fn a_pid_zero_row_beside_an_owner_names_no_owner() {
        let rows = [
            listen(Ipv4Addr::LOCALHOST, A),
            listen(Ipv4Addr::LOCALHOST, 0),
        ];

        assert_eq!(listener_owner_of(&rows, P), None);
    }

    #[test]
    fn a_listener_on_another_port_never_matches() {
        let rows = [row(Ipv4Addr::LOCALHOST, P + 1, A, MIB_TCP_STATE_LISTEN)];

        assert_eq!(listener_owner_of(&rows, P), None);
    }
}
