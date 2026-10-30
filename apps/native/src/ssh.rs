//! The SSH transport: find `ssh`, run it with a host's string and its remote
//! command, and speak the daemon protocol over the child's stdio as one
//! compact JSON message per line. The remote command bridges those lines to
//! the host's daemon.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures::{Sink, Stream, StreamExt as _};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::{self, Message};
use tracing::warn;

use crate::target::SshHost;

/// The variable naming the `ssh` binary to run; empty counts as unset.
pub const SSH_ENV: &str = "RUSTLING_TULIP_SSH";

/// The command run on the host when the saved host names none.
pub const DEFAULT_REMOTE_COMMAND: &str = "rustling-tulipd bridge";

/// The longest line read from the host; a longer one fails the connection.
const MAX_LINE: usize = 16 * 1024 * 1024;

/// How much of the end of `ssh`'s stderr is kept for a close reason.
const STDERR_TAIL: usize = 2048;

/// How long the end of stdout waits for stderr to end before the close
/// reason is built without the rest of it.
const STDERR_GRACE: Duration = Duration::from_secs(2);

/// The `ssh` options every connection runs with: no prompt ever, a dead link
/// found within 45 s, a connect given up after 10 s, and no compression.
const SSH_OPTIONS: [&str; 11] = [
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "Compression=no",
];

/// The `ssh` binary: [`SSH_ENV`] when set, else `ssh` on `PATH`, else
/// Windows' own `%SystemRoot%\System32\OpenSSH\ssh.exe` when it is there;
/// `None` when none is found.
#[must_use]
pub fn locate() -> Option<PathBuf> {
    locate_with(|name| std::env::var_os(name), Path::is_file)
}

/// [`locate`] over the environment `env` and the file test `is_file`.
fn locate_with(
    env: impl Fn(&str) -> Option<OsString>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(binary) = env(SSH_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(binary));
    }
    let exe = if cfg!(windows) { "ssh.exe" } else { "ssh" };
    let on_path = env("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(exe))
            .find(|candidate| is_file(candidate))
    });
    on_path.or_else(|| {
        let system = PathBuf::from(env("SystemRoot")?)
            .join("System32")
            .join("OpenSSH")
            .join("ssh.exe");
        is_file(&system).then_some(system)
    })
}

/// The arguments `ssh` runs with for `host`: the fixed options, then `--`
/// so the host string is never read as an option, the host string, and the
/// remote command as one argument.
fn ssh_args(host: &SshHost) -> Vec<OsString> {
    let command = host.remote_command().unwrap_or(DEFAULT_REMOTE_COMMAND);
    SSH_OPTIONS
        .iter()
        .copied()
        .chain(["--", host.name(), command])
        .map(OsString::from)
        .collect()
}

/// Start `ssh` to `host` and wrap its stdio as a [`LineSocket`]. The child
/// is killed when the socket is dropped.
///
/// # Errors
///
/// When no `ssh` is found or it cannot be started.
pub fn open(host: &SshHost) -> Result<LineSocket> {
    let binary = locate().with_context(|| {
        format!("ssh was not found: install the OpenSSH client or set {SSH_ENV} to its path")
    })?;
    let mut command = tokio::process::Command::new(&binary);
    command
        .args(ssh_args(host))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {} for {}", binary.display(), host.name()))?;
    let stdout = child.stdout.take().context("ssh has no stdout")?;
    let stdin = child.stdin.take().context("ssh has no stdin")?;
    let stderr = child.stderr.take().context("ssh has no stderr")?;
    let mut socket = LineSocket::new(stdout, stdin, stderr);
    socket.child = Some(child);
    Ok(socket)
}

/// The last [`STDERR_TAIL`] bytes a child wrote to stderr.
#[derive(Debug, Clone, Default)]
pub struct StderrTail(Arc<Mutex<Vec<u8>>>);

impl StderrTail {
    fn push(&self, bytes: &[u8]) {
        let mut tail = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        tail.extend_from_slice(bytes);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        tail.drain(..excess);
    }

    /// The tail as text, trimmed; empty when nothing was written.
    #[must_use]
    pub fn text(&self) -> String {
        let tail = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&tail).trim().to_owned()
    }
}

type Lines = Pin<Box<dyn Stream<Item = Result<Message, tungstenite::Error>>>>;
type Writes = Pin<Box<dyn Sink<Message, Error = tungstenite::Error>>>;

/// A duplex of daemon messages over a byte stream pair: each received line
/// is one `Text` message (empty lines skipped), and each sent `Text` message
/// is written as one line and flushed. Only text is sent.
pub struct LineSocket {
    lines: Lines,
    writes: Writes,
    stderr: StderrTail,
    /// The `ssh` child the streams belong to, killed on drop.
    child: Option<tokio::process::Child>,
}

impl LineSocket {
    /// Lines read from `reader`, written to `writer`, with `stderr` read
    /// alongside into [`LineSocket::stderr_tail`]. The reader's end waits for
    /// stderr to end, so the tail is whole when the stream ends. Must run
    /// inside a tokio runtime, which reads stderr.
    #[must_use]
    pub fn new<R, W, E>(reader: R, writer: W, stderr: E) -> Self
    where
        R: AsyncRead + Unpin + 'static,
        W: AsyncWrite + Unpin + 'static,
        E: AsyncRead + Unpin + Send + 'static,
    {
        let tail = StderrTail::default();
        let drained = tokio::spawn(drain_stderr(stderr, tail.clone()));
        let lines = futures::stream::unfold(
            (BufReader::new(reader), drained),
            |(mut reader, drained)| async move {
                let item = read_message(&mut reader).await;
                if item.is_none() {
                    wait_for_stderr(drained).await;
                    return None;
                }
                item.map(|item| (item, (reader, drained)))
            },
        )
        .fuse();
        let writes = futures::sink::unfold(writer, |mut writer, message: Message| async move {
            write_message(&mut writer, message).await?;
            Ok::<_, tungstenite::Error>(writer)
        });
        Self {
            lines: Box::pin(lines),
            writes: Box::pin(writes),
            stderr: tail,
            child: None,
        }
    }

    /// The end of what the child wrote to stderr so far.
    #[must_use]
    pub fn stderr_tail(&self) -> StderrTail {
        self.stderr.clone()
    }
}

/// Copy `stderr` into `tail` until it ends.
async fn drain_stderr<E: AsyncRead + Unpin>(mut stderr: E, tail: StderrTail) {
    let mut chunk = [0u8; 1024];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) => return,
            Ok(read) => tail.push(&chunk[..read]),
            Err(err) => {
                warn!("reading ssh's stderr: {err}");
                return;
            }
        }
    }
}

/// Wait up to [`STDERR_GRACE`] for stderr to end.
async fn wait_for_stderr(drained: JoinHandle<()>) {
    match tokio::time::timeout(STDERR_GRACE, drained).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!("the ssh stderr reader failed: {err}"),
        Err(_) => warn!("ssh's stderr stayed open after its stdout ended"),
    }
}

/// The next non-empty line as a `Text` message; `None` at the end of the
/// stream. A line over [`MAX_LINE`] bytes or not UTF-8 is an error.
async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Option<Result<Message, tungstenite::Error>> {
    loop {
        let mut line = Vec::new();
        let limit = u64::try_from(MAX_LINE + 1).unwrap_or(u64::MAX);
        match (&mut *reader)
            .take(limit)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) => return None,
            Ok(_) => {}
            Err(err) => return Some(Err(tungstenite::Error::Io(err))),
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        }
        if line.len() > MAX_LINE {
            return Some(Err(io_error(format!(
                "a line from the host is over {MAX_LINE} bytes"
            ))));
        }
        if line.is_empty() {
            continue;
        }
        return Some(match String::from_utf8(line) {
            Ok(text) => Ok(Message::Text(text.into())),
            Err(err) => Err(io_error(format!(
                "a line from the host is not UTF-8: {err}"
            ))),
        });
    }
}

/// Write a `Text` message as one line and flush it; any other message is
/// refused.
async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: Message,
) -> Result<(), tungstenite::Error> {
    let text = match message {
        Message::Text(text) => text,
        other => return Err(io_error(format!("only text goes over ssh, not {other:?}"))),
    };
    let mut line = Vec::with_capacity(text.len() + 1);
    line.extend_from_slice(text.as_bytes());
    line.push(b'\n');
    writer
        .write_all(&line)
        .await
        .map_err(tungstenite::Error::Io)?;
    writer.flush().await.map_err(tungstenite::Error::Io)?;
    Ok(())
}

fn io_error(message: String) -> tungstenite::Error {
    tungstenite::Error::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

impl Stream for LineSocket {
    type Item = Result<Message, tungstenite::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.lines.as_mut().poll_next(cx)
    }
}

impl Sink<Message> for LineSocket {
    type Error = tungstenite::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.writes.as_mut().poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        self.writes.as_mut().start_send(item)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.writes.as_mut().poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.writes.as_mut().poll_close(cx)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use futures::{SinkExt as _, StreamExt as _};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_tungstenite::tungstenite::Message;

    use super::{LineSocket, MAX_LINE, locate_with};

    /// How long a test waits on the socket before calling it hung.
    const BACKSTOP: Duration = Duration::from_secs(30);

    /// A socket over in-memory pipes: the host's stdout writer, its stdin
    /// reader, and the socket. Stderr is already ended.
    fn piped() -> (tokio::io::DuplexStream, tokio::io::DuplexStream, LineSocket) {
        let (host_out, client_in) = tokio::io::duplex(1 << 16);
        let (client_out, host_in) = tokio::io::duplex(1 << 16);
        let (_, stderr) = tokio::io::duplex(64);
        (
            host_out,
            host_in,
            LineSocket::new(client_in, client_out, stderr),
        )
    }

    async fn next(socket: &mut LineSocket) -> Option<Result<Message, String>> {
        tokio::time::timeout(BACKSTOP, socket.next())
            .await
            .expect("the socket hung")
            .map(|item| item.map_err(|err| err.to_string()))
    }

    #[tokio::test]
    async fn empty_lines_are_skipped_and_one_message_is_one_line() {
        let (mut host_out, _host_in, mut socket) = piped();
        host_out
            .write_all(b"\n{\"a\":1}\n\r\n\n{\"b\":2}\r\n{\"c\":3}")
            .await
            .expect("write lines");
        drop(host_out);
        assert_eq!(
            next(&mut socket).await,
            Some(Ok(Message::Text("{\"a\":1}".into())))
        );
        assert_eq!(
            next(&mut socket).await,
            Some(Ok(Message::Text("{\"b\":2}".into())))
        );
        assert_eq!(
            next(&mut socket).await,
            Some(Ok(Message::Text("{\"c\":3}".into()))),
            "a last line without a newline is still a message"
        );
        assert_eq!(
            next(&mut socket).await,
            None,
            "the end of stdout ends the stream"
        );
    }

    #[tokio::test]
    async fn a_message_is_written_with_a_newline_and_flushed() {
        let (_host_out, mut host_in, mut socket) = piped();
        tokio::time::timeout(BACKSTOP, socket.send(Message::Text("{\"x\":1}".into())))
            .await
            .expect("the send hung")
            .expect("send a text message");
        let mut written = vec![0u8; "{\"x\":1}\n".len()];
        tokio::time::timeout(BACKSTOP, host_in.read_exact(&mut written))
            .await
            .expect("the line never arrived")
            .expect("read the line");
        assert_eq!(written, b"{\"x\":1}\n");
        let refused = socket.send(Message::Binary(vec![1, 2].into())).await;
        assert!(refused.is_err(), "only text goes over the line transport");
    }

    #[tokio::test]
    async fn an_oversize_line_is_an_error() {
        let (mut host_out, _host_in, mut socket) = piped();
        let writer = tokio::spawn(async move {
            let long = vec![b'a'; MAX_LINE + 1];
            // The reader stops at the cap, so the rest of the write may fail.
            let _ = host_out.write_all(&long).await;
            let _ = host_out.write_all(b"\n").await;
        });
        let item = next(&mut socket).await;
        assert!(
            matches!(&item, Some(Err(reason)) if reason.contains("over 16777216 bytes")),
            "a line over 16 MiB fails the read: {item:?}"
        );
        drop(socket);
        writer.await.expect("the writer ends");
    }

    /// The binary [`locate_with`] finds with `vars` set and `files` present.
    fn locate(vars: &[(&str, OsString)], files: &[&Path]) -> Option<PathBuf> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        locate_with(|name| vars.get(name).cloned(), |path| files.contains(&path))
    }

    #[test]
    fn locate_prefers_the_env_var_then_path_then_system32() {
        let exe = if cfg!(windows) { "ssh.exe" } else { "ssh" };
        let bin = PathBuf::from("D:").join("bin");
        let tools = PathBuf::from("D:").join("tools");
        let path = std::env::join_paths([&bin, &tools]).expect("a PATH value");
        let system = PathBuf::from("C:\\Windows")
            .join("System32")
            .join("OpenSSH")
            .join("ssh.exe");
        let on_path = tools.join(exe);
        let all = [
            ("RUSTLING_TULIP_SSH", OsString::from("E:\\ssh\\ssh.exe")),
            ("PATH", path.clone()),
            ("SystemRoot", OsString::from("C:\\Windows")),
        ];
        let files = [on_path.as_path(), system.as_path()];
        assert_eq!(
            locate(&all, &files),
            Some(PathBuf::from("E:\\ssh\\ssh.exe"))
        );
        assert_eq!(
            locate(&all[1..], &files),
            Some(on_path.clone()),
            "the first PATH folder holding it"
        );
        assert_eq!(locate(&all[1..], &[system.as_path()]), Some(system.clone()));
        assert_eq!(locate(&all[1..], &[]), None, "nothing found is none");
        assert_eq!(
            locate(&all[1..2], &[system.as_path()]),
            None,
            "no SystemRoot, no System32 path"
        );
    }

    #[test]
    fn an_empty_env_var_counts_as_unset() {
        let system = PathBuf::from("C:\\Windows")
            .join("System32")
            .join("OpenSSH")
            .join("ssh.exe");
        let vars = [
            ("RUSTLING_TULIP_SSH", OsString::new()),
            ("SystemRoot", OsString::from("C:\\Windows")),
        ];
        assert_eq!(locate(&vars, &[system.as_path()]), Some(system));
    }
}
