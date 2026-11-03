//! Windows integrity levels: this daemon's own, a loopback peer's, and the
//! door that keeps a process below an elevated daemon's level out of it.
//!
//! A daemon whose token is at or above High integrity ("elevated": started from an
//! elevated terminal, or by `rustling-tulipd bridge` over an administrator's
//! SSH logon) hosts sessions that inherit that token. For each request to its
//! loopback `/ws`, `/shutdown` and `/hook`, such a daemon finds the peer
//! process from the TCP table, reads its token's integrity level, and admits
//! only a peer at or above its own level, capped at High ([`admit_local_peer`]). Any failure
//! of that lookup refuses, since an ordinary process can make it fail. An
//! ordinary daemon never runs the lookup.
//!
//! Elsewhere than Windows there are no integrity levels: the daemon counts as
//! Medium, so it is never elevated and the door is never consulted.

use std::net::SocketAddr;

/// A token's mandatory integrity level: the last sub-authority (the RID) of
/// its label SID, `S-1-16-<rid>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct IntegrityLevel(u32);

impl IntegrityLevel {
    /// `S-1-16-4096`.
    pub const LOW: Self = Self(0x1000);
    /// `S-1-16-8192`, an ordinary process's level.
    pub const MEDIUM: Self = Self(0x2000);
    /// `S-1-16-12288`, an elevated administrator's level.
    pub const HIGH: Self = Self(0x3000);
    /// `S-1-16-16384`.
    pub const SYSTEM: Self = Self(0x4000);

    /// The level whose label SID ends in `rid`.
    #[must_use]
    pub const fn from_rid(rid: u32) -> Self {
        Self(rid)
    }

    /// Whether a token at this level is elevated: at or above High. A level
    /// between Medium and High (Medium Plus) is not: its handshake label is
    /// always High, which such a daemon could not set above its own level.
    #[must_use]
    pub fn is_elevated(self) -> bool {
        self >= Self::HIGH
    }
}

impl std::fmt::Display for IntegrityLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::LOW => f.write_str("Low"),
            Self::MEDIUM => f.write_str("Medium"),
            Self::HIGH => f.write_str("High"),
            Self::SYSTEM => f.write_str("System"),
            Self(rid) => write!(f, "S-1-16-{rid}"),
        }
    }
}

/// Why a peer's integrity level could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupError(String);

impl LookupError {
    /// A lookup failure described by `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An elevated daemon's refusal of a loopback peer: the reason a client shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether an elevated daemon at `daemon` admits a loopback peer whose level
/// the lookup returned as `peer`: only a peer at or above the lower of the
/// daemon's level and High is admitted, the level the handshake's label keeps
/// out, so a System daemon still serves the High peers that can read its
/// token. A failed lookup refuses, naming the failure.
///
/// # Errors
///
/// The [`Refusal`] the peer is answered with.
pub fn admit_local_peer(
    daemon: IntegrityLevel,
    peer: Result<IntegrityLevel, LookupError>,
) -> Result<(), Refusal> {
    let refusal = daemon_client::ELEVATED_DAEMON_REFUSAL;
    let floor = daemon.min(IntegrityLevel::HIGH);
    match peer {
        Ok(level) if level >= floor => Ok(()),
        Ok(_) => Err(Refusal(refusal.to_owned())),
        Err(err) => Err(Refusal(format!(
            "{refusal} (could not check this process's elevation: {err})"
        ))),
    }
}

/// Both ends of one loopback connection, as the daemon's listener accepted
/// it: `remote` is the peer's address, `local` the daemon's (`None` when the
/// socket could not report it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopbackPeer {
    pub remote: SocketAddr,
    pub local: Option<SocketAddr>,
}

/// Reads the integrity level of the process at the other end of a loopback
/// connection. The server holds one, so specs can inject a fake.
pub trait PeerIntegrity: Send + Sync {
    /// The integrity level of the process that owns `peer.remote`'s end of
    /// the connection to `peer.local`.
    ///
    /// # Errors
    ///
    /// The owning process could not be found or its token could not be read.
    fn level_of(&self, peer: LoopbackPeer) -> Result<IntegrityLevel, LookupError>;
}

/// The real lookup: the TCP table names the connection's owning process,
/// whose token holds its level.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemPeerIntegrity;

impl PeerIntegrity for SystemPeerIntegrity {
    fn level_of(&self, peer: LoopbackPeer) -> Result<IntegrityLevel, LookupError> {
        imp::peer_level(peer)
    }
}

/// This process's own integrity level.
///
/// # Errors
///
/// This process's token could not be read.
pub fn own_level() -> Result<IntegrityLevel, LookupError> {
    imp::own_level()
}

/// `MIB_TCP_STATE_ESTAB`: an established connection, the only state a live
/// peer's row is in.
#[cfg(any(windows, test))]
const MIB_TCP_STATE_ESTAB: u32 = 5;

/// `MIB_TCP_STATE_TIME_WAIT`: a closed connection the table still lists,
/// owned by pid 0.
#[cfg(test)]
const MIB_TCP_STATE_TIME_WAIT: u32 = 11;

/// One row of the IPv4 TCP table: its state, both ends and the owning pid.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TcpRow {
    state: u32,
    local: std::net::SocketAddrV4,
    remote: std::net::SocketAddrV4,
    pid: u32,
}

/// The pid that owns the peer's end of the connection from `peer` to
/// `daemon`: the established row whose local end is `peer` and whose remote
/// end is `daemon`. Rows of other states (a `TIME_WAIT` row of an earlier
/// connection on the same ports) and rows owned by pid 0 never match.
#[cfg(any(windows, test))]
fn owner_of(
    rows: &[TcpRow],
    peer: std::net::SocketAddrV4,
    daemon: std::net::SocketAddrV4,
) -> Option<u32> {
    rows.iter()
        .find(|row| {
            row.state == MIB_TCP_STATE_ESTAB
                && row.pid != 0
                && row.local == peer
                && row.remote == daemon
        })
        .map(|row| row.pid)
}

#[cfg(windows)]
mod imp {
    use super::{IntegrityLevel, LookupError, LoopbackPeer, TcpRow, owner_of};
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_CONNECTIONS,
    };
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TokenIntegrityLevel,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
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

    /// A handle closed when dropped.
    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: the handle was opened by this module and is closed once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    pub(super) fn own_level() -> Result<IntegrityLevel, LookupError> {
        // SAFETY: the pseudo-handle of the current process needs no closing.
        let process = unsafe { GetCurrentProcess() };
        token_level_of(process)
    }

    pub(super) fn peer_level(peer: LoopbackPeer) -> Result<IntegrityLevel, LookupError> {
        let pid = owning_pid(peer)?;
        // SAFETY: a plain open by pid; the handle is closed by `OwnedHandle`.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(|err| LookupError::new(format!("opening process {pid}: {err}")))?;
        let process = OwnedHandle(process);
        token_level_of(process.0).map_err(|err| LookupError::new(format!("process {pid}: {err}")))
    }

    /// The level in `process`'s token.
    fn token_level_of(process: HANDLE) -> Result<IntegrityLevel, LookupError> {
        let mut token = HANDLE::default();
        // SAFETY: `process` is a valid process handle; `token` receives a
        // handle closed by `OwnedHandle`.
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }
            .map_err(|err| LookupError::new(format!("opening its token: {err}")))?;
        let token = OwnedHandle(token);
        let mut needed = 0u32;
        // SAFETY: a size query with no buffer; it fails with
        // ERROR_INSUFFICIENT_BUFFER and sets `needed`.
        let _ =
            unsafe { GetTokenInformation(token.0, TokenIntegrityLevel, None, 0, &raw mut needed) };
        if needed == 0 {
            return Err(LookupError::new("its token reported no integrity label"));
        }
        // u64 words keep the buffer aligned for TOKEN_MANDATORY_LABEL.
        let words = usize::try_from(needed).unwrap_or(usize::MAX).div_ceil(8);
        let mut buffer = vec![0u64; words];
        // SAFETY: `buffer` holds at least `needed` bytes and outlives the call.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenIntegrityLevel,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &raw mut needed,
            )
        }
        .map_err(|err| LookupError::new(format!("reading its integrity label: {err}")))?;
        // SAFETY: the call filled `buffer` with a TOKEN_MANDATORY_LABEL whose
        // SID points into the same buffer, which is alive and aligned.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
        // SAFETY: `sid` is the valid label SID read above.
        let count = unsafe { *GetSidSubAuthorityCount(sid) };
        let Some(last) = count.checked_sub(1) else {
            return Err(LookupError::new("its integrity label has no sub-authority"));
        };
        // SAFETY: `last` is below the SID's sub-authority count.
        let rid = unsafe { *GetSidSubAuthority(sid, u32::from(last)) };
        Ok(IntegrityLevel::from_rid(rid))
    }

    /// The pid that owns the peer's end of the connection, from the TCP
    /// table ([`owner_of`]).
    fn owning_pid(peer: LoopbackPeer) -> Result<u32, LookupError> {
        let (SocketAddr::V4(remote), Some(SocketAddr::V4(local))) = (peer.remote, peer.local)
        else {
            return Err(LookupError::new(format!(
                "the connection {peer:?} is not an IPv4 loopback connection"
            )));
        };
        owner_of(&tcp_rows()?, remote, local).ok_or_else(|| {
            LookupError::new(format!(
                "no TCP connection from {remote} to {local} is in the system table"
            ))
        })
    }

    fn row_end(addr: u32, port: u32) -> SocketAddrV4 {
        let port = u16::from_be(u16::try_from(port & 0xFFFF).unwrap_or_default());
        SocketAddrV4::new(Ipv4Addr::from(addr.to_ne_bytes()), port)
    }

    /// Every IPv4 TCP connection with its owning pid.
    fn tcp_rows() -> Result<Vec<TcpRow>, LookupError> {
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
                    TCP_TABLE_OWNER_PID_CONNECTIONS,
                    0,
                )
            };
            match status {
                NO_ERROR => return Ok(rows_of(&buffer)),
                ERROR_INSUFFICIENT_BUFFER => size = capacity,
                other => {
                    return Err(LookupError::new(format!(
                        "reading the TCP table failed with error {other}"
                    )));
                }
            }
        }
        Err(LookupError::new(
            "the TCP table kept growing while it was read",
        ))
    }

    /// The rows of the `MIB_TCPTABLE_OWNER_PID` in `buffer`: a u32 count
    /// followed by that many rows.
    fn rows_of(buffer: &[u32]) -> Vec<TcpRow> {
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
                |&[state, local_addr, local_port, remote_addr, remote_port, pid]| TcpRow {
                    state,
                    local: row_end(local_addr, local_port),
                    remote: row_end(remote_addr, remote_port),
                    pid,
                },
            )
            .collect()
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{IntegrityLevel, LookupError, LoopbackPeer};

    pub(super) fn own_level() -> Result<IntegrityLevel, LookupError> {
        Ok(IntegrityLevel::MEDIUM)
    }

    pub(super) fn peer_level(_peer: LoopbackPeer) -> Result<IntegrityLevel, LookupError> {
        Err(LookupError::new("integrity levels exist only on Windows"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IntegrityLevel, LookupError, MIB_TCP_STATE_ESTAB, MIB_TCP_STATE_TIME_WAIT, TcpRow,
        admit_local_peer, owner_of,
    };
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn admit_local_peer_admits_only_a_peer_at_or_above_the_daemon() {
        let refusal = daemon_client::ELEVATED_DAEMON_REFUSAL;
        let high = IntegrityLevel::HIGH;
        let rows: [(Result<IntegrityLevel, LookupError>, Result<(), String>); 5] = [
            (Ok(IntegrityLevel::MEDIUM), Err(refusal.to_owned())),
            (Ok(IntegrityLevel::HIGH), Ok(())),
            (Ok(IntegrityLevel::SYSTEM), Ok(())),
            (
                Err(LookupError::new("opening process 42: Access is denied.")),
                Err(format!(
                    "{refusal} (could not check this process's elevation: \
                     opening process 42: Access is denied.)"
                )),
            ),
            (Ok(IntegrityLevel::LOW), Err(refusal.to_owned())),
        ];
        for (peer, want) in rows {
            let shown = format!("{peer:?}");
            let got = admit_local_peer(high, peer).map_err(|refusal| refusal.to_string());
            assert_eq!(got, want, "High daemon, peer {shown}");
        }
    }

    #[test]
    fn elevation_starts_at_high_and_the_door_compares_against_high() {
        let medium_plus = IntegrityLevel::from_rid(0x2100);
        let elevated = [
            (IntegrityLevel::LOW, false),
            (IntegrityLevel::MEDIUM, false),
            (medium_plus, false),
            (IntegrityLevel::HIGH, true),
            (IntegrityLevel::from_rid(0x3100), true),
            (IntegrityLevel::SYSTEM, true),
        ];
        for (level, want) in elevated {
            assert_eq!(level.is_elevated(), want, "{level}");
        }

        // (daemon, peer, admitted)
        let door = [
            (IntegrityLevel::SYSTEM, IntegrityLevel::HIGH, true),
            (IntegrityLevel::SYSTEM, IntegrityLevel::SYSTEM, true),
            (IntegrityLevel::SYSTEM, medium_plus, false),
            (IntegrityLevel::SYSTEM, IntegrityLevel::MEDIUM, false),
            (IntegrityLevel::HIGH, IntegrityLevel::HIGH, true),
            (IntegrityLevel::HIGH, IntegrityLevel::SYSTEM, true),
            (IntegrityLevel::HIGH, medium_plus, false),
        ];
        for (daemon, peer, want) in door {
            assert_eq!(
                admit_local_peer(daemon, Ok(peer)).is_ok(),
                want,
                "{daemon} daemon, {peer} peer"
            );
        }
    }

    #[test]
    fn owning_pid_skips_time_wait_and_pid_zero_rows() {
        let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 50_000);
        let daemon = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 40_000);
        let row = |state, pid| TcpRow {
            state,
            local: peer,
            remote: daemon,
            pid,
        };
        let other = TcpRow {
            state: MIB_TCP_STATE_ESTAB,
            local: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 50_001),
            remote: daemon,
            pid: 99,
        };
        let table = [
            row(MIB_TCP_STATE_TIME_WAIT, 0),
            row(MIB_TCP_STATE_TIME_WAIT, 7),
            row(MIB_TCP_STATE_ESTAB, 0),
            other,
            row(MIB_TCP_STATE_ESTAB, 42),
        ];
        assert_eq!(owner_of(&table, peer, daemon), Some(42));
        assert_eq!(
            owner_of(&table[..4], peer, daemon),
            None,
            "only TIME_WAIT, pid 0 and other connections' rows are left"
        );
        let reversed = [TcpRow {
            local: daemon,
            remote: peer,
            ..row(MIB_TCP_STATE_ESTAB, 5)
        }];
        assert_eq!(
            owner_of(&reversed, peer, daemon),
            None,
            "the daemon's own end of the connection is not the peer"
        );
    }

    #[test]
    fn integrity_of_this_process_reads() {
        let level = super::own_level();
        assert!(level.is_ok(), "{level:?}");
    }
}
