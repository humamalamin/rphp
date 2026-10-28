//! Execution backends: turning an HTTP request into a PHP response.

pub mod cgi;
pub mod classic;

use std::time::Duration;

use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::{
    Request, Response, StatusCode,
    body::Incoming,
    header::{self, HeaderMap},
};
use tokio::time::{Instant, timeout_at};
use tracing::{debug, warn};

use crate::{body::ResponseBody, server::plain};
use cgi::{ConnInfo, Script};
use classic::{ClassicPool, ExecError};

/// Everything needed to run requests through the front controller.
#[derive(Clone)]
pub struct Php {
    pub pool: ClassicPool,
    pub script: Script,
    pub max_body_bytes: u64,
    pub request_timeout: Duration,
}

impl Php {
    /// Read the body (bounded), run the entry point, map failures to HTTP.
    pub async fn handle(&self, req: Request<Incoming>, conn: ConnInfo) -> Response<ResponseBody> {
        let deadline = Instant::now() + self.request_timeout;
        // Refuse early, before spending time reading a body nobody can serve.
        if self.pool.is_saturated() {
            warn!(method = %req.method(), path = req.uri().path(), "PHP workers saturated; refusing request");
            return overloaded();
        }
        let (parts, body) = req.into_parts();

        if declared_length(&parts.headers).is_some_and(|len| len > self.max_body_bytes) {
            return too_large();
        }
        let limit = usize::try_from(self.max_body_bytes).unwrap_or(usize::MAX);
        let body = match timeout_at(deadline, Limited::new(body, limit).collect()).await {
            Ok(Ok(collected)) => collected.to_bytes(),
            Ok(Err(err)) if err.downcast_ref::<LengthLimitError>().is_some() => {
                return too_large();
            }
            Ok(Err(err)) => {
                debug!(error = %err, "failed to read request body");
                return plain(StatusCode::BAD_REQUEST, "malformed request body\n");
            }
            Err(_) => {
                return plain(
                    StatusCode::REQUEST_TIMEOUT,
                    "request body was not received in time\n",
                );
            }
        };

        let params = cgi::params(&self.script, &parts, conn, body.len());
        match self.pool.execute(params, body, deadline).await {
            Ok(res) => res,
            Err(err) => {
                warn!(method = %parts.method, path = parts.uri.path(), error = %err, "PHP request failed");
                match err {
                    ExecError::Timeout => {
                        plain(StatusCode::GATEWAY_TIMEOUT, "PHP did not respond in time\n")
                    }
                    ExecError::Overloaded => overloaded(),
                    ExecError::Unavailable => {
                        plain(StatusCode::SERVICE_UNAVAILABLE, "server is shutting down\n")
                    }
                    ExecError::Upstream(_) => plain(
                        StatusCode::BAD_GATEWAY,
                        "PHP failed to produce a valid response\n",
                    ),
                }
            }
        }
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
