//! HTTP front end: accept loop, routing, graceful shutdown.

use std::{convert::Infallible, future::Future, io, time::Duration};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::Incoming,
    header::{ALLOW, CONTENT_TYPE, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    server::graceful::GracefulShutdown,
};
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

pub type ResponseBody = BoxBody<Bytes, Infallible>;

/// Server behaviour that does not depend on PHP.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub shutdown_timeout: Duration,
    pub header_read_timeout: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            shutdown_timeout: Duration::from_secs(10),
            header_read_timeout: Duration::from_secs(30),
        }
    }
}

/// Accept connections on `listener` until `shutdown` resolves, then stop
/// accepting and give in-flight connections up to `shutdown_timeout` to finish.
///
/// Accept errors never stop the server: per-connection errors are skipped and
/// resource exhaustion (e.g. `EMFILE`) backs off briefly before retrying.
pub async fn run<F>(listener: TcpListener, options: ServerOptions, shutdown: F) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(options.header_read_timeout);

    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer_addr) = match accepted {
                    Ok(conn) => conn,
                    Err(err) if is_connection_error(&err) => {
                        debug!(error = %err, "client connection failed during accept");
                        continue;
                    }
                    Err(err) => {
                        error!(error = %err, "accept failed; retrying in 1s");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };

                let conn = builder.serve_connection(TokioIo::new(stream), service_fn(handle_request));
                let conn = graceful.watch(conn);
                tokio::spawn(async move {
                    if let Err(err) = conn.await {
                        debug!(%peer_addr, error = %err, "HTTP connection closed with error");
                    }
                });
            }
            () = &mut shutdown => {
                info!("shutdown requested; no longer accepting connections");
                break;
            }
        }
    }

    drop(listener);

    let in_flight = graceful.count();
    if in_flight > 0 {
        info!(in_flight, "waiting for open connections to finish");
    }
    match tokio::time::timeout(options.shutdown_timeout, graceful.shutdown()).await {
        Ok(()) => info!("all connections closed"),
        Err(_) => warn!(
            timeout_ms = options.shutdown_timeout.as_millis() as u64,
            "shutdown timeout elapsed; remaining connections were dropped"
        ),
    }
    Ok(())
}

/// Errors that concern a single client and say nothing about the listener.
fn is_connection_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

async fn handle_request(req: Request<Incoming>) -> Result<Response<ResponseBody>, Infallible> {
    Ok(route(req.method(), req.uri().path()))
}

/// Routing is separated from I/O so it can be unit-tested.
pub fn route(method: &Method, path: &str) -> Response<ResponseBody> {
    match path {
        "/health" => match *method {
            Method::GET => text(StatusCode::OK, "ok\n"),
            // HEAD: same status and headers as GET, no body.
            Method::HEAD => {
                let mut res = text(StatusCode::OK, "");
                *res.body_mut() = Empty::<Bytes>::new().boxed();
                res
            }
            _ => {
                let mut res = text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
                res.headers_mut()
                    .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
                res
            }
        },
        _ => text(
            StatusCode::NOT_IMPLEMENTED,
            "RPHP: PHP execution is not implemented yet.\n",
        ),
    }
}

fn text(status: StatusCode, body: &'static str) -> Response<ResponseBody> {
    let mut res = Response::new(Full::new(Bytes::from_static(body.as_bytes())).boxed());
    *res.status_mut() = status;
    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_get_and_head() {
        assert_eq!(route(&Method::GET, "/health").status(), StatusCode::OK);
        assert_eq!(route(&Method::HEAD, "/health").status(), StatusCode::OK);
    }

    #[test]
    fn health_rejects_other_methods_with_allow_header() {
        let res = route(&Method::POST, "/health");
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(res.headers()[ALLOW], "GET, HEAD");
    }

    #[test]
    fn everything_else_is_not_implemented() {
        assert_eq!(
            route(&Method::GET, "/").status(),
            StatusCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            route(&Method::POST, "/index.php").status(),
            StatusCode::NOT_IMPLEMENTED
        );
    }
}
