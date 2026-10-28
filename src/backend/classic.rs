//! Classic mode: FastCGI to a pool of `php-cgi` processes.
//!
//! Each worker is one `php-cgi -b <socket>` process with
//! `PHP_FCGI_CHILDREN=0`, so it serves exactly one request at a time. RPHP
//! opens one Unix-socket connection per request. `php-cgi` also exits by
//! itself after `PHP_FCGI_MAX_REQUESTS`, matching the pool's `max_requests`.
//! Supervision, queueing and shutdown are in [`super::pool`].

use std::{fmt, io, path::PathBuf, time::Duration};

use bytes::{Bytes, BytesMut};
use hyper::Response;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Child,
    sync::oneshot,
    time::{Instant, sleep, timeout, timeout_at},
};
use tracing::{Instrument, Span, debug, warn};

use super::{
    cgi::{self, Head},
    pool::{self, ExecError, Lease, Outcome, Pool, PoolConfig, WorkerKind},
};
use crate::{
    body::{self, BodySender, ResponseBody},
    fastcgi::{self, ProtocolError, Record},
};

/// Largest CGI header block accepted from PHP before answering 502.
pub const MAX_RESPONSE_HEAD_BYTES: usize = 64 * 1024;

/// How long a freshly spawned php-cgi may take to accept connections.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Response chunks buffered between PHP and a slow client.
const BODY_CHANNEL_CAPACITY: usize = 16;
/// Longest single PHP stderr record logged.
const STDERR_LOG_LIMIT: usize = 4096;

pub type ClassicPool = Pool<Classic>;

/// How to start `php-cgi` workers.
#[derive(Debug, Clone)]
pub struct Classic {
    pub binary: PathBuf,
    /// Working directory for php-cgi.
    pub working_dir: PathBuf,
}

impl WorkerKind for Classic {
    /// Nothing: a fresh connection is opened per request.
    type Conn = ();

    async fn spawn(
        &self,
        id: usize,
        socket: PathBuf,
        config: &PoolConfig,
    ) -> io::Result<(Child, ())> {
        let mut command = pool::php_command(&self.binary, &self.working_dir);
        command
            .arg("-b")
            .arg(&socket)
            .env("PHP_FCGI_CHILDREN", "0")
            // php-cgi exits by itself at the same count the pool recycles at.
            .env("PHP_FCGI_MAX_REQUESTS", config.max_requests.to_string());
        let mut child = command.spawn().map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to start {}: {err}", self.binary.display()),
            )
        })?;
        pool::log_output(&mut child, id, "php-cgi");

        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                return Err(io::Error::other(format!(
                    "php-cgi worker {id} exited during startup with {status}"
                )));
            }
            if UnixStream::connect(&socket).await.is_ok() {
                return Ok((child, ()));
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
    }
}

impl Pool<Classic> {
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
            let outcome = match failure {
                None => {
                    lease.worker().served += 1;
                    Outcome::Reuse
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
                    Outcome::Discard
                }
            };
            lease.finish(outcome);
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

    /// Check out a worker and connect to it. A worker whose socket refuses
    /// the connection is replaced and the next one is tried; nothing has been
    /// sent to PHP at that point, so retrying is safe.
    async fn connect(&self) -> Result<(Lease<Classic>, UnixStream), ExecError> {
        let mut attempts = 0;
        loop {
            let mut lease = self.checkout().await?;
            let worker = lease.worker();
            match UnixStream::connect(&worker.socket).await {
                Ok(stream) => return Ok((lease, stream)),
                Err(err) => {
                    warn!(worker = worker.id, error = %err, "cannot connect to PHP worker; replacing it");
                    lease.finish(Outcome::Discard);
                    attempts += 1;
                    if attempts >= self.size().max(2) {
                        return Err(ExecError::Upstream(format!(
                            "no PHP worker accepted a connection: {err}"
                        )));
                    }
                }
            }
        }
    }
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
