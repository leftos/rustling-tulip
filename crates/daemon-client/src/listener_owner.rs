//! Which process owns the TCP listener on a loopback port: a retiring or
//! stopping client force-terminates the pid `daemon.json` names only when
//! that pid owns the listener on the port that just proved it holds the
//! token, so a `daemon.json` cannot aim the kill at an unrelated process.
//!
//! Windows reads the listener table through `GetExtendedTcpTable`, mirroring
//! the daemon crate's `integrity.rs` (which this crate cannot call). Unix asks
//! `lsof`, which must answer within [`LSOF_TIMEOUT`]. Both feed the same
//! [`listener_owner_of`] loopback-and-wildcard rule.

#[cfg(any(unix, test))]
use anyhow::anyhow;
#[cfg(unix)]
use anyhow::{Context as _, bail};
#[cfg(any(windows, unix))]
use std::net::{Ipv4Addr, SocketAddrV4};

/// How a retire or stop asks which pid owns the listener on a port:
/// [`listener_owner`] in production, a stand-in in tests whose listener lives
/// in the test process.
pub(crate) type OwnerLookup = dyn Fn(u16) -> anyhow::Result<Option<u32>> + Sync;

/// `MIB_TCP_STATE_LISTEN`: a listening socket.
#[cfg(any(windows, unix))]
const MIB_TCP_STATE_LISTEN: u32 = 2;

/// `MIB_TCP_STATE_ESTAB`: an established connection.
#[cfg(test)]
const MIB_TCP_STATE_ESTAB: u32 = 5;

/// One row of the IPv4 TCP listener table: its local end, owning pid and
/// state.
#[cfg(any(windows, unix))]
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
#[cfg(any(windows, unix))]
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

/// The listener rows in the `-Fptn` output of
/// `lsof -nP -iTCP:<port> -sTCP:LISTEN`: a `p<pid>` line sets the pid of the
/// rows that follow it, a `t<type>` line their address family, and each
/// `n<addr>:<port>` line one listening IPv4 socket. IPv6 files are skipped:
/// lsof prints an IPv6 wildcard end as `*:<port>`, the same text as the IPv4
/// wildcard, and the daemon binds `127.0.0.1` only.
#[cfg(any(unix, test))]
fn listen_rows_from_lsof(text: &str) -> Vec<ListenRow> {
    let mut pid = None;
    let mut ipv6 = false;
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        match (line.get(..1), line.get(1..)) {
            (Some("p"), Some(rest)) => {
                pid = rest.parse::<u32>().ok();
                ipv6 = false;
            }
            (Some("t"), Some(rest)) => ipv6 = rest == "IPv6",
            (Some("n"), Some(rest)) if !ipv6 => {
                if let (Some(pid), Some(local)) = (pid, endpoint(rest)) {
                    rows.push(ListenRow {
                        local,
                        pid,
                        state: MIB_TCP_STATE_LISTEN,
                    });
                }
            }
            _ => {}
        }
    }
    rows
}

/// The IPv4 `addr:port` end in `text`: `*` is the unspecified address, and an
/// IPv6 end (`[::1]:40123`) has none.
#[cfg(any(unix, test))]
fn endpoint(text: &str) -> Option<SocketAddrV4> {
    let (host, port) = text.rsplit_once(':')?;
    let host = if host == "*" { "0.0.0.0" } else { host };
    Some(SocketAddrV4::new(host.parse().ok()?, port.parse().ok()?))
}

/// The owner [`listener_owner_of`] names in `lsof`'s `stdout`, for a run that
/// exited with `status`.
///
/// `lsof` exits 1 when nothing matched, so exit 1 with no output is no owner;
/// any other unsuccessful exit is an error.
///
/// # Errors
///
/// Fails when `lsof` exited unsuccessfully other than with no match.
#[cfg(any(unix, test))]
fn owner_from_lsof(status: Option<i32>, stdout: &[u8], port: u16) -> anyhow::Result<Option<u32>> {
    if status == Some(1) && stdout.is_empty() {
        return Ok(None);
    }
    if status != Some(0) {
        return Err(anyhow!("lsof on port {port} exited with status {status:?}"));
    }
    let text = String::from_utf8_lossy(stdout);
    Ok(listener_owner_of(&listen_rows_from_lsof(&text), port))
}

/// How long `lsof` may run before it is killed and the lookup fails.
#[cfg(unix)]
const LSOF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How often the running `lsof`'s exit is polled.
#[cfg(unix)]
const LSOF_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// The program asked for the listener table: `/usr/sbin/lsof` where the
/// system keeps it, else `lsof` from `PATH`.
#[cfg(unix)]
fn lsof_program() -> std::path::PathBuf {
    let system = std::path::Path::new("/usr/sbin/lsof");
    if system.is_file() {
        system.to_owned()
    } else {
        std::path::PathBuf::from("lsof")
    }
}

/// The listeners on `port` as `lsof` reports them, killed and reported as an
/// error when it is still running at [`LSOF_TIMEOUT`].
///
/// # Errors
///
/// Fails when `lsof` cannot be started, its exit cannot be read, or it does
/// not finish within [`LSOF_TIMEOUT`].
#[cfg(unix)]
fn run_lsof(port: u16) -> anyhow::Result<std::process::Output> {
    let program = lsof_program();
    let mut child = std::process::Command::new(&program)
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fptn"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("running {} on port {port}", program.display()))?;
    let deadline = std::time::Instant::now() + LSOF_TIMEOUT;
    loop {
        if child.try_wait().context("polling lsof")?.is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} on port {port} did not finish within {} s",
                program.display(),
                LSOF_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(LSOF_POLL);
    }
    child.wait_with_output().context("reading lsof's output")
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

/// The sole pid listening on `127.0.0.1:port`, as [`listener_owner_of`]
/// judges it; `None` when none does or another process listens there too.
///
/// # Errors
///
/// Fails when `lsof` cannot be run, exits unsuccessfully or does not answer
/// within [`LSOF_TIMEOUT`].
#[cfg(unix)]
pub(crate) fn listener_owner(port: u16) -> anyhow::Result<Option<u32>> {
    let output = run_lsof(port)?;
    owner_from_lsof(output.status.code(), &output.stdout, port)
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
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        ListenRow, MIB_TCP_STATE_ESTAB, MIB_TCP_STATE_LISTEN, listen_rows_from_lsof,
        listener_owner_of, owner_from_lsof,
    };
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

    #[test]
    fn lsof_rows_name_a_loopback_listener() {
        let rows = listen_rows_from_lsof("p4242\nn127.0.0.1:40123\n");

        assert_eq!(listener_owner_of(&rows, P), Some(A));
    }

    #[test]
    fn an_lsof_wildcard_row_is_the_unspecified_address() {
        let rows = listen_rows_from_lsof("p4242\nn*:40123\n");

        assert_eq!(
            rows.first().map(|row| row.local),
            Some(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, P))
        );
        assert_eq!(
            listener_owner_of(&rows, P),
            None,
            "a lone wildcard row names no owner"
        );
    }

    #[test]
    fn an_lsof_pid_with_several_addresses_keeps_its_rows() {
        let rows = listen_rows_from_lsof("p4242\nn127.0.0.1:40123\nn127.0.0.1:40124\n");

        assert_eq!(rows.len(), 2, "rows: {rows:?}");
        assert!(rows.iter().all(|row| row.pid == A), "rows: {rows:?}");
        assert_eq!(listener_owner_of(&rows, P), Some(A));
    }

    #[test]
    fn an_lsof_ipv6_row_is_ignored() {
        let rows =
            listen_rows_from_lsof("p4242\ntIPv4\nn127.0.0.1:40123\np5151\ntIPv6\nn[::1]:40123\n");

        assert_eq!(rows.len(), 1, "IPv6 rows are ignored: {rows:?}");
        assert_eq!(listener_owner_of(&rows, P), Some(A));
    }

    #[test]
    fn an_lsof_ipv6_wildcard_does_not_name_a_second_owner() {
        // lsof prints an IPv6 wildcard listener as `*:<port>`, the same text
        // as the IPv4 wildcard, so only the file's type tells them apart.
        let rows =
            listen_rows_from_lsof("p4242\ntIPv4\nn127.0.0.1:40123\np5151\ntIPv6\nn*:40123\n");

        assert_eq!(listener_owner_of(&rows, P), Some(A));
    }

    #[test]
    fn an_lsof_row_on_another_port_never_matches() {
        let rows = listen_rows_from_lsof("p4242\ntIPv4\nn127.0.0.1:4012\n");

        assert_eq!(rows.len(), 1, "rows: {rows:?}");
        assert_eq!(listener_owner_of(&rows, P), None);
    }

    #[test]
    fn an_lsof_hostname_row_is_dropped() {
        let rows = listen_rows_from_lsof("p4242\ntIPv4\nnlocalhost:40123\n");

        assert!(rows.is_empty(), "rows: {rows:?}");
        assert_eq!(listener_owner_of(&rows, P), None);
    }

    #[test]
    fn an_empty_lsof_output_names_no_owner() {
        let rows = listen_rows_from_lsof("");

        assert!(rows.is_empty(), "rows: {rows:?}");
        assert_eq!(listener_owner_of(&rows, P), None);
    }

    #[test]
    fn an_lsof_exit_of_one_with_no_output_names_no_owner() {
        let owner = owner_from_lsof(Some(1), b"", P);

        assert_eq!(owner.expect("no match is not a failure"), None);
    }

    #[test]
    fn another_unsuccessful_lsof_exit_is_an_error() {
        let quiet_failure = owner_from_lsof(Some(2), b"", P);
        let noisy_no_match = owner_from_lsof(Some(1), b"p4242\nn127.0.0.1:40123\n", P);

        assert!(quiet_failure.is_err(), "exit 2 is a failure");
        assert!(noisy_no_match.is_err(), "exit 1 with output is a failure");
    }

    /// The real macOS lookup: `lsof` names the process that holds the
    /// listener.
    #[cfg(target_os = "macos")]
    #[test]
    fn lsof_names_the_owner_of_a_real_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a listener");
        let port = listener
            .local_addr()
            .expect("the listener's address")
            .port();

        let owner = super::listener_owner(port).expect("lsof reads the listener's owner");

        assert_eq!(owner, Some(std::process::id()));
    }
}
