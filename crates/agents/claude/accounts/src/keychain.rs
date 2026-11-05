//! Claude Code's live login in the macOS Keychain, through Apple's `security` tool at the path the caller gives
//! ([`crate::LiveStore::Keychain`]), as Claude Code itself reads and writes it: the generic-password item of a service
//! and an account.
//!
//! A read is `find-generic-password -a <account> -w -s <service>`: exit 0 prints the value and one newline, exit 44
//! means there is no item, and any other exit is a failure (a locked or refusing Keychain), never taken for "no item".
//! A write is `add-generic-password -U` with the value hex-encoded (`-X`), sent as one command line on the stdin of
//! `security -i`, which keeps the value out of the process list there; a line longer than `security -i` reads in one go
//! ([`STDIN_LINE_LIMIT`]) would be cut short and corrupt the item, so such a value goes in the arguments instead, where
//! it does show in the process list. Only a value of printable ASCII is written at all ([`Failure::NotPrintable`]): a
//! read of any other value comes back hex-encoded, ours and Claude Code's. A delete is `delete-generic-password`, exit
//! 44 counting as done. Every call is bounded to [`CALL_TIMEOUT`]: a locked login Keychain on a host with no one at the
//! screen prompts for an unlock that never comes. The value is read into and written from buffers zeroed on drop, and
//! no call logs it.

use std::fmt;
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

/// The longest a `security` call may take before it is killed and counted as failed. A healthy Keychain answers in
/// well under 100 ms.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest command line, newline included, sent on `security -i`'s stdin: it reads a line into a 4096-byte buffer
/// and cuts a longer one short, which would write a corrupt item. 64 bytes are kept back for line-ending accounting.
pub(crate) const STDIN_LINE_LIMIT: usize = 4096 - 64;

/// `errSecItemNotFound`, the exit code of `find-generic-password` and `delete-generic-password` for no such item.
const NOT_FOUND: i32 = 44;

/// The most a read takes from `security`'s output: the store's cap on a live credential file.
const VALUE_CAP: usize = 64 * 1024;

/// How often a call that has not finished is looked at again.
const WAIT_POLL: Duration = Duration::from_millis(10);

/// Why a `security` call failed. It carries no value and no path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// `security` exited with this code: the Keychain is locked, refused access, or the call was not understood.
    Exit(i32),
    /// `security` was ended by a signal.
    Signal,
    /// `security` did not finish within [`CALL_TIMEOUT`] and was killed.
    TimedOut,
    /// `security` could not be started or waited on.
    Io(io::ErrorKind),
    /// `security` printed more than [`VALUE_CAP`], or its output could not be read.
    Output,
    /// The value holds a byte outside printable ASCII, which a later read through `security -w` would print back
    /// hex-encoded. Nothing was run.
    NotPrintable,
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exit(code) => write!(formatter, "`security` exited with code {code}"),
            Self::Signal => formatter.write_str("`security` was ended by a signal"),
            Self::TimedOut => write!(
                formatter,
                "`security` did not answer within {} s",
                CALL_TIMEOUT.as_secs()
            ),
            Self::Io(kind) => write!(formatter, "`security` could not be run: {kind}"),
            Self::Output => formatter.write_str("`security`'s output could not be read"),
            Self::NotPrintable => formatter.write_str(
                "the value is not printable text, which `security` would read back hex-encoded",
            ),
        }
    }
}

/// The value of the item of `service` and `account`, without the newline `security` prints after it; `None` when
/// there is no such item.
pub(crate) fn find(
    security: &Path,
    service: &str,
    account: &str,
) -> Result<Option<Zeroizing<Vec<u8>>>, Failure> {
    let (status, mut value) = run(
        security,
        &["find-generic-password", "-a", account, "-w", "-s", service],
        None,
    )?;
    match status.code() {
        Some(0) => {
            if value.last() == Some(&b'\n') {
                value.pop();
            }
            Ok(Some(value))
        }
        Some(NOT_FOUND) => Ok(None),
        Some(code) => Err(Failure::Exit(code)),
        None => Err(Failure::Signal),
    }
}

/// Whether every byte of `value` is printable ASCII (`0x20..=0x7E`): the only values `security -w` prints raw, and so
/// the only ones an item can hold and read back unchanged.
pub(crate) fn printable(value: &[u8]) -> bool {
    value.iter().all(|byte| (0x20..=0x7e).contains(byte))
}

/// Creates or replaces the item of `service` and `account` with `value`, which must be printable ASCII
/// ([`printable`]; [`Failure::NotPrintable`] otherwise): one `add-generic-password -U` command line on `security -i`'s
/// stdin, or, for a line longer than [`STDIN_LINE_LIMIT`], the same command in the arguments.
pub(crate) fn add(
    security: &Path,
    service: &str,
    account: &str,
    value: &[u8],
) -> Result<(), Failure> {
    if !printable(value) {
        return Err(Failure::NotPrintable);
    }
    let hex = hex(value);
    let mut line = Zeroizing::new(String::with_capacity(
        hex.len() + 2 * (service.len() + account.len()) + 64,
    ));
    line.push_str("add-generic-password -U -a ");
    push_quoted(&mut line, account);
    line.push_str(" -s ");
    push_quoted(&mut line, service);
    line.push_str(" -X ");
    line.push_str(&hex);
    line.push('\n');
    let (status, _) = if line.len() <= STDIN_LINE_LIMIT {
        run(security, &["-i"], Some(line.as_bytes()))?
    } else {
        run(
            security,
            &[
                "add-generic-password",
                "-U",
                "-a",
                account,
                "-s",
                service,
                "-X",
                &hex,
            ],
            None,
        )?
    };
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(Failure::Exit(code)),
        None => Err(Failure::Signal),
    }
}

/// Deletes the item of `service` and `account`; one that does not exist counts as deleted.
pub(crate) fn delete(security: &Path, service: &str, account: &str) -> Result<(), Failure> {
    let (status, _) = run(
        security,
        &["delete-generic-password", "-a", account, "-s", service],
        None,
    )?;
    match status.code() {
        Some(0 | NOT_FOUND) => Ok(()),
        Some(code) => Err(Failure::Exit(code)),
        None => Err(Failure::Signal),
    }
}

/// Runs `security` with `args`, `input` written to its stdin when given, within [`CALL_TIMEOUT`]: its exit status and
/// what it printed, in a buffer zeroed on drop. What it writes to stderr is dropped.
fn run(
    security: &Path,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<(ExitStatus, Zeroizing<Vec<u8>>), Failure> {
    let mut child = Command::new(security)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Failure::Io(error.kind()))?;
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        // A line shorter than a pipe's buffer never blocks; a `security` that exits before reading it is judged by its
        // exit status below.
        let _sent = stdin.write_all(bytes);
    }
    let stdout = child.stdout.take();
    let reader = thread::spawn(move || read_capped(stdout));
    let status = wait_bounded(&mut child)?;
    let output = reader
        .join()
        .map_err(|_| Failure::Output)?
        .ok_or(Failure::Output)?;
    Ok((status, output))
}

/// `child`'s exit status once it exits; [`Failure::TimedOut`] once [`CALL_TIMEOUT`] passes first, the child killed
/// and reaped.
fn wait_bounded(child: &mut Child) -> Result<ExitStatus, Failure> {
    let deadline = Instant::now() + CALL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(WAIT_POLL),
            outcome => {
                let _killed = child.kill();
                let _reaped = child.wait();
                return Err(match outcome {
                    Err(error) => Failure::Io(error.kind()),
                    Ok(_) => Failure::TimedOut,
                });
            }
        }
    }
}

/// Everything `stdout` gives up to its end, in a buffer sized once and zeroed on drop; `None` when it holds more than
/// [`VALUE_CAP`] or cannot be read.
fn read_capped(stdout: Option<ChildStdout>) -> Option<Zeroizing<Vec<u8>>> {
    let mut output = Zeroizing::new(Vec::with_capacity(VALUE_CAP + 1));
    if let Some(stdout) = stdout {
        stdout
            .take(u64::try_from(VALUE_CAP + 1).unwrap_or(u64::MAX))
            .read_to_end(&mut output)
            .ok()?;
    }
    (output.len() <= VALUE_CAP).then_some(output)
}

/// `bytes` as lowercase hex, in a buffer sized exactly and zeroed on drop.
fn hex(bytes: &[u8]) -> Zeroizing<String> {
    let mut text = Zeroizing::new(String::with_capacity(bytes.len() * 2));
    for byte in bytes {
        for digit in [byte >> 4, byte & 0x0f] {
            text.push(char::from_digit(u32::from(digit), 16).unwrap_or('0'));
        }
    }
    text
}

/// `value` in double quotes, `\` and `"` escaped with a backslash: `security -i` re-reads each line as a shell would,
/// and the default service name holds a space.
fn push_quoted(line: &mut String, value: &str) {
    line.push('"');
    for character in value.chars() {
        if matches!(character, '\\' | '"') {
            line.push('\\');
        }
        line.push(character);
    }
    line.push('"');
}

/// The name of the user this process runs as, from the user database; `None` when it has no entry or its name is not
/// UTF-8. Claude Code names its Keychain item's account `$USER`, else this, else `claude-code-user`.
#[cfg(unix)]
#[must_use]
pub fn os_user_name() -> Option<String> {
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: `passwd` is plain old data; all-zero is a valid value for it, and `getpwuid_r` fills it in.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is to a live local that outlives the call, and the buffer's length is the one passed.
    let code = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &raw mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut found,
        )
    };
    if code != 0 || found.is_null() || entry.pw_name.is_null() {
        return None;
    }
    // SAFETY: `getpwuid_r` succeeded, so `pw_name` points at a NUL-terminated string inside `buffer`, still alive.
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    name.to_str()
        .ok()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

#[cfg(all(test, unix))]
pub(crate) mod fake {
    //! A fake `security` for the tests: `tests/support/fake-security.sh`, run through a wrapper that names its folder.

    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use crate::paths::ClaudePaths;

    /// The service the fake item is kept under: Claude Code's default one, which holds a space.
    pub(crate) const SERVICE: &str = "Claude Code-credentials";
    /// The account the fake item is kept under.
    pub(crate) const ACCOUNT: &str = "tester";

    /// A fresh folder with a fake `security` wrapper, its item folder and Claude Code's files beside them.
    pub(crate) struct FakeKeychain {
        /// The fake's folder: its items, its call logs and its failure switch.
        pub(crate) dir: PathBuf,
        /// The wrapper to run as `security`.
        pub(crate) security: PathBuf,
        /// The folder Claude Code's files are made in.
        pub(crate) claude: PathBuf,
    }

    impl FakeKeychain {
        /// A fake named after the test, any folder left by an earlier run wiped first.
        #[expect(
            clippy::expect_used,
            reason = "a test that cannot set up its fake must fail loudly"
        )]
        pub(crate) fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "claude-accounts-keychain-{name}-{}",
                std::process::id()
            ));
            // A folder left by an earlier run is wiped first; one that does not exist is the usual case.
            let _wiped = fs::remove_dir_all(&root);
            let dir = root.join("keychain");
            let claude = root.join("home");
            fs::create_dir_all(dir.join("items")).expect("the fake's folder can be made");
            fs::create_dir_all(claude.join(".claude")).expect("Claude Code's folder can be made");
            let script =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-security.sh");
            let security = dir.join("security");
            fs::write(
                &security,
                format!(
                    "#!/bin/sh\nexport FAKE_SECURITY_DIR='{}'\nexec /bin/sh '{}' \"$@\"\n",
                    dir.display(),
                    script.display()
                ),
            )
            .expect("the fake's wrapper can be written");
            fs::set_permissions(&security, fs::Permissions::from_mode(0o700))
                .expect("the fake's wrapper can be made runnable");
            Self {
                dir,
                security,
                claude,
            }
        }

        /// Claude Code's files in the fake's folder, the live login kept in the fake's item.
        pub(crate) fn paths(&self) -> ClaudePaths {
            ClaudePaths::new(
                self.claude.join(".claude"),
                self.claude.join(".claude.json"),
            )
            .with_keychain(SERVICE, ACCOUNT, &self.security)
        }

        /// Each call's arguments, joined by spaces, in order.
        pub(crate) fn argv(&self) -> Vec<String> {
            lines(&self.dir.join("argv.log"))
        }

        /// Each command line `security -i` read, in order.
        pub(crate) fn stdin(&self) -> Vec<String> {
            lines(&self.dir.join("stdin.log"))
        }

        /// Makes every later call exit with `code`.
        #[expect(
            clippy::expect_used,
            reason = "a test that cannot set up its fake must fail loudly"
        )]
        pub(crate) fn fail_with(&self, code: i32) {
            fs::write(self.dir.join("fail"), code.to_string())
                .expect("the fake's failure switch can be written");
        }

        /// The item's value, `None` when there is no item.
        pub(crate) fn item(&self) -> Option<Vec<u8>> {
            let hex = fs::read_to_string(self.item_file()).ok()?;
            Some(
                (0..hex.len())
                    .step_by(2)
                    .filter_map(|at| hex.get(at..at + 2))
                    .filter_map(|pair| u8::from_str_radix(pair, 16).ok())
                    .collect(),
            )
        }

        /// Puts `value` in the item, as another writer would.
        #[expect(
            clippy::expect_used,
            reason = "a test that cannot set up its fake must fail loudly"
        )]
        pub(crate) fn put(&self, value: &[u8]) {
            let hex = super::hex(value);
            fs::write(self.item_file(), hex.as_bytes()).expect("the fake item can be written");
        }

        fn item_file(&self) -> PathBuf {
            let name = super::hex(format!("{SERVICE}\n{ACCOUNT}").as_bytes());
            self.dir.join("items").join(name.as_str())
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

#[cfg(all(test, unix))]
#[expect(
    clippy::expect_used,
    reason = "a test whose Keychain call fails must fail loudly"
)]
mod tests {
    use super::fake::{ACCOUNT, FakeKeychain, SERVICE};
    use super::{Failure, STDIN_LINE_LIMIT, add, find};

    #[test]
    fn keychain_writes_put_no_secret_in_argv() {
        let fake = FakeKeychain::new("argv");
        let value = r#"{"claudeAiOauth":{"accessToken":"fake-access-secret"}}"#;

        add(&fake.security, SERVICE, ACCOUNT, value.as_bytes()).expect("the item is written");

        let hex = super::hex(value.as_bytes()).as_str().to_owned();
        assert_eq!(fake.argv(), ["-i"], "the write runs `security -i` alone");
        assert_eq!(
            fake.stdin(),
            [format!(
                r#"add-generic-password -U -a "{ACCOUNT}" -s "{SERVICE}" -X {hex}"#
            )],
            "the one command line names the item and carries the value as hex"
        );
        assert!(
            fake.argv()
                .iter()
                .all(|line| !line.contains("fake-access-secret") && !line.contains(&hex)),
            "neither the value nor its hex is in any argument"
        );
        assert_eq!(fake.item().as_deref(), Some(value.as_bytes()));
        assert_eq!(
            find(&fake.security, SERVICE, ACCOUNT)
                .expect("the item is read")
                .as_deref()
                .map(Vec::as_slice),
            Some(value.as_bytes()),
            "the value reads back without its trailing newline"
        );
    }

    #[test]
    fn keychain_long_values_use_the_argv_form() {
        let fake = FakeKeychain::new("long");
        let value = "x".repeat(STDIN_LINE_LIMIT);

        add(&fake.security, SERVICE, ACCOUNT, value.as_bytes()).expect("the item is written");

        let hex = super::hex(value.as_bytes()).as_str().to_owned();
        assert_eq!(
            fake.argv(),
            [format!(
                "add-generic-password -U -a {ACCOUNT} -s {SERVICE} -X {hex}"
            )],
            "a line `security -i` would cut short goes in the arguments"
        );
        assert!(fake.stdin().is_empty(), "nothing is sent on stdin");
        assert_eq!(fake.item().as_deref(), Some(value.as_bytes()));
    }

    #[test]
    fn add_refuses_a_value_that_is_not_printable() {
        let fake = FakeKeychain::new("not-printable");
        let value = r#"{"claudeAiOauth":{"accessToken":"fake-access-secret"}}"#.to_owned() + "\n";

        let refused = add(&fake.security, SERVICE, ACCOUNT, value.as_bytes());

        assert_eq!(
            refused,
            Err(Failure::NotPrintable),
            "a value `security -w` would read back as hex is refused before any call"
        );
        assert!(fake.argv().is_empty(), "`security` was never run");
        assert_eq!(fake.item(), None, "no item was written");
    }
}
