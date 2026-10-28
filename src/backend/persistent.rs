//! Persistent mode (experimental): long-running PHP CLI workers that boot the
//! application once and serve many requests over the framed protocol in
//! [`super::protocol`].
//!
//! RPHP listens on a private per-worker Unix socket, starts
//! `php <worker script>` with `RPHP_WORKER_SOCKET` pointing at it, and
//! accepts exactly one connection, from that child only (checked with
//! `SO_PEERCRED`). The connection stays open for the worker's lifetime and
//! carries one request at a time. Supervision, queueing, recycling and
//! shutdown are shared with classic mode ([`super::pool`]).

use std::{fmt, io, path::PathBuf, time::Duration};

use bytes::{Bytes, BytesMut};
use hyper::{
    HeaderMap, Response, StatusCode,
    header::{HeaderName, HeaderValue},
    http::request::Parts,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::Child,
    sync::oneshot,
    time::{Instant, sleep, timeout, timeout_at},
};
use tracing::{Instrument, Span, debug, warn};

use super::{
    cgi::{self, ConnInfo},
    pool::{self, ExecError, Outcome, Pool, PoolConfig, WorkerKind},
    protocol::{self, Frame, ProtocolError, RequestHead},
};
use crate::body::{self, BodySender, ResponseBody};

/// How long a worker may take to boot the application and say hello.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_CHANNEL_CAPACITY: usize = 16;

pub type PersistentPool = Pool<Persistent>;

/// How to start persistent workers.
#[derive(Debug, Clone)]
pub struct Persistent {
    /// A PHP CLI binary.
    pub binary: PathBuf,
    /// The worker script (runs the `rphp/worker` loop).
    pub script: PathBuf,
    pub working_dir: PathBuf,
    /// Recycle a worker whose `memory_get_usage()` reaches this many bytes
    /// after a request.
    pub max_memory: Option<u64>,
}

/// The open connection to a worker.
pub struct Connection {
    stream: UnixStream,
    buf: BytesMut,
    next_id: u32,
}

impl WorkerKind for Persistent {
    type Conn = Connection;

    async fn spawn(
        &self,
        id: usize,
        socket: PathBuf,
        _config: &PoolConfig,
    ) -> io::Result<(Child, Connection)> {
        let listener = UnixListener::bind(&socket)?;
        let mut command = pool::php_command(&self.binary, &self.working_dir);
        command
            .arg(&self.script)
            .env("RPHP_WORKER_SOCKET", &socket)
            .env("RPHP_WORKER_ID", id.to_string())
            .env("RPHP_WORKER_PROTOCOL", protocol::VERSION.to_string());
        let mut child = command.spawn().map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to start {}: {err}", self.binary.display()),
            )
        })?;
        pool::log_output(&mut child, id, "php worker");

        let mut rejected_peer = None;
        let result = timeout(
            READY_TIMEOUT,
            handshake(&listener, &mut child, id, &mut rejected_peer),
        )
        .await;
        // One connection per worker; nothing else may connect afterwards.
        drop(listener);
        let _ = std::fs::remove_file(&socket);
        match result {
            Ok(Ok(conn)) => Ok((child, conn)),
            Ok(Err(err)) => {
                let _ = child.start_kill();
                Err(err)
            }
            Err(_) => {
                let _ = child.start_kill();
                let reason = match rejected_peer {
                    // Someone connected, just not the process RPHP started:
                    // typically a wrapper around php that forks instead of
                    // exec'ing it.
                    Some(peer) => format!(
                        "a process with PID {peer} connected instead of the worker (PID {}); \
                         if {} is a wrapper script, it must `exec` php",
                        child.id().unwrap_or_default(),
                        self.binary.display()
                    ),
                    None => format!("is {} an rphp/worker script?", self.script.display()),
                };
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "PHP worker {id} did not connect within {}s; {reason}",
                        READY_TIMEOUT.as_secs()
                    ),
                ))
            }
        }
    }
}

/// Accept the worker's connection (refusing any other process) and read its
/// `HELLO`.
async fn handshake(
    listener: &UnixListener,
    child: &mut Child,
    id: usize,
    rejected_peer: &mut Option<i32>,
) -> io::Result<Connection> {
    let stream = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let peer = stream.peer_cred()?.pid();
                let expected = child.id().and_then(|pid| i32::try_from(pid).ok());
                if peer.is_some() && peer == expected {
                    break stream;
                }
                warn!(worker = id, ?peer, ?expected, "rejected a connection that is not from the worker");
                *rejected_peer = peer.or(Some(-1));
            }
            () = sleep(Duration::from_millis(50)) => {
                if let Some(status) = child.try_wait()? {
                    return Err(io::Error::other(format!(
                        "PHP worker {id} exited during startup with {status}"
                    )));
                }
            }
        }
    };

    let mut conn = Connection {
        stream,
        buf: BytesMut::with_capacity(16 * 1024),
        next_id: 1,
    };
    let frame = conn.read_frame().await.map_err(io::Error::other)?;
    if frame.frame_type != protocol::HELLO {
        return Err(io::Error::other(format!(
            "PHP worker {id} sent frame 0x{:02x} instead of HELLO",
            frame.frame_type
        )));
    }
    let hello: protocol::Hello = frame.json().map_err(io::Error::other)?;
    if hello.protocol != protocol::VERSION {
        return Err(io::Error::other(format!(
            "PHP worker {id} speaks protocol {}, RPHP speaks {}",
            hello.protocol,
            protocol::VERSION
        )));
    }
    debug!(worker = id, pid = hello.pid, php = %hello.php, "PHP worker connected");
    Ok(conn)
}

#[derive(Debug)]
enum ExchangeError {
    Io(io::Error),
    Protocol(ProtocolError),
    /// The worker broke the protocol's rules (wrong frame order, bad id...).
    Unexpected(String),
    BadHead(String),
    /// The application threw; the worker is still alive.
    Application(String),
}

impl fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Protocol(err) => err.fmt(f),
            Self::Unexpected(msg) => write!(f, "protocol violation: {msg}"),
            Self::BadHead(msg) => write!(f, "invalid response head: {msg}"),
            Self::Application(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ExchangeError {}

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

impl Connection {
    async fn read_frame(&mut self) -> Result<Frame, ExchangeError> {
        loop {
            if let Some(frame) = protocol::decode_frame(&mut self.buf)? {
                return Ok(frame);
            }
            if self.stream.read_buf(&mut self.buf).await? == 0 {
                return Err(ExchangeError::Io(io::ErrorKind::UnexpectedEof.into()));
            }
        }
    }
}

/// Build the JSON request head sent to the worker.
pub fn request_head(
    document_root: &std::path::Path,
    req: &Parts,
    conn: ConnInfo,
    body_len: usize,
    deadline: Instant,
) -> Vec<u8> {
    let headers = req
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let head = RequestHead {
        method: req.method.as_str().to_string(),
        uri: req
            .uri
            .path_and_query()
            .map_or(req.uri.path(), |pq| pq.as_str())
            .to_string(),
        path: req.uri.path().to_string(),
        query: req.uri.query().unwrap_or("").to_string(),
        protocol: cgi::protocol(req.version).to_string(),
        scheme: "http".into(),
        headers,
        remote_addr: conn.remote.ip().to_string(),
        remote_port: conn.remote.port(),
        server_addr: conn.local.ip().to_string(),
        server_port: conn.local.port(),
        server_name: cgi::server_name(&req.headers, conn.local),
        document_root: document_root.to_string_lossy().into_owned(),
        body_length: body_len,
        timeout_ms: deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
    };
    serde_json::to_vec(&head).expect("request head serializes")
}

type HeadSender = oneshot::Sender<Result<(StatusCode, HeaderMap), ExecError>>;

impl Pool<Persistent> {
    /// Run one request on a persistent worker. Same contract as the classic
    /// [`Pool::execute`]: everything must finish by `deadline`, the body
    /// streams once the head is returned, and failures after that abort the
    /// stream.
    pub async fn execute(
        &self,
        head: Vec<u8>,
        body: Bytes,
        deadline: Instant,
    ) -> Result<Response<ResponseBody>, ExecError> {
        let max_memory = self.kind().max_memory;
        let lease = match timeout_at(deadline, self.checkout()).await {
            Ok(result) => result?,
            Err(_) => return Err(ExecError::Timeout),
        };

        let (head_tx, head_rx) = oneshot::channel();
        let (body_tx, response_body) = body::channel(BODY_CHANNEL_CAPACITY);

        let task = async move {
            let mut lease = lease;
            let mut head_tx = Some(head_tx);
            let worker = lease.worker();
            let id = worker.id;
            let result = timeout_at(
                deadline,
                exchange(&mut worker.conn, &head, &body, &mut head_tx, &body_tx),
            )
            .await;

            let (outcome, failure) = match result {
                Ok(Ok(memory)) => {
                    worker.served += 1;
                    if max_memory.is_some_and(|limit| memory >= limit) {
                        debug!(
                            worker = id,
                            memory, "PHP worker reached max_memory; recycling"
                        );
                        (Outcome::Recycle, None)
                    } else {
                        (Outcome::Reuse, None)
                    }
                }
                // The application threw, but the worker caught it, ran its
                // reset hooks and is ready for the next request. Reusing it
                // keeps a client that can trigger exceptions from forcing a
                // full application reboot per request.
                Ok(Err(ExchangeError::Application(msg))) => {
                    worker.served += 1;
                    (Outcome::Reuse, Some(ExecError::Application(msg)))
                }
                Ok(Err(err)) => (Outcome::Discard, Some(ExecError::Upstream(err.to_string()))),
                Err(_) => (Outcome::Discard, Some(ExecError::Timeout)),
            };
            if let Some(err) = failure {
                warn!(worker = id, error = %err, "PHP request failed");
                match head_tx.take() {
                    Some(tx) => {
                        let _ = tx.send(Err(err));
                    }
                    None => {
                        let _ =
                            timeout(Duration::from_secs(1), body_tx.send(Err(Box::new(err)))).await;
                    }
                }
            }
            lease.finish(outcome);
        };
        tokio::spawn(task.instrument(Span::current()));

        match head_rx.await {
            Ok(Ok((status, headers))) => {
                let mut res = Response::new(response_body);
                *res.status_mut() = status;
                *res.headers_mut() = headers;
                Ok(res)
            }
            Ok(Err(err)) => Err(err),
            Err(_) => Err(ExecError::Upstream(
                "worker task ended without a response".into(),
            )),
        }
    }
}

/// Send one request and relay the response. Returns the worker's reported
/// memory usage. The worker reads the whole request before running the
/// application, so writing first and reading second cannot deadlock.
async fn exchange(
    conn: &mut Connection,
    head: &[u8],
    body: &[u8],
    head_tx: &mut Option<HeadSender>,
    body_tx: &BodySender,
) -> Result<u64, ExchangeError> {
    let id = conn.next_id;
    conn.next_id = conn.next_id.wrapping_add(1).max(1);

    // Header and payload are written separately so the (already buffered)
    // body is not copied again.
    let mut send = async |frame_type: u8, payload: &[u8]| -> io::Result<()> {
        let header = protocol::frame_header(frame_type, id, payload.len())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        conn.stream.write_all(&header).await?;
        conn.stream.write_all(payload).await
    };
    send(protocol::REQUEST, head).await?;
    for chunk in body.chunks(protocol::BODY_CHUNK) {
        send(protocol::REQUEST_BODY, chunk).await?;
    }
    send(protocol::REQUEST_END, &[]).await?;

    let mut client_gone = false;
    loop {
        let frame = conn.read_frame().await?;
        if frame.request_id != id {
            return Err(ExchangeError::Unexpected(format!(
                "frame for request {} while serving {id}",
                frame.request_id
            )));
        }
        match frame.frame_type {
            protocol::RESPONSE if head_tx.is_some() => {
                let head = response_head(frame.json()?).map_err(ExchangeError::BadHead)?;
                let sent = head_tx.take().is_some_and(|tx| tx.send(Ok(head)).is_ok());
                client_gone = !sent;
            }
            protocol::RESPONSE_BODY if head_tx.is_none() => {
                // After a client disconnects, keep draining so the worker
                // finishes cleanly and can be reused.
                if !client_gone
                    && !frame.payload.is_empty()
                    && body_tx.send(Ok(frame.payload)).await.is_err()
                {
                    client_gone = true;
                }
            }
            protocol::RESPONSE_END if head_tx.is_none() => {
                let end: protocol::ResponseEnd = frame.json()?;
                return Ok(end.memory);
            }
            protocol::ERROR => {
                let err: protocol::WorkerError = frame.json()?;
                return Err(ExchangeError::Application(err.message));
            }
            other => {
                return Err(ExchangeError::Unexpected(format!(
                    "frame 0x{other:02x} not allowed {} the response head",
                    if head_tx.is_some() { "before" } else { "after" }
                )));
            }
        }
    }
}

/// Validate the worker's status and headers, as for CGI responses.
fn response_head(head: protocol::ResponseHead) -> Result<(StatusCode, HeaderMap), String> {
    let status = StatusCode::from_u16(head.status)
        .ok()
        .filter(|s| !s.is_informational())
        .ok_or_else(|| format!("invalid status {}", head.status))?;
    let mut headers = HeaderMap::new();
    for (name, value) in head.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name {name:?}"))?;
        if cgi::is_hop_by_hop(&name) {
            continue;
        }
        let value = HeaderValue::from_str(&value)
            .map_err(|_| format!("invalid value for header {name}"))?;
        headers.append(name, value);
    }
    Ok((status, headers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ResponseHead;

    #[test]
    fn response_head_is_validated() {
        let (status, headers) = response_head(ResponseHead {
            status: 201,
            headers: vec![
                ("Set-Cookie".into(), "a=1".into()),
                ("Set-Cookie".into(), "b=2".into()),
                ("Transfer-Encoding".into(), "chunked".into()),
            ],
        })
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(headers.get_all("set-cookie").iter().count(), 2);
        assert!(!headers.contains_key("transfer-encoding"));

        for (status, name, value) in [
            (99, "x", "y"),
            (101, "x", "y"),
            (200, "bad name", "y"),
            (200, "x", "a\nb"),
        ] {
            let head = ResponseHead {
                status,
                headers: vec![(name.into(), value.into())],
            };
            assert!(response_head(head).is_err(), "{status} {name:?} {value:?}");
        }
    }
}
