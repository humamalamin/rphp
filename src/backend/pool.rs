//! Worker pool shared by every execution mode: supervision, bounded queue,
//! recycling and shutdown. What a worker *is* (how it is spawned, what state
//! it keeps between requests) comes from a [`WorkerKind`]; how a request is
//! run on a checked-out worker lives in the mode's module.
//!
//! Requests that find no idle worker wait in a queue bounded by
//! `max_queue_size`; beyond that they are refused (503). Workers are recycled
//! after `max_requests` requests or `max_age`, and replaced after any error,
//! timeout or crash.

use std::{
    collections::HashMap,
    ffi::OsString,
    fmt,
    future::Future,
    io,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    sync::{Semaphore, TryAcquireError},
    time::{Instant, sleep, timeout},
};
use tracing::{debug, error, info, warn};

/// How long a worker being recycled may take to exit after SIGTERM.
const RECYCLE_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Variables passed through from RPHP's environment to PHP. Everything else
/// is cleared: php-cgi copies its environment into `$_SERVER`, so an
/// inherited `REQUEST_METHOD` or secret would leak into every request.
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TMPDIR",
    "PHPRC",
    "PHP_INI_SCAN_DIR",
];

/// What differs between execution modes when managing a worker process.
pub trait WorkerKind: Send + Sync + 'static {
    /// State kept with an idle worker between requests (e.g. an open
    /// connection).
    type Conn: Send + 'static;

    /// Start worker `id`, using `socket` as its private socket path, and
    /// return once it can take a request. `config` is the pool's, so limits
    /// the PHP side must know (e.g. `max_requests`) have one source.
    fn spawn(
        &self,
        id: usize,
        socket: PathBuf,
        config: &PoolConfig,
    ) -> impl Future<Output = io::Result<(Child, Self::Conn)>> + Send;
}

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub count: usize,
    pub max_requests: u64,
    /// Requests allowed to wait for a worker before new ones get 503.
    pub max_queue_size: usize,
    /// Recycle a worker once it has been running this long.
    pub max_age: Option<Duration>,
}

/// A point-in-time view of the pool, for metrics and health checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    /// `worker.count`.
    pub configured: usize,
    /// Workers ready to take a request.
    pub idle: usize,
    /// Workers running a request.
    pub busy: usize,
    /// Requests waiting for a worker.
    pub waiting: usize,
    /// Workers replaced after reaching a recycling limit.
    pub recycled: u64,
    /// Workers replaced after an error, timeout or crash.
    pub failed: u64,
    /// Failed attempts to start a replacement worker.
    pub spawn_failures: u64,
    /// The pool is shutting down.
    pub closed: bool,
}

impl PoolStats {
    /// Workers being stopped or (re)started, so neither idle nor busy.
    pub fn starting(&self) -> usize {
        self.configured.saturating_sub(self.idle + self.busy)
    }

    /// False when no worker can take a request now or after the queue: none
    /// is running (all are being respawned) or the pool is shutting down.
    pub fn is_available(&self) -> bool {
        !self.closed && self.idle + self.busy > 0
    }
}

/// Why a request could not be answered by PHP.
#[derive(Debug)]
pub enum ExecError {
    /// The request deadline passed (504).
    Timeout,
    /// The pool is shutting down (503).
    Unavailable,
    /// Every worker is busy and the wait queue is full (503 + Retry-After).
    Overloaded,
    /// PHP failed or produced an invalid response (502).
    Upstream(String),
    /// The application threw before producing a response (500). The detail
    /// is for logs only, never for the client.
    Application(String),
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("PHP did not respond before the request deadline"),
            Self::Unavailable => f.write_str("PHP worker pool is shutting down"),
            Self::Overloaded => f.write_str("all PHP workers are busy and the queue is full"),
            Self::Upstream(msg) => write!(f, "PHP worker failed: {msg}"),
            Self::Application(msg) => write!(f, "PHP application error: {msg}"),
        }
    }
}

impl std::error::Error for ExecError {}

#[derive(Debug)]
pub struct StartError(String);

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot start PHP workers: {}", self.0)
    }
}

impl std::error::Error for StartError {}

/// What to do with a worker after a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The request completed; reuse the worker unless a limit is reached.
    Reuse,
    /// The request completed but the worker should be retired gracefully
    /// (e.g. its memory limit was reached).
    Recycle,
    /// The worker is in an unknown state (error, timeout, crash): kill it.
    Discard,
}

/// Handle to the pool; cheap to clone.
pub struct Pool<K: WorkerKind> {
    inner: Arc<Inner<K>>,
}

impl<K: WorkerKind> Clone for Pool<K> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct Inner<K: WorkerKind> {
    kind: Arc<K>,
    config: PoolConfig,
    /// Held for its `Drop`, which removes the sockets.
    socket_dir: SocketDir,
    idle: Mutex<Vec<Worker<K::Conn>>>,
    /// One permit per idle worker.
    available: Semaphore,
    /// Requests currently waiting for a worker.
    waiting: AtomicUsize,
    /// Workers checked out by a request.
    busy: AtomicUsize,
    recycled: AtomicU64,
    failed: AtomicU64,
    /// Respawn attempts that failed (each is retried with backoff).
    spawn_failures: AtomicU64,
    /// PIDs of every worker process not yet reaped, idle or busy, so
    /// shutdown can signal the ones it does not hold.
    live: Mutex<HashMap<usize, u32>>,
    closed: AtomicBool,
}

pub struct Worker<C> {
    pub id: usize,
    pub socket: PathBuf,
    pub conn: C,
    pub served: u64,
    child: Child,
    started: Instant,
}

/// A worker checked out of the pool. Dropping it without [`Lease::finish`]
/// (a cancelled request, an aborted task) discards the worker, so the pool
/// never shrinks and never reuses a worker in an unknown state.
pub struct Lease<K: WorkerKind> {
    worker: Option<Worker<K::Conn>>,
    inner: Arc<Inner<K>>,
}

impl<K: WorkerKind> Lease<K> {
    fn new(worker: Worker<K::Conn>, inner: &Arc<Inner<K>>) -> Self {
        inner.busy.fetch_add(1, Ordering::SeqCst);
        Self {
            worker: Some(worker),
            inner: Arc::clone(inner),
        }
    }

    pub fn worker(&mut self) -> &mut Worker<K::Conn> {
        self.worker.as_mut().expect("lease already finished")
    }

    fn take(&mut self) -> Option<Worker<K::Conn>> {
        let worker = self.worker.take()?;
        self.inner.busy.fetch_sub(1, Ordering::SeqCst);
        Some(worker)
    }

    pub fn finish(mut self, outcome: Outcome) {
        if let Some(worker) = self.take() {
            Arc::clone(&self.inner).release(worker, outcome);
        }
    }
}

impl<K: WorkerKind> Drop for Lease<K> {
    fn drop(&mut self) {
        if let Some(worker) = self.take() {
            // Without a runtime (process exit), `kill_on_drop` still kills it.
            if tokio::runtime::Handle::try_current().is_ok() {
                Arc::clone(&self.inner).release(worker, Outcome::Discard);
            }
        }
    }
}

/// A place in the bounded wait queue, released on drop.
struct QueueSlot<'a>(&'a AtomicUsize);

impl Drop for QueueSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<K: WorkerKind> Pool<K> {
    /// Spawn `count` workers and wait until all are ready.
    pub async fn start(kind: K, config: PoolConfig) -> Result<Self, StartError> {
        let socket_dir = SocketDir::create().map_err(|err| {
            StartError(format!("cannot create a private socket directory: {err}"))
        })?;
        let kind = Arc::new(kind);

        // Spawn concurrently; startup time is one PHP boot, not `count`.
        let handles: Vec<_> = (0..config.count)
            .map(|id| {
                let socket = socket_dir.socket(id);
                let kind = Arc::clone(&kind);
                let config = config.clone();
                tokio::spawn(async move { new_worker(&*kind, id, socket, &config).await })
            })
            .collect();
        let mut workers = Vec::with_capacity(config.count);
        let mut first_error = None;
        for handle in handles {
            match handle.await {
                Ok(Ok(worker)) => workers.push(worker),
                Ok(Err(err)) => {
                    first_error.get_or_insert_with(|| err.to_string());
                }
                Err(err) => {
                    first_error.get_or_insert_with(|| err.to_string());
                }
            }
        }
        if let Some(err) = first_error {
            // Dropping `workers` kills the ones that did start.
            return Err(StartError(err));
        }

        info!(
            workers = workers.len(),
            max_requests = config.max_requests,
            max_queue_size = config.max_queue_size,
            "PHP workers ready"
        );
        let live = workers
            .iter()
            .filter_map(|w| Some((w.id, w.child.id()?)))
            .collect();
        Ok(Self {
            inner: Arc::new(Inner {
                kind,
                available: Semaphore::new(workers.len()),
                idle: Mutex::new(workers),
                waiting: AtomicUsize::new(0),
                busy: AtomicUsize::new(0),
                recycled: AtomicU64::new(0),
                failed: AtomicU64::new(0),
                spawn_failures: AtomicU64::new(0),
                live: Mutex::new(live),
                config,
                socket_dir,
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// True when no worker is free and the wait queue is full. A cheap,
    /// non-binding check that lets callers refuse work before reading a
    /// request body; [`checkout`](Self::checkout) enforces the limit itself.
    pub fn is_saturated(&self) -> bool {
        self.inner.available.available_permits() == 0
            && self.inner.waiting.load(Ordering::SeqCst) >= self.inner.config.max_queue_size
    }

    pub fn stats(&self) -> PoolStats {
        let inner = &self.inner;
        // Read `busy` after `idle`: a worker moving from idle to busy in
        // between is counted twice rather than not at all.
        let idle = inner.idle.lock().unwrap().len();
        PoolStats {
            configured: inner.config.count,
            idle,
            busy: inner.busy.load(Ordering::SeqCst),
            waiting: inner.waiting.load(Ordering::SeqCst),
            recycled: inner.recycled.load(Ordering::Relaxed),
            failed: inner.failed.load(Ordering::Relaxed),
            spawn_failures: inner.spawn_failures.load(Ordering::Relaxed),
            closed: inner.closed.load(Ordering::SeqCst),
        }
    }

    /// PIDs of the worker processes currently alive.
    pub fn worker_pids(&self) -> Vec<u32> {
        self.inner.live.lock().unwrap().values().copied().collect()
    }

    /// How this pool's workers are started.
    pub fn kind(&self) -> &K {
        &self.inner.kind
    }

    /// `worker.count`.
    pub fn size(&self) -> usize {
        self.inner.config.count
    }

    /// Check out a live worker, waiting in the bounded queue if none is
    /// free. Workers that died or expired while idle are retired here and
    /// the next one is tried.
    pub async fn checkout(&self) -> Result<Lease<K>, ExecError> {
        loop {
            let mut lease = self.acquire().await?;
            let worker = lease.worker();
            if let Ok(Some(status)) = worker.child.try_wait() {
                warn!(worker = worker.id, %status, "PHP worker exited while idle; replacing it");
                lease.finish(Outcome::Discard);
                continue;
            }
            if self.inner.expired(worker) {
                debug!(worker = worker.id, "PHP worker reached max_age while idle");
                lease.finish(Outcome::Recycle);
                continue;
            }
            return Ok(lease);
        }
    }

    /// Stop the pool: refuse new work, send SIGTERM to every worker, give
    /// them up to `grace` to finish their current request and exit, then
    /// SIGKILL whatever is left.
    pub async fn shutdown(&self, grace: Duration) {
        let inner = &self.inner;
        inner.closed.store(true, Ordering::SeqCst);
        inner.available.close();

        let pids = self.worker_pids();
        for &pid in &pids {
            signal(pid, libc::SIGTERM);
        }

        let idle = std::mem::take(&mut *inner.idle.lock().unwrap());
        let stops = idle.into_iter().map(|worker| {
            let inner = Arc::clone(inner);
            tokio::spawn(async move { inner.stop(worker, grace).await })
        });
        for handle in stops.collect::<Vec<_>>() {
            let _ = handle.await;
        }

        // Busy workers are reaped by their request task once it ends.
        let deadline = Instant::now() + grace;
        while !inner.live.lock().unwrap().is_empty() && Instant::now() < deadline {
            sleep(Duration::from_millis(20)).await;
        }
        let remaining = self.worker_pids();
        for &pid in &remaining {
            signal(pid, libc::SIGKILL);
        }
        while !inner.live.lock().unwrap().is_empty()
            && Instant::now() < deadline + Duration::from_secs(1)
        {
            sleep(Duration::from_millis(20)).await;
        }
        info!(
            stopped = pids.len(),
            killed = remaining.len(),
            "PHP workers stopped"
        );
    }

    /// Take an idle worker, waiting in the bounded queue if none is free.
    async fn acquire(&self) -> Result<Lease<K>, ExecError> {
        let inner = &self.inner;
        let permit = match inner.available.try_acquire() {
            Ok(permit) => permit,
            Err(TryAcquireError::Closed) => return Err(ExecError::Unavailable),
            Err(TryAcquireError::NoPermits) => {
                let _slot = inner.enter_queue().ok_or(ExecError::Overloaded)?;
                inner
                    .available
                    .acquire()
                    .await
                    .map_err(|_| ExecError::Unavailable)?
            }
        };
        permit.forget();
        let worker = inner.idle.lock().unwrap().pop();
        match worker {
            Some(worker) => Ok(Lease::new(worker, inner)),
            // Only possible if the pool closed between the permit and here.
            None => Err(ExecError::Unavailable),
        }
    }
}

impl<K: WorkerKind> Inner<K> {
    fn enter_queue(&self) -> Option<QueueSlot<'_>> {
        // compare_exchange loop rather than `fetch_update`, which is
        // deprecated on newer toolchains but its replacement is not in 1.85.
        let mut current = self.waiting.load(Ordering::SeqCst);
        loop {
            if current >= self.config.max_queue_size {
                return None;
            }
            match self.waiting.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Some(QueueSlot(&self.waiting)),
                Err(actual) => current = actual,
            }
        }
    }

    fn expired(&self, worker: &Worker<K::Conn>) -> bool {
        self.config
            .max_age
            .is_some_and(|age| worker.started.elapsed() >= age)
    }

    fn release(self: Arc<Self>, worker: Worker<K::Conn>, outcome: Outcome) {
        if self.closed.load(Ordering::SeqCst) {
            tokio::spawn(async move { self.stop(worker, Duration::ZERO).await });
            return;
        }
        let outcome = match outcome {
            Outcome::Reuse
                if worker.served >= self.config.max_requests || self.expired(&worker) =>
            {
                Outcome::Recycle
            }
            other => other,
        };
        let grace = match outcome {
            Outcome::Reuse => {
                self.idle.lock().unwrap().push(worker);
                self.available.add_permits(1);
                return;
            }
            // Graceful: SIGTERM, then a grace period. php-cgi at
            // PHP_FCGI_MAX_REQUESTS is already exiting by itself.
            Outcome::Recycle => {
                self.recycled.fetch_add(1, Ordering::Relaxed);
                if let Some(pid) = worker.child.id() {
                    signal(pid, libc::SIGTERM);
                }
                RECYCLE_EXIT_TIMEOUT
            }
            Outcome::Discard => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                Duration::ZERO
            }
        };
        tokio::spawn(self.replace(worker, grace));
    }

    async fn replace(self: Arc<Self>, old: Worker<K::Conn>, grace: Duration) {
        let (id, served) = (old.id, old.served);
        self.stop(old, grace).await;
        debug!(worker = id, served, "worker stopped; respawning");

        let mut backoff = Duration::from_millis(100);
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            match new_worker(&*self.kind, id, self.socket_dir.socket(id), &self.config).await {
                Ok(worker) => {
                    if let Some(pid) = worker.child.id() {
                        self.live.lock().unwrap().insert(id, pid);
                    }
                    if self.closed.load(Ordering::SeqCst) {
                        self.stop(worker, Duration::ZERO).await;
                        return;
                    }
                    self.idle.lock().unwrap().push(worker);
                    self.available.add_permits(1);
                    return;
                }
                Err(err) => {
                    self.spawn_failures.fetch_add(1, Ordering::Relaxed);
                    error!(worker = id, error = %err, retry_in_ms = backoff.as_millis() as u64,
                        "cannot respawn PHP worker");
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(RESPAWN_BACKOFF_MAX);
                }
            }
        }
    }

    /// Close the worker's connection (a persistent worker sees EOF and leaves
    /// its loop), wait up to `grace` for the process to exit, then kill it and
    /// reap it.
    async fn stop(&self, worker: Worker<K::Conn>, grace: Duration) {
        let Worker {
            id,
            mut child,
            conn,
            ..
        } = worker;
        drop(conn);
        if grace.is_zero() || timeout(grace, child.wait()).await.is_err() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.live.lock().unwrap().remove(&id);
    }
}

async fn new_worker<K: WorkerKind>(
    kind: &K,
    id: usize,
    socket: PathBuf,
    config: &PoolConfig,
) -> io::Result<Worker<K::Conn>> {
    match std::fs::remove_file(&socket) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    let (child, conn) = kind.spawn(id, socket.clone(), config).await?;
    debug!(worker = id, pid = child.id(), "PHP worker started");
    Ok(Worker {
        id,
        socket,
        conn,
        served: 0,
        child,
        started: Instant::now(),
    })
}

/// Send `sig` to a worker. Only PIDs of unreaped children are passed, so the
/// PID cannot have been reused by another process.
fn signal(pid: u32, sig: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: kill(2) has no memory-safety preconditions.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// A PHP command with the hardening every mode needs: no shell, cleared
/// environment plus [`ENV_ALLOWLIST`], stdin closed, stdout and stderr piped
/// (see [`log_output`]), killed when dropped and, on Linux, when RPHP dies.
pub fn php_command(binary: &Path, working_dir: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .envs(allowed_env())
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    // SAFETY: the closure runs between fork and exec and only calls prctl(2),
    // which is async-signal-safe.
    unsafe {
        // Kill PHP if RPHP dies without cleaning up (e.g. SIGKILL), so no
        // worker is orphaned. It fires when the spawning thread exits; Tokio
        // worker threads live as long as the runtime.
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

/// Forward a worker's stdout and stderr to the log, line by line.
///
/// The pipes are read until EOF whatever they contain. Stopping early (e.g.
/// on a line that is not UTF-8) would close the pipe: PHP ignores SIGPIPE, so
/// everything it logs afterwards would be lost silently.
pub fn log_output(child: &mut Child, id: usize, label: &'static str) {
    fn forward(stream: impl AsyncRead + Unpin + Send + 'static, id: usize, label: &'static str) {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&line);
                        let text = text.trim_end();
                        if !text.is_empty() {
                            warn!(worker = id, "{label}: {text}");
                        }
                    }
                }
            }
        });
    }
    if let Some(stdout) = child.stdout.take() {
        forward(stdout, id, label);
    }
    if let Some(stderr) = child.stderr.take() {
        forward(stderr, id, label);
    }
}

fn allowed_env() -> Vec<(&'static str, OsString)> {
    ENV_ALLOWLIST
        .iter()
        .filter_map(|&name| std::env::var_os(name).map(|value| (name, value)))
        .collect()
}

/// Private, per-process directory holding the worker sockets.
struct SocketDir(PathBuf);

impl SocketDir {
    fn create() -> io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!("rphp-{}-{nanos:x}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Self(dir))
    }

    fn socket(&self, id: usize) -> PathBuf {
        self.0.join(format!("w{id}.sock"))
    }
}

impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
