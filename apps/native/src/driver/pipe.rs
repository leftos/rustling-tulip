//! The driver's named pipe, `rt-driver-<pid>`: one JSON request per line in,
//! one reply line out per request, in order. Only the current user can open
//! it, and only from this machine.
//!
//! The pipe runs on its own `driver-pipe` thread. Each request is handed to
//! the UI thread's dispatcher as a [`Call`], and its reply is written before
//! the next line of that connection is read.

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
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

use crate::driver::protocol::{ErrorCode, Reply, Request};
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

/// Answers one connection's lines in order until it closes, or until a
/// line runs past [`MAX_LINE`].
async fn connection(stream: Stream, calls: UnboundedSender<Call>) {
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let mut line = Vec::new();
    loop {
        line.clear();
        let limit = u64::try_from(MAX_LINE + 1).unwrap_or(u64::MAX);
        let read = (&mut reader).take(limit).read_until(b'\n', &mut line).await;
        match read {
            Ok(0) => return,
            Ok(_) => {}
            Err(err) => {
                tracing::debug!("driver pipe: reading a request: {err}");
                return;
            }
        }
        let terminated = line.last() == Some(&b'\n');
        if terminated {
            line.pop();
        } else if line.len() > MAX_LINE {
            let reply = Reply::error(
                0,
                ErrorCode::BadParams,
                "the request line is longer than 64 KiB".to_owned(),
                None,
            );
            if let Err(err) = write_reply(&mut write, &reply).await {
                tracing::warn!("driver pipe: writing the over-long line's refusal: {err:#}");
            }
            return;
        }
        let reply = answer(&line, &calls).await;
        if let Err(err) = write_reply(&mut write, &reply).await {
            tracing::warn!("driver pipe: writing a reply: {err:#}");
            return;
        }
    }
}

/// The reply to one request line: decoded and dispatched, or refused.
async fn answer(line: &[u8], calls: &UnboundedSender<Call>) -> Reply {
    let Ok(text) = std::str::from_utf8(line) else {
        return Reply::error(
            0,
            ErrorCode::BadParams,
            "the request line is not UTF-8".to_owned(),
            None,
        );
    };
    let request = match Request::decode(text) {
        Ok(request) => request,
        Err(reply) => return reply,
    };
    let id = request.id;
    let (reply, answered) = oneshot::channel();
    if calls.unbounded_send(Call { request, reply }).is_err() {
        return closing(id);
    }
    answered.await.unwrap_or_else(|_| closing(id))
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
    use crate::driver::protocol::Reply;

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
}
