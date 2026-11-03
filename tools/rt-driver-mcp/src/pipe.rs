//! The client side of a driven client's pipe: one request line out, the
//! reply line with the same id back, within [`REPLY_GUARD`].

use std::fs::File;
use std::io::{BufRead as _, BufReader, Write as _};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long a call waits for its reply: longer than the endpoint's longest
/// wait (60 s), so the transport never cuts a full-length wait short.
pub const REPLY_GUARD: Duration = Duration::from_secs(70);

/// How long opening the pipe retries while every instance is busy.
const BUSY_GUARD: Duration = Duration::from_secs(5);

/// `ERROR_PIPE_BUSY`: every instance of the pipe is serving a client.
const ERROR_PIPE_BUSY: i32 = 231;

/// Why a call to the endpoint brought no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The endpoint answered with an error.
    Endpoint {
        code: String,
        message: String,
        hint: Option<String>,
    },
    /// No answer: the pipe could not be opened, closed, or the reply did
    /// not come within [`REPLY_GUARD`].
    Transport(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Endpoint {
                code,
                message,
                hint,
            } => {
                write!(f, "{code}: {message}")?;
                match hint {
                    Some(hint) => write!(f, "\nhint: {hint}"),
                    None => Ok(()),
                }
            }
            Self::Transport(why) => write!(f, "transport: {why}"),
        }
    }
}

/// One call at a time to a driver endpoint.
pub trait Transport {
    /// Calls `method` with `params` and returns the endpoint's `result`.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError>;
}

/// A driven client's pipe, opened on first use and again after a call that
/// ended uncleanly.
pub struct PipeClient {
    path: String,
    connection: Option<BufReader<File>>,
    next_id: u64,
}

impl PipeClient {
    /// The client of the pipe at `path` (`\\.\pipe\rt-driver-<pid>`);
    /// nothing is opened yet.
    #[must_use]
    pub fn new(path: String) -> Self {
        Self {
            path,
            connection: None,
            next_id: 1,
        }
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Writes `line` and reads lines until the reply to `id` (or to the
    /// reserved id 0, a refusal of the line itself). A failure drops the
    /// connection, so a late reply is never read as the next call's.
    fn exchange(&mut self, line: &str, id: u64) -> Result<Value, String> {
        let mut connection = match self.connection.take() {
            Some(connection) => connection,
            None => open(&self.path)?,
        };
        let pipe = connection.get_mut();
        pipe.write_all(line.as_bytes())
            .and_then(|()| pipe.flush())
            .map_err(|err| format!("writing to {}: {err}", self.path))?;
        let deadline = Instant::now() + REPLY_GUARD;
        loop {
            let (back, text) = read_line_by(connection, deadline)?;
            connection = back;
            let reply: Value = serde_json::from_str(&text)
                .map_err(|err| format!("the endpoint's reply is not JSON ({err}): {text}"))?;
            match reply.get("id").and_then(Value::as_u64) {
                Some(got) if got == id || got == 0 => {
                    self.connection = Some(connection);
                    return Ok(reply);
                }
                other => tracing::warn!(?other, id, "dropping a reply to another request"),
            }
        }
    }
}

impl Transport for PipeClient {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = json!({"id": id, "method": method, "params": params}).to_string();
        line.push('\n');
        let reply = self.exchange(&line, id).map_err(CallError::Transport)?;
        outcome(reply)
    }
}

/// The `result` of an endpoint reply, or its `error`.
fn outcome(mut reply: Value) -> Result<Value, CallError> {
    if let Some(result) = reply.get_mut("result") {
        return Ok(result.take());
    }
    let error = &reply["error"];
    let text = |key: &str| error[key].as_str().map(str::to_owned);
    match (text("code"), text("message")) {
        (Some(code), Some(message)) => Err(CallError::Endpoint {
            code,
            message,
            hint: text("hint"),
        }),
        _ => Err(CallError::Transport(format!(
            "the endpoint's reply has neither a result nor an error: {reply}"
        ))),
    }
}

/// Opens the pipe, retrying while every instance is busy.
fn open(path: &str) -> Result<BufReader<File>, String> {
    let deadline = Instant::now() + BUSY_GUARD;
    loop {
        match File::options().read(true).write(true).open(path) {
            Ok(file) => return Ok(BufReader::new(file)),
            Err(err)
                if err.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(format!("opening {path}: {err}")),
        }
    }
}

/// One line read on a thread of its own, so the wait ends at `deadline`; a
/// read still pending then keeps the connection, which is dropped.
fn read_line_by(
    mut connection: BufReader<File>,
    deadline: Instant,
) -> Result<(BufReader<File>, String), String> {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = connection.read_line(&mut line);
        let _ = send.send((connection, read, line));
    });
    match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok((connection, Ok(read), line)) if read > 0 => Ok((connection, line)),
        Ok((_, Ok(_), _)) => Err("the pipe closed before the reply".to_owned()),
        Ok((_, Err(err), _)) => Err(format!("reading the reply: {err}")),
        Err(_) => Err(format!("hung: no reply within {REPLY_GUARD:?}")),
    }
}
