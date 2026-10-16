//! The tailnet listener: one server per tailnet address of this host, on a
//! fixed port, rebinding when Tailscale's address set changes.
//!
//! The addresses come from `tailscale whoami`, never the unspecified address.
//! All Tailscale work runs on a supervisor task, so loopback serving never
//! waits on it: Tailscale being down is a status and a retry, not an error.
//!
//! Every server serves TLS only, with the certificate `tailscale cert` writes
//! for the host's `MagicDNS` name. It is checked at start and every
//! `cert_interval`, and swapped into every server at once when its PEM
//! changes. Until one has loaded, nothing binds: there is no plaintext path
//! and no self-signed fallback.
//!
//! Every accepted connection passes the [`TailnetAcceptor`] before its TLS
//! handshake: a peer outside Tailscale's ranges, a peer `tailscale whois`
//! cannot identify, and a peer the owner rule refuses are dropped without a
//! byte written. An admitted connection's requests carry its
//! [`AuthorizedPeer`].

use anyhow::Context as _;
use axum::middleware::AddExtension;
use axum_server::Handle;
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, MutexGuard, PoisonError};
use std::time::Duration;
use tailnet::{Denied, PeerIdentity, TailnetError, WhoIs};
use tokio::sync::{OnceCell, Semaphore, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tower::Layer as _;

/// Consecutive transient `whoami` failures after which Tailscale counts as
/// down.
const TRANSIENT_LIMIT: u32 = 3;

/// The longest pause between retries of a failing certificate check while a
/// certificate is in service.
const CERT_BACKOFF_CAP: Duration = Duration::from_hours(1);

/// The longest pause between retries of a failing certificate check while
/// none is in service, so HTTPS being enabled (or Tailscale finishing its
/// connect) is picked up soon.
const CERT_BACKOFF_CAP_DOWN: Duration = Duration::from_mins(5);

/// What the listener is doing, published on a watch for the status surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerStatus {
    /// Started; Tailscale not asked yet.
    Starting,
    /// Serving on `addrs` (one server per address), as `name`: the
    /// `MagicDNS` name (no trailing dot) of the certificate in service, which
    /// lags a rename until the new name's certificate has loaded.
    Serving {
        name: String,
        addrs: Vec<SocketAddr>,
    },
    /// HTTPS certificates are disabled in the tailnet's admin console; no
    /// server runs, even when a certificate had loaded before.
    HttpsDisabled,
    /// No certificate is in service: `tailscale cert` failed for another
    /// reason, its files were unusable, or `whoami` gave no name to ask for.
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
    /// How often `tailscale cert` is asked again once a certificate loaded.
    pub cert_interval: Duration,
    /// The validity left below which `tailscale cert` renews the certificate.
    pub cert_min_validity: Duration,
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
        cert_interval: Duration::from_hours(24),
        cert_min_validity: Duration::from_hours(14 * 24),
    };
}

/// Serves the connections of one bound listener with `tls`, admitting only
/// the peers `acceptor` lets through, until its handle is shut down. Every
/// server is handed the same `tls` and the same `acceptor`, so a reload
/// reaches all of them at once and they share one verdict cache.
pub trait Serve<A: TailnetApi>: Send + Sync + 'static {
    fn serve(
        &self,
        listener: std::net::TcpListener,
        handle: Handle<SocketAddr>,
        tls: RustlsConfig,
        acceptor: TailnetAcceptor<A>,
    ) -> impl Future<Output = std::io::Result<()>> + Send;
}

/// The production [`Serve`]: `router` over TLS, behind the whois acceptor.
struct TlsServe {
    router: axum::Router,
}

impl<A: TailnetApi> Serve<A> for TlsServe {
    async fn serve(
        &self,
        listener: std::net::TcpListener,
        handle: Handle<SocketAddr>,
        tls: RustlsConfig,
        acceptor: TailnetAcceptor<A>,
    ) -> std::io::Result<()> {
        axum_server::from_tcp(listener)?
            .handle(handle)
            .acceptor(RustlsAcceptor::new(tls).acceptor(acceptor))
            .serve(self.router.clone().into_make_service())
            .await
    }
}

/// What [`TailnetListener::start_with`] serves with: the CLI seam, the
/// server, and the peer policy.
struct Wiring<A, S> {
    api: Arc<A>,
    serve: Arc<S>,
    policy: PeerPolicy,
}

/// The running listener: a supervisor task that owns the servers, and the
/// watches it publishes. Dropping it stops the supervisor and every server.
pub struct TailnetListener {
    status: watch::Receiver<ListenerStatus>,
    name: watch::Receiver<Option<String>>,
    /// The supervisor task. [`TailnetListener::stopped`] waits on it in
    /// place rather than taking it, so a `stopped()` dropped before it
    /// returns still leaves the handle here for [`Drop`] to abort.
    supervisor: JoinHandle<()>,
    /// What the certificate task publishes to the supervisor; the sender goes
    /// with the task, so `changed` stops yielding `Ok` once the task has
    /// ended, whether it ended on its own or was aborted by the supervisor's
    /// own end.
    cert_state: watch::Receiver<CertState>,
    /// How many times a changed certificate was swapped in.
    #[cfg(test)]
    reloads: Arc<AtomicUsize>,
    /// Ends the cert task for the test that needs it ended while the
    /// supervisor runs; the supervisor's own end already ends it.
    #[cfg(test)]
    cert_task: tokio::task::AbortHandle,
    /// Whether the cert task had finished when the supervisor returned.
    #[cfg(test)]
    cert_done_at_exit: Arc<AtomicBool>,
    /// Whether the supervisor should panic at the top of its loop.
    #[cfg(test)]
    panic_supervisor: Arc<AtomicBool>,
}

impl TailnetListener {
    /// Start the listener and return at once: it serves `router` over TLS on
    /// port 8787 of each of the host's tailnet addresses, with the
    /// certificate `tailscale cert` writes into `tailnet_dir` as
    /// `<name>.crt` and `<name>.key`, until `shutdown` turns true (or its
    /// sender is dropped) or the listener is dropped. Only the host owner's
    /// own devices get past the [`TailnetAcceptor`], and every request
    /// `router` sees carries their [`AuthorizedPeer`].
    pub fn start(
        router: axum::Router,
        tailnet_dir: PathBuf,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        let wiring = Wiring {
            api: Arc::new(tailnet::Cli::locate()),
            serve: Arc::new(TlsServe { router }),
            policy: PeerPolicy::PRODUCTION,
        };
        Self::start_with(wiring, ListenerOptions::PRODUCTION, tailnet_dir, shutdown)
    }

    /// Start the supervisor and return at once; it asks `whoami` for the
    /// addresses, `tailscale cert` for the certificate, binds one server per
    /// address on `options.port`, and polls for changes until `shutdown`
    /// turns true (or its sender is dropped), then shuts every server down.
    fn start_with<A: TailnetApi, S: Serve<A>>(
        wiring: Wiring<A, S>,
        options: ListenerOptions,
        tailnet_dir: PathBuf,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        let Wiring { api, serve, policy } = wiring;
        let (host, host_answer) = watch::channel(None);
        let acceptor = TailnetAcceptor::new(
            Arc::clone(&api),
            host_answer,
            policy,
            WhoisLimits::PRODUCTION,
        );
        let (status_tx, status) = watch::channel(ListenerStatus::Starting);
        let (name_tx, name) = watch::channel(None);
        #[cfg(test)]
        let reloads = Arc::new(AtomicUsize::new(0));
        let (cert_tx, cert) = watch::channel(CertState::Pending);
        let (cert_name, wanted) = watch::channel(None);
        let worker = CertWorker {
            api: Arc::clone(&api),
            options,
            tailnet_dir,
            state: cert_tx,
            tls: None,
            loaded: None,
            checked_name: None,
            next_check: Instant::now(),
            backoff: options.poll_interval,
            last_failure: None,
            #[cfg(test)]
            reloads: Arc::clone(&reloads),
        };
        let cert_task = AbortOnDrop(tokio::spawn(worker.run(wanted)));
        #[cfg(test)]
        let cert_abort = cert_task.0.abort_handle();
        let cert_state = cert.clone();
        #[cfg(test)]
        let cert_done_at_exit = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let panic_supervisor = Arc::new(AtomicBool::new(false));
        let supervisor = Supervisor {
            api,
            serve,
            acceptor,
            host,
            options,
            status: status_tx,
            name: name_tx,
            servers: BTreeMap::new(),
            cert,
            cert_name,
            cert_task,
            cert_ended: false,
            last_who: None,
            no_name_reported: false,
            bind_failures: BTreeMap::new(),
            transient_failures: 0,
            last_down_error: None,
            #[cfg(test)]
            cert_done_at_exit: Arc::clone(&cert_done_at_exit),
            #[cfg(test)]
            panic_now: Arc::clone(&panic_supervisor),
        };
        let supervisor = tokio::spawn(supervisor.run(shutdown));
        Self {
            status,
            name,
            supervisor,
            cert_state,
            #[cfg(test)]
            reloads,
            #[cfg(test)]
            cert_task: cert_abort,
            #[cfg(test)]
            cert_done_at_exit,
            #[cfg(test)]
            panic_supervisor,
        }
    }

    #[cfg(test)]
    fn reloads(&self) -> usize {
        self.reloads.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn abort_cert_task(&self) {
        self.cert_task.abort();
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

    /// Wait until the supervisor has ended and the cert task has ended too,
    /// so no `tailscale cert` call outlives the listener.
    ///
    /// On the normal path the supervisor has released every server's address
    /// by then as well: it aborts the cert task and waits for it before it
    /// stops the servers. A supervisor that panicked never runs that
    /// shutdown, and its servers are dropped — each asking its handle to shut
    /// down — without waiting out their grace, so one may hold its address
    /// for up to [`ListenerOptions::shutdown_grace`] after this returns. What
    /// holds on every path is the cert task's end; the supervisor's own end
    /// aborts it, and the wait below sees that abort through.
    ///
    /// Dropping this future before it returns hands the listener to [`Drop`],
    /// which aborts the supervisor, and with it the cert task.
    pub async fn stopped(mut self) {
        if let Err(err) = (&mut self.supervisor).await
            && !err.is_cancelled()
        {
            tracing::warn!(%err, "tailnet listener supervisor failed");
        }
        // The task's sender goes with it, so this ends once its future is
        // gone.
        while self.cert_state.changed().await.is_ok() {}
    }
}

impl Drop for TailnetListener {
    /// Stop the supervisor; dropping it drops every [`Server`], which shuts
    /// each one down, and its `AbortOnDrop`, which ends the cert task.
    fn drop(&mut self) {
        self.supervisor.abort();
    }
}

/// Aborts its task when dropped, so the task never outlives its owner.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The certificate in service: the SHA-256 digests of the files it was built
/// from (never the key itself), and the name it was asked for.
struct LoadedCert {
    name: String,
    cert_digest: [u8; 32],
    key_digest: [u8; 32],
}

/// The certificate as the cert task last left it, for the supervisor.
#[derive(Clone)]
enum CertState {
    /// None is in service and the first check for the wanted name is still
    /// running (or has not begun).
    Pending,
    /// No certificate is in service; the last check, for `name`, failed and
    /// `status` says why.
    Down {
        name: String,
        status: ListenerStatus,
    },
    /// The certificate for `name` is in service in `tls`.
    Ready { name: String, tls: RustlsConfig },
}

impl CertState {
    /// Whether `other` tells the supervisor nothing new. Every `Ready`
    /// carries the one config the worker keeps, so its name is all that
    /// can change.
    fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Pending, Self::Pending) => true,
            (
                Self::Down { name, status },
                Self::Down {
                    name: other_name,
                    status: other_status,
                },
            ) => name == other_name && status == other_status,
            (
                Self::Ready { name, .. },
                Self::Ready {
                    name: other_name, ..
                },
            ) => name == other_name,
            _ => false,
        }
    }
}

/// Why a certificate check failed.
enum CertFailure {
    /// `tailscale cert` failed.
    Cli(TailnetError),
    /// The files it wrote could not be read.
    Read(std::io::Error),
    /// The files do not make a usable certificate and key.
    Unusable(anyhow::Error),
}

impl CertFailure {
    /// The status while no certificate is in service.
    fn status(&self) -> ListenerStatus {
        match self {
            Self::Cli(TailnetError::HttpsDisabled) => ListenerStatus::HttpsDisabled,
            _ => ListenerStatus::CertFailed,
        }
    }
}

impl std::fmt::Display for CertFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cli(err) => write!(f, "tailscale cert failed: {err}"),
            Self::Read(err) => write!(f, "could not read the certificate files: {err}"),
            Self::Unusable(err) => write!(f, "the certificate files are unusable: {err:#}"),
        }
    }
}

/// The cert task: runs `tailscale cert` for the name the supervisor wants,
/// at once (and at once again for a new name), then every `cert_interval`,
/// and loads what it writes. A failed check is retried after a pause that
/// starts at `poll_interval` (again on a new name) and doubles up to
/// [`CERT_BACKOFF_CAP`] while the wanted name's certificate is in service,
/// up to [`CERT_BACKOFF_CAP_DOWN`] while it is not. HTTPS being disabled
/// takes the certificate out of service. It runs beside the supervisor, so
/// `whoami` and rebinding never wait on it, and publishes a state only when
/// it changes.
struct CertWorker<A> {
    api: Arc<A>,
    options: ListenerOptions,
    /// Where `tailscale cert` writes the certificate and key.
    tailnet_dir: PathBuf,
    state: watch::Sender<CertState>,
    /// The TLS config every server shares, one for the worker's lifetime so
    /// every server gets every reload; `None` until a certificate first
    /// loaded.
    tls: Option<RustlsConfig>,
    /// The certificate in service in `tls`; `None` while none is (before
    /// the first load, and after HTTPS was found disabled).
    loaded: Option<LoadedCert>,
    /// The name the last check asked for, so a rename is checked at once.
    checked_name: Option<String>,
    next_check: Instant,
    /// The pause after the next failed check.
    backoff: Duration,
    /// The name and text of the last failure, so a repeat logs at debug.
    last_failure: Option<(String, String)>,
    #[cfg(test)]
    reloads: Arc<AtomicUsize>,
}

impl<A: TailnetApi> CertWorker<A> {
    /// Check whenever one is due for the wanted name, until the supervisor
    /// drops its sender.
    async fn run(mut self, mut wanted: watch::Receiver<Option<String>>) {
        loop {
            let name = wanted.borrow_and_update().clone();
            if let Some(name) = name.as_deref() {
                let renamed = self.checked_name.as_deref() != Some(name);
                if renamed {
                    self.renamed();
                }
                if renamed || Instant::now() >= self.next_check {
                    self.check(name).await;
                }
            }
            let changed = if name.is_some() {
                tokio::select! {
                    changed = wanted.changed() => changed,
                    () = tokio::time::sleep_until(self.next_check) => Ok(()),
                }
            } else {
                wanted.changed().await
            };
            if changed.is_err() {
                return;
            }
        }
    }

    /// A new name starts its backoff afresh; while no certificate is in
    /// service, its first check is pending.
    fn renamed(&mut self) {
        self.backoff = self.options.poll_interval;
        if self.loaded.is_none() {
            self.set_state(CertState::Pending);
        }
    }

    /// Publish `state` to the supervisor, waking it only when it changed.
    fn set_state(&self, state: CertState) {
        self.state.send_if_modified(|current| {
            let changed = !current.same_as(&state);
            if changed {
                *current = state;
            }
            changed
        });
    }

    /// Ask `tailscale cert` for `name`'s certificate and load what it wrote;
    /// schedule the next check after `cert_interval` only once that loaded.
    async fn check(&mut self, name: &str) {
        self.checked_name = Some(name.to_owned());
        let cert_file = self.tailnet_dir.join(format!("{name}.crt"));
        let key_file = self.tailnet_dir.join(format!("{name}.key"));
        let asked = self
            .api
            .cert(name, &cert_file, &key_file, self.options.cert_min_validity)
            .await;
        let outcome = match asked {
            Ok(()) => self.load(name, &cert_file, &key_file),
            Err(err) => Err(CertFailure::Cli(err)),
        };
        match outcome {
            Ok(()) => {
                self.last_failure = None;
                self.backoff = self.options.poll_interval;
                self.next_check = Instant::now() + self.options.cert_interval;
            }
            Err(failure) => self.failed(name, &failure),
        }
    }

    /// Log a failed check and schedule its retry. HTTPS being disabled takes
    /// the certificate in service out of it, so every server stops.
    fn failed(&mut self, name: &str, failure: &CertFailure) {
        let disabled = matches!(failure, CertFailure::Cli(TailnetError::HttpsDisabled));
        self.report(name, failure, disabled && self.loaded.is_some());
        if disabled {
            self.loaded = None;
        }
        if self.loaded.is_none() {
            self.set_state(CertState::Down {
                name: name.to_owned(),
                status: failure.status(),
            });
        }
        let in_service = self
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.name == name);
        let cap = if in_service {
            CERT_BACKOFF_CAP
        } else {
            CERT_BACKOFF_CAP_DOWN
        };
        let pause = self.backoff.min(cap);
        self.next_check = Instant::now() + pause;
        self.backoff = (pause * 2).min(cap);
    }

    /// Load the pair `tailscale cert` wrote. Changed files swap every
    /// server's certificate at once; unchanged ones only take the new name.
    /// On any failure the certificate in service stays.
    fn load(&mut self, name: &str, cert_file: &Path, key_file: &Path) -> Result<(), CertFailure> {
        let cert_pem = std::fs::read(cert_file).map_err(CertFailure::Read)?;
        let key_pem = std::fs::read(key_file).map_err(CertFailure::Read)?;
        let cert_digest: [u8; 32] = Sha256::digest(&cert_pem).into();
        let key_digest: [u8; 32] = Sha256::digest(&key_pem).into();
        let unchanged = self.loaded.as_ref().is_some_and(|loaded| {
            loaded.cert_digest == cert_digest && loaded.key_digest == key_digest
        });
        if !unchanged {
            let config = server_config(&cert_pem, &key_pem).map_err(CertFailure::Unusable)?;
            if let Some(tls) = &self.tls {
                tls.reload_from_config(config);
                #[cfg(test)]
                self.reloads.fetch_add(1, Ordering::SeqCst);
                tracing::info!(%name, "tailnet listener: certificate reloaded");
            } else {
                self.tls = Some(RustlsConfig::from_config(config));
                tracing::info!(%name, "tailnet listener: certificate loaded");
            }
        }
        let renamed = self
            .loaded
            .as_ref()
            .is_none_or(|loaded| loaded.name != name);
        self.loaded = Some(LoadedCert {
            name: name.to_owned(),
            cert_digest,
            key_digest,
        });
        if renamed && let Some(tls) = &self.tls {
            self.set_state(CertState::Ready {
                name: name.to_owned(),
                tls: tls.clone(),
            });
        }
        Ok(())
    }

    /// Log a failed check at warn the first time `name` fails this way (and
    /// whenever it stops the servers), at debug while it keeps failing the
    /// same way.
    fn report(&mut self, name: &str, failure: &CertFailure, stopping: bool) {
        let key = (name.to_owned(), failure.to_string());
        let text = key.1.as_str();
        if !stopping && self.last_failure.as_ref() == Some(&key) {
            tracing::debug!(%name, failure = %text, "tailnet listener: certificate check still failing");
        } else {
            match &self.loaded {
                _ if stopping => tracing::warn!(
                    %name,
                    failure = %text,
                    "tailnet listener: HTTPS certificates are disabled for the tailnet; servers stopped, retrying"
                ),
                None => tracing::warn!(
                    %name,
                    failure = %text,
                    "tailnet listener: no certificate; staying down, retrying"
                ),
                Some(loaded) if loaded.name != name => tracing::warn!(
                    %name,
                    serving = %loaded.name,
                    failure = %text,
                    "tailnet listener: no certificate for the new name yet; serving the old name's, retrying"
                ),
                Some(_) => tracing::warn!(
                    %name,
                    failure = %text,
                    "tailnet listener: certificate check failed; keeping the certificate in service, retrying"
                ),
            }
        }
        self.last_failure = Some(key);
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
    /// The gate every server's connections pass, shared by all of them.
    acceptor: TailnetAcceptor<A>,
    /// The last `whoami` answer while Tailscale is up, for the acceptor's
    /// owner rule; `None` before one and after Tailscale went down.
    host: watch::Sender<Option<WhoIs>>,
    options: ListenerOptions,
    status: watch::Sender<ListenerStatus>,
    name: watch::Sender<Option<String>>,
    servers: BTreeMap<IpAddr, Server>,
    /// What the cert task has in service.
    cert: watch::Receiver<CertState>,
    /// The name the cert task asks certificates for.
    cert_name: watch::Sender<Option<String>>,
    cert_task: AbortOnDrop,
    /// Whether the cert task has ended while the supervisor runs: no
    /// certificate will load again, and `cert` is no longer waited on.
    cert_ended: bool,
    /// The last `whoami` answer while Tailscale is up, applied again when the
    /// certificate changes.
    last_who: Option<WhoIs>,
    /// Whether a missing `MagicDNS` name was logged at warn already.
    no_name_reported: bool,
    /// The last bind failure per address, so a repeat logs at debug.
    bind_failures: BTreeMap<IpAddr, ErrorKind>,
    /// Consecutive transient `whoami` failures.
    transient_failures: u32,
    /// The last `whoami` failure that took Tailscale down, so a repeat with
    /// no server to stop logs at debug.
    last_down_error: Option<TailnetError>,
    #[cfg(test)]
    cert_done_at_exit: Arc<AtomicBool>,
    /// Whether a test asked the supervisor to panic at the top of its loop.
    #[cfg(test)]
    panic_now: Arc<AtomicBool>,
}

impl<A: TailnetApi, S: Serve<A>> Supervisor<A, S> {
    /// A test can make the supervisor panic here, standing in for a panic
    /// anywhere in it, so the rest of its own shutdown — the wait for the
    /// cert task's abort included — never runs. The `AbortOnDrop` the cert
    /// task's abort comes from lives in the task's state machine, not on the
    /// stack: the unwind does not drop it, tokio drops the future once it has
    /// caught the panic, and that drop still aborts the cert task.
    #[cfg(test)]
    #[expect(clippy::panic, reason = "the test asks for a supervisor panic")]
    fn panic_if_asked(&self) {
        if !self.panic_now.load(Ordering::SeqCst) {
            return;
        }
        panic!("scripted tailnet supervisor panic");
    }

    async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut down_backoff = self.options.down_backoff;
        while !*shutdown.borrow() {
            #[cfg(test)]
            self.panic_if_asked();
            let Some(answer) = until_shutdown(self.api.whoami(), &mut shutdown).await else {
                break;
            };
            let pause = match answer {
                Ok(who) => {
                    self.host.send_replace(Some(who.clone()));
                    down_backoff = self.options.down_backoff;
                    self.transient_failures = 0;
                    self.last_down_error = None;
                    let flow = self.apply(&who, &mut shutdown).await;
                    self.last_who = Some(who);
                    if flow == Flow::Shutdown {
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
            if self.pause(pause, &mut shutdown).await == Flow::Shutdown {
                break;
            }
        }
        // No `tailscale cert` call may outlive the listener, nor wait out the
        // servers' drain.
        self.cert_task.0.abort();
        if let Err(err) = (&mut self.cert_task.0).await
            && !err.is_cancelled()
            && !self.cert_ended
        {
            tracing::warn!(%err, "tailnet listener: the certificate task failed");
        }
        self.stop_all().await;
        #[cfg(test)]
        self.cert_done_at_exit
            .store(self.cert_task.0.is_finished(), Ordering::SeqCst);
    }

    /// Wait `pause` before the next `whoami`, applying the last answer again
    /// whenever the certificate changes, so a newly loaded certificate starts
    /// the servers at once.
    async fn pause(&mut self, pause: Duration, shutdown: &mut watch::Receiver<bool>) -> Flow {
        let deadline = Instant::now() + pause;
        loop {
            if self.cert_ended {
                let slept = until_shutdown(tokio::time::sleep_until(deadline), shutdown).await;
                return if slept.is_some() {
                    Flow::Continue
                } else {
                    Flow::Shutdown
                };
            }
            let cert_alive = tokio::select! {
                biased;
                _ = shutdown.wait_for(|stop| *stop) => return Flow::Shutdown,
                () = tokio::time::sleep_until(deadline) => return Flow::Continue,
                changed = self.cert.changed() => changed.is_ok(),
            };
            if !cert_alive {
                self.cert_task_ended().await;
                continue;
            }
            if let Some(who) = self.last_who.clone()
                && self.apply(&who, shutdown).await == Flow::Shutdown
            {
                return Flow::Shutdown;
            }
        }
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
        self.last_who = None;
        self.host.send_replace(None);
        self.stop_all().await;
        self.publish(ListenerStatus::TailscaleDown(err));
    }

    /// Make sure a certificate is in service for `who`'s name, then bring the
    /// servers in line with `who`'s addresses: stop the ones whose address
    /// vanished (or whose task ended), start the missing ones.
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
        self.report_no_name(name.is_none());
        self.cert_name.send_if_modified(|current| {
            let changed = *current != name;
            if changed {
                current.clone_from(&name);
            }
            changed
        });
        if self.cert_ended || self.cert.has_changed().is_err() {
            self.cert_task_ended().await;
            return Flow::Continue;
        }
        let state = self.cert.borrow_and_update().clone();
        let down = match state {
            CertState::Ready { name, tls } => {
                return self.serve_addresses(who, name, &tls, shutdown).await;
            }
            _ if name.is_none() => ListenerStatus::CertFailed,
            CertState::Down {
                name: failed_for,
                status,
            } if name.as_deref() == Some(failed_for.as_str()) => status,
            // The wanted name's first check is still to finish.
            CertState::Pending | CertState::Down { .. } => ListenerStatus::Starting,
        };
        self.stop_all().await;
        self.publish(down);
        Flow::Continue
    }

    /// The cert task ended while the supervisor runs: log it once, stop every
    /// server, and say no certificate is in service.
    async fn cert_task_ended(&mut self) {
        if !self.cert_ended {
            self.cert_ended = true;
            tracing::error!(
                "tailnet listener: the certificate task ended; servers stopped until the daemon restarts"
            );
        }
        self.stop_all().await;
        self.publish(ListenerStatus::CertFailed);
    }

    /// Bring the servers in line with `who`'s addresses, serving `tls` as
    /// `cert_name`.
    async fn serve_addresses(
        &mut self,
        who: &WhoIs,
        cert_name: String,
        tls: &RustlsConfig,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Flow {
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
                && self.start_server(ip, tls, &mut outcome, shutdown).await == Flow::Shutdown
            {
                return Flow::Shutdown;
            }
        }
        self.publish(self.status_after(cert_name, outcome));
        Flow::Continue
    }

    /// Log a `whoami` answer without a `MagicDNS` name (so no certificate to
    /// ask for) at warn the first time, at debug while it repeats.
    fn report_no_name(&mut self, missing: bool) {
        if !missing {
            self.no_name_reported = false;
        } else if self.no_name_reported {
            tracing::debug!("tailnet listener: whoami still gives no MagicDNS name");
        } else {
            tracing::warn!(
                "tailnet listener: whoami gave no MagicDNS name, so there is no certificate to ask for"
            );
            self.no_name_reported = true;
        }
    }

    async fn start_server(
        &mut self,
        ip: IpAddr,
        tls: &RustlsConfig,
        outcome: &mut BindOutcome,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Flow {
        let addr = SocketAddr::new(ip, self.options.port);
        let bind = bind_with_retry(addr, self.options.bind_budget, self.options.bind_backoff);
        let Some(bound) = until_shutdown(bind, shutdown).await else {
            return Flow::Shutdown;
        };
        let spawned = match bound {
            Ok(listener) => self.spawn_server(listener, tls).map_err(BindError::Other),
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

    fn spawn_server(
        &self,
        listener: std::net::TcpListener,
        tls: &RustlsConfig,
    ) -> std::io::Result<Server> {
        let local = listener.local_addr()?;
        let handle = Handle::new();
        let serve = Arc::clone(&self.serve);
        let task_handle = handle.clone();
        let tls = tls.clone();
        let acceptor = self.acceptor.clone();
        let task = tokio::spawn(async move {
            if let Err(err) = serve.serve(listener, task_handle, tls, acceptor).await {
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

    fn status_after(&self, name: String, outcome: BindOutcome) -> ListenerStatus {
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

/// How long a verdict on a peer IP is reused before `whois` is asked again.
const VERDICT_TTL: Duration = Duration::from_secs(30);

/// The most peer IPs the verdict cache holds.
const VERDICT_CAP: usize = 256;

/// The most `whois` calls in flight at once, across every peer.
const WHOIS_CONCURRENCY: usize = 4;

/// The tailnet device a connection came from, on every request the tailnet
/// listener serves. Only this module makes one, and only for a connection
/// the owner rule admitted, so a handler that finds it can trust it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedPeer(PeerIdentity);

impl AuthorizedPeer {
    /// The device's identity from `tailscale whois`.
    #[must_use]
    pub fn identity(&self) -> &PeerIdentity {
        &self.0
    }
}

/// Which peer addresses are tailnet ones, and the owner rule. The listener
/// serves with [`PeerPolicy::PRODUCTION`]; only tests pass another, through
/// the private [`TailnetListener::start_with`].
#[derive(Clone, Copy)]
struct PeerPolicy {
    in_range: fn(IpAddr) -> bool,
    authorize: fn(IpAddr, &WhoIs, &WhoIs) -> Result<PeerIdentity, Denied>,
}

impl PeerPolicy {
    const PRODUCTION: Self = Self {
        in_range: tailnet::in_tailscale_range,
        authorize: tailnet::authorize,
    };
}

/// What the owner rule decided about a peer IP.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Allow(PeerIdentity),
    Deny(Denied),
}

/// How long a peer IP whose `whois` failed is refused without asking again.
const FAILURE_BACKOFF: Duration = Duration::from_secs(5);

/// How long a `whois` may take, and how long a connection may wait for one
/// of the permits to ask it.
#[derive(Debug, Clone, Copy)]
struct WhoisLimits {
    timeout: Duration,
    permit_wait: Duration,
}

impl WhoisLimits {
    const PRODUCTION: Self = Self {
        timeout: tailnet::DEFAULT_TIMEOUT,
        permit_wait: tailnet::DEFAULT_TIMEOUT,
    };
}

/// Entries per canonical peer IP, each good for `ttl`. Holds at most `cap`:
/// a new IP in a full map first drops the expired entries, then the oldest.
struct TtlMap<V> {
    ttl: Duration,
    cap: usize,
    entries: HashMap<IpAddr, (Instant, V)>,
}

impl<V> TtlMap<V> {
    fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap,
            entries: HashMap::new(),
        }
    }

    /// The entry for `ip`, unless it is older than the TTL at `now`.
    fn get(&self, ip: IpAddr, now: Instant) -> Option<&V> {
        self.entries
            .get(&ip)
            .filter(|(at, _)| now.saturating_duration_since(*at) < self.ttl)
            .map(|(_, value)| value)
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    fn insert(&mut self, ip: IpAddr, value: V, now: Instant) {
        if !self.entries.contains_key(&ip) && self.entries.len() >= self.cap {
            let ttl = self.ttl;
            self.entries
                .retain(|_, (at, _)| now.saturating_duration_since(*at) < ttl);
            if self.entries.len() >= self.cap {
                let oldest = self
                    .entries
                    .iter()
                    .min_by_key(|(_, (at, _))| *at)
                    .map(|(oldest, _)| *oldest);
                if let Some(oldest) = oldest {
                    self.entries.remove(&oldest);
                }
            }
        }
        self.entries.insert(ip, (now, value));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The gate before every tailnet connection's TLS handshake: it admits a
/// peer only when its address is in Tailscale's ranges, `tailscale whois`
/// identifies it, and the owner rule allows it, and drops every other
/// connection without writing a byte. Every server of one listener shares
/// it, and so its verdict cache.
pub struct TailnetAcceptor<A> {
    gate: Arc<Gate<A>>,
}

impl<A> Clone for TailnetAcceptor<A> {
    fn clone(&self) -> Self {
        Self {
            gate: Arc::clone(&self.gate),
        }
    }
}

struct Gate<A> {
    api: Arc<A>,
    /// The host's latest `whoami` answer; `None` denies every peer.
    host: watch::Receiver<Option<WhoIs>>,
    policy: PeerPolicy,
    limits: WhoisLimits,
    permits: Semaphore,
    state: std::sync::Mutex<GateState>,
}

/// The host identity verdicts are bound to: its node id and user id.
type HostKey = (i64, i64);

fn host_key(host: &WhoIs) -> HostKey {
    (host.node.id, host.node.user)
}

/// One `whois` round for a peer IP; `None` when it failed.
type Round = Arc<OnceCell<Option<Verdict>>>;

struct GateState {
    /// The host the verdicts and the rounds under way were made against;
    /// `None` while there is no host answer.
    host: Option<HostKey>,
    /// Whether "no whoami answer" was logged at warn since the host went.
    no_host_reported: bool,
    cache: TtlMap<Verdict>,
    /// Peer IPs whose last `whois` round failed, refused until it expires.
    failures: TtlMap<()>,
    /// The `whois` round under way per peer IP, which every connection from
    /// that IP waits on.
    flights: HashMap<IpAddr, Round>,
}

impl GateState {
    /// Bind the state to `host`: a changed host identity drops every
    /// verdict and every round under way.
    fn follow_host(&mut self, host: Option<HostKey>) {
        if self.host != host {
            self.host = host;
            self.no_host_reported = false;
            self.cache.clear();
            self.flights.clear();
        }
    }
}

/// How a connection's verdict is reached.
enum Lookup {
    /// Without a round: a cached verdict, or `None` to refuse.
    Settled(Option<Verdict>),
    /// The `whois` round to run or join, against `host`.
    Round { round: Round, host: WhoIs },
}

impl<A: TailnetApi> TailnetAcceptor<A> {
    fn new(
        api: Arc<A>,
        host: watch::Receiver<Option<WhoIs>>,
        policy: PeerPolicy,
        limits: WhoisLimits,
    ) -> Self {
        let gate = Gate {
            api,
            host,
            policy,
            limits,
            permits: Semaphore::new(WHOIS_CONCURRENCY),
            state: std::sync::Mutex::new(GateState {
                host: None,
                no_host_reported: false,
                cache: TtlMap::new(VERDICT_TTL, VERDICT_CAP),
                failures: TtlMap::new(FAILURE_BACKOFF, VERDICT_CAP),
                flights: HashMap::new(),
            }),
        };
        Self {
            gate: Arc::new(gate),
        }
    }
}

impl<A: TailnetApi> Gate<A> {
    /// Admit the connection from `peer`, the accepted socket's own peer
    /// address, or say why not.
    async fn admit(&self, peer: SocketAddr) -> std::io::Result<PeerIdentity> {
        let peer = SocketAddr::new(peer.ip().to_canonical(), peer.port());
        if !(self.policy.in_range)(peer.ip()) {
            tracing::debug!(%peer, "tailnet listener: dropped a peer outside Tailscale's ranges");
            return Err(refused("outside Tailscale's ranges"));
        }
        let verdict = match self.lookup(peer) {
            Lookup::Settled(verdict) => verdict,
            Lookup::Round { round, host } => round
                .get_or_init(|| self.run_round(peer, &round, host))
                .await
                .clone(),
        };
        match verdict {
            Some(Verdict::Allow(identity)) => Ok(identity),
            Some(Verdict::Deny(denied)) => Err(refused(&denied.to_string())),
            None => Err(refused("no verdict from whois")),
        }
    }

    fn state(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn current_host(&self) -> Option<WhoIs> {
        self.host.borrow().clone()
    }

    /// Settle `peer`'s connection without a round where one is not needed:
    /// no host answer refuses, a cached verdict (against the current host)
    /// answers, a recent failure refuses. Else the round to run or join.
    /// Refusals here log at debug: their window's first one warned.
    fn lookup(&self, peer: SocketAddr) -> Lookup {
        let ip = peer.ip();
        let host = self.current_host();
        let now = Instant::now();
        let mut state = self.state();
        state.follow_host(host.as_ref().map(host_key));
        let Some(host) = host else {
            if state.no_host_reported {
                tracing::debug!(%peer, "tailnet listener: no whoami answer yet; connection dropped");
            } else {
                tracing::warn!(%peer, "tailnet listener: no whoami answer yet; connection dropped");
                state.no_host_reported = true;
            }
            return Lookup::Settled(None);
        };
        if let Some(verdict) = state.cache.get(ip, now).cloned() {
            if let Verdict::Deny(reason) = &verdict {
                tracing::debug!(%peer, %reason, "tailnet listener: peer refused by the owner rule (cached)");
            }
            return Lookup::Settled(Some(verdict));
        }
        if state.failures.get(ip, now).is_some() {
            tracing::debug!(%peer, "tailnet listener: whois failed recently; connection dropped");
            return Lookup::Settled(None);
        }
        let round = Arc::clone(state.flights.entry(ip).or_default());
        Lookup::Round { round, host }
    }

    /// Run `round` for `peer` against `host` and land it. It runs inside the
    /// round's cell, so it lands once and every waiting connection gets its
    /// outcome.
    async fn run_round(&self, peer: SocketAddr, round: &Round, host: WhoIs) -> Option<Verdict> {
        let verdict = self.decide(peer, &host).await;
        self.land(peer, round, &host, verdict)
    }

    /// End `round`: no connection joins it from now on, its verdict is
    /// cached, and a failure starts the peer's backoff. A round that ran
    /// against a host that has changed since is discarded and refuses.
    fn land(
        &self,
        peer: SocketAddr,
        round: &Round,
        host: &WhoIs,
        verdict: Option<Verdict>,
    ) -> Option<Verdict> {
        let ip = peer.ip();
        let current = self.current_host().as_ref().map(host_key);
        let mut state = self.state();
        state.follow_host(current);
        if current != Some(host_key(host)) {
            tracing::warn!(%peer, "tailnet listener: the host's identity changed during whois; connection dropped");
            return None;
        }
        if state
            .flights
            .get(&ip)
            .is_some_and(|under_way| Arc::ptr_eq(under_way, round))
        {
            state.flights.remove(&ip);
        }
        let now = Instant::now();
        match &verdict {
            Some(verdict) => state.cache.insert(ip, verdict.clone(), now),
            None => state.failures.insert(ip, (), now),
        }
        verdict
    }

    /// One `whois` round for `peer` against `host`: a permit, `whois`, then
    /// the owner rule; `None` on any failure. Each outcome logs once per
    /// round, so once per window per peer IP.
    async fn decide(&self, peer: SocketAddr, host: &WhoIs) -> Option<Verdict> {
        let permit = tokio::time::timeout(self.limits.permit_wait, self.permits.acquire()).await;
        let Ok(Ok(permit)) = permit else {
            tracing::warn!(%peer, "tailnet listener: too many whois calls in flight; connection dropped");
            return None;
        };
        let answer = tokio::time::timeout(self.limits.timeout, self.api.whois(peer))
            .await
            .unwrap_or(Err(TailnetError::Timeout));
        drop(permit);
        let who = match answer {
            Ok(who) => who,
            Err(err) => {
                tracing::warn!(%peer, %err, "tailnet listener: whois failed; connection dropped");
                return None;
            }
        };
        match (self.policy.authorize)(peer.ip(), &who, host) {
            Ok(identity) => {
                tracing::info!(
                    %peer,
                    device = %identity.name,
                    stable_id = %identity.stable_id,
                    "tailnet listener: peer admitted"
                );
                Some(Verdict::Allow(identity))
            }
            Err(denied) => {
                tracing::warn!(%peer, reason = %denied, "tailnet listener: peer refused by the owner rule");
                Some(Verdict::Deny(denied))
            }
        }
    }
}

/// The refusal a dropped connection's accept ends with; axum-server drops
/// the connection on it, before the TLS handshake.
fn refused(reason: &str) -> std::io::Error {
    std::io::Error::new(
        ErrorKind::PermissionDenied,
        format!("tailnet peer refused: {reason}"),
    )
}

impl<A: TailnetApi, S: Send + 'static> Accept<tokio::net::TcpStream, S> for TailnetAcceptor<A> {
    type Stream = tokio::net::TcpStream;
    type Service = AddExtension<S, AuthorizedPeer>;
    type Future =
        Pin<Box<dyn Future<Output = std::io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: tokio::net::TcpStream, service: S) -> Self::Future {
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            let peer = stream.peer_addr().inspect_err(|err| {
                tracing::debug!(%err, "tailnet listener: the accepted socket has no peer address; connection dropped");
            })?;
            let identity = gate.admit(peer).await?;
            let service = axum::Extension(AuthorizedPeer(identity)).layer(service);
            Ok((stream, service))
        })
    }
}

/// The rustls config for a PEM certificate chain and key, on ring, with ALPN
/// `http/1.1` only: axum serves WebSocket over HTTP/1.1, not h2. Fails when
/// either does not parse or the key does not match the certificate.
fn server_config(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let chain = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .context("parsing the certificate PEM")?;
    anyhow::ensure!(
        !chain.is_empty(),
        "the certificate PEM holds no certificate"
    );
    let key = PrivateKeyDer::from_pem_slice(key_pem).context("parsing the key PEM")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("configuring rustls protocol versions")?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .context("pairing the certificate with its key")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
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
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose,
    };
    use rustls::pki_types::ServerName;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tailnet::{Hostinfo, Node, UserProfile};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::time::Instant;
    use tokio_rustls::TlsConnector;

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

    /// The name a certificate is asked for: `HOST_NAME` without its trailing
    /// dot.
    fn host() -> &'static str {
        HOST_NAME.trim_end_matches('.')
    }

    /// A scratch tailnet dir, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let tag = NEXT.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("rt-tailnet-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the scratch tailnet dir");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A test certificate authority, the only root the test client trusts.
    struct TestCa {
        der: rustls::pki_types::CertificateDer<'static>,
        issuer: Issuer<'static, KeyPair>,
    }

    impl TestCa {
        fn new() -> Self {
            let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(DnType::CommonName, "rustling-tulip test CA");
            params.key_usages = vec![
                KeyUsagePurpose::KeyCertSign,
                KeyUsagePurpose::DigitalSignature,
            ];
            let key = KeyPair::generate().expect("CA key");
            let cert = params.self_signed(&key).expect("self-signed CA");
            Self {
                der: cert.der().clone(),
                issuer: Issuer::new(params, key),
            }
        }

        /// A leaf for [`host`] with a fresh key, signed by this CA.
        fn leaf(&self) -> TestLeaf {
            let mut params = CertificateParams::new(vec![host().to_owned()]).expect("leaf params");
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            let key = KeyPair::generate().expect("leaf key");
            let cert = params
                .signed_by(&key, &self.issuer)
                .expect("leaf signed by the CA");
            TestLeaf {
                cert_pem: cert.pem(),
                key_pem: key.serialize_pem(),
                der: cert.der().to_vec(),
            }
        }
    }

    /// A certificate and its key as PEM, and the certificate's DER bytes.
    #[derive(Clone)]
    struct TestLeaf {
        cert_pem: String,
        key_pem: String,
        der: Vec<u8>,
    }

    /// What the fake `tailscale cert` does.
    #[derive(Clone)]
    enum CertScript {
        /// Write these bytes to the certificate and key files.
        Write {
            cert: Vec<u8>,
            key: Vec<u8>,
        },
        Fail(TailnetError),
        /// Panic inside the call, ending the cert task.
        Panic,
    }

    impl CertScript {
        fn leaf(leaf: &TestLeaf) -> Self {
            Self::Write {
                cert: leaf.cert_pem.clone().into_bytes(),
                key: leaf.key_pem.clone().into_bytes(),
            }
        }
    }

    /// One `tailscale cert` call's arguments, and when it was made.
    #[derive(Clone)]
    struct CertCall {
        name: String,
        cert_file: PathBuf,
        key_file: PathBuf,
        min_validity: Duration,
        at: Instant,
    }

    /// A scripted `tailscale`: each call returns the response set for it and
    /// is counted (`whoami` with the time of each call, `cert` with its
    /// arguments). `cert` writes `leaf`, minted by its own `ca`, into the
    /// files it is given unless scripted otherwise.
    struct FakeTailnet {
        whoami: Mutex<Result<WhoIs, TailnetError>>,
        whois: Mutex<Result<WhoIs, TailnetError>>,
        /// Per-IP `whois` answers, ahead of `whois`.
        whois_for: Mutex<HashMap<IpAddr, Result<WhoIs, TailnetError>>>,
        cert: Mutex<CertScript>,
        whoami_times: Mutex<Vec<Instant>>,
        /// The address each `whois` call asked about.
        whois_addrs: Mutex<Vec<SocketAddr>>,
        /// While true, a `whois` call waits before it answers.
        whois_held: watch::Sender<bool>,
        cert_calls: Mutex<Vec<CertCall>>,
        /// `cert` calls that ran to their end.
        cert_finished: AtomicUsize,
        /// `cert` calls whose future was dropped before it ended.
        cert_dropped: AtomicUsize,
        /// While true, a `cert` call waits before it acts.
        cert_held: watch::Sender<bool>,
        /// While true, a `whoami` call waits before it answers.
        whoami_held: watch::Sender<bool>,
        /// Ordered events shared with a [`PlainServe`], to observe ordering.
        events: Arc<Mutex<Vec<&'static str>>>,
        ca: TestCa,
        leaf: TestLeaf,
        dir: ScratchDir,
    }

    /// Counts a `cert` call as finished, or as dropped when its future is
    /// dropped before it ends.
    struct CertCallEnd<'a> {
        fake: &'a FakeTailnet,
        finished: bool,
    }

    impl Drop for CertCallEnd<'_> {
        fn drop(&mut self) {
            if self.finished {
                self.fake.cert_finished.fetch_add(1, Ordering::SeqCst);
            } else {
                self.fake.cert_dropped.fetch_add(1, Ordering::SeqCst);
                self.fake
                    .events
                    .lock()
                    .expect("events lock")
                    .push("cert call dropped");
            }
        }
    }

    impl FakeTailnet {
        fn with_whoami(whoami: Result<WhoIs, TailnetError>) -> Self {
            let ca = TestCa::new();
            let leaf = ca.leaf();
            Self {
                whoami: Mutex::new(whoami),
                whois: Mutex::new(Ok(peer_whois())),
                whois_for: Mutex::new(HashMap::new()),
                cert: Mutex::new(CertScript::leaf(&leaf)),
                whoami_times: Mutex::new(Vec::new()),
                whois_addrs: Mutex::new(Vec::new()),
                whois_held: watch::Sender::new(false),
                cert_calls: Mutex::new(Vec::new()),
                cert_finished: AtomicUsize::new(0),
                cert_dropped: AtomicUsize::new(0),
                cert_held: watch::Sender::new(false),
                whoami_held: watch::Sender::new(false),
                events: Arc::new(Mutex::new(Vec::new())),
                ca,
                leaf,
                dir: ScratchDir::new(),
            }
        }

        fn set_cert(&self, script: CertScript) {
            *self.cert.lock().expect("cert lock") = script;
        }

        fn cert_calls(&self) -> Vec<CertCall> {
            self.cert_calls.lock().expect("cert calls lock").clone()
        }

        fn cert_finished(&self) -> usize {
            self.cert_finished.load(Ordering::SeqCst)
        }

        fn cert_dropped(&self) -> usize {
            self.cert_dropped.load(Ordering::SeqCst)
        }

        fn dir(&self) -> PathBuf {
            self.dir.0.clone()
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

        fn set_whois(&self, whois: Result<WhoIs, TailnetError>) {
            *self.whois.lock().expect("whois lock") = whois;
        }

        fn whois_addrs(&self) -> Vec<SocketAddr> {
            self.whois_addrs.lock().expect("whois addrs lock").clone()
        }

        fn set_whois_for(&self, ip: IpAddr, whois: Result<WhoIs, TailnetError>) {
            self.whois_for
                .lock()
                .expect("whois for lock")
                .insert(ip, whois);
        }

        /// The `whois` calls that asked about `ip`.
        fn whois_calls_for(&self, ip: IpAddr) -> usize {
            self.whois_addrs()
                .iter()
                .filter(|addr| addr.ip() == ip)
                .count()
        }
    }

    impl TailnetApi for FakeTailnet {
        fn whois(
            &self,
            addr: SocketAddr,
        ) -> impl Future<Output = Result<WhoIs, TailnetError>> + Send {
            self.whois_addrs
                .lock()
                .expect("whois addrs lock")
                .push(addr);
            let scripted = self
                .whois_for
                .lock()
                .expect("whois for lock")
                .get(&addr.ip())
                .cloned();
            let answer = scripted.unwrap_or_else(|| self.whois.lock().expect("whois lock").clone());
            let mut held = self.whois_held.subscribe();
            async move {
                held.wait_for(|held| !*held)
                    .await
                    .expect("the fake outlives its whois calls");
                answer
            }
        }

        fn whoami(&self) -> impl Future<Output = Result<WhoIs, TailnetError>> + Send {
            self.whoami_times
                .lock()
                .expect("whoami times lock")
                .push(Instant::now());
            let answer = self.whoami.lock().expect("whoami lock").clone();
            let mut held = self.whoami_held.subscribe();
            async move {
                held.wait_for(|held| !*held)
                    .await
                    .expect("the fake outlives its whoami calls");
                answer
            }
        }

        fn cert(
            &self,
            name: &str,
            cert_file: &Path,
            key_file: &Path,
            min_validity: Duration,
        ) -> impl Future<Output = Result<(), TailnetError>> + Send {
            self.cert_calls
                .lock()
                .expect("cert calls lock")
                .push(CertCall {
                    name: name.to_owned(),
                    cert_file: cert_file.to_owned(),
                    key_file: key_file.to_owned(),
                    min_validity,
                    at: Instant::now(),
                });
            let script = self.cert.lock().expect("cert lock").clone();
            let mut held = self.cert_held.subscribe();
            let (cert_file, key_file) = (cert_file.to_owned(), key_file.to_owned());
            let end = CertCallEnd {
                fake: self,
                finished: false,
            };
            async move {
                // Bound whole, so the guard lives (and drops) with the call.
                let mut end = end;
                held.wait_for(|held| !*held)
                    .await
                    .expect("the fake outlives its cert calls");
                let result = match script {
                    CertScript::Write { cert, key } => {
                        std::fs::write(cert_file, cert).expect("write the scripted certificate");
                        std::fs::write(key_file, key).expect("write the scripted key");
                        Ok(())
                    }
                    CertScript::Fail(err) => Err(err),
                    #[expect(clippy::panic, reason = "the script ends the cert task with a panic")]
                    CertScript::Panic => std::panic::panic_any("scripted tailscale cert panic"),
                };
                end.finished = true;
                result
            }
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
        /// Where "server stopped" is logged when a server ends.
        events: Option<Arc<Mutex<Vec<&'static str>>>>,
    }

    impl<A: TailnetApi> Serve<A> for PlainServe {
        async fn serve(
            &self,
            listener: std::net::TcpListener,
            handle: Handle<SocketAddr>,
            _tls: RustlsConfig,
            _acceptor: TailnetAcceptor<A>,
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
            if let Some(events) = &self.events {
                events.lock().expect("events lock").push("server stopped");
            }
            result
        }
    }

    fn options(port: u16) -> ListenerOptions {
        ListenerOptions {
            port,
            ..ListenerOptions::PRODUCTION
        }
    }

    /// The tailnet address the loopback test peer stands for.
    const PEER_TAILNET_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 101, 102, 103));

    /// The owner's laptop as `whois` describes it: the host's user, untagged,
    /// at [`PEER_TAILNET_IP`].
    fn peer_whois() -> WhoIs {
        WhoIs {
            node: Node {
                id: 2,
                stable_id: "nPeerCNTRL".to_owned(),
                name: "laptop.tail1234.ts.net.".to_owned(),
                user: 2,
                sharer: 0,
                addresses: vec![format!("{PEER_TAILNET_IP}/32")],
                tags: Vec::new(),
                hostinfo: Hostinfo::default(),
            },
            user_profile: UserProfile {
                id: 2,
                login_name: "owner@example.com".to_owned(),
                display_name: "Owner".to_owned(),
            },
        }
    }

    /// Admits loopback peers, the only ones a test can make, and judges each
    /// by the real owner rule as if it came from [`PEER_TAILNET_IP`].
    fn loopback_policy() -> PeerPolicy {
        PeerPolicy {
            in_range: |ip: IpAddr| ip.is_loopback(),
            authorize: |ip: IpAddr, peer: &WhoIs, host: &WhoIs| {
                let ip = if ip.is_loopback() {
                    PEER_TAILNET_IP
                } else {
                    ip
                };
                tailnet::authorize(ip, peer, host)
            },
        }
    }

    fn launch<S: Serve<FakeTailnet>>(
        fake: &Arc<FakeTailnet>,
        serve: &Arc<S>,
        options: ListenerOptions,
        policy: PeerPolicy,
        shutdown: watch::Receiver<bool>,
    ) -> TailnetListener {
        let wiring = Wiring {
            api: Arc::clone(fake),
            serve: Arc::clone(serve),
            policy,
        };
        TailnetListener::start_with(wiring, options, fake.dir(), shutdown)
    }

    fn start_with_shutdown<S: Serve<FakeTailnet>>(
        fake: &Arc<FakeTailnet>,
        serve: &Arc<S>,
        options: ListenerOptions,
        shutdown: watch::Receiver<bool>,
    ) -> TailnetListener {
        launch(fake, serve, options, loopback_policy(), shutdown)
    }

    fn start_policy<S: Serve<FakeTailnet>>(
        fake: &Arc<FakeTailnet>,
        serve: &Arc<S>,
        options: ListenerOptions,
        policy: PeerPolicy,
    ) -> (TailnetListener, watch::Sender<bool>) {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let listener = launch(fake, serve, options, policy, shutdown_rx);
        (listener, shutdown_tx)
    }

    /// Start with [`loopback_policy`], so a loopback client is the owner's
    /// laptop.
    fn start<S: Serve<FakeTailnet>>(
        fake: &Arc<FakeTailnet>,
        serve: &Arc<S>,
        options: ListenerOptions,
    ) -> (TailnetListener, watch::Sender<bool>) {
        start_policy(fake, serve, options, loopback_policy())
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
        let status = wait_for_status(&listener, |s| *s != ListenerStatus::Starting).await;
        assert_eq!(status, ListenerStatus::CertFailed);
        assert_eq!(listener.name(), None);
        assert!(
            fake.cert_calls().is_empty(),
            "no name, no certificate to ask for"
        );
        assert_eq!(serve.started.load(Ordering::SeqCst), 0, "nothing bound");
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

    /// `/probe` answers "probe ok"; `/peer` answers the [`AuthorizedPeer`]
    /// the request carries, as `stable_id|name|user_id|login`.
    fn tls_serve() -> Arc<TlsServe> {
        let peer = |axum::Extension(peer): axum::Extension<AuthorizedPeer>| async move {
            let identity = peer.identity();
            format!(
                "{}|{}|{}|{}",
                identity.stable_id, identity.name, identity.user_id, identity.login
            )
        };
        let router = axum::Router::new()
            .route("/probe", axum::routing::get(|| async { "probe ok" }))
            .route("/peer", axum::routing::get(peer));
        Arc::new(TlsServe { router })
    }

    /// Real-clock options that check the certificate every 50 ms, so a test
    /// sees several checks.
    fn quick_options() -> ListenerOptions {
        ListenerOptions {
            port: 0,
            poll_interval: Duration::from_millis(50),
            cert_interval: Duration::from_millis(50),
            ..ListenerOptions::PRODUCTION
        }
    }

    /// Poll `done` until it holds; the deadline is far past any legitimate
    /// wait on either clock.
    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_for_cert_calls(fake: &FakeTailnet, count: usize) -> Vec<CertCall> {
        wait_until("the cert calls", || fake.cert_calls().len() >= count).await;
        fake.cert_calls()
    }

    /// A finished TLS handshake: the negotiated ALPN, the leaf certificate the
    /// server presented, and the stream.
    struct Handshake {
        alpn: Option<Vec<u8>>,
        leaf: Vec<u8>,
        stream: tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    }

    /// A client config trusting only `ca`, offering ALPN `h2` and `http/1.1`.
    fn client_config(ca: &TestCa) -> Arc<rustls::ClientConfig> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der.clone()).expect("trust the test CA");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("client protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Arc::new(config)
    }

    fn server_name() -> ServerName<'static> {
        ServerName::try_from(host().to_owned()).expect("a valid server name")
    }

    /// Open a plain TCP connection, send a TLS `ClientHello` for [`host`],
    /// and return every byte the server sends until it ends the exchange.
    /// A dropped connection returns nothing; a TLS server would at least
    /// answer with its `ServerHello`.
    async fn hello_reply(addr: SocketAddr, ca: &TestCa) -> Vec<u8> {
        hello_reply_from(V4_LOOPBACK, addr, ca).await
    }

    /// [`hello_reply`] from a client socket bound to `from`.
    async fn hello_reply_from(from: IpAddr, addr: SocketAddr, ca: &TestCa) -> Vec<u8> {
        let mut client =
            rustls::ClientConnection::new(client_config(ca), server_name()).expect("a TLS client");
        let mut hello = Vec::new();
        client
            .write_tls(&mut hello)
            .expect("encode the ClientHello");
        let socket = tokio::net::TcpSocket::new_v4().expect("a client socket");
        socket
            .bind(SocketAddr::new(from, 0))
            .expect("bind the client socket");
        let mut tcp = socket.connect(addr).await.expect("connect to the listener");
        let mut seen = Vec::new();
        if tcp.write_all(&hello).await.is_err() {
            return seen;
        }
        let mut chunk = [0u8; 1024];
        // A dropped connection ends at once; a served one at the latest at
        // the server's handshake timeout, waiting for the client's reply.
        let read_all = async {
            loop {
                match tcp.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => seen.extend_from_slice(&chunk[..read]),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), read_all)
            .await
            .expect("the server ends the exchange");
        seen
    }

    /// Connect over TLS trusting only `ca`, offering ALPN `h2` and `http/1.1`.
    async fn tls_connect(addr: SocketAddr, ca: &TestCa) -> Handshake {
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the listener");
        let stream = TlsConnector::from(client_config(ca))
            .connect(server_name(), tcp)
            .await
            .expect("the TLS handshake completes against the test CA");
        let (_, connection) = stream.get_ref();
        let alpn = connection.alpn_protocol().map(<[u8]>::to_vec);
        let leaf = connection
            .peer_certificates()
            .and_then(|chain| chain.first())
            .expect("the server presented a certificate")
            .to_vec();
        Handshake { alpn, leaf, stream }
    }

    /// Send an HTTP/1.1 `GET` of `path`; the whole response, read until the
    /// server closes the connection.
    async fn http_get(
        stream: &mut tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
        path: &str,
    ) -> String {
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            host()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("send the request");
        let mut response = Vec::new();
        let mut chunk = [0u8; 1024];
        // `Connection: close`: the server ends the stream after the response.
        let read_all = async {
            loop {
                let read = stream.read(&mut chunk).await.expect("read the response");
                if read == 0 {
                    break;
                }
                response.extend_from_slice(&chunk[..read]);
            }
        };
        tokio::time::timeout(Duration::from_secs(60), read_all)
            .await
            .expect("the server ends the response");
        String::from_utf8_lossy(&response).into_owned()
    }

    #[tokio::test]
    async fn serves_tls_with_the_tailscale_cert() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (listener, shutdown) = start(&fake, &tls_serve(), options(0));
        let addr = serving_addrs(&wait_for_status(&listener, is_serving).await)[0];
        let mut handshake = tls_connect(addr, &fake.ca).await;
        assert_eq!(
            handshake.alpn.as_deref(),
            Some(&b"http/1.1"[..]),
            "HTTP/1.1 only, though the client offered h2 first"
        );
        assert_eq!(
            handshake.leaf, fake.leaf.der,
            "the certificate tailscale cert wrote"
        );
        let response = http_get(&mut handshake.stream, "/probe").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        shutdown.send_replace(true);
    }

    #[tokio::test]
    async fn plain_http_gets_no_http_response() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (listener, shutdown) = start(&fake, &tls_serve(), options(0));
        let addr = serving_addrs(&wait_for_status(&listener, is_serving).await)[0];
        let mut tcp = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the listener");
        tcp.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("send plain HTTP");
        let mut seen = Vec::new();
        let mut chunk = [0u8; 64];
        // The server ends the exchange once the handshake fails, at the latest
        // at its 10 s handshake timeout; a reset ends the read as well.
        let read_some = async {
            while seen.len() < 5 {
                match tcp.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => seen.extend_from_slice(&chunk[..read]),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), read_some)
            .await
            .expect("the server ends the exchange");
        assert!(
            !seen.starts_with(b"HTTP/"),
            "plain HTTP got an HTTP response: {:?}",
            String::from_utf8_lossy(&seen)
        );
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn checks_the_cert_at_start_and_every_interval() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let options = ListenerOptions {
            port: 0,
            cert_interval: Duration::from_secs(60),
            cert_min_validity: Duration::from_secs(1234),
            ..ListenerOptions::PRODUCTION
        };
        let (listener, shutdown) = start(&fake, &serve, options);
        wait_for_status(&listener, is_serving).await;
        let calls = wait_for_cert_calls(&fake, 2).await;
        for call in &calls[..2] {
            assert_eq!(call.name, host(), "the name without its trailing dot");
            assert_eq!(call.cert_file, fake.dir().join(format!("{}.crt", host())));
            assert_eq!(call.key_file, fake.dir().join(format!("{}.key", host())));
            assert_eq!(call.min_validity, options.cert_min_validity);
        }
        let gap = calls[1].at - calls[0].at;
        assert_eq!(
            gap, options.cert_interval,
            "checked again cert_interval after the check that loaded"
        );
        assert_eq!(
            ListenerOptions::PRODUCTION.cert_interval,
            Duration::from_hours(24)
        );
        assert_eq!(
            ListenerOptions::PRODUCTION.cert_min_validity,
            Duration::from_hours(14 * 24)
        );
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn https_disabled_keeps_the_listener_down() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        fake.set_cert(CertScript::Fail(TailnetError::HttpsDisabled));
        let serve = Arc::new(PlainServe::default());
        let options = options(0);
        let (listener, shutdown) = start(&fake, &serve, options);
        let seen = record_statuses(&listener);
        let status = wait_for_status(&listener, |s| *s != ListenerStatus::Starting).await;
        assert_eq!(status, ListenerStatus::HttpsDisabled);
        let calls = wait_for_cert_calls(&fake, 2).await;
        assert_eq!(
            calls[1].at - calls[0].at,
            options.poll_interval,
            "retried after the first backoff step, poll_interval"
        );
        assert_eq!(*listener.status().borrow(), ListenerStatus::HttpsDisabled);
        assert_eq!(
            serve.started.load(Ordering::SeqCst),
            0,
            "no server, no self-signed fallback"
        );
        assert!(
            !seen.lock().expect("status log lock").iter().any(is_serving),
            "never Serving: nothing was bound"
        );
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn other_cert_failure_is_cert_failed() {
        let errors = [
            TailnetError::Timeout,
            TailnetError::NotRunning,
            TailnetError::Failed("invalid domain \"x\"; must be one of [\"y\"]".to_owned()),
        ];
        for err in errors {
            let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
            fake.set_cert(CertScript::Fail(err.clone()));
            let serve = Arc::new(PlainServe::default());
            let (listener, shutdown) = start(&fake, &serve, options(0));
            let status = wait_for_status(&listener, |s| *s != ListenerStatus::Starting).await;
            assert_eq!(status, ListenerStatus::CertFailed, "{err:?}");
            assert_eq!(serve.started.load(Ordering::SeqCst), 0, "{err:?}");
            shutdown.send_replace(true);
            listener.stopped().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_recheck_keeps_serving() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let options = ListenerOptions {
            port: 0,
            cert_interval: Duration::from_secs(60),
            ..ListenerOptions::PRODUCTION
        };
        let (listener, shutdown) = start(&fake, &serve, options);
        let serving = wait_for_status(&listener, is_serving).await;
        let seen = record_statuses(&listener);
        fake.set_cert(CertScript::Fail(TailnetError::Timeout));
        let calls = wait_for_cert_calls(&fake, 3).await;
        assert_eq!(
            calls[2].at - calls[1].at,
            options.poll_interval,
            "a failed recheck is retried after the first backoff step, poll_interval"
        );
        assert!(
            seen.lock().expect("status log lock").is_empty(),
            "the status stayed Serving"
        );
        assert_eq!(*listener.status().borrow(), serving);
        assert_eq!(
            serve.finished.load(Ordering::SeqCst),
            0,
            "the server ran on"
        );
        std::net::TcpStream::connect(serving_addrs(&serving)[0])
            .expect("the server still accepts connections");
        shutdown.send_replace(true);
    }

    #[tokio::test]
    async fn reloads_when_the_pem_changes() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (listener, shutdown) = start(&fake, &tls_serve(), quick_options());
        let addr = serving_addrs(&wait_for_status(&listener, is_serving).await)[0];
        assert_eq!(tls_connect(addr, &fake.ca).await.leaf, fake.leaf.der);

        let second = fake.ca.leaf();
        fake.set_cert(CertScript::leaf(&second));
        wait_until("the reload", || listener.reloads() >= 1).await;
        let mut after = tls_connect(addr, &fake.ca).await;
        assert_eq!(
            after.leaf, second.der,
            "a new connection sees the new certificate"
        );
        assert_eq!(
            serving_addrs(&listener.status().borrow()),
            [addr],
            "the same server, not restarted"
        );
        let response = http_get(&mut after.stream, "/probe").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        shutdown.send_replace(true);
    }

    #[tokio::test]
    async fn unchanged_pem_is_not_reloaded() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (listener, shutdown) = start(&fake, &tls_serve(), quick_options());
        let addr = serving_addrs(&wait_for_status(&listener, is_serving).await)[0];
        let before = fake.cert_calls().len();
        wait_for_cert_calls(&fake, before + 3).await;
        assert_eq!(listener.reloads(), 0, "the same bytes reload nothing");
        assert_eq!(tls_connect(addr, &fake.ca).await.leaf, fake.leaf.der);
        shutdown.send_replace(true);
    }

    #[tokio::test]
    async fn corrupt_pem_keeps_the_old_certificate() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (listener, shutdown) = start(&fake, &tls_serve(), quick_options());
        let addr = serving_addrs(&wait_for_status(&listener, is_serving).await)[0];
        let second = fake.ca.leaf();
        let scripts = [
            CertScript::Write {
                cert: b"not a usable pem".to_vec(),
                key: b"not a usable pem".to_vec(),
            },
            // A new certificate beside the old key: the pair does not match.
            CertScript::Write {
                cert: second.cert_pem.clone().into_bytes(),
                key: fake.leaf.key_pem.clone().into_bytes(),
            },
        ];
        for script in scripts {
            fake.set_cert(script);
            let before = fake.cert_calls().len();
            wait_for_cert_calls(&fake, before + 2).await;
            assert_eq!(listener.reloads(), 0, "nothing usable to reload");
            assert!(is_serving(&listener.status().borrow()));
            assert_eq!(
                tls_connect(addr, &fake.ca).await.leaf,
                fake.leaf.der,
                "the old certificate stays in service"
            );
        }
        shutdown.send_replace(true);
    }

    #[tokio::test]
    async fn dropping_the_listener_stops_its_servers() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        let addr = serving_addrs(&wait_for_status(&listener, is_serving).await)[0];
        drop(listener);
        wait_until("the address to bind again", || {
            std::net::TcpListener::bind(addr).is_ok()
        })
        .await;
        assert!(!*shutdown.borrow(), "stopped by the drop, not by shutdown");
    }

    #[tokio::test(start_paused = true)]
    async fn rebinds_while_a_cert_check_is_pending() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let options = ListenerOptions {
            port: 0,
            cert_interval: Duration::from_secs(60),
            ..ListenerOptions::PRODUCTION
        };
        let (listener, shutdown) = start(&fake, &serve, options);
        wait_for_status(&listener, is_serving).await;
        fake.cert_held.send_replace(true);
        wait_for_cert_calls(&fake, 2).await;

        fake.set_addresses(&["127.0.0.2/32"]);
        wait_for_status(&listener, |s| {
            serving_ips(s) == [IpAddr::from([127, 0, 0, 2])]
        })
        .await;
        assert_eq!(fake.cert_calls().len(), 2, "no further check began");
        assert_eq!(
            fake.cert_finished(),
            1,
            "rebound while the recheck was still pending: only the first check had finished"
        );
        fake.cert_held.send_replace(false);
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_recheck_backs_off() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let options = ListenerOptions {
            port: 0,
            cert_interval: Duration::from_secs(60),
            ..ListenerOptions::PRODUCTION
        };
        let (listener, shutdown) = start(&fake, &serve, options);
        wait_for_status(&listener, is_serving).await;

        // Checks at 0 s and 60 s (failing from then on), then after 30 s,
        // 60 s, 120 s … up to an hour.
        fake.set_cert(CertScript::Fail(TailnetError::Timeout));
        tokio::time::sleep(Duration::from_secs(14_000)).await;
        assert_eq!(
            cert_gaps(&fake.cert_calls()),
            [60, 30, 60, 120, 240, 480, 960, 1920, 3600, 3600]
        );
        assert!(is_serving(&listener.status().borrow()));

        // A successful check at 14 670 s resets the backoff.
        fake.set_cert(CertScript::leaf(&fake.leaf));
        tokio::time::sleep(Duration::from_secs(680)).await;
        fake.set_cert(CertScript::Fail(TailnetError::Timeout));
        tokio::time::sleep(Duration::from_secs(100)).await;
        let calls = fake.cert_calls();
        assert_eq!(
            cert_gaps(&calls[calls.len() - 4..]),
            [3600, 60, 30],
            "after a success, a failure is retried after poll_interval again"
        );
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unusable_pair_after_a_successful_cert_is_retried() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let options = ListenerOptions {
            port: 0,
            cert_interval: Duration::from_secs(3600),
            ..ListenerOptions::PRODUCTION
        };
        let (listener, shutdown) = start(&fake, &serve, options);
        wait_for_status(&listener, is_serving).await;
        fake.set_cert(CertScript::Write {
            cert: b"not a usable pem".to_vec(),
            key: b"not a usable pem".to_vec(),
        });
        tokio::time::sleep(Duration::from_secs(3700)).await;
        let calls = fake.cert_calls();
        assert!(calls.len() >= 3, "{} cert calls", calls.len());
        assert_eq!(
            calls[2].at - calls[1].at,
            options.poll_interval,
            "tailscale cert succeeded but left an unusable pair: a failed check, retried after the backoff"
        );
        assert!(is_serving(&listener.status().borrow()));
        assert_eq!(listener.reloads(), 0);
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rename_keeps_the_old_name_until_its_cert_loads() {
        const RENAMED: &str = "laptop.tail1234.ts.net";
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        let first = wait_for_status(&listener, is_serving).await;
        assert!(
            matches!(&first, ListenerStatus::Serving { name, .. } if name == host()),
            "{first:?}"
        );

        fake.set_cert(CertScript::Fail(TailnetError::Timeout));
        fake.set_whoami(Ok(whoami_named(&format!("{RENAMED}."), &["127.0.0.1/32"])));
        let seen = record_statuses(&listener);
        tokio::time::sleep(Duration::from_secs(150)).await;
        let renamed_checks = fake
            .cert_calls()
            .iter()
            .filter(|call| call.name == RENAMED)
            .count();
        assert!(
            renamed_checks >= 2,
            "the new name's check is retried: {renamed_checks}"
        );
        assert_eq!(listener.name().as_deref(), Some(RENAMED), "whoami's name");
        assert!(
            seen.lock().expect("status log lock").is_empty(),
            "the status kept the name of the certificate in service"
        );

        fake.set_cert(CertScript::leaf(&fake.leaf));
        let renamed = wait_for_status(
            &listener,
            |s| matches!(s, ListenerStatus::Serving { name, .. } if name == RENAMED),
        )
        .await;
        assert_eq!(
            serving_addrs(&renamed),
            serving_addrs(&first),
            "the same server"
        );
        shutdown.send_replace(true);
    }

    /// The seconds between consecutive `cert` calls.
    fn cert_gaps(calls: &[CertCall]) -> Vec<u64> {
        calls
            .windows(2)
            .map(|pair| (pair[1].at - pair[0].at).as_secs())
            .collect()
    }

    /// Everything the listener logs while installed, as text, from the
    /// test's thread (the current-thread runtime runs every task there).
    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl LogCapture {
        fn install(&self) -> tracing::subscriber::DefaultGuard {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .finish();
            tracing::subscriber::set_default(subscriber)
        }

        /// The lines logged at `level` that contain `text`.
        fn count(&self, level: &str, text: &str) -> usize {
            let bytes = self.0.lock().expect("log buffer lock").clone();
            String::from_utf8_lossy(&bytes)
                .lines()
                .filter(|line| line.contains(level) && line.contains(text))
                .count()
        }
    }

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer lock")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for LogCapture {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_first_check_backs_off_to_five_minutes() {
        const RENAMED: &str = "laptop.tail1234.ts.net";
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        fake.set_cert(CertScript::Fail(TailnetError::Timeout));
        let serve = Arc::new(PlainServe::default());
        let options = options(0);
        let (_listener, shutdown) = start(&fake, &serve, options);

        // Checks at 0, 30, 90, 210, 450, 750 and 1050 s: with nothing in
        // service the backoff stops at 5 minutes, not an hour.
        tokio::time::sleep(Duration::from_secs(1100)).await;
        assert_eq!(cert_gaps(&fake.cert_calls()), [30, 60, 120, 240, 300, 300]);

        fake.set_whoami(Ok(whoami_named(&format!("{RENAMED}."), &["127.0.0.1/32"])));
        let renamed = || -> Vec<CertCall> {
            fake.cert_calls()
                .into_iter()
                .filter(|call| call.name == RENAMED)
                .collect()
        };
        wait_until("two checks for the new name", || renamed().len() >= 2).await;
        let renamed = renamed();
        assert_eq!(
            renamed[1].at - renamed[0].at,
            options.poll_interval,
            "a name change resets the backoff"
        );
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn https_disabled_while_serving_stops_the_servers() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let options = ListenerOptions {
            port: 0,
            cert_interval: Duration::from_secs(60),
            ..ListenerOptions::PRODUCTION
        };
        let (listener, shutdown) = start(&fake, &serve, options);
        let addr = serving_addrs(&wait_for_status(&listener, is_serving).await)[0];

        fake.set_cert(CertScript::Fail(TailnetError::HttpsDisabled));
        let down = wait_for_status(&listener, |s| !is_serving(s)).await;
        assert_eq!(down, ListenerStatus::HttpsDisabled);
        assert_eq!(serve.finished.load(Ordering::SeqCst), 1, "server stopped");
        std::net::TcpListener::bind(addr).expect("nothing bound: the address was released");

        fake.set_cert(CertScript::leaf(&fake.leaf));
        let again = wait_for_status(&listener, is_serving).await;
        assert_eq!(serving_ips(&again), [V4_LOOPBACK]);
        assert_eq!(serve.started.load(Ordering::SeqCst), 2, "served again");
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn status_is_starting_while_the_first_cert_check_runs() {
        let fake = Arc::new(FakeTailnet::with_whoami(Err(TailnetError::NotRunning)));
        fake.cert_held.send_replace(true);
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        wait_for_status(&listener, |s| matches!(s, ListenerStatus::TailscaleDown(_))).await;

        fake.set_addresses(&["127.0.0.1/32"]);
        wait_for_status(&listener, |s| *s == ListenerStatus::Starting).await;
        wait_for_cert_calls(&fake, 1).await;
        assert_eq!(fake.cert_finished(), 0, "the first check is still running");
        assert_eq!(*listener.status().borrow(), ListenerStatus::Starting);

        fake.cert_held.send_replace(false);
        wait_for_status(&listener, is_serving).await;
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn the_cert_task_ending_stops_the_servers() {
        let logs = LogCapture::default();
        let _logs = logs.install();
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let options = options(0);
        let (listener, shutdown) = start(&fake, &serve, options);
        let addr = serving_addrs(&wait_for_status(&listener, is_serving).await)[0];

        listener.abort_cert_task();
        let down = wait_for_status(&listener, |s| !is_serving(s)).await;
        assert_eq!(down, ListenerStatus::CertFailed);
        assert_eq!(serve.finished.load(Ordering::SeqCst), 1, "server stopped");
        std::net::TcpListener::bind(addr).expect("nothing bound: the address was released");

        let before = fake.whoami_calls();
        tokio::time::sleep(Duration::from_secs(95)).await;
        assert_eq!(
            fake.whoami_calls(),
            before + 3,
            "whoami is still polled, every poll interval"
        );
        assert_eq!(*listener.status().borrow(), ListenerStatus::CertFailed);
        assert_eq!(
            serve.started.load(Ordering::SeqCst),
            1,
            "nothing bound again"
        );
        assert_eq!(
            logs.count("ERROR", "the certificate task ended"),
            1,
            "logged once, at error"
        );
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rename_warns_on_its_first_failure() {
        const RENAMED: &str = "laptop.tail1234.ts.net";
        let logs = LogCapture::default();
        let _logs = logs.install();
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let options = ListenerOptions {
            port: 0,
            cert_interval: Duration::from_secs(60),
            ..ListenerOptions::PRODUCTION
        };
        let (listener, shutdown) = start(&fake, &serve, options);
        wait_for_status(&listener, is_serving).await;

        // Rechecks at 60 s (warn) and 90 s (debug) fail the same way.
        fake.set_cert(CertScript::Fail(TailnetError::Timeout));
        wait_for_cert_calls(&fake, 3).await;
        assert_eq!(logs.count("WARN", "certificate check failed"), 1);
        assert_eq!(logs.count("DEBUG", "certificate check still failing"), 1);

        fake.set_whoami(Ok(whoami_named(&format!("{RENAMED}."), &["127.0.0.1/32"])));
        wait_until("the new name's check", || {
            fake.cert_calls().iter().any(|call| call.name == RENAMED)
        })
        .await;
        assert_eq!(
            logs.count("WARN", "no certificate for the new name yet"),
            1,
            "the new name's first failure warns, though its text matches the old name's"
        );
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_waits_for_the_cert_task() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        fake.cert_held.send_replace(true);
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        wait_for_cert_calls(&fake, 1).await;

        shutdown.send_replace(true);
        let cert_done_at_exit = Arc::clone(&listener.cert_done_at_exit);
        listener.stopped().await;
        assert!(
            cert_done_at_exit.load(Ordering::SeqCst),
            "the cert task had ended before the supervisor returned"
        );
        assert_eq!(
            fake.cert_dropped(),
            1,
            "the held tailscale cert call was dropped before stopped returned"
        );
    }

    /// A supervisor that panics still has its cert task aborted, by the
    /// `AbortOnDrop` tokio drops once it has caught the panic, but nothing
    /// waits for that abort, so `stopped` has to: it returns with the held
    /// `tailscale cert` call accounted for.
    ///
    /// This passes on the pre-fix code as well, under this test's
    /// current-thread runtime: there the supervisor task's drop, and so the
    /// cert task's abort and its marker, lands before the pre-fix waiter
    /// wakes. It is a regression guard — a cert task nothing aborts fails it
    /// — not red evidence for the wait.
    #[tokio::test(start_paused = true)]
    async fn stopped_waits_for_the_cert_task_when_the_supervisor_panics() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        fake.cert_held.send_replace(true);
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        wait_for_cert_calls(&fake, 1).await;

        listener.panic_supervisor.store(true, Ordering::SeqCst);
        listener.stopped().await;
        assert_eq!(
            fake.cert_dropped(),
            1,
            "the held tailscale cert call was dropped before stopped returned"
        );
        drop(shutdown);
    }

    /// A `stopped` dropped before it returns leaves the supervisor and the
    /// cert task running unless it hands them to `Drop`.
    #[tokio::test(start_paused = true)]
    async fn dropping_stopped_midway_aborts_the_supervisor_and_the_cert_task() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        fake.cert_held.send_replace(true);
        let serve = Arc::new(PlainServe::default());
        let options = options(0);
        let (listener, shutdown) = start(&fake, &serve, options);
        wait_for_cert_calls(&fake, 1).await;

        let outcome = tokio::time::timeout(Duration::from_secs(1), listener.stopped()).await;
        assert!(
            outcome.is_err(),
            "the supervisor never ends on its own, so the timeout wins"
        );

        wait_until("the cert call to drop", || fake.cert_dropped() == 1).await;
        let asked = fake.whoami_calls();
        // Three poll intervals: a supervisor that was not aborted would have
        // asked Tailscale again by then.
        tokio::time::sleep(options.poll_interval * 3).await;
        assert_eq!(
            fake.whoami_calls(),
            asked,
            "the supervisor was aborted, so Tailscale is not asked again"
        );
        drop(shutdown);
    }

    /// Set the fake's `cert` script and wait until a call made with it has
    /// finished.
    async fn finish_a_check_with(fake: &FakeTailnet, script: CertScript) {
        fake.set_cert(script);
        let before = fake.cert_calls().len();
        wait_until("a check with the new script", || {
            fake.cert_finished() > before
        })
        .await;
    }

    #[tokio::test]
    async fn a_missed_down_state_still_gets_later_reloads() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (listener, shutdown) = start(&fake, &tls_serve(), quick_options());
        let addr = serving_addrs(&wait_for_status(&listener, is_serving).await)[0];

        // Hold the supervisor inside whoami, so it sees neither the Down
        // state nor the Ready that follows it.
        fake.whoami_held.send_replace(true);
        let polls = fake.whoami_calls();
        wait_until("the supervisor to wait on whoami", || {
            fake.whoami_calls() > polls
        })
        .await;
        finish_a_check_with(&fake, CertScript::Fail(TailnetError::HttpsDisabled)).await;
        finish_a_check_with(&fake, CertScript::leaf(&fake.leaf)).await;
        fake.whoami_held.send_replace(false);
        let polls = fake.whoami_calls();
        wait_until("the supervisor to poll again", || {
            fake.whoami_calls() > polls + 1
        })
        .await;
        assert_eq!(
            serving_addrs(&listener.status().borrow()),
            [addr],
            "the server survived the missed Down state"
        );

        let reloads = listener.reloads();
        let second = fake.ca.leaf();
        fake.set_cert(CertScript::leaf(&second));
        wait_until("the reload", || listener.reloads() > reloads).await;
        assert!(
            tls_connect(addr, &fake.ca).await.leaf == second.der,
            "the running server gets the reload"
        );
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn a_down_state_for_the_old_name_reads_starting() {
        const RENAMED: &str = "laptop.tail1234.ts.net";
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        fake.set_cert(CertScript::Fail(TailnetError::HttpsDisabled));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        wait_for_status(&listener, |s| *s == ListenerStatus::HttpsDisabled).await;

        // The old name's retry is held, so the worker has not seen the rename.
        fake.cert_held.send_replace(true);
        let before = fake.cert_calls().len();
        wait_for_cert_calls(&fake, before + 1).await;
        fake.set_whoami(Ok(whoami_named(&format!("{RENAMED}."), &["127.0.0.1/32"])));
        let status = wait_for_status(&listener, |s| *s != ListenerStatus::HttpsDisabled).await;
        assert_eq!(
            status,
            ListenerStatus::Starting,
            "the old name's HttpsDisabled says nothing about the new name"
        );
        assert!(
            fake.cert_calls().iter().all(|call| call.name != RENAMED),
            "the new name's check has not begun"
        );
        fake.cert_held.send_replace(false);
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicked_cert_task_is_logged_once() {
        let logs = LogCapture::default();
        let _logs = logs.install();
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        fake.set_cert(CertScript::Panic);
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        wait_for_status(&listener, |s| *s == ListenerStatus::CertFailed).await;
        shutdown.send_replace(true);
        listener.stopped().await;
        assert_eq!(
            logs.count("", "the certificate task"),
            1,
            "the panic is logged when it ends the task, not again at shutdown"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_aborts_the_cert_task_before_draining() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe {
            events: Some(Arc::clone(&fake.events)),
            ..PlainServe::default()
        });
        let options = ListenerOptions {
            port: 0,
            cert_interval: Duration::from_secs(60),
            ..ListenerOptions::PRODUCTION
        };
        let (listener, shutdown) = start(&fake, &serve, options);
        wait_for_status(&listener, is_serving).await;
        fake.cert_held.send_replace(true);
        wait_for_cert_calls(&fake, 2).await;

        shutdown.send_replace(true);
        listener.stopped().await;
        assert_eq!(
            *fake.events.lock().expect("events lock"),
            ["cert call dropped", "server stopped"],
            "the cert call is dropped before the servers drain"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_renamed_name_without_a_cert_backs_off_to_five_minutes() {
        const RENAMED: &str = "laptop.tail1234.ts.net";
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        wait_for_status(&listener, is_serving).await;

        // The new name is seen on the poll at 30 s; its checks fail at 30,
        // 60, 120, 240, 480, 780 and 1080 s.
        fake.set_cert(CertScript::Fail(TailnetError::Timeout));
        fake.set_whoami(Ok(whoami_named(&format!("{RENAMED}."), &["127.0.0.1/32"])));
        tokio::time::sleep(Duration::from_secs(1100)).await;
        let renamed: Vec<CertCall> = fake
            .cert_calls()
            .into_iter()
            .filter(|call| call.name == RENAMED)
            .collect();
        assert_eq!(
            cert_gaps(&renamed),
            [30, 60, 120, 240, 300, 300],
            "the new name has no certificate in service: the 5-minute cap"
        );
        assert!(
            is_serving(&listener.status().borrow()),
            "the old name's certificate stays in service"
        );
        shutdown.send_replace(true);
    }

    #[tokio::test(start_paused = true)]
    async fn a_repeated_failure_does_not_wake_the_supervisor() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        fake.set_cert(CertScript::Fail(TailnetError::Timeout));
        let serve = Arc::new(PlainServe::default());
        let (listener, shutdown) = start(&fake, &serve, options(0));
        wait_for_status(&listener, |s| *s == ListenerStatus::CertFailed).await;

        let mut state = listener.cert_state.clone();
        state.borrow_and_update();
        let before = fake.cert_calls().len();
        wait_for_cert_calls(&fake, before + 2).await;
        assert!(
            !state.has_changed().expect("the cert task runs"),
            "the same failure again publishes no new state"
        );
        shutdown.send_replace(true);
    }

    /// Start a [`tls_serve`] listener with `policy`; its serving address.
    async fn serving_tls(
        fake: &Arc<FakeTailnet>,
        policy: PeerPolicy,
    ) -> (TailnetListener, watch::Sender<bool>, SocketAddr) {
        let (listener, shutdown) = start_policy(fake, &tls_serve(), options(0), policy);
        let addr = serving_addrs(&wait_for_status(&listener, is_serving).await)[0];
        (listener, shutdown, addr)
    }

    #[tokio::test]
    async fn an_out_of_range_peer_is_dropped_before_whois() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (_listener, shutdown, addr) = serving_tls(&fake, PeerPolicy::PRODUCTION).await;
        assert_eq!(
            hello_reply(addr, &fake.ca).await.len(),
            0,
            "127.0.0.1 is outside Tailscale's ranges: dropped before the TLS handshake"
        );
        assert_eq!(fake.whois_addrs(), [], "whois is never asked");
        shutdown.send_replace(true);
    }

    #[tokio::test]
    async fn every_whois_error_drops_the_connection() {
        let errors = [
            TailnetError::NotInstalled,
            TailnetError::NotRunning,
            TailnetError::AccessDenied,
            TailnetError::NotConnected,
            TailnetError::HttpsDisabled,
            TailnetError::Timeout,
            TailnetError::BadOutput("whois --json: expected value".to_owned()),
            TailnetError::Failed("exited with exit code: 1".to_owned()),
        ];
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (_listener, shutdown, addr) = serving_tls(&fake, loopback_policy()).await;
        // Each error from its own client IP, so no failure backoff carries over.
        for (n, err) in (10u8..).zip(errors) {
            fake.set_whois(Err(err.clone()));
            let from = IpAddr::from([127, 0, 0, n]);
            for attempt in ["first", "second"] {
                assert_eq!(
                    hello_reply_from(from, addr, &fake.ca).await.len(),
                    0,
                    "{err:?}: the {attempt} connection is dropped before the TLS handshake"
                );
            }
            assert_eq!(
                fake.whois_calls_for(from),
                1,
                "{err:?}: the second connection, inside the failure backoff, asks no whois"
            );
        }
        shutdown.send_replace(true);
    }

    /// An acceptor over `fake` with [`loopback_policy`], the host's `whoami`
    /// answer `host`, and `limits`; and the sender of that answer.
    fn gate(
        fake: &Arc<FakeTailnet>,
        host: Option<WhoIs>,
        limits: WhoisLimits,
    ) -> (TailnetAcceptor<FakeTailnet>, watch::Sender<Option<WhoIs>>) {
        let (host_tx, host_rx) = watch::channel(host);
        let acceptor = TailnetAcceptor::new(Arc::clone(fake), host_rx, loopback_policy(), limits);
        (acceptor, host_tx)
    }

    /// A loopback peer address, `127.0.0.<n>`.
    fn loopback_peer(n: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::from([127, 0, 0, n]), 40_000 + u16::from(n))
    }

    /// The host's `whoami` answer the gate tests start from.
    fn test_host() -> WhoIs {
        whoami_with(&["127.0.0.1/32"])
    }

    /// The verdict `acceptor` holds for `ip` now.
    fn cached(acceptor: &TailnetAcceptor<FakeTailnet>, ip: IpAddr) -> Option<Verdict> {
        acceptor.gate.state().cache.get(ip, Instant::now()).cloned()
    }

    /// Connect to `listener` and pass the accepted socket through
    /// `acceptor`: whether it was admitted.
    async fn admitted(
        acceptor: &TailnetAcceptor<FakeTailnet>,
        listener: &tokio::net::TcpListener,
    ) -> bool {
        let addr = listener.local_addr().expect("the test listener's address");
        let (client, accepted) =
            tokio::join!(tokio::net::TcpStream::connect(addr), listener.accept());
        let _client = client.expect("connect to the test listener");
        let (stream, _) = accepted.expect("accept the test connection");
        acceptor.accept(stream, ()).await.is_ok()
    }

    async fn loopback_listener() -> tokio::net::TcpListener {
        tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind a loopback test listener")
    }

    #[tokio::test]
    async fn no_host_identity_yet_denies() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (acceptor, host) = gate(&fake, None, WhoisLimits::PRODUCTION);
        let listener = loopback_listener().await;
        assert!(
            !admitted(&acceptor, &listener).await,
            "no whoami answer yet: denied"
        );
        assert_eq!(fake.whois_addrs(), [], "no whois while nothing can allow");
        host.send_replace(Some(whoami_with(&["127.0.0.1/32"])));
        assert!(
            admitted(&acceptor, &listener).await,
            "the denial was not cached: admitted once the host answered"
        );
    }

    #[tokio::test]
    async fn whois_is_asked_about_the_accepted_peer_addr() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (_listener, shutdown, addr) = serving_tls(&fake, loopback_policy()).await;
        let client = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to the listener");
        let local = client.local_addr().expect("the client's own address");
        wait_until("the whois call", || !fake.whois_addrs().is_empty()).await;
        assert_eq!(
            fake.whois_addrs(),
            [local],
            "whois asked about the client socket's own address"
        );
        shutdown.send_replace(true);
    }

    #[tokio::test]
    async fn an_allowed_peer_reaches_the_router_with_its_identity() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (_listener, shutdown, addr) = serving_tls(&fake, loopback_policy()).await;
        let mut handshake = tls_connect(addr, &fake.ca).await;
        let response = http_get(&mut handshake.stream, "/peer").await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let who = peer_whois();
        let expected = format!(
            "{}|laptop.tail1234.ts.net|{}|{}",
            who.node.stable_id, who.node.user, who.user_profile.login_name
        );
        assert!(
            response.ends_with(&expected),
            "the handler sees the fake's whois identity: {response}"
        );
        shutdown.send_replace(true);
    }

    #[tokio::test]
    async fn denied_owner_rule_outcomes_drop() {
        let mut tagged = peer_whois();
        tagged.node.tags = vec!["tag:server".to_owned()];
        let mut other_user = peer_whois();
        other_user.node.user = 3;
        // The host's own whoami answer: the host's user and node id.
        let host_node = whoami_with(&["127.0.0.1/32"]);
        for (what, whois, reason) in [
            ("a tagged node", tagged, Denied::Tagged),
            ("another user's node", other_user, Denied::OtherUser),
            ("the host node itself", host_node, Denied::HostNode),
        ] {
            let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
            fake.set_whois(Ok(whois));
            let (acceptor, _host) = gate(&fake, Some(test_host()), WhoisLimits::PRODUCTION);
            assert!(
                acceptor.gate.admit(loopback_peer(1)).await.is_err(),
                "{what}"
            );
            assert_eq!(
                cached(&acceptor, loopback_peer(1).ip()),
                Some(Verdict::Deny(reason)),
                "{what}: refused for its own reason, and cached"
            );
            fake.whois_addrs.lock().expect("whois addrs lock").clear();
            let (listener, shutdown, addr) = serving_tls(&fake, loopback_policy()).await;
            for attempt in ["first", "second"] {
                assert_eq!(
                    hello_reply(addr, &fake.ca).await.len(),
                    0,
                    "{what}: the {attempt} connection is dropped before the TLS handshake"
                );
            }
            assert_eq!(
                fake.whois_addrs().len(),
                1,
                "{what}: the denial is cached, so whois was asked once"
            );
            shutdown.send_replace(true);
            listener.stopped().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_verdict_is_cached_for_thirty_seconds() {
        assert_eq!(VERDICT_TTL, Duration::from_secs(30));
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (acceptor, _host) = gate(&fake, Some(test_host()), WhoisLimits::PRODUCTION);
        let listener = loopback_listener().await;
        assert!(admitted(&acceptor, &listener).await);
        assert_eq!(fake.whois_addrs().len(), 1);
        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(admitted(&acceptor, &listener).await);
        assert_eq!(fake.whois_addrs().len(), 1, "inside the TTL: no whois");
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(admitted(&acceptor, &listener).await);
        assert_eq!(fake.whois_addrs().len(), 2, "past the TTL: whois again");
    }

    /// How many connections wait on `ip`'s `whois` round.
    fn round_waiters(acceptor: &TailnetAcceptor<FakeTailnet>, ip: IpAddr) -> usize {
        acceptor
            .gate
            .state()
            .flights
            .get(&ip)
            .map_or(0, |round| Arc::strong_count(round) - 1)
    }

    #[tokio::test]
    async fn concurrent_connections_from_one_ip_share_one_whois() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        fake.whois_held.send_replace(true);
        // Real time and a whois timeout no busy machine reaches: the round is
        // held until both connections wait on it.
        let limits = WhoisLimits {
            timeout: Duration::from_secs(120),
            permit_wait: Duration::from_secs(120),
        };
        let (acceptor, _host) = gate(&fake, Some(test_host()), limits);
        let listener = loopback_listener().await;
        let both = async {
            tokio::join!(
                admitted(&acceptor, &listener),
                admitted(&acceptor, &listener)
            )
        };
        let release = async {
            wait_until("both connections to wait on one round", || {
                round_waiters(&acceptor, V4_LOOPBACK) == 2
            })
            .await;
            fake.whois_held.send_replace(false);
        };
        let ((first, second), ()) = tokio::join!(both, release);
        assert!(first && second, "both admitted");
        assert_eq!(fake.whois_addrs().len(), 1, "one whois for both");
    }

    #[test]
    fn the_cache_holds_at_most_256_entries() {
        assert_eq!(VERDICT_CAP, 256);
        let base = Instant::now();
        let at = |millis: u64| base + Duration::from_millis(millis);
        let ip = |n: u32| IpAddr::V4(Ipv4Addr::from(0x6440_0000 + n));
        let deny = Verdict::Deny(Denied::Tagged);
        let mut cache = TtlMap::new(VERDICT_TTL, VERDICT_CAP);
        for n in 0..256 {
            cache.insert(ip(n), deny.clone(), at(u64::from(n)));
        }
        assert_eq!(cache.len(), 256);

        cache.insert(ip(256), deny.clone(), at(300));
        assert_eq!(cache.len(), 256, "full: the oldest made room");
        assert_eq!(cache.get(ip(0), at(300)), None, "the oldest was dropped");
        assert!(cache.get(ip(1), at(300)).is_some());
        assert!(cache.get(ip(256), at(300)).is_some());

        // Entries 1 to 100 are past the TTL by now: they go first, the
        // fresher ones stay.
        let later = base + VERDICT_TTL + Duration::from_millis(100);
        cache.insert(ip(257), deny, later);
        assert_eq!(cache.len(), 157, "the expired entries were dropped");
        assert_eq!(cache.get(ip(100), later), None);
        assert!(cache.get(ip(101), later).is_some());
        assert!(cache.get(ip(257), later).is_some());
    }

    /// The host's `whoami` answer once another Tailscale user logged in.
    fn other_user_host() -> WhoIs {
        let mut host = whoami_with(&["127.0.0.1/32"]);
        host.node.user = 3;
        host
    }

    /// Yield to the other tasks until `done` holds, without letting a paused
    /// clock advance.
    async fn yield_until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..10_000 {
            if done() {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert!(done(), "never saw {what}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_change_drops_the_cached_verdicts() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (acceptor, host) = gate(&fake, Some(test_host()), WhoisLimits::PRODUCTION);
        let peer = loopback_peer(1);
        assert!(acceptor.gate.admit(peer).await.is_ok());
        assert!(acceptor.gate.admit(peer).await.is_ok());
        assert_eq!(fake.whois_addrs().len(), 1, "the allow is cached");

        host.send_replace(Some(other_user_host()));
        assert!(
            acceptor.gate.admit(peer).await.is_err(),
            "refused under the new host"
        );
        assert_eq!(fake.whois_addrs().len(), 2, "whois was asked again");
        assert_eq!(
            cached(&acceptor, peer.ip()),
            Some(Verdict::Deny(Denied::OtherUser)),
            "by the owner rule, against the new host"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_cached_allow_is_denied_once_the_host_is_gone() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (acceptor, host) = gate(&fake, Some(test_host()), WhoisLimits::PRODUCTION);
        let peer = loopback_peer(1);
        assert!(acceptor.gate.admit(peer).await.is_ok());
        host.send_replace(None);
        assert!(
            acceptor.gate.admit(peer).await.is_err(),
            "no host answer: the cached allow no longer admits"
        );
        assert_eq!(fake.whois_addrs().len(), 1, "and no whois is asked");
    }

    #[tokio::test(start_paused = true)]
    async fn a_round_that_outlives_its_host_is_discarded() {
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        fake.whois_held.send_replace(true);
        let limits = WhoisLimits {
            timeout: Duration::from_secs(3600),
            permit_wait: Duration::from_secs(3600),
        };
        let (acceptor, host) = gate(&fake, Some(test_host()), limits);
        let peer = loopback_peer(1);
        let gate = Arc::clone(&acceptor.gate);
        let round = tokio::spawn(async move { gate.admit(peer).await.is_ok() });
        yield_until("the whois call", || fake.whois_addrs().len() == 1).await;

        host.send_replace(Some(other_user_host()));
        fake.whois_held.send_replace(false);
        assert!(
            !round.await.expect("the round's task"),
            "allowed against the old host, so denied"
        );
        assert_eq!(cached(&acceptor, peer.ip()), None, "and not cached");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_whois_backs_the_peer_off_for_five_seconds() {
        assert_eq!(FAILURE_BACKOFF, Duration::from_secs(5));
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (failing, other) = (loopback_peer(1), loopback_peer(2));
        fake.set_whois_for(failing.ip(), Err(TailnetError::NotRunning));
        let (acceptor, _host) = gate(&fake, Some(test_host()), WhoisLimits::PRODUCTION);
        assert!(acceptor.gate.admit(failing).await.is_err());
        assert_eq!(fake.whois_calls_for(failing.ip()), 1);

        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(acceptor.gate.admit(failing).await.is_err());
        assert_eq!(
            fake.whois_calls_for(failing.ip()),
            1,
            "inside the backoff: refused without a whois"
        );
        assert!(
            acceptor.gate.admit(other).await.is_ok(),
            "another peer is still admitted"
        );

        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(acceptor.gate.admit(failing).await.is_err());
        assert_eq!(
            fake.whois_calls_for(failing.ip()),
            2,
            "past the backoff: whois again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_warns_once_per_ip_per_window() {
        let logs = LogCapture::default();
        let _logs = logs.install();
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        let (failing, tagged) = (loopback_peer(1), loopback_peer(2));
        fake.set_whois_for(failing.ip(), Err(TailnetError::NotRunning));
        let mut tagged_whois = peer_whois();
        tagged_whois.node.tags = vec!["tag:server".to_owned()];
        fake.set_whois_for(tagged.ip(), Ok(tagged_whois));
        let (acceptor, _host) = gate(&fake, Some(test_host()), WhoisLimits::PRODUCTION);
        for _ in 0..3 {
            assert!(acceptor.gate.admit(failing).await.is_err());
            assert!(acceptor.gate.admit(tagged).await.is_err());
        }
        assert_eq!(logs.count("WARN", "whois failed"), 1);
        assert_eq!(logs.count("DEBUG", "whois failed"), 2);
        assert_eq!(logs.count("WARN", "refused by the owner rule"), 1);
        assert_eq!(logs.count("DEBUG", "refused by the owner rule"), 2);

        tokio::time::advance(FAILURE_BACKOFF).await;
        assert!(acceptor.gate.admit(failing).await.is_err());
        assert_eq!(
            logs.count("WARN", "whois failed"),
            2,
            "a new window warns again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_fifth_round_waits_no_longer_than_the_whois_timeout() {
        assert_eq!(WHOIS_CONCURRENCY, 4);
        let fake = Arc::new(FakeTailnet::with_addresses(&["127.0.0.1/32"]));
        fake.whois_held.send_replace(true);
        // The four rounds' own whois outlasts the fifth's wait for a permit.
        let limits = WhoisLimits {
            timeout: Duration::from_secs(3600),
            ..WhoisLimits::PRODUCTION
        };
        let (acceptor, _host) = gate(&fake, Some(test_host()), limits);
        let rounds: Vec<_> = (1..=4)
            .map(|n| {
                let gate = Arc::clone(&acceptor.gate);
                tokio::spawn(async move { gate.admit(loopback_peer(n)).await.is_ok() })
            })
            .collect();
        yield_until("four whois calls", || fake.whois_addrs().len() == 4).await;

        let began = Instant::now();
        let fifth = loopback_peer(5);
        assert!(
            acceptor.gate.admit(fifth).await.is_err(),
            "no permit: denied"
        );
        let waited = began.elapsed();
        assert!(
            waited >= WhoisLimits::PRODUCTION.permit_wait
                && waited < WhoisLimits::PRODUCTION.permit_wait + Duration::from_secs(1),
            "denied once the whois timeout passed: {waited:?}"
        );
        assert_eq!(fake.whois_calls_for(fifth.ip()), 0, "with no whois call");

        fake.whois_held.send_replace(false);
        for round in rounds {
            assert!(
                round.await.expect("a round's task"),
                "the four rounds are admitted"
            );
        }
    }
}
