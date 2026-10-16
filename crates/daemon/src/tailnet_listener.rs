//! The tailnet listener: one server per tailnet address of this host, on a
//! fixed port, rebinding when Tailscale's address set changes.
//!
//! The addresses come from `tailscale whoami`, never the unspecified address.
//! All Tailscale work runs on a supervisor task, so loopback serving never
//! waits on it: Tailscale being down is a status and a retry, not an error.

use axum_server::Handle;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tailnet::{TailnetError, WhoIs};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Consecutive transient `whoami` failures after which Tailscale counts as
/// down.
const TRANSIENT_LIMIT: u32 = 3;

/// What the listener is doing, published on a watch for the status surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerStatus {
    /// Started; Tailscale not asked yet.
    Starting,
    /// Serving on `addrs` (one server per address), as `name`, the host's
    /// `MagicDNS` name without its trailing dot (`None` when `whoami` gave
    /// none).
    Serving {
        name: Option<String>,
        addrs: Vec<SocketAddr>,
    },
    /// HTTPS certificates are disabled in the tailnet's admin console.
    HttpsDisabled,
    /// `tailscale cert` failed for another reason.
    CertFailed,
    /// Every address failed to bind because `port` stayed in use past the
    /// retry budget.
    PortInUse { port: u16 },
    /// Every address failed to bind for a reason other than the port being in
    /// use.
    BindFailed(String),
    /// `tailscale whoami` failed definitively (or transiently several times
    /// in a row), or listed no address to bind; no server runs, and `whoami`
    /// is retried with backoff.
    TailscaleDown(TailnetError),
}

/// Ports, intervals and budgets; [`ListenerOptions::PRODUCTION`] in the
/// daemon, shortened or zeroed by tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenerOptions {
    /// The port every address is bound on.
    pub port: u16,
    /// How often `whoami` is asked for the address set once it answered.
    pub poll_interval: Duration,
    /// How long a bind that fails with `AddrInUse` is retried.
    pub bind_budget: Duration,
    /// The pause between those retries.
    pub bind_backoff: Duration,
    /// The first pause after a failed `whoami`; it doubles on each failure,
    /// up to `poll_interval`.
    pub down_backoff: Duration,
    /// How long a server being shut down may drain its connections.
    pub shutdown_grace: Duration,
}

impl ListenerOptions {
    pub const PRODUCTION: Self = Self {
        port: 8787,
        poll_interval: Duration::from_secs(30),
        // Outlasts the previous daemon's 3 s drain on a supervisor handoff.
        bind_budget: Duration::from_secs(10),
        bind_backoff: Duration::from_millis(100),
        down_backoff: Duration::from_secs(1),
        shutdown_grace: Duration::from_secs(3),
    };
}

/// Serves the connections of one bound listener until its handle is shut
/// down. The production implementation serves TLS only.
pub trait Serve: Send + Sync + 'static {
    fn serve(
        &self,
        listener: std::net::TcpListener,
        handle: Handle<SocketAddr>,
    ) -> impl Future<Output = std::io::Result<()>> + Send;
}

/// The running listener: a supervisor task that owns the servers, and the
/// watches it publishes.
pub struct TailnetListener {
    status: watch::Receiver<ListenerStatus>,
    name: watch::Receiver<Option<String>>,
    supervisor: JoinHandle<()>,
}

impl TailnetListener {
    /// Start the supervisor and return at once; it asks `whoami` for the
    /// addresses, binds one server per address on `options.port`, and polls
    /// for changes until `shutdown` turns true (or its sender is dropped),
    /// then shuts every server down.
    pub fn start_with<A: TailnetApi, S: Serve>(
        api: Arc<A>,
        serve: Arc<S>,
        options: ListenerOptions,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        let (status_tx, status) = watch::channel(ListenerStatus::Starting);
        let (name_tx, name) = watch::channel(None);
        let supervisor = Supervisor {
            api,
            serve,
            options,
            status: status_tx,
            name: name_tx,
            servers: BTreeMap::new(),
            bind_failures: BTreeMap::new(),
            transient_failures: 0,
            last_down_error: None,
        };
        let supervisor = tokio::spawn(supervisor.run(shutdown));
        Self {
            status,
            name,
            supervisor,
        }
    }

    /// A receiver of the listener's status.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<ListenerStatus> {
        self.status.clone()
    }

    /// The host's `MagicDNS` name (`*.ts.net`, no trailing dot) from the last
    /// `whoami` that answered; `None` before one has, or when it gave none.
    #[must_use]
    pub fn name(&self) -> Option<String> {
        self.name.borrow().clone()
    }

    /// Wait until the supervisor has ended, after shutdown was asked for, and
    /// every server has released its address.
    pub async fn stopped(self) {
        if let Err(err) = self.supervisor.await {
            tracing::warn!(%err, "tailnet listener supervisor failed");
        }
    }
}

/// One address's server. Dropping it shuts the server down, so a server
/// never outlives its supervisor.
struct Server {
    local: SocketAddr,
    handle: Handle<SocketAddr>,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}

/// How binding one address failed.
enum BindError {
    InUse,
    Other(std::io::Error),
}

/// What reconciling the address set left unbound, and why.
#[derive(Default)]
struct BindOutcome {
    in_use: bool,
    failure: Option<String>,
}

/// Whether the supervisor goes on or shutdown cut the work short.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Shutdown,
}

struct Supervisor<A, S> {
    api: Arc<A>,
    serve: Arc<S>,
    options: ListenerOptions,
    status: watch::Sender<ListenerStatus>,
    name: watch::Sender<Option<String>>,
    servers: BTreeMap<IpAddr, Server>,
    /// The last bind failure per address, so a repeat logs at debug.
    bind_failures: BTreeMap<IpAddr, ErrorKind>,
    /// Consecutive transient `whoami` failures.
    transient_failures: u32,
    /// The last `whoami` failure that took Tailscale down, so a repeat with
    /// no server to stop logs at debug.
    last_down_error: Option<TailnetError>,
}

impl<A: TailnetApi, S: Serve> Supervisor<A, S> {
    async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut down_backoff = self.options.down_backoff;
        while !*shutdown.borrow() {
            let Some(answer) = until_shutdown(self.api.whoami(), &mut shutdown).await else {
                break;
            };
            let pause = match answer {
                Ok(who) => {
                    down_backoff = self.options.down_backoff;
                    self.transient_failures = 0;
                    self.last_down_error = None;
                    if self.apply(&who, &mut shutdown).await == Flow::Shutdown {
                        break;
                    }
                    self.options.poll_interval
                }
                Err(err) => {
                    self.whoami_failed(err).await;
                    let pause = down_backoff;
                    down_backoff = (down_backoff * 2).min(self.options.poll_interval);
                    pause
                }
            };
            if until_shutdown(tokio::time::sleep(pause), &mut shutdown)
                .await
                .is_none()
            {
                break;
            }
        }
        self.stop_all().await;
    }

    /// A definitive failure (Tailscale not installed, not running, refusing
    /// this user, not connected) stops every server and is the status. A
    /// transient one (timeout, unparsable output, other CLI failure) keeps
    /// the servers and the status, until [`TRANSIENT_LIMIT`] in a row.
    async fn whoami_failed(&mut self, err: TailnetError) {
        if is_transient(&err) {
            self.transient_failures += 1;
            if self.transient_failures < TRANSIENT_LIMIT {
                tracing::warn!(
                    %err,
                    failures = self.transient_failures,
                    "tailnet listener: tailscale whoami failed; keeping the servers and retrying"
                );
                return;
            }
        }
        let stopping = !self.servers.is_empty();
        if stopping || self.last_down_error.as_ref() != Some(&err) {
            tracing::warn!(%err, "tailnet listener: Tailscale is down; servers stopped, retrying");
        } else {
            tracing::debug!(%err, "tailnet listener: Tailscale still down; retrying");
        }
        self.last_down_error = Some(err.clone());
        self.stop_all().await;
        self.publish(ListenerStatus::TailscaleDown(err));
    }

    /// Bring the servers in line with `who`'s addresses: stop the ones whose
    /// address vanished (or whose task ended), start the missing ones.
    async fn apply(&mut self, who: &WhoIs, shutdown: &mut watch::Receiver<bool>) -> Flow {
        let trimmed = who.node.name.trim_end_matches('.');
        let name = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        self.name.send_if_modified(|current| {
            let changed = *current != name;
            if changed {
                current.clone_from(&name);
            }
            changed
        });
        let wanted = bind_addresses(&who.node.addresses);
        self.bind_failures.retain(|ip, _| wanted.contains(ip));
        let stale: Vec<IpAddr> = self
            .servers
            .iter()
            .filter(|(ip, server)| !wanted.contains(ip) || server.task.is_finished())
            .map(|(ip, _)| *ip)
            .collect();
        for ip in stale {
            if let Some(server) = self.servers.remove(&ip) {
                self.stop(server).await;
            }
        }
        let mut outcome = BindOutcome::default();
        for ip in wanted {
            if !self.servers.contains_key(&ip)
                && self.start_server(ip, &mut outcome, shutdown).await == Flow::Shutdown
            {
                return Flow::Shutdown;
            }
        }
        self.publish(self.status_after(name, outcome));
        Flow::Continue
    }

    async fn start_server(
        &mut self,
        ip: IpAddr,
        outcome: &mut BindOutcome,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Flow {
        let addr = SocketAddr::new(ip, self.options.port);
        let bind = bind_with_retry(addr, self.options.bind_budget, self.options.bind_backoff);
        let Some(bound) = until_shutdown(bind, shutdown).await else {
            return Flow::Shutdown;
        };
        let spawned = match bound {
            Ok(listener) => self.spawn_server(listener).map_err(BindError::Other),
            Err(err) => Err(err),
        };
        match spawned {
            Ok(server) => {
                tracing::info!(addr = %server.local, "tailnet listener bound");
                self.bind_failures.remove(&ip);
                self.servers.insert(ip, server);
            }
            Err(BindError::InUse) => {
                let detail = format!(
                    "port still in use after retrying for {}s",
                    self.options.bind_budget.as_secs()
                );
                self.report_bind_failure(addr, ErrorKind::AddrInUse, &detail);
                outcome.in_use = true;
            }
            Err(BindError::Other(err)) => {
                self.report_bind_failure(addr, err.kind(), &err.to_string());
                outcome.failure = Some(format!("{addr}: {err}"));
            }
        }
        Flow::Continue
    }

    /// Log a bind failure at warn the first time `addr` fails this way, at
    /// debug while it keeps failing the same way.
    fn report_bind_failure(&mut self, addr: SocketAddr, kind: ErrorKind, detail: &str) {
        if self.bind_failures.insert(addr.ip(), kind) == Some(kind) {
            tracing::debug!(%addr, %detail, "tailnet listener: bind still failing");
        } else {
            tracing::warn!(%addr, %detail, "tailnet listener: bind failed");
        }
    }

    fn spawn_server(&self, listener: std::net::TcpListener) -> std::io::Result<Server> {
        let local = listener.local_addr()?;
        let handle = Handle::new();
        let serve = Arc::clone(&self.serve);
        let task_handle = handle.clone();
        let task = tokio::spawn(async move {
            if let Err(err) = serve.serve(listener, task_handle).await {
                tracing::warn!(addr = %local, %err, "tailnet server stopped with an error");
            }
        });
        Ok(Server {
            local,
            handle,
            task,
        })
    }

    /// Shut `server` down, letting its connections drain for the grace
    /// period, and wait until it has released its address.
    async fn stop(&self, mut server: Server) {
        server
            .handle
            .graceful_shutdown(Some(self.options.shutdown_grace));
        if let Err(err) = (&mut server.task).await {
            tracing::warn!(addr = %server.local, %err, "tailnet server task failed");
        }
        tracing::info!(addr = %server.local, "tailnet listener closed");
    }

    async fn stop_all(&mut self) {
        let servers = std::mem::take(&mut self.servers);
        for server in servers.into_values() {
            self.stop(server).await;
        }
    }

    fn status_after(&self, name: Option<String>, outcome: BindOutcome) -> ListenerStatus {
        if !self.servers.is_empty() {
            let addrs = self.servers.values().map(|server| server.local).collect();
            return ListenerStatus::Serving { name, addrs };
        }
        if outcome.in_use {
            return ListenerStatus::PortInUse {
                port: self.options.port,
            };
        }
        match outcome.failure {
            Some(reason) => ListenerStatus::BindFailed(reason),
            None => ListenerStatus::TailscaleDown(TailnetError::BadOutput(
                "whoami listed no address to bind".to_owned(),
            )),
        }
    }

    /// Publish `status`, waking watchers only when it changed.
    fn publish(&self, status: ListenerStatus) {
        self.status.send_if_modified(|current| {
            let changed = *current != status;
            if changed {
                *current = status;
            }
            changed
        });
    }
}

/// Whether a `whoami` failure may clear up by itself on the next call.
fn is_transient(err: &TailnetError) -> bool {
    matches!(
        err,
        TailnetError::Timeout | TailnetError::BadOutput(_) | TailnetError::Failed(_)
    )
}

/// The addresses to bind from `whoami`'s CIDR prefixes (`100.x.y.z/32`,
/// `fd7a:…/128`): prefix length stripped, IPv4-mapped forms canonicalised,
/// the unspecified address and anything unparsable dropped.
fn bind_addresses(prefixes: &[String]) -> BTreeSet<IpAddr> {
    let mut out = BTreeSet::new();
    for prefix in prefixes {
        let text = prefix.split_once('/').map_or(prefix.as_str(), |(ip, _)| ip);
        match text.parse::<IpAddr>() {
            Ok(ip) if ip.to_canonical().is_unspecified() => {
                tracing::warn!(%prefix, "tailnet listener: never binds the unspecified address");
            }
            Ok(ip) => {
                out.insert(ip.to_canonical());
            }
            Err(err) => {
                tracing::warn!(%prefix, %err, "tailnet listener: unparsable whoami address");
            }
        }
    }
    out
}

/// Bind `addr`, retrying while it is in use (the previous daemon draining on
/// a handoff) until `budget` has passed. The listener is set nonblocking.
async fn bind_with_retry(
    addr: SocketAddr,
    budget: Duration,
    backoff: Duration,
) -> Result<std::net::TcpListener, BindError> {
    let start = tokio::time::Instant::now();
    loop {
        match std::net::TcpListener::bind(addr) {
            Ok(listener) => {
                listener.set_nonblocking(true).map_err(BindError::Other)?;
                return Ok(listener);
            }
            Err(err) if err.kind() == ErrorKind::AddrInUse => {
                if start.elapsed() >= budget {
                    return Err(BindError::InUse);
                }
                tracing::debug!(%addr, "tailnet port still in use; retrying bind");
                tokio::time::sleep(backoff).await;
            }
            Err(err) => return Err(BindError::Other(err)),
        }
    }
}

/// Run `work` unless shutdown is asked for first; `None` on shutdown. A
/// dropped shutdown sender counts as shutdown.
async fn until_shutdown<F: Future>(
    work: F,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = shutdown.wait_for(|stop| *stop) => None,
        out = work => Some(out),
    }
}

/// The `tailscale` CLI calls the listener makes, as a seam so tests can script
/// them. Signatures match [`tailnet::Cli`]'s.
pub trait TailnetApi: Send + Sync + 'static {
    /// Identify the node behind the accepted socket's peer address.
    fn whois(&self, addr: SocketAddr) -> impl Future<Output = Result<WhoIs, TailnetError>> + Send;

    /// Identify this host's own node: its `MagicDNS` name and addresses.
    fn whoami(&self) -> impl Future<Output = Result<WhoIs, TailnetError>> + Send;

    /// Write the TLS certificate and key for `name`, renewing it when it is
    /// valid for less than `min_validity`.
    fn cert(
        &self,
        name: &str,
        cert_file: &Path,
        key_file: &Path,
        min_validity: Duration,
    ) -> impl Future<Output = Result<(), TailnetError>> + Send;
}

impl TailnetApi for tailnet::Cli {
    fn whois(&self, addr: SocketAddr) -> impl Future<Output = Result<WhoIs, TailnetError>> + Send {
        tailnet::Cli::whois(self, addr)
    }

    fn whoami(&self) -> impl Future<Output = Result<WhoIs, TailnetError>> + Send {
        tailnet::Cli::whoami(self)
    }

    fn cert(
        &self,
        name: &str,
        cert_file: &Path,
        key_file: &Path,
        min_validity: Duration,
    ) -> impl Future<Output = Result<(), TailnetError>> + Send {
        tailnet::Cli::cert(self, name, cert_file, key_file, min_validity)
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests assert preconditions with expect")]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tailnet::{Hostinfo, Node, UserProfile};
    use tokio::time::Instant;

    const HOST_NAME: &str = "desktop.tail1234.ts.net.";
    const V4_LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    fn whoami_named(name: &str, addresses: &[&str]) -> WhoIs {
        WhoIs {
            node: Node {
                id: 1,
                stable_id: "nHostCNTRL".to_owned(),
                name: name.to_owned(),
                user: 2,
                sharer: 0,
                addresses: addresses.iter().map(|a| (*a).to_owned()).collect(),
                tags: Vec::new(),
                hostinfo: Hostinfo::default(),
            },
            user_profile: UserProfile::default(),
        }
    }

    fn whoami_with(addresses: &[&str]) -> WhoIs {
        whoami_named(HOST_NAME, addresses)
    }

    /// A scripted `tailscale`: each call returns the response set for it and
    /// is counted (`whoami` with the time of each call).
    struct FakeTailnet {
        whoami: Mutex<Result<WhoIs, TailnetError>>,
        whois: Mutex<Result<WhoIs, TailnetError>>,
        cert: Mutex<Result<(), TailnetError>>,
        whoami_times: Mutex<Vec<Instant>>,
        whois_calls: AtomicUsize,
        cert_calls: AtomicUsize,
    }

    impl FakeTailnet {
        fn with_whoami(whoami: Result<WhoIs, TailnetError>) -> Self {
            Self {
                whoami: Mutex::new(whoami),
                whois: Mutex::new(Err(TailnetError::NotConnected)),
                cert: Mutex::new(Ok(())),
                whoami_times: Mutex::new(Vec::new()),
                whois_calls: AtomicUsize::new(0),
                cert_calls: AtomicUsize::new(0),
            }
        }

        fn with_addresses(addresses: &[&str]) -> Self {
            Self::with_whoami(Ok(whoami_with(addresses)))
        }

        fn set_whoami(&self, whoami: Result<WhoIs, TailnetError>) {
            *self.whoami.lock().expect("whoami lock") = whoami;
        }

        fn set_addresses(&self, addresses: &[&str]) {
            self.set_whoami(Ok(whoami_with(addresses)));
        }

        fn whoami_times(&self) -> Vec<Instant> {
            self.whoami_times.lock().expect("whoami times lock").clone()
        }

        fn whoami_calls(&self) -> usize {
            self.whoami_times().len()
        }
    }

    impl TailnetApi for FakeTailnet {
        fn whois(
            &self,
            _addr: SocketAddr,
        ) -> impl Future<Output = Result<WhoIs, TailnetError>> + Send {
            self.whois_calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(self.whois.lock().expect("whois lock").clone())
        }

        fn whoami(&self) -> impl Future<Output = Result<WhoIs, TailnetError>> + Send {
            self.whoami_times
                .lock()
                .expect("whoami times lock")
                .push(Instant::now());
            std::future::ready(self.whoami.lock().expect("whoami lock").clone())
        }

        fn cert(
            &self,
            _name: &str,
            _cert_file: &Path,
            _key_file: &Path,
            _min_validity: Duration,
        ) -> impl Future<Output = Result<(), TailnetError>> + Send {
            self.cert_calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(self.cert.lock().expect("cert lock").clone())
        }
    }

    /// Plain-TCP serving, for tests only: the bind tests need a live server
    /// behind each handle, and no production path serves plaintext. With
    /// `fail_first` set, the first server dies at once, releasing its port.
    #[derive(Default)]
    struct PlainServe {
        fail_first: AtomicBool,
        started: AtomicUsize,
        finished: AtomicUsize,
    }

    impl Serve for PlainServe {
        async fn serve(
            &self,
            listener: std::net::TcpListener,
            handle: Handle<SocketAddr>,
        ) -> std::io::Result<()> {
            self.started.fetch_add(1, Ordering::SeqCst);
            if self.fail_first.swap(false, Ordering::SeqCst) {
                return Err(std::io::Error::other("scripted server failure"));
            }
            let result = axum_server::from_tcp(listener)?
                .handle(handle)
                .serve(axum::Router::new().into_make_service())
                .await;
            self.finished.fetch_add(1, Ordering::SeqCst);
            result
        }
    }

    fn options(port: u16) -> ListenerOptions {
        ListenerOptions {
            port,
            ..ListenerOptions::PRODUCTION
        }
    }

    fn start_with_shutdown(
        fake: &Arc<FakeTailnet>,
        serve: &Arc<PlainServe>,
        options: ListenerOptions,
        shutdown: watch::Receiver<bool>,
    ) -> TailnetListener {
        TailnetListener::start_with(Arc::clone(fake), Arc::clone(serve), options, shutdown)
    }

    fn start(
        fake: &Arc<FakeTailnet>,
        serve: &Arc<PlainServe>,
        options: ListenerOptions,
    ) -> (TailnetListener, watch::Sender<bool>) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let listener = start_with_shutdown(fake, serve, options, shutdown_rx);
        (listener, shutdown_tx)
    }

    async fn wait_for_status(
        listener: &TailnetListener,
        mut pred: impl FnMut(&ListenerStatus) -> bool,
    ) -> ListenerStatus {
        let mut status = listener.status();
        // Four production poll intervals: on the paused clock a missing
        // behaviour fails here instead of hanging; on the real clock it is
        // far past any legitimate wait.
        let seen = tokio::time::timeout(Duration::from_secs(120), status.wait_for(|s| pred(s)))
            .await
            .expect("the awaited status arrives within four poll intervals")
            .expect("the supervisor keeps the status sender alive");
        seen.clone()
    }

    #[test]
    fn bind_addresses_canonicalises_mapped_forms() {
        let prefixes = [
            "::ffff:127.0.0.1/128".to_owned(),
            "::ffff:0.0.0.0/128".to_owned(),
        ];
        assert_eq!(
            bind_addresses(&prefixes),
            BTreeSet::from([V4_LOOPBACK]),
            "a mapped address binds as its IPv4 form; a mapped unspecified one not at all"
        );
    }

    /// Every status the listener publishes from now on, in order.
    fn record_statuses(listener: &TailnetListener) -> Arc<Mutex<Vec<ListenerStatus>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mut status = listener.status();
        // The current value is not "from now on".
        status.borrow_and_update();
        tokio::spawn(async move {
            while status.changed().await.is_ok() {
                let current = status.borrow_and_update().clone();
                sink.lock().expect("status log lock").push(current);
            }
        });
        seen
    }

    fn serving_addrs(status: &ListenerStatus) -> Vec<SocketAddr> {
        match status {
            ListenerStatus::Serving { addrs, .. } => addrs.clone(),
            _ => Vec::new(),
        }
    }

    fn serving_ips(status: &ListenerStatus) -> Vec<IpAddr> {
        serving_addrs(status).iter().map(SocketAddr::ip).collect()
    }

    fn is_serving(status: &ListenerStatus) -> bool {
        matches!(status, ListenerStatus::Serving { .. })
    }

    #[tokio::test]
    async fn binds_only_the_whoami_addresses() {
        let fake = Arc::new(FakeTailnet::with_addresses(&[
            "127.0.0.1/32",
            "0.0.0.0/0",
            "::/0",
            "::ffff:0.0.0.0/128",
            "::ffff:127.0.0.1/128",
        ]));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));

        let status = wait_for_status(&listener, is_serving).await;
        let addrs = serving_addrs(&status);
        assert_eq!(
            serving_ips(&status),
            [V4_LOOPBACK],
            "bound exactly the whoami address, the mapped form deduplicated"
        );
        assert!(addrs.iter().all(|a| !a.ip().is_unspecified()));
        assert!(addrs.iter().all(|a| a.port() != 0), "{addrs:?}");
        std::net::TcpStream::connect(addrs[0]).expect("the bound address accepts connections");
        assert_eq!(
            listener.name().as_deref(),
            Some("desktop.tail1234.ts.net"),
            "the name drops its trailing dot"
        );
        shutdown.send_replace(true);
    }

    #[tokio::test]
    async fn an_empty_whoami_name_is_none() {
        let fake = Arc::new(FakeTailnet::with_whoami(Ok(whoami_named(
            "",
            &["127.0.0.1/32"],
        ))));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        let status = wait_for_status(&listener, is_serving).await;
        assert!(
            matches!(status, ListenerStatus::Serving { name: None, .. }),
            "{status:?}"
        );
        assert_eq!(listener.name(), None);
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_while_the_port_is_held() {
        // Released mid-budget: the bind retries until the holder lets go,
        // inside the one whoami pass and without reporting the port in use.
        let holder = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("hold a port");
        let port = holder.local_addr().expect("held addr").port();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(holder);
        });
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let began = Instant::now();
        let (listener, released_shutdown) = start(&fake, &serve, options(port));
        let seen = record_statuses(&listener);
        let status = wait_for_status(&listener, is_serving).await;
        assert_eq!(serving_addrs(&status), [SocketAddr::new(V4_LOOPBACK, port)]);
        let elapsed = began.elapsed();
        assert!(
            elapsed >= Duration::from_secs(1),
            "bound only after the holder let go"
        );
        assert!(
            elapsed < ListenerOptions::PRODUCTION.bind_budget,
            "bound inside the retry budget, not on a later poll: {elapsed:?}"
        );
        assert_eq!(fake.whoami_calls(), 1, "one whoami pass");
        assert!(
            !seen
                .lock()
                .expect("status log lock")
                .iter()
                .any(|s| matches!(s, ListenerStatus::PortInUse { .. })),
            "PortInUse is never published while the retry succeeds"
        );
        released_shutdown.send_replace(true);

        // Held past the budget: the listener stays down and says why.
        let held = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("hold a port");
        let held_port = held.local_addr().expect("held addr").port();
        let (listener, held_shutdown) = start(&fake, &serve, options(held_port));
        let status =
            wait_for_status(&listener, |s| matches!(s, ListenerStatus::PortInUse { .. })).await;
        assert_eq!(status, ListenerStatus::PortInUse { port: held_port });
        held_shutdown.send_replace(true);
        drop(held);
    }

    #[tokio::test]
    async fn one_family_failing_keeps_the_other() {
        // 192.0.2.0/24 is a documentation prefix no interface has, so its
        // bind fails; it sorts before ::1, so the IPv6 bind runs after it.
        let fake = Arc::new(FakeTailnet::with_addresses(&["::1/128", "192.0.2.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        let status = wait_for_status(&listener, is_serving).await;
        assert_eq!(
            serving_ips(&status),
            [IpAddr::V6(Ipv6Addr::LOCALHOST)],
            "the IPv6 server stays up"
        );
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn rebinds_when_the_address_set_changes() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        let first = serving_addrs(&wait_for_status(&listener, is_serving).await);
        assert_eq!(first.len(), 1);

        fake.set_addresses(&["127.0.0.2/32"]);
        let second = wait_for_status(&listener, |s| {
            serving_ips(s).contains(&IpAddr::from([127, 0, 0, 2]))
        })
        .await;
        assert_eq!(
            serving_ips(&second),
            [IpAddr::from([127, 0, 0, 2])],
            "only the new address"
        );
        assert_eq!(
            serve.finished.load(Ordering::SeqCst),
            1,
            "the old server was shut down"
        );
        std::net::TcpListener::bind(first[0]).expect("the old address was released");
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dead_server_is_rebound_on_the_next_poll() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe {
            fail_first: AtomicBool::new(true),
            ..PlainServe::default()
        });
        let (listener, shutdown) = start(&fake, &serve, options(0));
        let first = serving_addrs(&wait_for_status(&listener, is_serving).await);
        let second =
            wait_for_status(&listener, |s| is_serving(s) && serving_addrs(s) != first).await;
        assert_eq!(serve.started.load(Ordering::SeqCst), 2, "served again");
        assert_eq!(fake.whoami_calls(), 2, "on the next poll");
        assert_eq!(serving_ips(&second), [V4_LOOPBACK]);
        std::net::TcpStream::connect(serving_addrs(&second)[0])
            .expect("the rebound server accepts connections");
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn tailscale_down_at_start_retries_without_blocking() {
        let fake = Arc::new(FakeTailnet::with_whoami(Err(TailnetError::NotRunning)));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        assert_eq!(
            *listener.status().borrow(),
            ListenerStatus::Starting,
            "start returns before asking Tailscale anything"
        );
        let down =
            wait_for_status(&listener, |s| matches!(s, ListenerStatus::TailscaleDown(_))).await;
        assert_eq!(
            down,
            ListenerStatus::TailscaleDown(TailnetError::NotRunning)
        );

        fake.set_addresses(&["127.0.0.1/32"]);
        let status = wait_for_status(&listener, is_serving).await;
        assert_eq!(serving_addrs(&status).len(), 1);
        assert!(fake.whoami_calls() >= 2, "whoami was retried");
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn down_backoff_doubles_up_to_the_poll_interval() {
        let fake = Arc::new(FakeTailnet::with_whoami(Err(TailnetError::NotRunning)));
        let serve = Arc::new(PlainServe::default());
        let (_listener, shutdown) = start(&fake, &serve, options(0));
        // Calls at 0, 1, 3, 7, 15, 31, 61 and 91 s.
        tokio::time::sleep(Duration::from_secs(95)).await;
        let times = fake.whoami_times();
        let gaps: Vec<u64> = times
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).as_secs())
            .collect();
        assert_eq!(gaps, [1, 2, 4, 8, 16, 30, 30]);
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn a_definitive_whoami_error_stops_the_servers() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        let first = serving_addrs(&wait_for_status(&listener, is_serving).await);

        fake.set_whoami(Err(TailnetError::NotConnected));
        let down = wait_for_status(&listener, |s| !is_serving(s)).await;
        assert_eq!(
            down,
            ListenerStatus::TailscaleDown(TailnetError::NotConnected)
        );
        assert_eq!(fake.whoami_calls(), 2, "down on the first failure");
        assert_eq!(serve.finished.load(Ordering::SeqCst), 1, "server stopped");
        std::net::TcpListener::bind(first[0]).expect("the address was released");
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn transient_whoami_errors_keep_serving_until_the_third() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        wait_for_status(&listener, is_serving).await;
        let seen = record_statuses(&listener);

        fake.set_whoami(Err(TailnetError::Timeout));
        let down = wait_for_status(&listener, |s| !is_serving(s)).await;
        assert_eq!(down, ListenerStatus::TailscaleDown(TailnetError::Timeout));
        assert_eq!(
            fake.whoami_calls(),
            4,
            "one answer, then two transient failures kept serving and the third stopped it"
        );
        assert_eq!(
            *seen.lock().expect("status log lock"),
            [ListenerStatus::TailscaleDown(TailnetError::Timeout)],
            "the status stayed Serving through the first two failures"
        );
        assert_eq!(serve.finished.load(Ordering::SeqCst), 1, "server stopped");
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_waits_until_every_port_is_released() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        let addrs = serving_addrs(&wait_for_status(&listener, is_serving).await);
        shutdown.send_replace(true);
        listener.stopped().await;
        std::net::TcpListener::bind(addrs[0]).expect("the port binds again at once");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_before_start_binds_nothing() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let (shutdown_tx, shutdown_rx) = watch::channel(true);
        let listener = start_with_shutdown(&fake, &serve, options(0), shutdown_rx);
        let status = listener.status();
        listener.stopped().await;
        assert_eq!(*status.borrow(), ListenerStatus::Starting);
        assert_eq!(fake.whoami_calls(), 0, "Tailscale was never asked");
        assert_eq!(serve.started.load(Ordering::SeqCst), 0, "nothing bound");
        drop(shutdown_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_mid_bind_stops_the_retry() {
        let held = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("hold a port");
        let port = held.local_addr().expect("held addr").port();
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let began = Instant::now();
        let (listener, shutdown) = start(&fake, &serve, options(port));
        let status = listener.status();
        tokio::time::sleep(Duration::from_millis(500)).await;
        shutdown.send_replace(true);
        listener.stopped().await;
        assert!(
            began.elapsed() < ListenerOptions::PRODUCTION.bind_budget,
            "the retry stopped at shutdown: {:?}",
            began.elapsed()
        );
        assert_eq!(*status.borrow(), ListenerStatus::Starting);
        assert_eq!(serve.started.load(Ordering::SeqCst), 0, "nothing bound");
        drop(held);
    }
}
