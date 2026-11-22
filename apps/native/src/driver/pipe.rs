//! The driver's named pipe, `rt-driver-<pid>`: one JSON request per line in,
//! one reply line out per request, in line order. Only the current user can
//! open it, and only from this machine.
//!
//! The pipe runs on its own `driver-pipe` thread. Each request is handed to
//! the UI thread's dispatcher as a [`Call`] with a cancel token, and the
//! connection keeps reading lines while replies are pending. A `cancel` line
//! is answered here, never by the dispatcher, and its reply is written at
//! once: it fires the token of the named request pending on the same
//! connection. A connection that ends fires every token it holds.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use futures::channel::mpsc::UnboundedSender;
use futures::channel::oneshot;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions};
use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
use interprocess::os::windows::security_descriptor::{
    AsSecurityDescriptorExt as _, BorrowedSecurityDescriptor, SecurityDescriptor,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio_util::sync::CancellationToken;

use crate::driver::protocol::{CancelParams, ErrorCode, Reply, Request};
use crate::offscreen::DRIVER_REFUSED_EXIT;

/// The longest request line read, without its `\n`.
pub const MAX_LINE: usize = 64 * 1024;

/// The wait after a failed accept, doubled after each failure in a row.
const ACCEPT_BACKOFF_FIRST: Duration = Duration::from_millis(100);

/// The longest wait between accepts after failures in a row.
const ACCEPT_BACKOFF_CAP: Duration = Duration::from_secs(2);

/// One request for the dispatcher, and where its reply goes.
pub struct Call {
    pub request: Request,
    pub reply: oneshot::Sender<Reply>,
    /// Fired when the client cancels the request, or its connection ends.
    pub cancel: CancellationToken,
}

/// The pipe name of the client whose process id is `pid`.
#[must_use]
pub fn pipe_name(pid: u32) -> String {
    format!("rt-driver-{pid}")
}

/// Starts the `driver-pipe` thread, which listens on this process's pipe
/// and hands each request to `calls`. A pipe that cannot be created ends the
/// process with [`DRIVER_REFUSED_EXIT`]: a driven run never listens under
/// another name.
///
/// The returned receiver gets `()` once the listener exists; when it cannot
/// be created, the sender is dropped and the process ends.
pub fn spawn(calls: UnboundedSender<Call>) -> oneshot::Receiver<()> {
    let name = pipe_name(std::process::id());
    let (listening, created) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("driver-pipe".to_owned())
        .spawn(move || {
            if let Err(err) = run_thread(&name, calls, listening) {
                exit_refused(&format!("the driver pipe {name}: {err:#}"));
            }
        });
    if let Err(err) = spawned {
        exit_refused(&format!("starting the driver pipe thread: {err}"));
    }
    created
}

/// The thread's body: a current-thread runtime that creates the listener,
/// says so on `listening`, and serves it until the process ends.
fn run_thread(
    name: &str,
    calls: UnboundedSender<Call>,
    listening: oneshot::Sender<()>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the driver pipe's runtime")?;
    runtime.block_on(async move {
        let descriptor = current_user_only()?;
        let accept = serve(name, descriptor, calls)?;
        tracing::info!(pipe = %name, "driver pipe listening");
        if listening.send(()).is_err() {
            tracing::debug!("driver pipe: the endpoint stopped waiting for the listener");
        }
        accept.await;
        Ok(())
    })
}

/// Ends the process before a driven run goes on without its pipe.
#[expect(
    clippy::exit,
    reason = "a driven run without its pipe must stop, never listen under another name"
)]
fn exit_refused(what: &str) -> ! {
    tracing::error!("driver run refused: {what}");
    std::process::exit(DRIVER_REFUSED_EXIT)
}

/// Creates the listener for `name` under `descriptor` and returns the loop
/// that accepts its connections, each served on a task of its own. It must
/// be called inside a tokio runtime, which the loop then runs on.
///
/// The listener refuses remote clients and fails when `name` is already
/// held (both `interprocess`'s defaults).
///
/// # Errors
///
/// When the name is invalid or the listener cannot be created.
pub fn serve(
    name: &str,
    descriptor: SecurityDescriptor,
    calls: UnboundedSender<Call>,
) -> io::Result<impl Future<Output = ()> + use<>> {
    let local = name.to_ns_name::<GenericNamespaced>()?;
    let listener = ListenerOptions::new()
        .name(local)
        .security_descriptor(descriptor)
        .create_tokio()?;
    Ok(async move {
        let mut backoff = ACCEPT_BACKOFF_FIRST;
        let mut last_error: Option<String> = None;
        loop {
            match listener.accept().await {
                Ok(stream) => {
                    backoff = ACCEPT_BACKOFF_FIRST;
                    last_error = None;
                    tokio::spawn(connection(stream, calls.clone()));
                }
                Err(err) => {
                    let text = err.to_string();
                    if last_error.as_deref() != Some(text.as_str()) {
                        tracing::warn!("driver pipe: accepting a connection: {text}");
                        last_error = Some(text);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(ACCEPT_BACKOFF_CAP);
                }
            }
        }
    })
}

/// Answers one connection's lines until it closes, or until a line runs
/// past [`MAX_LINE`]. Lines are read while replies are pending; replies go
/// out in line order, except a `cancel`'s, which is written at once. The
/// connection's end fires the token of every request it still has pending.
async fn connection(stream: Stream, calls: UnboundedSender<Call>) {
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = Lines::new(read);
    let mut connection = Connection::new(calls);
    loop {
        let reply = tokio::select! {
            biased;
            reply = connection.next_reply(), if connection.has_queued() => reply,
            line = lines.next() => match line {
                Line::Text(text) => match connection.take_line(&text) {
                    Some(reply) => reply,
                    None => continue,
                },
                Line::TooLong => {
                    refuse_too_long(&mut write).await;
                    return;
                }
                Line::Closed => return,
            },
        };
        if let Err(err) = write_reply(&mut write, &reply).await {
            tracing::warn!("driver pipe: writing a reply: {err:#}");
            return;
        }
    }
}

/// Writes the refusal of a line longer than [`MAX_LINE`].
async fn refuse_too_long<W>(write: &mut W)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let reply = Reply::error(
        0,
        ErrorCode::BadParams,
        "the request line is longer than 64 KiB".to_owned(),
        None,
    );
    if let Err(err) = write_reply(write, &reply).await {
        tracing::warn!("driver pipe: writing the over-long line's refusal: {err:#}");
    }
}

/// What a connection's next read came to.
enum Line {
    /// One whole line, without its `\n`.
    Text(Vec<u8>),
    /// A line that runs past [`MAX_LINE`].
    TooLong,
    /// End of file, a read error, or a last line the client never ended:
    /// never dispatched.
    Closed,
}

/// One connection's request lines. A read dropped before it ends keeps the
/// bytes it took (`read_until` appends to `line` as it goes), and the next
/// read carries on from them against what is left of the limit.
struct Lines<R> {
    reader: BufReader<R>,
    line: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> Lines<R> {
    fn new(read: R) -> Self {
        Self {
            reader: BufReader::new(read),
            line: Vec::new(),
        }
    }

    /// The next line. Cancel safe: a partial line stays in `self.line`.
    async fn next(&mut self) -> Line {
        if self.line.len() > MAX_LINE {
            return Line::TooLong;
        }
        let budget = u64::try_from(MAX_LINE + 1 - self.line.len()).unwrap_or(u64::MAX);
        let read = (&mut self.reader)
            .take(budget)
            .read_until(b'\n', &mut self.line)
            .await;
        match read {
            Ok(0) => Line::Closed,
            Ok(_) if self.line.last() == Some(&b'\n') => {
                self.line.pop();
                Line::Text(std::mem::take(&mut self.line))
            }
            Ok(_) if self.line.len() > MAX_LINE => Line::TooLong,
            // Under the limit with no `\n`: the stream ended mid-line.
            Ok(_) => Line::Closed,
            Err(err) => {
                tracing::debug!("driver pipe: reading a request: {err}");
                Line::Closed
            }
        }
    }
}

/// One entry of a connection's reply queue, in line order.
enum Queued {
    /// A reply the pipe made itself.
    Ready(Reply),
    /// A request handed to the dispatcher, whose reply is still to come.
    Pending {
        id: u64,
        answer: oneshot::Receiver<Reply>,
    },
}

/// One connection's replies in line order, and the cancel token of each
/// request it has pending, by id. Dropping it fires every token it holds.
struct Connection {
    calls: UnboundedSender<Call>,
    queue: VecDeque<Queued>,
    tokens: HashMap<u64, CancellationToken>,
}

impl Connection {
    fn new(calls: UnboundedSender<Call>) -> Self {
        Self {
            calls,
            queue: VecDeque::new(),
            tokens: HashMap::new(),
        }
    }

    /// Takes one request line: a `cancel`'s reply is returned, to be written
    /// at once; any other line's reply is queued behind those before it.
    fn take_line(&mut self, line: &[u8]) -> Option<Reply> {
        let request = match decode(line) {
            Ok(request) => request,
            Err(reply) => {
                self.queue.push_back(Queued::Ready(reply));
                return None;
            }
        };
        if request.method == "cancel" {
            return Some(self.cancel(&request));
        }
        self.dispatch(request);
        None
    }

    /// The reply to a `cancel`: whether it named a request still pending on
    /// this connection, whose token it fires.
    fn cancel(&self, request: &Request) -> Reply {
        match request.params::<CancelParams>() {
            Ok(CancelParams { request: target }) => {
                let token = self.tokens.get(&target);
                if let Some(token) = token {
                    token.cancel();
                }
                Reply::ok(request.id, json!({"cancelled": token.is_some()}))
            }
            Err(reply) => reply,
        }
    }

    /// Hands `request` to the dispatcher with a token of its own, and queues
    /// its pending reply; an id already pending here is refused instead.
    fn dispatch(&mut self, request: Request) {
        let id = request.id;
        if self.tokens.contains_key(&id) {
            let refused = Reply::error(
                id,
                ErrorCode::BadParams,
                format!("request {id} is already pending on this connection"),
                None,
            );
            self.queue.push_back(Queued::Ready(refused));
            return;
        }
        let cancel = CancellationToken::new();
        self.tokens.insert(id, cancel.clone());
        let (reply, answer) = oneshot::channel();
        let call = Call {
            request,
            reply,
            cancel,
        };
        if self.calls.unbounded_send(call).is_err() {
            self.tokens.remove(&id);
            self.queue.push_back(Queued::Ready(closing(id)));
            return;
        }
        self.queue.push_back(Queued::Pending { id, answer });
    }

    fn has_queued(&self) -> bool {
        !self.queue.is_empty()
    }

    /// The reply at the head of the queue once it is ready, taken off the
    /// queue, its token with it. Cancel safe: the head stays until its reply
    /// is taken. Polled only while the queue is not empty.
    async fn next_reply(&mut self) -> Reply {
        if let Some(Queued::Pending { id, answer }) = self.queue.front_mut() {
            let id = *id;
            let answered = answer.await;
            self.queue.pop_front();
            self.tokens.remove(&id);
            return answered.unwrap_or_else(|_| closing(id));
        }
        match self.queue.pop_front() {
            Some(Queued::Ready(reply)) => reply,
            // A pending head returned above, and an empty queue is not polled.
            Some(Queued::Pending { .. }) | None => std::future::pending().await,
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        for (_, token) in self.tokens.drain() {
            token.cancel();
        }
    }
}

/// The request one line holds, or the reply refusing it.
fn decode(line: &[u8]) -> Result<Request, Reply> {
    let text = std::str::from_utf8(line).map_err(|_| {
        Reply::error(
            0,
            ErrorCode::BadParams,
            "the request line is not UTF-8".to_owned(),
            None,
        )
    })?;
    Request::decode(text)
}

/// The reply to request `id` once the dispatcher is gone.
fn closing(id: u64) -> Reply {
    Reply::error(
        id,
        ErrorCode::Refused,
        "the client is closing".to_owned(),
        None,
    )
}

async fn write_reply<W>(write: &mut W, reply: &Reply) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(reply).context("encoding the reply")?;
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await?;
    Ok(())
}

/// A security descriptor whose DACL grants the current user, and no one
/// else, full access: `D:P(A;;GA;;;<user SID>)`. The user's SID is named
/// rather than `OW` or `CO`, because an elevated token's owner is the
/// Administrators group.
///
/// # Errors
///
/// When the process token's user cannot be read or the descriptor built.
pub fn current_user_only() -> Result<SecurityDescriptor> {
    let sid = current_user_sid()?;
    descriptor_from_sddl(&format!("D:P(A;;GA;;;{sid})"))
}

/// The current process token's user SID, as a string (`S-1-5-21-…`).
fn current_user_sid() -> Result<String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::PWSTR;

    let mut token = HANDLE::default();
    // SAFETY: the current process's pseudo handle and an out-pointer to a
    // local.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }
        .context("opening the process token")?;
    let mut needed = 0u32;
    // SAFETY: a size query on the token just opened; it fails with
    // ERROR_INSUFFICIENT_BUFFER and fills `needed`.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &raw mut needed) };
    // u64 words keep the buffer aligned for TOKEN_USER's pointer.
    let words = usize::try_from(needed)?.div_ceil(size_of::<u64>()).max(1);
    let mut buffer = vec![0u64; words];
    let length = u32::try_from(words * size_of::<u64>())?;
    // SAFETY: `buffer` holds `length` bytes, aligned for TOKEN_USER.
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &raw mut needed,
        )
    };
    // SAFETY: the token handle opened above, closed once.
    let _ = unsafe { CloseHandle(token) };
    read.context("reading the token's user")?;
    // SAFETY: GetTokenInformation wrote a TOKEN_USER at the buffer's start.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    // SAFETY: the SID lives in `buffer`; `text` receives a LocalAlloc'd
    // string, freed below.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut text) }
        .context("converting the user's SID to a string")?;
    // SAFETY: ConvertSidToStringSidW wrote a NUL-terminated string.
    let sid = unsafe { text.to_string() };
    // SAFETY: the string ConvertSidToStringSidW allocated, freed once.
    let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    sid.map_err(|err| anyhow!("the user's SID is not UTF-16: {err}"))
}

/// The owned security descriptor that the SDDL string `sddl` describes.
fn descriptor_from_sddl(sddl: &str) -> Result<SecurityDescriptor> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;
    use windows::core::HSTRING;

    let text = HSTRING::from(sddl);
    let mut relative = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `text` is NUL-terminated and outlives the call; `relative`
    // receives a LocalAlloc'd descriptor, freed below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &text,
            SDDL_REVISION_1,
            &raw mut relative,
            None,
        )
    }
    .with_context(|| format!("building the security descriptor {sddl}"))?;
    // SAFETY: `relative` is the valid self-relative descriptor just built;
    // the copy owns its contents, so the original is freed after it.
    let owned =
        unsafe { BorrowedSecurityDescriptor::from_ptr(relative.0.cast_const()) }.to_owned_sd();
    // SAFETY: the descriptor the conversion allocated, freed once.
    let _ = unsafe { LocalFree(Some(HLOCAL(relative.0))) };
    owned.context("copying the security descriptor")
}

#[cfg(all(test, windows))]
#[expect(
    clippy::expect_used,
    reason = "a spec fails with the message of the step it lost"
)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use futures::StreamExt as _;
    use futures::channel::mpsc;
    use interprocess::local_socket::GenericNamespaced;
    use interprocess::local_socket::tokio::Stream;
    use interprocess::local_socket::tokio::prelude::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::{Call, MAX_LINE, current_user_only, serve};
    use crate::driver::protocol::{ErrorCode, Reply};

    /// How long a spec waits on a reply line before it fails as hung.
    const HANG_GUARD: Duration = Duration::from_secs(10);

    /// A pipe name no other spec or client uses.
    fn unique_name() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        format!("rt-driver-test-{}-{n}", std::process::id())
    }

    /// Serves a pipe whose dispatcher answers `ping` with a fixed result and
    /// returns its name; `with_dispatcher` false drops the dispatcher.
    fn start(with_dispatcher: bool) -> String {
        let name = unique_name();
        let (calls, mut received) = mpsc::unbounded::<Call>();
        let accept = serve(&name, current_user_only().expect("a descriptor"), calls)
            .expect("the listener is created");
        tokio::spawn(accept);
        if with_dispatcher {
            tokio::spawn(async move {
                while let Some(call) = received.next().await {
                    let reply = Reply::ok(call.request.id, json!({"pong": true}));
                    let _ = call.reply.send(reply);
                }
            });
        }
        name
    }

    async fn connect(
        name: &str,
    ) -> (
        BufReader<tokio::io::ReadHalf<Stream>>,
        tokio::io::WriteHalf<Stream>,
    ) {
        let local = name.to_ns_name::<GenericNamespaced>().expect("a pipe name");
        let stream = Stream::connect(local).await.expect("the pipe accepts");
        let (read, write) = tokio::io::split(stream);
        (BufReader::new(read), write)
    }

    /// The next reply line, decoded.
    async fn reply_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(HANG_GUARD, reader.read_line(&mut line))
            .await
            .expect("hung: no reply line within the hang guard")
            .expect("reading the reply");
        assert!(read > 0, "the pipe closed before it replied");
        assert!(line.ends_with('\n'), "a reply is one line: {line:?}");
        serde_json::from_str(&line).expect("the reply is JSON")
    }

    #[tokio::test]
    async fn driver_pipe_answers_ping() {
        let name = start(true);
        let (mut reader, mut writer) = connect(&name).await;
        writer
            .write_all(b"{\"id\":3,\"method\":\"ping\"}\n")
            .await
            .expect("writes");
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 3, "result": {"pong": true}})
        );
    }

    #[tokio::test]
    async fn driver_pipe_answers_two_lines_in_order() {
        let name = start(true);
        let (mut reader, mut writer) = connect(&name).await;
        writer
            .write_all(b"{\"id\":1,\"method\":\"ping\"}\nnot json\n")
            .await
            .expect("writes");
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 1, "result": {"pong": true}})
        );
        let second = reply_line(&mut reader).await;
        assert_eq!(second["id"], json!(0), "{second}");
        assert_eq!(second["error"]["code"], json!("bad_params"), "{second}");
    }

    #[tokio::test]
    async fn driver_pipe_refuses_an_over_long_line() {
        let name = start(true);
        let (mut reader, mut writer) = connect(&name).await;
        let mut long = vec![b' '; MAX_LINE + 1];
        long.push(b'\n');
        writer.write_all(&long).await.expect("writes");
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 0, "error": {
                "code": "bad_params",
                "message": "the request line is longer than 64 KiB",
            }})
        );
        let mut rest = String::new();
        let read = tokio::time::timeout(HANG_GUARD, reader.read_line(&mut rest))
            .await
            .expect("hung: the connection did not close");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "the connection closes after the refusal: {rest:?}"
        );
    }

    #[tokio::test]
    async fn driver_pipe_answers_a_closed_dispatcher_with_refused() {
        let name = start(false);
        let (mut reader, mut writer) = connect(&name).await;
        writer
            .write_all(b"{\"id\":9,\"method\":\"ping\"}\n")
            .await
            .expect("writes");
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 9, "error": {"code": "refused", "message": "the client is closing"}})
        );
    }

    /// Serves a pipe whose calls the spec answers itself, and returns its
    /// name and the calls as they arrive.
    fn start_held() -> (String, mpsc::UnboundedReceiver<Call>) {
        let name = unique_name();
        let (calls, received) = mpsc::unbounded::<Call>();
        let accept = serve(&name, current_user_only().expect("a descriptor"), calls)
            .expect("the listener is created");
        tokio::spawn(accept);
        (name, received)
    }

    /// The next call the pipe hands its dispatcher.
    async fn next_call(received: &mut mpsc::UnboundedReceiver<Call>) -> Call {
        tokio::time::timeout(HANG_GUARD, received.next())
            .await
            .expect("hung: no call within the hang guard")
            .expect("the pipe holds its sender")
    }

    /// Waits until `call`'s token has fired.
    async fn fired(call: &Call) {
        tokio::time::timeout(HANG_GUARD, call.cancel.cancelled())
            .await
            .expect("hung: the call's token did not fire");
    }

    /// Answers `call` as `ping`'s dispatcher does.
    fn pong(call: Call) {
        let reply = Reply::ok(call.request.id, json!({"pong": true}));
        assert!(call.reply.send(reply).is_ok(), "the pipe awaits the reply");
    }

    async fn send(writer: &mut tokio::io::WriteHalf<Stream>, lines: &[u8]) {
        writer.write_all(lines).await.expect("writes");
    }

    #[tokio::test]
    async fn driver_pipe_cancel_fires_a_held_requests_token() {
        let (name, mut received) = start_held();
        let (mut reader, mut writer) = connect(&name).await;
        send(&mut writer, b"{\"id\":3,\"method\":\"ping\"}\n").await;
        let held = next_call(&mut received).await;
        assert!(!held.cancel.is_cancelled(), "nothing cancelled it yet");
        send(
            &mut writer,
            b"{\"id\":7,\"method\":\"cancel\",\"params\":{\"request\":3}}\n",
        )
        .await;
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 7, "result": {"cancelled": true}})
        );
        fired(&held).await;
    }

    #[tokio::test]
    async fn driver_pipe_answers_the_cancel_before_the_request_it_cancelled() {
        let (name, mut received) = start_held();
        let (mut reader, mut writer) = connect(&name).await;
        send(
            &mut writer,
            b"{\"id\":3,\"method\":\"ping\"}\n{\"id\":7,\"method\":\"cancel\",\"params\":{\"request\":3}}\n",
        )
        .await;
        let held = next_call(&mut received).await;
        fired(&held).await;
        let refused = Reply::error(3, ErrorCode::Refused, "cancelled".to_owned(), None);
        assert!(
            held.reply.send(refused).is_ok(),
            "the pipe awaits the reply"
        );
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 7, "result": {"cancelled": true}})
        );
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 3, "error": {"code": "refused", "message": "cancelled"}})
        );
    }

    #[tokio::test]
    async fn driver_pipe_cancel_after_the_reply_answers_false() {
        let name = start(true);
        let (mut reader, mut writer) = connect(&name).await;
        send(&mut writer, b"{\"id\":3,\"method\":\"ping\"}\n").await;
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 3, "result": {"pong": true}})
        );
        send(
            &mut writer,
            b"{\"id\":7,\"method\":\"cancel\",\"params\":{\"request\":3}}\n",
        )
        .await;
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 7, "result": {"cancelled": false}})
        );
    }

    #[tokio::test]
    async fn driver_pipe_cancel_of_an_unknown_id_answers_false() {
        let name = start(true);
        let (mut reader, mut writer) = connect(&name).await;
        send(
            &mut writer,
            b"{\"id\":7,\"method\":\"cancel\",\"params\":{\"request\":99}}\n",
        )
        .await;
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 7, "result": {"cancelled": false}})
        );
    }

    #[tokio::test]
    async fn driver_pipe_cancel_never_reaches_another_connection() {
        let (name, mut received) = start_held();
        let (mut held_reader, mut held_writer) = connect(&name).await;
        send(&mut held_writer, b"{\"id\":3,\"method\":\"ping\"}\n").await;
        let held = next_call(&mut received).await;
        let (mut other_reader, mut other_writer) = connect(&name).await;
        send(
            &mut other_writer,
            b"{\"id\":7,\"method\":\"cancel\",\"params\":{\"request\":3}}\n",
        )
        .await;
        assert_eq!(
            reply_line(&mut other_reader).await,
            json!({"id": 7, "result": {"cancelled": false}})
        );
        assert!(
            !held.cancel.is_cancelled(),
            "a cancel on one connection leaves another's token alone"
        );
        pong(held);
        assert_eq!(
            reply_line(&mut held_reader).await,
            json!({"id": 3, "result": {"pong": true}})
        );
    }

    #[tokio::test]
    async fn driver_pipe_refuses_a_malformed_cancel() {
        let (name, mut received) = start_held();
        let (mut reader, mut writer) = connect(&name).await;
        send(&mut writer, b"{\"id\":3,\"method\":\"ping\"}\n").await;
        let held = next_call(&mut received).await;
        send(
            &mut writer,
            b"{\"id\":7,\"method\":\"cancel\",\"params\":{}}\n{\"id\":8,\"method\":\"cancel\",\"params\":{\"request\":\"3\"}}\n",
        )
        .await;
        for id in [7, 8] {
            let refused = reply_line(&mut reader).await;
            assert_eq!(refused["id"], json!(id), "{refused}");
            assert_eq!(refused["error"]["code"], json!("bad_params"), "{refused}");
        }
        assert!(
            !held.cancel.is_cancelled(),
            "a malformed cancel fires nothing"
        );
        pong(held);
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 3, "result": {"pong": true}})
        );
    }

    #[tokio::test]
    async fn driver_pipe_close_fires_every_pending_token() {
        let (name, mut received) = start_held();
        let (reader, mut writer) = connect(&name).await;
        send(
            &mut writer,
            b"{\"id\":3,\"method\":\"ping\"}\n{\"id\":4,\"method\":\"ping\"}\n",
        )
        .await;
        let first = next_call(&mut received).await;
        let second = next_call(&mut received).await;
        drop(writer);
        drop(reader);
        fired(&first).await;
        fired(&second).await;
    }

    #[tokio::test]
    async fn driver_pipe_refuses_a_duplicate_pending_id() {
        let (name, mut received) = start_held();
        let (mut reader, mut writer) = connect(&name).await;
        send(
            &mut writer,
            b"{\"id\":3,\"method\":\"ping\"}\n{\"id\":3,\"method\":\"ping\"}\n{\"id\":4,\"method\":\"ping\"}\n",
        )
        .await;
        let first = next_call(&mut received).await;
        let marker = next_call(&mut received).await;
        assert_eq!(first.request.id, 3);
        assert_eq!(marker.request.id, 4, "the duplicate is not dispatched");
        pong(first);
        pong(marker);
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 3, "result": {"pong": true}})
        );
        let duplicate = reply_line(&mut reader).await;
        assert_eq!(duplicate["id"], json!(3), "{duplicate}");
        assert_eq!(
            duplicate["error"]["code"],
            json!("bad_params"),
            "{duplicate}"
        );
        assert_eq!(
            reply_line(&mut reader).await,
            json!({"id": 4, "result": {"pong": true}})
        );
    }

    #[tokio::test]
    async fn driver_pipe_reuses_an_id_once_answered() {
        let name = start(true);
        let (mut reader, mut writer) = connect(&name).await;
        for _ in 0..2 {
            send(&mut writer, b"{\"id\":3,\"method\":\"ping\"}\n").await;
            assert_eq!(
                reply_line(&mut reader).await,
                json!({"id": 3, "result": {"pong": true}})
            );
        }
    }
}
