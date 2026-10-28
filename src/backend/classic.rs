//! Classic mode: a pool of `php-cgi` FastCGI processes.
//!
//! Each worker is one `php-cgi -b <socket>` process with
//! `PHP_FCGI_CHILDREN=0`, so it serves exactly one request at a time. RPHP
//! opens one Unix-socket connection per request. Workers are recycled after
//! `max_requests` requests (php-cgi exits on its own via
//! `PHP_FCGI_MAX_REQUESTS`) and replaced after any error, timeout or crash.
//!
//! Requests that find no idle worker wait in a queue bounded by
//! `max_queue_size`; beyond that they are refused (503).

use std::{
    collections::HashMap,
    ffi::OsString,
    fmt, io,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use hyper::Response;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
    sync::{Semaphore, TryAcquireError, oneshot},
    time::{Instant, sleep, timeout, timeout_at},
};
use tracing::{Instrument, Span, debug, error, info, warn};

use super::cgi::{self, Head};
use crate::{
    body::{self, BodySender, ResponseBody},
    fastcgi::{self, ProtocolError, Record},
};

/// Largest CGI header block accepted from PHP before answering 502.
pub const MAX_RESPONSE_HEAD_BYTES: usize = 64 * 1024;

/// How long a freshly spawned php-cgi may take to accept connections.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a worker that reached `max_requests` may take to exit by itself.
const RECYCLE_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Response chunks buffered between PHP and a slow client.
const BODY_CHANNEL_CAPACITY: usize = 16;
/// Longest single PHP stderr record logged.
const STDERR_LOG_LIMIT: usize = 4096;

/// Variables passed through from RPHP's environment to php-cgi. Everything
/// else is cleared: php-cgi copies its environment into `$_SERVER`, so an
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

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub binary: PathBuf,
    pub count: usize,
    pub max_requests: u64,
    /// Requests allowed to wait for a worker before new ones get 503.
    pub max_queue_size: usize,
    /// Working directory for php-cgi.
    pub working_dir: PathBuf,
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
    /// Workers replaced after reaching `max_requests`.
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
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("PHP did not respond before the request deadline"),
            Self::Unavailable => f.write_str("PHP worker pool is shutting down"),
            Self::Overloaded => f.write_str("all PHP workers are busy and the queue is full"),
            Self::Upstream(msg) => write!(f, "PHP worker failed: {msg}"),
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

/// Handle to the pool; cheap to clone.
#[derive(Clone)]
pub struct ClassicPool {
    inner: Arc<Inner>,
}

struct Inner {
    config: PoolConfig,
    /// Held for its `Drop`, which removes the sockets.
    _socket_dir: SocketDir,
    idle: Mutex<Vec<Worker>>,
    /// One permit per idle worker.
    available: Semaphore,
    /// Requests currently waiting for a worker.
    waiting: AtomicUsize,
    /// Workers checked out by a request.
    busy: AtomicUsize,
    /// Workers replaced after reaching `max_requests`.
    recycled: AtomicU64,
    /// Workers replaced after an error, timeout or crash.
    failed: AtomicU64,
    /// Respawn attempts that failed (each is retried with backoff).
    spawn_failures: AtomicU64,
    /// PIDs of every worker process not yet reaped, idle or busy, so
    /// shutdown can signal the ones it does not hold.
    live: Mutex<HashMap<usize, u32>>,
    closed: AtomicBool,
}

struct Worker {
    id: usize,
    socket: PathBuf,
    child: Child,
    served: u64,
}

/// A worker checked out of the pool. Dropping it without [`Lease::finish`]
/// (a cancelled request, an aborted task) replaces the worker, so the pool
/// never shrinks and never reuses a worker in an unknown state.
struct Lease {
    worker: Option<Worker>,
    inner: Arc<Inner>,
}

impl Lease {
    fn new(worker: Worker, inner: &Arc<Inner>) -> Self {
        inner.busy.fetch_add(1, Ordering::SeqCst);
        Self {
            worker: Some(worker),
            inner: Arc::clone(inner),
        }
    }

    fn worker(&mut self) -> &mut Worker {
        self.worker.as_mut().expect("lease already finished")
    }

    fn take(&mut self) -> Option<Worker> {
        let worker = self.worker.take()?;
        self.inner.busy.fetch_sub(1, Ordering::SeqCst);
        Some(worker)
    }

    fn finish(mut self, reusable: bool) {
        if let Some(worker) = self.take() {
            Arc::clone(&self.inner).release(worker, reusable);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(worker) = self.take() {
            // Without a runtime (process exit), `kill_on_drop` still kills it.
            if tokio::runtime::Handle::try_current().is_ok() {
                Arc::clone(&self.inner).release(worker, false);
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

impl ClassicPool {
    /// Spawn `count` workers and wait until all accept connections.
    pub async fn start(config: PoolConfig) -> Result<Self, StartError> {
        let socket_dir = SocketDir::create().map_err(|err| {
            StartError(format!("cannot create a private socket directory: {err}"))
        })?;

        // Spawn concurrently; startup time is one php-cgi boot, not `count`.
        let handles: Vec<_> = (0..config.count)
            .map(|id| {
                let socket = socket_dir.socket(id);
                let config = config.clone();
                tokio::spawn(async move { spawn_worker(&config, id, socket).await })
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
                available: Semaphore::new(workers.len()),
                idle: Mutex::new(workers),
                waiting: AtomicUsize::new(0),
                busy: AtomicUsize::new(0),
                recycled: AtomicU64::new(0),
                failed: AtomicU64::new(0),
                spawn_failures: AtomicU64::new(0),
                live: Mutex::new(live),
                config,
                _socket_dir: socket_dir,
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// True when no worker is free and the wait queue is full. A cheap,
    /// non-binding check that lets callers refuse work before reading a
    /// request body; [`execute`](Self::execute) enforces the limit itself.
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

    /// Run one request through a worker.
    ///
    /// `params` are encoded FastCGI params, `body` the complete request body.
    /// Everything, including waiting for a free worker, must finish by
    /// `deadline`. Once the response head has been returned, the body streams
    /// from PHP; a failure after that point aborts the stream.
    pub async fn execute(
        &self,
        params: Vec<u8>,
        body: Bytes,
        deadline: Instant,
    ) -> Result<Response<ResponseBody>, ExecError> {
        let (lease, stream) = match timeout_at(deadline, self.connect()).await {
            Ok(result) => result?,
            Err(_) => return Err(ExecError::Timeout),
        };

        let (head_tx, head_rx) = oneshot::channel();
        let (body_tx, response_body) = body::channel(BODY_CHANNEL_CAPACITY);

        // The exchange runs in its own task so a client that disconnects
        // cannot leave a worker in the middle of a request. It keeps the
        // request's span, so PHP stderr is logged with the request ID.
        let task = async move {
            let mut lease = lease;
            let mut head_tx = Some(head_tx);
            let result = timeout_at(
                deadline,
                exchange(stream, params, body, &mut head_tx, &body_tx),
            )
            .await;

            let failure = match result {
                Ok(Ok(())) => None,
                Ok(Err(err)) => Some(ExecError::Upstream(err.to_string())),
                Err(_) => Some(ExecError::Timeout),
            };
            let reusable = match failure {
                None => {
                    lease.worker().served += 1;
                    true
                }
                Some(err) => {
                    warn!(worker = lease.worker().id, error = %err, "PHP request failed; replacing worker");
                    match head_tx.take() {
                        Some(tx) => {
                            let _ = tx.send(Err(err));
                        }
                        None => {
                            let _ =
                                timeout(Duration::from_secs(1), body_tx.send(Err(Box::new(err))))
                                    .await;
                        }
                    }
                    false
                }
            };
            lease.finish(reusable);
        };
        tokio::spawn(task.instrument(Span::current()));

        match head_rx.await {
            Ok(Ok(head)) => {
                let mut res = Response::new(response_body);
                *res.status_mut() = head.status;
                *res.headers_mut() = head.headers;
                Ok(res)
            }
            Ok(Err(err)) => Err(err),
            Err(_) => Err(ExecError::Upstream(
                "worker task ended without a response".into(),
            )),
        }
    }

    /// Stop the pool: refuse new work, send SIGTERM to every worker, give
    /// them up to `grace` to finish (php-cgi exits after its current
    /// request), then SIGKILL whatever is left.
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

    /// Check out a live worker and connect to it. A worker that died while
    /// idle, or whose socket refuses the connection, is replaced and the next
    /// one is tried; nothing has been sent to PHP at that point, so retrying
    /// is safe.
    async fn connect(&self) -> Result<(Lease, UnixStream), ExecError> {
        let mut attempts = 0;
        loop {
            let mut lease = self.acquire().await?;
            let worker = lease.worker();
            if let Ok(Some(status)) = worker.child.try_wait() {
                warn!(worker = worker.id, %status, "PHP worker exited while idle; replacing it");
                lease.finish(false);
                continue;
            }
            match UnixStream::connect(&worker.socket).await {
                Ok(stream) => return Ok((lease, stream)),
                Err(err) => {
                    warn!(worker = worker.id, error = %err, "cannot connect to PHP worker; replacing it");
                    lease.finish(false);
                    attempts += 1;
                    if attempts >= self.inner.config.count.max(2) {
                        return Err(ExecError::Upstream(format!(
                            "no PHP worker accepted a connection: {err}"
                        )));
                    }
                }
            }
        }
    }

    /// Take an idle worker, waiting in the bounded queue if none is free.
    async fn acquire(&self) -> Result<Lease, ExecError> {
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

impl Inner {
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

    fn release(self: Arc<Self>, worker: Worker, reusable: bool) {
        if self.closed.load(Ordering::SeqCst) {
            tokio::spawn(async move { self.stop(worker, Duration::ZERO).await });
            return;
        }
        if reusable && worker.served < self.config.max_requests {
            self.idle.lock().unwrap().push(worker);
            self.available.add_permits(1);
            return;
        }
        // php-cgi exits by itself after PHP_FCGI_MAX_REQUESTS; anything else
        // (error, timeout, crash) is killed straight away.
        let grace = if reusable {
            self.recycled.fetch_add(1, Ordering::Relaxed);
            RECYCLE_EXIT_TIMEOUT
        } else {
            self.failed.fetch_add(1, Ordering::Relaxed);
            Duration::ZERO
        };
        tokio::spawn(self.replace(worker, grace));
    }

    async fn replace(self: Arc<Self>, old: Worker, grace: Duration) {
        let (id, socket, served) = (old.id, old.socket.clone(), old.served);
        self.stop(old, grace).await;
        debug!(worker = id, served, "worker stopped; respawning");

        let mut backoff = Duration::from_millis(100);
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            match spawn_worker(&self.config, id, socket.clone()).await {
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

    /// Wait up to `grace` for the process to exit, then kill it and reap it.
    async fn stop(&self, mut worker: Worker, grace: Duration) {
        if grace.is_zero() || timeout(grace, worker.child.wait()).await.is_err() {
            let _ = worker.child.start_kill();
            let _ = worker.child.wait().await;
        }
        self.live.lock().unwrap().remove(&worker.id);
    }
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

async fn spawn_worker(config: &PoolConfig, id: usize, socket: PathBuf) -> io::Result<Worker> {
    match std::fs::remove_file(&socket) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }

    let mut command = Command::new(&config.binary);
    command
        .arg("-b")
        .arg(&socket)
        .env_clear()
        .envs(allowed_env())
        .env("PHP_FCGI_CHILDREN", "0")
        .env("PHP_FCGI_MAX_REQUESTS", config.max_requests.to_string())
        .current_dir(&config.working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    // SAFETY: the closure runs between fork and exec and only calls prctl(2),
    // which is async-signal-safe.
    unsafe {
        // Kill php-cgi if RPHP dies without cleaning up (e.g. SIGKILL), so no
        // worker is orphaned. It fires when the spawning thread exits; Tokio
        // worker threads live as long as the runtime.
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("failed to start {}: {err}", config.binary.display()),
        )
    })?;

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                warn!(worker = id, "php-cgi: {line}");
            }
        });
    }

    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "php-cgi worker {id} exited during startup with {status}"
            )));
        }
        if UnixStream::connect(&socket).await.is_ok() {
            break;
        }
        if started.elapsed() > READY_TIMEOUT {
            let _ = child.start_kill();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "php-cgi worker {id} did not listen on {} within {}s",
                    socket.display(),
                    READY_TIMEOUT.as_secs()
                ),
            ));
        }
        sleep(Duration::from_millis(20)).await;
    }

    debug!(worker = id, pid = child.id(), "php-cgi worker started");
    Ok(Worker {
        id,
        socket,
        child,
        served: 0,
    })
}

fn allowed_env() -> Vec<(&'static str, OsString)> {
    ENV_ALLOWLIST
        .iter()
        .filter_map(|&name| std::env::var_os(name).map(|value| (name, value)))
        .collect()
}

#[derive(Debug)]
enum ExchangeError {
    Io(io::Error),
    Protocol(ProtocolError),
    BadHead(String),
    HeadTooLarge,
    UnexpectedEof,
    NoHead,
}

impl fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Protocol(err) => err.fmt(f),
            Self::BadHead(msg) => write!(f, "invalid response headers: {msg}"),
            Self::HeadTooLarge => {
                write!(f, "response headers exceed {MAX_RESPONSE_HEAD_BYTES} bytes")
            }
            Self::UnexpectedEof => f.write_str("connection closed before END_REQUEST"),
            Self::NoHead => f.write_str("request ended without response headers"),
        }
    }
}

impl From<io::Error> for ExchangeError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<ProtocolError> for ExchangeError {
    fn from(err: ProtocolError) -> Self {
        Self::Protocol(err)
    }
}

type HeadSender = oneshot::Sender<Result<Head, ExecError>>;

/// One FastCGI request over a freshly connected stream. Writing the request and
/// reading the response run concurrently: PHP may answer (or fail) before it
/// has read the whole body, and a large body must not deadlock against a
/// large response.
async fn exchange(
    stream: UnixStream,
    params: Vec<u8>,
    body: Bytes,
    head_tx: &mut Option<HeadSender>,
    body_tx: &BodySender,
) -> Result<(), ExchangeError> {
    let (mut rd, mut wr) = stream.into_split();

    let writer = async move {
        let mut buf = Vec::with_capacity(params.len() + 64);
        fastcgi::begin_request(&mut buf);
        fastcgi::write_stream(&mut buf, fastcgi::PARAMS, &params);
        fastcgi::end_stream(&mut buf, fastcgi::PARAMS);
        wr.write_all(&buf).await?;
        for chunk in body.chunks(fastcgi::MAX_CONTENT_LEN) {
            buf.clear();
            fastcgi::write_record(&mut buf, fastcgi::STDIN, chunk);
            wr.write_all(&buf).await?;
        }
        buf.clear();
        fastcgi::end_stream(&mut buf, fastcgi::STDIN);
        wr.write_all(&buf).await?;
        io::Result::Ok(())
    };

    let reader = async move {
        let mut buf = BytesMut::with_capacity(16 * 1024);
        let mut head = Some(BytesMut::new());
        let mut client_gone = false;
        loop {
            while let Some(record) = fastcgi::decode_record(&mut buf)? {
                match record {
                    Record::Stdout(data) if data.is_empty() => {}
                    Record::Stdout(data) => {
                        let chunk = match head.as_mut() {
                            Some(pending) => {
                                pending.extend_from_slice(&data);
                                let Some((head_len, body_start)) = cgi::find_head_end(pending)
                                else {
                                    if pending.len() > MAX_RESPONSE_HEAD_BYTES {
                                        return Err(ExchangeError::HeadTooLarge);
                                    }
                                    continue;
                                };
                                if head_len > MAX_RESPONSE_HEAD_BYTES {
                                    return Err(ExchangeError::HeadTooLarge);
                                }
                                let rest = pending.split_off(body_start).freeze();
                                let parsed = cgi::parse_head(&pending[..head_len])
                                    .map_err(ExchangeError::BadHead)?;
                                head = None;
                                let sent =
                                    head_tx.take().is_some_and(|tx| tx.send(Ok(parsed)).is_ok());
                                client_gone = !sent;
                                rest
                            }
                            None => data,
                        };
                        // After a client disconnects, keep draining so the
                        // worker finishes cleanly and can be reused.
                        if !client_gone
                            && !chunk.is_empty()
                            && body_tx.send(Ok(chunk)).await.is_err()
                        {
                            client_gone = true;
                        }
                    }
                    Record::Stderr(data) => log_php_stderr(&data),
                    Record::EndRequest { app_status } => {
                        if head.is_some() {
                            return Err(ExchangeError::NoHead);
                        }
                        debug!(app_status, "PHP request complete");
                        return Ok(());
                    }
                    Record::Other(record_type) => {
                        debug!(record_type, "ignoring FastCGI record");
                    }
                }
            }
            if rd.read_buf(&mut buf).await? == 0 {
                return Err(ExchangeError::UnexpectedEof);
            }
        }
    };

    tokio::pin!(writer, reader);
    let mut writing = true;
    loop {
        tokio::select! {
            result = &mut writer, if writing => {
                writing = false;
                if let Err(err) = result {
                    // PHP may stop reading early (e.g. after a fatal error);
                    // the reader decides whether the request succeeded.
                    debug!(error = %err, "writing request to PHP stopped early");
                }
            }
            result = &mut reader => return result,
        }
    }
}

fn log_php_stderr(data: &[u8]) {
    let data = &data[..data.len().min(STDERR_LOG_LIMIT)];
    for line in String::from_utf8_lossy(data).lines() {
        if !line.trim().is_empty() {
            warn!(target: "rphp::php", "{line}");
        }
    }
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
