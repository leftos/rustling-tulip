//! Running the `tailscale` CLI and mapping its every failure to a [`TailnetError`].
//!
//! The CLI is the boundary rather than the `LocalAPI`, whose Windows pipe path is
//! undocumented. Each call runs the binary with no console window, reads its
//! stdout, keeps its stderr for the error, and kills it at the timeout. Parsing
//! stdout and classifying stderr are pure functions, so tests feed them fixtures.

use crate::model::{Status, WhoIs};
use std::ffi::OsString;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt as _};

/// The timeout for `whois`, `whoami` and `status`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// The timeout for `cert`, which may wait on the certificate authority: a
/// first issuance waits on an ACME DNS-01 round (the TXT record published and
/// seen by the CA) before the certificate is written.
pub const CERT_TIMEOUT: Duration = Duration::from_mins(5);

/// The most stdout or stderr a call may print; more is [`TailnetError::BadOutput`].
const OUTPUT_CAP: usize = 4 * 1024 * 1024;

/// The environment variable that names the `tailscale` binary.
const BINARY_ENV: &str = "RUSTLING_TULIP_TAILSCALE";

/// Why a `tailscale` call failed. A caller treats every one as "deny".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TailnetError {
    #[error("the tailscale CLI was not found")]
    NotInstalled,
    #[error("the tailscale CLI could not reach tailscaled (not running?)")]
    NotRunning,
    #[error("tailscaled refused this Windows user (in use by another user, or server mode)")]
    AccessDenied,
    #[error("Tailscale is not connected (logged out or stopped)")]
    NotConnected,
    #[error("HTTPS certificates are disabled for this tailnet (admin console, DNS page)")]
    HttpsDisabled,
    #[error("the tailscale CLI did not finish in time")]
    Timeout,
    #[error("the tailscale CLI printed output that does not parse: {0}")]
    BadOutput(String),
    #[error("the tailscale CLI failed: {0}")]
    Failed(String),
}

/// A located `tailscale` binary and the timeout its quick calls get.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub binary: PathBuf,
    pub timeout: Duration,
}

impl Cli {
    /// Locate the binary: `RUSTLING_TULIP_TAILSCALE` if set, else
    /// `%ProgramFiles%\Tailscale\tailscale.exe` if it exists, else `tailscale`
    /// on `PATH`. A binary that is not there surfaces as
    /// [`TailnetError::NotInstalled`] on the first call.
    #[must_use]
    pub fn locate() -> Self {
        Self {
            binary: locate_with(|name| std::env::var_os(name), Path::is_file),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Identify the node behind `addr`, the accepted socket's peer address
    /// (an IPv4-mapped address is asked about as its IPv4 form).
    ///
    /// # Errors
    ///
    /// Any [`TailnetError`]; the peer is then denied.
    pub async fn whois(&self, addr: SocketAddr) -> Result<WhoIs, TailnetError> {
        let addr = SocketAddr::new(addr.ip().to_canonical(), addr.port());
        let stdout = self
            .run(&args(&["whois", "--json", &addr.to_string()]), self.timeout)
            .await?;
        parse_whois(&stdout)
    }

    /// Identify this host's own node.
    ///
    /// # Errors
    ///
    /// [`TailnetError::NotConnected`] when the node has no addresses (logged out
    /// or stopped), else any other [`TailnetError`].
    pub async fn whoami(&self) -> Result<WhoIs, TailnetError> {
        let stdout = self.run(&args(&["whoami", "--json"]), self.timeout).await?;
        parse_whoami(&stdout)
    }

    /// The tailnet as this node sees it: itself and its peers.
    ///
    /// # Errors
    ///
    /// Any [`TailnetError`].
    pub async fn status(&self) -> Result<Status, TailnetError> {
        let stdout = self.run(&args(&["status", "--json"]), self.timeout).await?;
        serde_json::from_slice(&stdout)
            .map_err(|e| TailnetError::BadOutput(format!("status --json: {e}")))
    }

    /// Write the TLS certificate and key for `name` (the host's `MagicDNS` name,
    /// no trailing dot), renewing it when it is valid for less than
    /// `min_validity`. Runs with [`CERT_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// Any [`TailnetError`]; HTTPS disabled in the admin console is
    /// [`TailnetError::HttpsDisabled`].
    pub async fn cert(
        &self,
        name: &str,
        cert_file: &Path,
        key_file: &Path,
        min_validity: Duration,
    ) -> Result<(), TailnetError> {
        let argv = cert_args(name, cert_file, key_file, min_validity);
        self.run(&argv, CERT_TIMEOUT).await.map(|_| ())
    }

    /// Run the binary with `argv`, returning its stdout when it exits 0.
    async fn run(&self, argv: &[OsString], timeout: Duration) -> Result<Vec<u8>, TailnetError> {
        let mut command = tokio::process::Command::new(&self.binary);
        command
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(|e| spawn_error(&e))?;
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        // An over-cap stream ends the join at once; any early return drops the
        // child, and kill_on_drop kills it.
        let finish = async {
            let (stdout, stderr) =
                tokio::try_join!(read_capped(stdout, "stdout"), read_capped(stderr, "stderr"))?;
            let status = child.wait().await.map_err(|e| {
                TailnetError::Failed(format!("waiting for {}: {e}", self.binary.display()))
            })?;
            Ok::<_, TailnetError>((status, stdout, stderr))
        };
        let (status, stdout, stderr) = tokio::time::timeout(timeout, finish)
            .await
            .map_err(|_elapsed| TailnetError::Timeout)??;
        if status.success() {
            return Ok(stdout);
        }
        let stderr = String::from_utf8_lossy(&stderr);
        let detail = if stderr.trim().is_empty() {
            format!("exited with {status}")
        } else {
            stderr.trim().to_owned()
        };
        Err(classify_failure(&detail))
    }
}

/// The binary lookup, with the environment and the file check injected.
fn locate_with(env: impl Fn(&str) -> Option<OsString>, is_file: impl Fn(&Path) -> bool) -> PathBuf {
    let set = |name: &str| env(name).filter(|value| !value.is_empty());
    if let Some(binary) = set(BINARY_ENV) {
        return PathBuf::from(binary);
    }
    if let Some(program_files) = set("ProgramFiles") {
        let installed = PathBuf::from(program_files)
            .join("Tailscale")
            .join("tailscale.exe");
        if is_file(&installed) {
            return installed;
        }
    }
    PathBuf::from("tailscale")
}

fn args(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

fn spawn_error(error: &io::Error) -> TailnetError {
    if error.kind() == io::ErrorKind::NotFound {
        TailnetError::NotInstalled
    } else {
        TailnetError::Failed(format!("starting the tailscale CLI: {error}"))
    }
}

/// The `cert` argument list; `--` ends the flags, so a name can never read as one.
fn cert_args(
    name: &str,
    cert_file: &Path,
    key_file: &Path,
    min_validity: Duration,
) -> Vec<OsString> {
    vec![
        OsString::from("cert"),
        OsString::from("--cert-file"),
        cert_file.as_os_str().to_owned(),
        OsString::from("--key-file"),
        key_file.as_os_str().to_owned(),
        OsString::from("--min-validity"),
        OsString::from(format!("{}s", min_validity.as_secs())),
        OsString::from("--"),
        OsString::from(name),
    ]
}

/// Refuse a stream longer than [`OUTPUT_CAP`]; `what` names it in the error.
fn cap_output(bytes: Vec<u8>, what: &str) -> Result<Vec<u8>, TailnetError> {
    if bytes.len() > OUTPUT_CAP {
        return Err(TailnetError::BadOutput(format!(
            "{what} is longer than {OUTPUT_CAP} bytes"
        )));
    }
    Ok(bytes)
}

/// Read a child's pipe up to one byte past [`OUTPUT_CAP`], then apply the cap.
async fn read_capped(
    pipe: Option<impl AsyncRead + Unpin>,
    what: &str,
) -> Result<Vec<u8>, TailnetError> {
    let Some(pipe) = pipe else {
        return Err(TailnetError::Failed(format!(
            "the CLI's {what} was not captured"
        )));
    };
    let mut bytes = Vec::new();
    pipe.take(OUTPUT_CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| TailnetError::Failed(format!("reading the CLI's {what}: {e}")))?;
    cap_output(bytes, what)
}

/// Parse `whois --json` / `whoami --json` stdout.
fn parse_whois(stdout: &[u8]) -> Result<WhoIs, TailnetError> {
    serde_json::from_slice(stdout)
        .map_err(|e| TailnetError::BadOutput(format!("whois --json: {e}")))
}

/// Parse `whoami --json` stdout: a node with no addresses is logged out or stopped.
fn parse_whoami(stdout: &[u8]) -> Result<WhoIs, TailnetError> {
    let who = parse_whois(stdout)?;
    if who.node.addresses.is_empty() {
        return Err(TailnetError::NotConnected);
    }
    Ok(who)
}

/// Map a failed call's stderr to its error.
///
/// The texts come from Tailscale's source: a 401 from tailscaled is printed as
/// `401 Unauthorized: …` (`client/local/local.go`, `httpStatusError`), whose
/// body for another Windows user is `Tailscale already in use by …`
/// (`ipn/ipnserver/server.go`); a CLI that cannot reach tailscaled prints
/// `Failed to connect to local Tailscale daemon …` (`client/local/local.go`);
/// a backend that is down prints `Tailscale is stopped.` or `Logged out.`
/// (`cmd/tailscale/cli/status.go`, `isRunningOrStarting`), and `whoami` with no
/// address prints `no current Tailscale IP address` (`cmd/tailscale/cli/whoami.go`).
fn classify_failure(stderr: &str) -> TailnetError {
    const ACCESS_DENIED: [&str; 3] = [
        "Tailscale running in server mode",
        "Tailscale already in use",
        "401 Unauthorized",
    ];
    const NOT_RUNNING: [&str; 1] = ["Failed to connect to local Tailscale daemon"];
    const NOT_CONNECTED: [&str; 5] = [
        "Tailscale is stopped",
        "Logged out",
        "NeedsLogin",
        "not logged in",
        "no current Tailscale IP address",
    ];
    // `tailscale cert -- <name>` when the netmap lists no certificate domain:
    // tailscale feature/acme/cert.go, `resolveCertDomain`, returned as the
    // error text by the LocalAPI cert handler (ipn/localapi/cert.go).
    const HTTPS_DISABLED: [&str; 1] = ["does not support getting TLS certs"];
    let has = |needles: &[&str]| needles.iter().any(|needle| stderr.contains(needle));
    if has(&HTTPS_DISABLED) {
        TailnetError::HttpsDisabled
    } else if has(&ACCESS_DENIED) {
        TailnetError::AccessDenied
    } else if has(&NOT_RUNNING) {
        TailnetError::NotRunning
    } else if has(&NOT_CONNECTED) {
        TailnetError::NotConnected
    } else {
        TailnetError::Failed(stderr.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::{
        Cli, OUTPUT_CAP, TailnetError, args, cap_output, cert_args, classify_failure, locate_with,
        parse_whoami, parse_whois,
    };
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    #[test]
    fn server_mode_401_maps_to_access_denied() {
        let in_use = "401 Unauthorized: Tailscale already in use by DESKTOP\\bob, pid 4321";
        assert_eq!(classify_failure(in_use), TailnetError::AccessDenied);
        let server_mode = "Tailscale running in server mode as DESKTOP\\bob; access denied";
        assert_eq!(classify_failure(server_mode), TailnetError::AccessDenied);
    }

    #[test]
    fn already_in_use_maps_to_access_denied() {
        assert_eq!(
            classify_failure("Tailscale already in use by DESKTOP\\bob, pid 4321"),
            TailnetError::AccessDenied
        );
    }

    #[test]
    fn peer_address_containing_401_is_not_access_denied() {
        let stderr = "no IP found in netmap for fd7a:115c:a1e0::401";
        assert_eq!(
            classify_failure(stderr),
            TailnetError::Failed(stderr.to_owned())
        );
    }

    #[test]
    fn not_running_maps_to_not_running() {
        let stderr = "Failed to connect to local Tailscale daemon for /localapi/v0/whois; not running? \
                      Error: open \\\\.\\pipe\\ProtectedPrefix\\Administrators\\Tailscale\\tailscaled: \
                      The system cannot find the file specified.";
        assert_eq!(classify_failure(stderr), TailnetError::NotRunning);
    }

    #[test]
    fn stopped_maps_to_not_connected() {
        assert_eq!(
            classify_failure("Tailscale is stopped."),
            TailnetError::NotConnected
        );
    }

    #[test]
    fn logged_out_maps_to_not_connected() {
        assert_eq!(
            classify_failure("Logged out.\nLog in at: https://login.tailscale.com/a/abc"),
            TailnetError::NotConnected
        );
    }

    #[test]
    fn needs_login_maps_to_not_connected() {
        assert_eq!(
            classify_failure("unexpected state: NeedsLogin"),
            TailnetError::NotConnected
        );
    }

    #[test]
    fn not_logged_in_maps_to_not_connected() {
        assert_eq!(
            classify_failure("not logged in"),
            TailnetError::NotConnected
        );
    }

    #[test]
    fn no_current_ip_maps_to_not_connected() {
        assert_eq!(
            classify_failure("no current Tailscale IP address; state: Stopped"),
            TailnetError::NotConnected
        );
    }

    #[test]
    fn https_disabled_maps_to_its_variant() {
        assert_eq!(
            classify_failure(
                "500 Internal Server Error: your Tailscale account does not support getting TLS certs"
            ),
            TailnetError::HttpsDisabled
        );
        assert_eq!(
            classify_failure("your Tailscale account does not support getting TLS certs"),
            TailnetError::HttpsDisabled
        );
    }

    #[test]
    fn unknown_stderr_maps_to_failed() {
        assert_eq!(
            classify_failure("peer \"100.64.0.9\" not found"),
            TailnetError::Failed("peer \"100.64.0.9\" not found".to_owned())
        );
    }

    #[test]
    fn unparsable_stdout_is_bad_output() {
        assert!(matches!(
            parse_whois(b"not json"),
            Err(TailnetError::BadOutput(_))
        ));
    }

    #[test]
    fn whoami_without_addresses_is_not_connected() {
        let stdout = br#"{"Node": {"ID": 1, "StableID": "n1", "User": 2, "Addresses": []}}"#;
        assert_eq!(parse_whoami(stdout), Err(TailnetError::NotConnected));
    }

    #[test]
    fn output_over_cap_is_bad_output() {
        assert_eq!(
            cap_output(vec![b'x'; OUTPUT_CAP], "stdout").map(|bytes| bytes.len()),
            Ok(OUTPUT_CAP)
        );
        assert!(matches!(
            cap_output(vec![b'x'; OUTPUT_CAP + 1], "stdout"),
            Err(TailnetError::BadOutput(_))
        ));
    }

    #[test]
    fn cert_args_end_flags_before_name() {
        let argv = cert_args(
            "desktop.tail1234.ts.net",
            Path::new("c.pem"),
            Path::new("k.pem"),
            Duration::from_secs(1_001),
        );
        assert_eq!(
            argv,
            args(&[
                "cert",
                "--cert-file",
                "c.pem",
                "--key-file",
                "k.pem",
                "--min-validity",
                "1001s",
                "--",
                "desktop.tail1234.ts.net",
            ])
        );
    }

    #[tokio::test]
    async fn missing_binary_is_not_installed() {
        let cli = Cli {
            binary: Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-tailscale.exe"),
            timeout: Duration::from_secs(5),
        };
        assert_eq!(cli.whoami().await, Err(TailnetError::NotInstalled));
    }

    #[tokio::test]
    async fn slow_cli_returns_timeout() {
        // A stand-in that never exits by itself, against a 1 ms timeout: only the
        // timeout can end the run, so a load that delays either side changes nothing.
        // A run that ignores its timeout hangs into nextest's hang guard. The stand-in
        // is started directly, not through a shell, so the kill on drop leaves no
        // grandchild holding the output pipes open.
        let (binary, argv) = if cfg!(windows) {
            ("ping", args(&["-t", "127.0.0.1"]))
        } else {
            ("sleep", args(&["100000"]))
        };
        let cli = Cli {
            binary: PathBuf::from(binary),
            timeout: Duration::from_millis(1),
        };
        assert_eq!(
            cli.run(&argv, cli.timeout).await,
            Err(TailnetError::Timeout)
        );
    }

    #[test]
    fn locate_prefers_env_then_program_files_then_path() {
        let installed = PathBuf::from("C:\\Program Files")
            .join("Tailscale")
            .join("tailscale.exe");
        let locate = |vars: &[(&str, &str)], files: &[&Path]| {
            let vars: HashMap<String, OsString> = vars
                .iter()
                .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
                .collect();
            locate_with(|name| vars.get(name).cloned(), |path| files.contains(&path))
        };
        let both = [
            ("RUSTLING_TULIP_TAILSCALE", "D:\\tools\\tailscale.exe"),
            ("ProgramFiles", "C:\\Program Files"),
        ];
        assert_eq!(
            locate(&both, &[installed.as_path()]),
            PathBuf::from("D:\\tools\\tailscale.exe")
        );
        let empty_env = [
            ("RUSTLING_TULIP_TAILSCALE", ""),
            ("ProgramFiles", "C:\\Program Files"),
        ];
        assert_eq!(locate(&empty_env, &[installed.as_path()]), installed);
        assert_eq!(
            locate(&[("ProgramFiles", "C:\\Program Files")], &[]),
            PathBuf::from("tailscale")
        );
        assert_eq!(locate(&[], &[]), PathBuf::from("tailscale"));
    }
}
