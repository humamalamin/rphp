//! Execution backends: turning an HTTP request into a PHP response.
//!
//! * [`classic`]: FastCGI to `php-cgi` (default).
//! * [`persistent`]: long-running PHP CLI workers (experimental).
//!
//! Both run on the shared, supervised worker [`pool`].

pub mod cgi;
pub mod classic;
pub mod persistent;
pub mod pool;
pub mod protocol;

use std::time::Duration;

use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::{
    Request, Response, StatusCode,
    body::Incoming,
    header::{self, HeaderMap},
};
use tokio::time::{Instant, timeout_at};
use tracing::{debug, error, warn};

use crate::{
    body::ResponseBody,
    metrics::{Failure, Metrics},
    server::plain,
};
use cgi::{ConnInfo, Script};
use classic::ClassicPool;
use persistent::PersistentPool;
use pool::{ExecError, PoolStats};

/// The worker pool of whichever mode is configured.
#[derive(Clone)]
pub enum PhpPool {
    Classic(ClassicPool),
    Persistent(PersistentPool),
}

impl PhpPool {
    pub fn is_saturated(&self) -> bool {
        match self {
            Self::Classic(p) => p.is_saturated(),
            Self::Persistent(p) => p.is_saturated(),
        }
    }

    pub fn stats(&self) -> PoolStats {
        match self {
            Self::Classic(p) => p.stats(),
            Self::Persistent(p) => p.stats(),
        }
    }

    pub fn worker_pids(&self) -> Vec<u32> {
        match self {
            Self::Classic(p) => p.worker_pids(),
            Self::Persistent(p) => p.worker_pids(),
        }
    }

    /// See [`pool::Pool::shutdown`].
    pub async fn shutdown(&self, grace: Duration) {
        match self {
            Self::Classic(p) => p.shutdown(grace).await,
            Self::Persistent(p) => p.shutdown(grace).await,
        }
    }
}

impl From<ClassicPool> for PhpPool {
    fn from(pool: ClassicPool) -> Self {
        Self::Classic(pool)
    }
}

impl From<PersistentPool> for PhpPool {
    fn from(pool: PersistentPool) -> Self {
        Self::Persistent(pool)
    }
}

/// Everything needed to run requests through PHP.
#[derive(Clone)]
pub struct Php {
    pub pool: PhpPool,
    /// The front controller (classic mode) and document root.
    pub script: Script,
    pub max_body_bytes: u64,
    pub request_timeout: Duration,
}

impl Php {
    /// Read the body (bounded), run PHP, map failures to HTTP.
    /// Failures are counted in `metrics` by source (client, server, PHP).
    pub async fn handle(
        &self,
        req: Request<Incoming>,
        conn: ConnInfo,
        metrics: &Metrics,
    ) -> Response<ResponseBody> {
        let deadline = Instant::now() + self.request_timeout;
        // Refuse early, before spending time reading a body nobody can serve.
        if self.pool.is_saturated() {
            warn!(method = %req.method(), path = req.uri().path(), "PHP workers saturated; refusing request");
            metrics.failure(Failure::Overloaded);
            return overloaded();
        }
        let (parts, body) = req.into_parts();

        if declared_length(&parts.headers).is_some_and(|len| len > self.max_body_bytes) {
            metrics.failure(Failure::BodyTooLarge);
            return too_large();
        }
        let limit = usize::try_from(self.max_body_bytes).unwrap_or(usize::MAX);
        let body = match timeout_at(deadline, Limited::new(body, limit).collect()).await {
            Ok(Ok(collected)) => collected.to_bytes(),
            Ok(Err(err)) if err.downcast_ref::<LengthLimitError>().is_some() => {
                metrics.failure(Failure::BodyTooLarge);
                return too_large();
            }
            Ok(Err(err)) => {
                debug!(error = %err, "failed to read request body");
                metrics.failure(Failure::MalformedBody);
                return plain(StatusCode::BAD_REQUEST, "malformed request body\n");
            }
            Err(_) => {
                metrics.failure(Failure::BodyTimeout);
                return plain(
                    StatusCode::REQUEST_TIMEOUT,
                    "request body was not received in time\n",
                );
            }
        };

        let result = match &self.pool {
            PhpPool::Classic(pool) => {
                let params = cgi::params(&self.script, &parts, conn, body.len());
                pool.execute(params, body, deadline).await
            }
            PhpPool::Persistent(pool) => {
                let head = persistent::request_head(
                    &self.script.document_root,
                    &parts,
                    conn,
                    body.len(),
                    deadline,
                );
                // Lossy UTF-8 and JSON escaping can grow headers past what one
                // frame carries: refuse here rather than fail in the exchange.
                if head.len() > protocol::MAX_PAYLOAD {
                    metrics.failure(Failure::HeadersTooLarge);
                    return plain(
                        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                        "request headers too large\n",
                    );
                }
                pool.execute(head, body, deadline).await
            }
        };
        let err = match result {
            Ok(res) => return res,
            Err(err) => err,
        };

        let (method, path) = (&parts.method, parts.uri.path());
        let (failure, response) = match &err {
            ExecError::Timeout => (
                Failure::PhpTimeout,
                plain(StatusCode::GATEWAY_TIMEOUT, "PHP did not respond in time\n"),
            ),
            ExecError::Overloaded => (Failure::Overloaded, overloaded()),
            ExecError::Unavailable => (
                Failure::ShuttingDown,
                plain(StatusCode::SERVICE_UNAVAILABLE, "server is shutting down\n"),
            ),
            ExecError::Upstream(_) => (
                Failure::PhpFailed,
                plain(
                    StatusCode::BAD_GATEWAY,
                    "PHP failed to produce a valid response\n",
                ),
            ),
            // The detail is for the operator's log; the client gets none.
            ExecError::Application(_) => (
                Failure::PhpException,
                plain(StatusCode::INTERNAL_SERVER_ERROR, "internal server error\n"),
            ),
        };
        if matches!(err, ExecError::Application(_)) {
            error!(%method, path, error = %err, "PHP application error");
        } else {
            warn!(%method, path, error = %err, "PHP request failed");
        }
        metrics.failure(failure);
        response
    }
}

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// 503 with `Retry-After`, so well-behaved clients back off.
fn overloaded() -> Response<ResponseBody> {
    let mut res = plain(
        StatusCode::SERVICE_UNAVAILABLE,
        "server is overloaded, retry later\n",
    );
    res.headers_mut()
        .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
    res
}

fn too_large() -> Response<ResponseBody> {
    plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large\n")
}
