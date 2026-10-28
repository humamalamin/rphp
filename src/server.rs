//! HTTP front end: accept loop, routing, graceful shutdown.

use std::{convert::Infallible, future::Future, io, path::PathBuf, sync::Arc, time::Duration};

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

pub use crate::body::ResponseBody;
use crate::{
    backend::{Php, cgi::ConnInfo},
    body, static_files,
};

/// What the server serves.
pub struct App {
    /// Canonical document root; existing files under it are served directly.
    pub document_root: PathBuf,
    /// Front controller for everything else. `None` serves only built-in
    /// endpoints and static files (used by tests that run without PHP).
    pub php: Option<Php>,
}

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
pub async fn run<F>(
    listener: TcpListener,
    app: Arc<App>,
    options: ServerOptions,
    shutdown: F,
) -> io::Result<()>
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

                let local_addr = match stream.local_addr() {
                    Ok(addr) => addr,
                    Err(err) => {
                        debug!(%peer_addr, error = %err, "connection closed before it was served");
                        continue;
                    }
                };
                let info = ConnInfo { remote: peer_addr, local: local_addr };
                let app = Arc::clone(&app);
                let service = service_fn(move |req| handle_request(Arc::clone(&app), info, req));
                let conn = builder.serve_connection(TokioIo::new(stream), service);
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

/// Routing order: built-in endpoints, then static files (GET/HEAD), then the
/// PHP front controller.
async fn handle_request(
    app: Arc<App>,
    conn: ConnInfo,
    req: Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    if req.uri().path() == "/health" {
        return Ok(health(req.method()));
    }
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        if let Some(file) = static_files::lookup(&app.document_root, req.uri().path()).await {
            return Ok(static_files::serve(file, req.method(), req.headers()).await);
        }
    }
    Ok(match &app.php {
        Some(php) => php.handle(req, conn).await,
        None => plain(StatusCode::NOT_IMPLEMENTED, "no PHP backend configured\n"),
    })
}

fn health(method: &Method) -> Response<ResponseBody> {
    match *method {
        // hyper strips the body from HEAD responses and keeps Content-Length.
        Method::GET | Method::HEAD => plain(StatusCode::OK, "ok\n"),
        _ => {
            let mut res = plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
            res.headers_mut()
                .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
            res
        }
    }
}

/// A short `text/plain` response.
pub fn plain(status: StatusCode, body: &'static str) -> Response<ResponseBody> {
    let mut res = Response::new(body::full(body));
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
        assert_eq!(health(&Method::GET).status(), StatusCode::OK);
        assert_eq!(health(&Method::HEAD).status(), StatusCode::OK);
    }

    #[test]
    fn health_rejects_other_methods_with_allow_header() {
        let res = health(&Method::POST);
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(res.headers()[ALLOW], "GET, HEAD");
    }
}
