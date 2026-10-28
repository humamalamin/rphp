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
use tokio::{net::TcpListener, time::Instant};
use tracing::{Instrument, debug, error, info, info_span, warn};

pub use crate::body::ResponseBody;
use crate::{
    backend::{
        Php,
        cgi::ConnInfo,
        classic::{ClassicPool, PoolStats},
    },
    body,
    metrics::{self, Handler, Metrics},
    request_id, static_files,
};

/// Something [`run`] can serve: the application listener ([`App`]) or the
/// admin listener ([`Admin`]).
pub trait Router: Send + Sync + 'static {
    fn route(
        self: Arc<Self>,
        conn: ConnInfo,
        req: Request<Incoming>,
    ) -> impl Future<Output = Response<ResponseBody>> + Send;

    /// Called when a connection ends with an HTTP or I/O error.
    fn connection_error(&self) {}
}

/// What the application listener serves.
pub struct App {
    /// Canonical document root; existing files under it are served directly.
    pub document_root: PathBuf,
    /// Front controller for everything else. `None` serves only built-in
    /// endpoints and static files (used by tests that run without PHP).
    pub php: Option<Php>,
    pub metrics: Arc<Metrics>,
    /// Answer `/health` and `/metrics` here. False when an admin listener
    /// answers them; the application then receives those paths like any other.
    pub builtin_endpoints: bool,
}

impl App {
    /// An application with its own metrics and the built-in endpoints on.
    pub fn new(document_root: PathBuf, php: Option<Php>) -> Self {
        Self {
            document_root,
            php,
            metrics: Arc::new(Metrics::new()),
            builtin_endpoints: true,
        }
    }

    fn pool(&self) -> Option<&ClassicPool> {
        self.php.as_ref().map(|php| &php.pool)
    }
}

/// The admin listener: `/health` and `/metrics` only, kept off the
/// application's address so they need not be exposed with it.
pub struct Admin {
    pub metrics: Arc<Metrics>,
    pub pool: Option<ClassicPool>,
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
pub async fn run<R, F>(
    listener: TcpListener,
    app: Arc<R>,
    options: ServerOptions,
    shutdown: F,
) -> io::Result<()>
where
    R: Router,
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
                let service = {
                    let app = Arc::clone(&app);
                    service_fn(move |req| {
                        let app = Arc::clone(&app);
                        async move { Ok::<_, Infallible>(app.route(info, req).await) }
                    })
                };
                let conn = builder.serve_connection(TokioIo::new(stream), service);
                let conn = graceful.watch(conn);
                let app = Arc::clone(&app);
                tokio::spawn(async move {
                    if let Err(err) = conn.await {
                        app.connection_error();
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

impl Router for App {
    /// Assign the request ID, route inside the request's span, then record
    /// metrics and the access log entry.
    async fn route(
        self: Arc<Self>,
        conn: ConnInfo,
        mut req: Request<Incoming>,
    ) -> Response<ResponseBody> {
        let started = Instant::now();
        let _in_flight = self.metrics.start_request();
        let id = request_id::for_request(req.headers());
        // PHP sees the same ID as HTTP_X_REQUEST_ID.
        req.headers_mut().insert(request_id::HEADER, id.clone());
        let method = req.method().clone();
        let path = req.uri().path().to_owned();

        let span = info_span!("request", request_id = id.to_str().unwrap_or_default());
        let (handler, mut res) = self.dispatch(conn, req).instrument(span.clone()).await;
        res.headers_mut().insert(request_id::HEADER, id);

        let elapsed = started.elapsed();
        self.metrics.observe(handler, res.status(), elapsed);
        span.in_scope(|| {
            // The query string is left out: it often carries tokens.
            let status = res.status().as_u16();
            let duration_ms = elapsed.as_secs_f64() * 1000.0;
            if handler == Handler::Builtin {
                debug!(target: "rphp::access", %method, path, status, duration_ms, "request");
            } else {
                info!(target: "rphp::access", %method, path, status, duration_ms, handler = handler.label(), "request");
            }
        });
        res
    }

    fn connection_error(&self) {
        self.metrics.connection_error();
    }
}

impl App {
    /// Routing order: built-in endpoints, then static files (GET/HEAD), then
    /// the PHP front controller.
    async fn dispatch(
        &self,
        conn: ConnInfo,
        req: Request<Incoming>,
    ) -> (Handler, Response<ResponseBody>) {
        if self.builtin_endpoints {
            if let Some(res) = builtin(req.uri().path(), req.method(), &self.metrics, self.pool()) {
                return (Handler::Builtin, res);
            }
        }
        if matches!(*req.method(), Method::GET | Method::HEAD) {
            if let Some(file) = static_files::lookup(&self.document_root, req.uri().path()).await {
                return (
                    Handler::Static,
                    static_files::serve(file, req.method(), req.headers()).await,
                );
            }
        }
        let res = match &self.php {
            Some(php) => php.handle(req, conn, &self.metrics).await,
            None => plain(StatusCode::NOT_IMPLEMENTED, "no PHP backend configured\n"),
        };
        (Handler::Php, res)
    }
}

impl Router for Admin {
    async fn route(
        self: Arc<Self>,
        _conn: ConnInfo,
        req: Request<Incoming>,
    ) -> Response<ResponseBody> {
        builtin(
            req.uri().path(),
            req.method(),
            &self.metrics,
            self.pool.as_ref(),
        )
        .unwrap_or_else(|| plain(StatusCode::NOT_FOUND, "not found\n"))
    }
}

/// `/health` and `/metrics`, or `None` for any other path.
fn builtin(
    path: &str,
    method: &Method,
    metrics: &Metrics,
    pool: Option<&ClassicPool>,
) -> Option<Response<ResponseBody>> {
    let stats = || pool.map(ClassicPool::stats);
    match path {
        "/health" => Some(only_get(method).unwrap_or_else(|| health(stats()))),
        "/metrics" => Some(only_get(method).unwrap_or_else(|| {
            let mut res = Response::new(body::full(metrics.render(stats())));
            res.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static(metrics::CONTENT_TYPE),
            );
            res
        })),
        _ => None,
    }
}

/// 405 for anything but GET and HEAD. hyper strips the body from HEAD
/// responses and keeps Content-Length.
fn only_get(method: &Method) -> Option<Response<ResponseBody>> {
    if matches!(*method, Method::GET | Method::HEAD) {
        return None;
    }
    let mut res = plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
    res.headers_mut()
        .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    Some(res)
}

/// 200 while PHP can take requests (busy workers count: requests queue for
/// them), 503 when no worker is running or the pool is shutting down, so a
/// load balancer stops sending traffic.
fn health(pool: Option<PoolStats>) -> Response<ResponseBody> {
    match pool {
        Some(stats) if stats.closed => plain(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable: shutting down\n",
        ),
        Some(stats) if !stats.is_available() => plain(
            StatusCode::SERVICE_UNAVAILABLE,
            "degraded: no PHP worker is running\n",
        ),
        _ => plain(StatusCode::OK, "ok\n"),
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

    fn stats(idle: usize, busy: usize, closed: bool) -> PoolStats {
        PoolStats {
            configured: 2,
            idle,
            busy,
            waiting: 0,
            recycled: 0,
            failed: 0,
            spawn_failures: 0,
            closed,
        }
    }

    #[test]
    fn builtin_get_and_head() {
        let m = Metrics::new();
        for path in ["/health", "/metrics"] {
            for method in [Method::GET, Method::HEAD] {
                let res = builtin(path, &method, &m, None).unwrap();
                assert_eq!(res.status(), StatusCode::OK, "{method} {path}");
            }
        }
        assert!(builtin("/other", &Method::GET, &m, None).is_none());
    }

    #[test]
    fn builtin_rejects_other_methods_with_allow_header() {
        for path in ["/health", "/metrics"] {
            let res = builtin(path, &Method::POST, &Metrics::new(), None).unwrap();
            assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert_eq!(res.headers()[ALLOW], "GET, HEAD");
        }
    }

    #[test]
    fn health_follows_worker_availability() {
        assert_eq!(health(None).status(), StatusCode::OK);
        assert_eq!(health(Some(stats(1, 0, false))).status(), StatusCode::OK);
        // All busy is not unhealthy: requests queue for them.
        assert_eq!(health(Some(stats(0, 2, false))).status(), StatusCode::OK);
        // Every worker being respawned.
        assert_eq!(
            health(Some(stats(0, 0, false))).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            health(Some(stats(2, 0, true))).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
