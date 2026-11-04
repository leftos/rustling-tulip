//! `run`: one timed transfer of one fixture shape by one candidate, printed as
//! one result line.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead as _, BufReader, BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitCode, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail, ensure};
use base64::Engine as _;
use protocol::{ClientMessage, DaemonMessage, InboundDaemonMessage};

use crate::fixtures::{self, FixtureFile, Scale};
use crate::frame::{self, Reported};
use crate::guard::{self, HangGuard, Shared};
use crate::remote::{self, DAEMON_WRAPPER, HOST, ISOLATED_WRAPPER, REMOTE_BIN, TAR};
use crate::report::{self, RunResult};
use crate::serve::{Codec, FrameKind};
use crate::{args, cpu};

pub const CANDIDATES: [&str; 9] = [
    "fetch-seq",
    "fetch-n",
    "tar-none",
    "tar-zstd1",
    "tar-zstd3",
    "tar-zstd1-raw",
    "scp",
    "sftp",
    "ssh-tar",
];

const DEFAULT_TIMEOUT_S: u64 = 15 * 60;
const DEFAULT_IN_FLIGHT: usize = 4;
/// Daemon CPU readings 1 s apart before the timed fetch: 30 s at most.
const IDLE_POLLS: usize = 31;

struct Plan<'a> {
    files: &'a [FixtureFile],
    shape: &'a str,
    /// The shape's folder on the builder.
    remote: String,
    /// The shape's folder here; it does not exist before the run.
    local: PathBuf,
    timeout: Duration,
}

/// What a candidate measured, in one window: [`Probe::begin`] starts the
/// clock and the local CPU baseline together.
#[derive(Default)]
struct Probe {
    started: Option<Instant>,
    own_before: Option<u64>,
    /// The end of the window when it closes before the candidate returns.
    wall: Option<Duration>,
    child_cpu_ms: Vec<(&'static str, Option<u64>)>,
    /// Children started in the window that were never waited.
    unwaited: usize,
    remote_cpu_ms: Option<u64>,
    notes: Vec<String>,
    hung: bool,
}

impl Probe {
    fn begin(&mut self) {
        self.own_before = cpu::own_cpu_ms();
        self.started = Some(Instant::now());
    }

    fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    /// The local CPU over the window, or why it is unknown.
    fn local_cpu(&self, own_after: Option<u64>) -> Result<u64, String> {
        if self.started.is_none() {
            return Err("the timed window never opened".to_owned());
        }
        if self.unwaited > 0 {
            return Err(format!(
                "local CPU unknown: {} child process(es) were not waited",
                self.unwaited
            ));
        }
        let own = match (self.own_before, own_after) {
            (Some(before), Some(after)) => Some(after.saturating_sub(before)),
            _ => None,
        };
        let unknown: Vec<&str> = std::iter::once(("folder-bench", own))
            .chain(self.child_cpu_ms.iter().copied())
            .filter(|(_, ms)| ms.is_none())
            .map(|(name, _)| name)
            .collect();
        if !unknown.is_empty() {
            return Err(format!("local CPU unknown for {}", unknown.join(", ")));
        }
        cpu::sum(std::iter::once(own).chain(self.child_cpu_ms.iter().map(|(_, ms)| *ms)))
            .ok_or_else(|| "local CPU unknown".to_owned())
    }
}

/// `--in-flight` for `candidate`: the count the fetch candidates use, `None`
/// for the rest, which refuse the flag.
pub fn in_flight_for(candidate: &str, flag: Option<&str>) -> anyhow::Result<Option<usize>> {
    match (candidate, flag) {
        ("fetch-n", Some(n)) => {
            let n = n.parse::<usize>().context("--in-flight takes a count")?;
            ensure!(n >= 1, "--in-flight must be at least 1");
            Ok(Some(n))
        }
        ("fetch-n", None) => Ok(Some(DEFAULT_IN_FLIGHT)),
        ("fetch-seq", None) => Ok(Some(1)),
        (other, Some(_)) => {
            bail!("--in-flight does not apply to {other}; only fetch-n takes it")
        }
        (_, None) => Ok(None),
    }
}

/// The candidates that start processes on the builder a hang may leave.
fn runs_remote_processes(candidate: &str) -> bool {
    candidate.starts_with("tar-") || candidate.starts_with("fetch-") || candidate == "ssh-tar"
}

/// The error of a hung run: `hung`, the first error seen with it, and a
/// failed remote cleanup.
pub fn hung_error(first: Option<&str>, cleanup: Result<(), String>) -> String {
    let mut text = match first {
        Some(err) => format!("hung after: {err}"),
        None => "hung".to_owned(),
    };
    if let Err(why) = cleanup {
        text.push_str("; remote cleanup failed: ");
        text.push_str(&why);
    }
    text
}

pub fn command(rest: &[String]) -> anyhow::Result<ExitCode> {
    let parsed = args::parse(
        rest,
        &[
            "shape",
            "remote-dir",
            "dest",
            "in-flight",
            "scale",
            "timeout-s",
            "zstd",
        ],
    )?;
    let candidate = parsed.single("candidate")?;
    ensure!(
        CANDIDATES.contains(&candidate),
        "unknown candidate {candidate:?}; the candidates are {CANDIDATES:?}"
    );
    let in_flight = in_flight_for(candidate, parsed.flag("in-flight"))?;
    let shape = parsed.required("shape")?;
    let zstd_exe = parsed.flag("zstd").unwrap_or(remote::LOCAL_ZSTD);
    let scale = Scale::parse(parsed.flag("scale"))?;
    let files = fixtures::manifest(shape, scale)?;
    let remote_dir = parsed.required("remote-dir")?.trim_end_matches('\\');
    remote::check_remote_path(remote_dir)?;
    let dest = Path::new(parsed.required("dest")?);
    let timeout_s = match parsed.flag("timeout-s") {
        Some(s) => s.parse::<u64>().context("--timeout-s takes seconds")?,
        None => DEFAULT_TIMEOUT_S,
    };
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let local = dest.join(shape);
    ensure!(
        !local.exists(),
        "{} already exists; give each run a fresh --dest",
        local.display()
    );
    let plan = Plan {
        files: &files,
        shape,
        remote: format!(r"{remote_dir}\{shape}"),
        local,
        timeout: Duration::from_secs(timeout_s),
    };

    let mut probe = Probe::default();
    // The fetch candidates open their window after their untimed setup.
    if !candidate.starts_with("fetch-") {
        probe.begin();
    }
    let outcome = match candidate {
        "fetch-seq" | "fetch-n" => fetch(&plan, in_flight.unwrap_or(1), &mut probe),
        "tar-none" => tar_stream(&plan, Codec::None, FrameKind::Base64, zstd_exe, &mut probe),
        "tar-zstd1" => tar_stream(&plan, Codec::Zstd1, FrameKind::Base64, zstd_exe, &mut probe),
        "tar-zstd3" => tar_stream(&plan, Codec::Zstd3, FrameKind::Base64, zstd_exe, &mut probe),
        "tar-zstd1-raw" => tar_stream(&plan, Codec::Zstd1, FrameKind::Raw, zstd_exe, &mut probe),
        "scp" => scp(&plan, &mut probe),
        "sftp" => sftp(&plan, &mut probe),
        _ => ssh_tar(&plan, &mut probe),
    };
    let wall = probe
        .wall
        .or_else(|| probe.started.map(|started| started.elapsed()));
    let own_after = cpu::own_cpu_ms();
    let local_cpu = probe.local_cpu(own_after);

    let error = run_error(candidate, &plan, &probe, outcome);
    let mut notes = Vec::new();
    if let Err(why) = &local_cpu {
        notes.push(why.clone());
    }
    notes.append(&mut probe.notes);
    let result = RunResult {
        candidate: candidate.to_owned(),
        shape: shape.to_owned(),
        files: files.len(),
        bytes: files.iter().map(|f| f.len).sum(),
        in_flight,
        wall_ms: wall.map_or(0, |w| u64::try_from(w.as_millis()).unwrap_or(u64::MAX)),
        local_cpu_ms: local_cpu.ok(),
        remote_cpu_ms: probe.remote_cpu_ms,
        cpu_note: (!notes.is_empty()).then(|| notes.join("; ")),
        ok: error.is_none(),
        error,
    };
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &result)?;
    writeln!(out)?;
    Ok(if result.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The run's `error`: the hang (after stopping what it left on the builder),
/// the candidate's failure, or the first file that does not match its
/// fixture.
fn run_error(
    candidate: &str,
    plan: &Plan,
    probe: &Probe,
    outcome: anyhow::Result<()>,
) -> Option<String> {
    if probe.hung {
        let cleanup = if runs_remote_processes(candidate) {
            remote::stop_remote(&plan.remote)
        } else {
            Ok(())
        };
        let first = outcome.as_ref().err().map(|e| format!("{e:#}"));
        return Some(hung_error(first.as_deref(), cleanup));
    }
    match outcome {
        Err(err) => Some(format!("{err:#}")),
        Ok(()) => report::verify_dir(plan.files, &plan.local).err(),
    }
}

fn spawn(cmd: &mut Command) -> anyhow::Result<Child> {
    cmd.spawn().with_context(|| format!("starting {cmd:?}"))
}

type Drained = JoinHandle<io::Result<String>>;

/// Read a child's stderr to the end on a thread.
fn drain(mut stderr: impl io::Read + Send + 'static) -> Drained {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })
}

/// The drained text, or the read's error or the thread's panic.
fn joined(handle: Drained) -> anyhow::Result<String> {
    match handle.join() {
        Ok(Ok(text)) => Ok(text.trim().to_owned()),
        Ok(Err(err)) => Err(anyhow::Error::new(err).context("reading a child's stderr")),
        Err(_) => Err(anyhow!("a child's stderr reader panicked")),
    }
}

/// Every drained stderr, joined by spaces; the first failed read is the error.
fn joined_all(handles: Vec<Drained>) -> anyhow::Result<String> {
    let mut texts = Vec::new();
    for handle in handles {
        let text = joined(handle)?;
        if !text.is_empty() {
            texts.push(text);
        }
    }
    Ok(texts.join(" "))
}

/// `result` with the children's stderr as its context; a failed stderr read
/// fails a run that otherwise passed.
fn explain<T>(result: anyhow::Result<T>, stderr: anyhow::Result<String>) -> anyhow::Result<T> {
    match (result, stderr) {
        (Ok(value), Ok(_)) => Ok(value),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Ok(text)) if text.is_empty() => Err(err),
        (Err(err), Ok(text)) => Err(err.context(text)),
        (Err(err), Err(read)) => Err(err.context(format!("{read:#}"))),
    }
}

/// Wait for every child, add its CPU, and fail on the first that exited
/// non-zero or could not be waited.
fn finish(children: &[(&'static str, Shared)], probe: &mut Probe) -> anyhow::Result<()> {
    let mut failed = None;
    for (name, child) in children {
        match guard::wait(child) {
            Ok(status) => {
                probe.child_cpu_ms.push((name, guard::cpu_ms(child)));
                if !status.success() && failed.is_none() {
                    failed = Some(format!("{name} exited with {status}"));
                }
            }
            Err(err) => {
                probe.unwaited += 1;
                if failed.is_none() {
                    failed = Some(format!("waiting for {name}: {err}"));
                }
            }
        }
    }
    match failed {
        Some(why) => bail!(why),
        None => Ok(()),
    }
}

/// Kill `child` after a failure that left it running.
fn abandon(child: &Shared, probe: &mut Probe) {
    if let Ok(mut c) = child.lock() {
        let _ = c.kill();
    }
    probe.unwaited += 1;
}

/// Run `cmd` alone under the guard.
fn run_one(
    name: &'static str,
    mut cmd: Command,
    plan: &Plan,
    probe: &mut Probe,
) -> anyhow::Result<()> {
    let mut child = spawn(
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    )?;
    let stderr = drain(child.stderr.take().context("stderr")?);
    let child = guard::share(child);
    let watch = HangGuard::start(vec![child.clone()], plan.timeout);
    let finished = finish(&[(name, child)], probe);
    probe.hung = watch.finish();
    explain(finished, joined(stderr))
}

fn scp(plan: &Plan, probe: &mut Probe) -> anyhow::Result<()> {
    let dest = plan.local.parent().context("the dest folder")?;
    let mut cmd = Command::new(remote::tool("scp"));
    cmd.args(["-r", "-q", "-o", "BatchMode=yes"])
        .arg(format!("{HOST}:{}", remote::sftp_path(&plan.remote)))
        .arg(dest);
    run_one("scp", cmd, plan, probe)
}

fn sftp(plan: &Plan, probe: &mut Probe) -> anyhow::Result<()> {
    let batch = std::env::temp_dir().join(format!("folder-bench-sftp-{}.txt", std::process::id()));
    let local = plan.local.to_string_lossy().replace('\\', "/");
    fs::write(
        &batch,
        format!("get -r {} \"{local}\"\n", remote::sftp_path(&plan.remote)),
    )
    .with_context(|| format!("writing {}", batch.display()))?;
    let mut cmd = Command::new(remote::tool("sftp"));
    cmd.args(["-q", "-o", "BatchMode=yes", "-b"])
        .arg(&batch)
        .arg(HOST);
    let ran = run_one("sftp", cmd, plan, probe);
    let removed = fs::remove_file(&batch).with_context(|| format!("removing {}", batch.display()));
    ran.and(removed)
}

/// The local `tar -x` into the shape's folder, reading `stdin`.
fn untar(plan: &Plan, stdin: Stdio) -> anyhow::Result<Child> {
    fs::create_dir_all(&plan.local)
        .with_context(|| format!("creating {}", plan.local.display()))?;
    spawn(
        Command::new(TAR)
            .args(["-x", "-f", "-", "-C"])
            .arg(&plan.local)
            .stdin(stdin)
            .stdout(Stdio::null())
            .stderr(Stdio::piped()),
    )
}

fn ssh_tar(plan: &Plan, probe: &mut Probe) -> anyhow::Result<()> {
    let mut sender = spawn(
        remote::ssh(&format!("{TAR} -c -f - -C {} .", plan.remote))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?;
    let packed = sender.stdout.take().context("ssh's stdout")?;
    let sender_err = drain(sender.stderr.take().context("ssh's stderr")?);
    let sender = guard::share(sender);
    let mut receiver = match untar(plan, Stdio::from(packed)) {
        Ok(child) => child,
        Err(err) => {
            abandon(&sender, probe);
            return Err(err);
        }
    };
    let receiver_err = drain(receiver.stderr.take().context("tar's stderr")?);
    let receiver = guard::share(receiver);
    let watch = HangGuard::start(vec![sender.clone(), receiver.clone()], plan.timeout);
    let finished = finish(&[("ssh", sender), ("tar -x", receiver)], probe);
    probe.hung = watch.finish();
    explain(finished, joined_all(vec![sender_err, receiver_err]))
}

/// Named children, in pipe order.
type Chain = Vec<(&'static str, Child)>;

/// The local end of a tar candidate: `zstd -d` (for a zstd codec) feeding
/// `tar -x`, reading `input`; the children, and the stdin to write when
/// `input` was piped.
fn receive_chain(
    plan: &Plan,
    codec: Codec,
    zstd_exe: &str,
    input: Stdio,
) -> anyhow::Result<(Chain, Option<ChildStdin>)> {
    if codec.level().is_none() {
        let mut tar = untar(plan, input)?;
        let stdin = tar.stdin.take();
        return Ok((vec![("tar -x", tar)], stdin));
    }
    let mut zstd = spawn(
        Command::new(zstd_exe)
            .args(["-d", "-c", "-q"])
            .stdin(input)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?;
    let stdin = zstd.stdin.take();
    let unpacked = zstd.stdout.take().context("zstd's stdout")?;
    match untar(plan, Stdio::from(unpacked)) {
        Ok(tar) => Ok((vec![("zstd -d", zstd), ("tar -x", tar)], stdin)),
        Err(err) => {
            let _ = zstd.kill();
            Err(err)
        }
    }
}

fn tar_stream(
    plan: &Plan,
    codec: Codec,
    kind: FrameKind,
    zstd_exe: &str,
    probe: &mut Probe,
) -> anyhow::Result<()> {
    let mut sender = spawn(
        remote::ssh(&format!(
            r"{REMOTE_BIN}\folder-bench.exe serve-tar {} --codec {} --frame {}",
            plan.remote,
            codec.name(),
            kind.name()
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped()),
    )?;
    let mut packed = sender.stdout.take();
    let sender_err = drain(sender.stderr.take().context("ssh's stderr")?);
    let sender = guard::share(sender);
    // Raw bytes go straight from ssh into the chain; frames are decoded here.
    let input = match kind {
        FrameKind::Raw => Stdio::from(packed.take().context("ssh's stdout")?),
        FrameKind::Base64 => Stdio::piped(),
    };
    let (chain, chain_in) = match receive_chain(plan, codec, zstd_exe, input) {
        Ok(chain) => chain,
        Err(err) => {
            abandon(&sender, probe);
            return Err(err);
        }
    };
    let mut errs = Vec::new();
    let mut children = vec![("ssh serve-tar", sender)];
    for (name, mut child) in chain {
        if let Some(stderr) = child.stderr.take() {
            errs.push(drain(stderr));
        }
        children.push((name, guard::share(child)));
    }
    let watch = HangGuard::start(
        children.iter().map(|(_, c)| c.clone()).collect(),
        plan.timeout,
    );

    let pumped = match (packed, chain_in) {
        (Some(packed), Some(chain_in)) => pump(packed, chain_in),
        _ => Ok(None),
    };
    let finished = finish(&children, probe);
    probe.hung = watch.finish();
    let sender_err = joined(sender_err);
    let reported = match kind {
        FrameKind::Base64 => pumped
            .as_ref()
            .map_or(Reported::Missing, |cpu| Reported::from_end(*cpu)),
        // Raw mode reports its CPU as an end frame on stderr.
        FrameKind::Raw => sender_err
            .as_ref()
            .map_or(Reported::Missing, |t| frame::end_line(t)),
    };
    probe.remote_cpu_ms = match reported {
        Reported::Ms(ms) => Some(ms),
        Reported::Unknown => {
            probe.note("serve-tar could not read its CPU");
            None
        }
        Reported::Missing => {
            probe.note("serve-tar sent no end frame");
            None
        }
    };
    let stderr = match (sender_err, joined_all(errs)) {
        (Ok(a), Ok(b)) => Ok(format!("{a} {b}").trim().to_owned()),
        (Err(err), _) | (_, Err(err)) => Err(err),
    };
    explain(pumped.map(|_| ()).and(finished), stderr)
}

/// Decode the sender's frames into the local chain's stdin as they arrive;
/// the end frame's CPU time.
fn pump(packed: ChildStdout, chain_in: ChildStdin) -> anyhow::Result<Option<u64>> {
    let mut input = BufReader::with_capacity(2 * frame::CHUNK, packed);
    let mut output = BufWriter::with_capacity(2 * frame::CHUNK, chain_in);
    let cpu = frame::decode(&mut input, &mut output)?;
    output.flush().context("flushing into the unpacker")?;
    drop(output);
    Ok(cpu)
}

struct Session {
    writer: BufWriter<ChildStdin>,
    reader: BufReader<ChildStdout>,
    line: String,
}

impl Session {
    /// Start `cmd` (an `ssh` to a bridge) with its stdio piped.
    fn open(cmd: &mut Command) -> anyhow::Result<(Self, Shared, Drained)> {
        let mut child = spawn(
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )?;
        let writer = BufWriter::new(child.stdin.take().context("the bridge's stdin")?);
        let reader = BufReader::with_capacity(
            2 * frame::CHUNK,
            child.stdout.take().context("the bridge's stdout")?,
        );
        let stderr = drain(child.stderr.take().context("the bridge's stderr")?);
        let session = Self {
            writer,
            reader,
            line: String::new(),
        };
        Ok((session, guard::share(child), stderr))
    }

    fn send(&mut self, message: &ClientMessage) -> anyhow::Result<()> {
        serde_json::to_writer(&mut self.writer, message)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush().context("writing to the bridge")
    }

    /// The next message the daemon sent that this build knows.
    fn next(&mut self) -> anyhow::Result<DaemonMessage> {
        loop {
            self.line.clear();
            if self.reader.read_line(&mut self.line)? == 0 {
                bail!("the bridge closed its output");
            }
            match InboundDaemonMessage::from_json_str(&self.line)
                .context("parsing a daemon message")?
            {
                InboundDaemonMessage::Known(message) => return Ok(*message),
                InboundDaemonMessage::Unknown { .. } => {}
            }
        }
    }

    fn hello(&mut self) -> anyhow::Result<()> {
        self.send(&ClientMessage::Hello {
            protocol_version: protocol::PROTOCOL_VERSION,
            protocol_versions: protocol::SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
            auth_token: String::new(),
            client_id: None,
            client_name: Some("folder-bench".to_owned()),
        })?;
        loop {
            match self.next()? {
                DaemonMessage::Welcome { .. } => return Ok(()),
                DaemonMessage::AuthFailed { reason } => {
                    bail!("the daemon refused the hello: {reason}")
                }
                _ => {}
            }
        }
    }

    /// Register the shape's folder as a repo (a non-git folder is accepted)
    /// and return its id.
    fn register(&mut self, plan: &Plan) -> anyhow::Result<String> {
        self.send(&ClientMessage::AddRepo {
            path: plan.remote.clone(),
            name: Some(repo_name(&plan.remote, plan.shape)),
        })?;
        self.send(&ClientMessage::ListRepos)?;
        let wanted = normalize(&plan.remote);
        loop {
            match self.next()? {
                DaemonMessage::Repos { repos } => {
                    if let Some(repo) = repos.iter().find(|r| normalize(&r.path) == wanted) {
                        return Ok(repo.id.clone());
                    }
                }
                DaemonMessage::Error { message, .. } => {
                    bail!("registering {}: {message}", plan.remote)
                }
                _ => {}
            }
        }
    }
}

/// The name a shape's folder is registered under:
/// `folder-bench-<fixtures folder leaf>-<shape>`, so the same shape made in
/// two fixtures folders never derives the same name. `shape_dir` is
/// `<remote-dir>\<shape>`; with no parent folder the leaf is left out.
fn repo_name(shape_dir: &str, shape: &str) -> String {
    let parent = shape_dir
        .trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .nth(1)
        .filter(|leaf| !leaf.is_empty() && !leaf.ends_with(':'));
    match parent {
        Some(leaf) => format!("folder-bench-{leaf}-{shape}"),
        None => format!("folder-bench-{shape}"),
    }
}

/// A Windows path compared the way the daemon stores it.
fn normalize(path: &str) -> String {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

/// The untimed setup session: start the isolated daemon through a plain
/// bridge, register the shape's folder, and return its repo id.
fn setup(plan: &Plan, probe: &mut Probe) -> anyhow::Result<String> {
    let (mut session, child, stderr) = Session::open(&mut remote::ssh(&format!(
        r"{REMOTE_BIN}\{DAEMON_WRAPPER} bridge"
    )))?;
    let watch = HangGuard::start(vec![child.clone()], plan.timeout);
    let registered = session.hello().and_then(|()| session.register(plan));
    // Stdin EOF ends the bridge; the daemon keeps running.
    drop(session);
    let status = guard::wait(&child);
    if watch.finish() {
        probe.hung = true;
        return explain(Err(anyhow!("the setup session hung")), joined(stderr));
    }
    let repo_id = explain(registered, joined(stderr))?;
    let status = status.context("waiting for the setup bridge")?;
    ensure!(status.success(), "the setup bridge exited with {status}");
    Ok(repo_id)
}

/// Per-file `FetchFile` with `in_flight` transfers outstanding. After the
/// untimed setup and an idle daemon, the window opens before the timed
/// session's SSH connect, which runs `rustling-tulipd bridge` under
/// `folder-bench relay` so the relay's CPU is counted with the daemon's.
fn fetch(plan: &Plan, in_flight: usize, probe: &mut Probe) -> anyhow::Result<()> {
    let repo_id = setup(plan, probe)?;
    let baseline = match remote::wait_idle(
        remote::daemon_cpu_ms,
        || thread::sleep(Duration::from_secs(1)),
        IDLE_POLLS,
    ) {
        Ok((reading, idle)) => {
            if !idle {
                probe.note("daemon not idle");
            }
            Some(reading)
        }
        Err(why) => {
            probe.note(format!("daemon CPU before: {why}"));
            None
        }
    };

    probe.begin();
    let (mut session, child, stderr) = Session::open(&mut remote::ssh(&format!(
        r"{REMOTE_BIN}\{ISOLATED_WRAPPER} {REMOTE_BIN}\folder-bench.exe relay -- {REMOTE_BIN}\rustling-tulipd.exe bridge"
    )))?;
    let watch = HangGuard::start(vec![child.clone()], plan.timeout);
    let fetched = session
        .hello()
        .and_then(|()| transfer(&mut session, plan, &repo_id, in_flight));
    if fetched.is_ok() {
        probe.wall = probe.started.map(|started| started.elapsed());
    }
    drop(session);
    let finished = finish(&[("ssh relay bridge", child)], probe);
    probe.hung = watch.finish();
    let stderr = joined(stderr);

    let relay = match stderr.as_ref().map(|text| frame::end_line(text)) {
        Ok(Reported::Ms(ms)) => Some(ms),
        Ok(Reported::Unknown) => {
            probe.note("the relay could not read the bridge's CPU");
            None
        }
        Ok(Reported::Missing) => {
            probe.note("the relay wrote no end line");
            None
        }
        Err(_) => {
            probe.note("the relay's stderr could not be read");
            None
        }
    };
    let daemon = match (baseline, remote::daemon_cpu_ms()) {
        (Some(before), Ok(after)) if before.0 == after.0 => Some(after.1.saturating_sub(before.1)),
        (Some(_), Ok(_)) => {
            probe.note("the daemon's pid changed during the run");
            None
        }
        (_, Err(why)) => {
            probe.note(format!("daemon CPU after: {why}"));
            None
        }
        (None, Ok(_)) => None,
    };
    probe.remote_cpu_ms = cpu::sum([daemon, relay]);
    explain(fetched.and(finished), stderr)
}

fn transfer(
    session: &mut Session,
    plan: &Plan,
    repo_id: &str,
    in_flight: usize,
) -> anyhow::Result<()> {
    let mut active: HashMap<String, (usize, File)> = HashMap::new();
    let mut next = 0;
    let mut done = 0;
    let mut bytes = Vec::with_capacity(frame::CHUNK);
    let start = |next: &mut usize,
                 active: &mut HashMap<String, (usize, File)>,
                 session: &mut Session|
     -> anyhow::Result<()> {
        let index = *next;
        let file = &plan.files[index];
        let path = plan.local.join(&file.rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let handle = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let id = format!("f{index}");
        active.insert(id.clone(), (index, handle));
        session.send(&ClientMessage::FetchFile {
            id,
            repo_id: repo_id.to_owned(),
            worktree_path: None,
            path: file.rel.replace('/', "\\"),
        })?;
        *next += 1;
        Ok(())
    };
    while next < plan.files.len() && active.len() < in_flight {
        start(&mut next, &mut active, session)?;
    }
    while done < plan.files.len() {
        match session.next()? {
            DaemonMessage::FileChunk { id, data_b64, .. } => {
                let (index, handle) = active
                    .get_mut(&id)
                    .with_context(|| format!("a chunk for an unknown fetch {id}"))?;
                bytes.clear();
                base64::engine::general_purpose::STANDARD
                    .decode_vec(data_b64.as_bytes(), &mut bytes)
                    .context("decoding a chunk")?;
                handle
                    .write_all(&bytes)
                    .with_context(|| format!("writing {}", plan.files[*index].rel))?;
            }
            DaemonMessage::FileFetchDone { id } => {
                active
                    .remove(&id)
                    .with_context(|| format!("done for an unknown fetch {id}"))?;
                done += 1;
                if next < plan.files.len() {
                    start(&mut next, &mut active, session)?;
                }
            }
            DaemonMessage::FileFetchError { id, error } => {
                let rel = active
                    .get(&id)
                    .map_or("?", |(index, _)| plan.files[*index].rel.as_str());
                bail!("fetching {rel}: {error}");
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn paths_compare_as_the_daemon_stores_them() {
        assert_eq!(
            normalize(r"\\?\C:\Users\Builder\.tmp\x\"),
            normalize("c:/users/builder/.tmp/x")
        );
    }

    #[test]
    fn repo_name_includes_the_fixtures_folder() {
        let full = repo_name(
            r"C:\Users\builder\.tmp\folder-bench\fx-full\small-files",
            "small-files",
        );
        let small = repo_name(
            r"C:\Users\builder\.tmp\folder-bench\fx\small-files",
            "small-files",
        );
        assert_eq!(full, "folder-bench-fx-full-small-files");
        assert_eq!(small, "folder-bench-fx-small-files");
        assert_ne!(full, small);
    }

    #[test]
    fn in_flight_applies_to_the_fetch_candidates_only() {
        assert_eq!(in_flight_for("fetch-n", Some("8")).expect("n"), Some(8));
        assert_eq!(
            in_flight_for("fetch-n", None).expect("default"),
            Some(DEFAULT_IN_FLIGHT)
        );
        assert_eq!(in_flight_for("fetch-seq", None).expect("seq"), Some(1));
        assert_eq!(in_flight_for("scp", None).expect("scp"), None);
        assert!(in_flight_for("fetch-n", Some("0")).is_err());
        let refused = in_flight_for("tar-zstd1", Some("4")).expect_err("refused");
        assert!(format!("{refused}").contains("tar-zstd1"), "{refused}");
        assert!(in_flight_for("fetch-seq", Some("2")).is_err());
    }

    #[test]
    fn a_hung_run_names_its_first_error_and_a_failed_cleanup() {
        assert_eq!(hung_error(None, Ok(())), "hung");
        assert_eq!(
            hung_error(Some("the bridge closed its output"), Ok(())),
            "hung after: the bridge closed its output"
        );
        assert_eq!(
            hung_error(None, Err("still running: 12".to_owned())),
            "hung; remote cleanup failed: still running: 12"
        );
    }

    #[test]
    fn local_cpu_is_unknown_when_any_part_is() {
        let mut probe = Probe {
            started: Some(Instant::now()),
            own_before: Some(10),
            ..Probe::default()
        };
        probe.child_cpu_ms.push(("ssh", Some(5)));
        assert_eq!(probe.local_cpu(Some(30)), Ok(25));
        probe.child_cpu_ms.push(("tar -x", None));
        assert_eq!(
            probe.local_cpu(Some(30)),
            Err("local CPU unknown for tar -x".to_owned())
        );
        probe.child_cpu_ms.pop();
        probe.unwaited = 1;
        assert!(probe.local_cpu(Some(30)).is_err());
        assert!(Probe::default().local_cpu(Some(1)).is_err());
    }

    struct Failing;

    impl io::Read for Failing {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("pipe broke"))
        }
    }

    #[test]
    fn a_failed_stderr_read_is_an_error_not_an_empty_string() {
        let err = joined(drain(Failing)).expect_err("the read failed");
        assert!(format!("{err:#}").contains("pipe broke"), "{err:#}");
        assert_eq!(joined(drain(&b"  text \n"[..])).expect("text"), "text");
        let failed: anyhow::Result<()> = explain(Ok(()), Err(anyhow!("pipe broke")));
        assert!(failed.is_err());
    }
}
