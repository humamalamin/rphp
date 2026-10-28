//! End-to-end tests for the HTTP shell over real TCP sockets.

use std::{path::PathBuf, sync::Arc, time::Duration};

use rphp::server::{self, Admin, App, ServerOptions};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

struct TestServer {
    addr: std::net::SocketAddr,
    app: Arc<App>,
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<std::io::Result<()>>,
    root: PathBuf,
}

/// A document root with a few static files, served without a PHP backend.
fn document_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rphp-http-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let public = dir.join("public");
    std::fs::create_dir_all(public.join("css")).unwrap();
    std::fs::write(public.join("css/app.css"), "body{}\n").unwrap();
    std::fs::write(public.join("index.php"), "<?php echo 'secret';").unwrap();
    std::fs::write(public.join(".env"), "SECRET=1").unwrap();
    std::fs::write(dir.join("outside.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(dir.join("outside.txt"), public.join("link.txt")).unwrap();
    public.canonicalize().unwrap()
}

async fn start(options: ServerOptions) -> TestServer {
    start_app(options, true).await
}

async fn start_app(options: ServerOptions, builtin_endpoints: bool) -> TestServer {
    let root = document_root();
    let app = Arc::new(App {
        builtin_endpoints,
        ..App::new(root.clone(), None)
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = tokio::spawn(server::run(listener, Arc::clone(&app), options, async {
        let _ = stopped.await;
    }));
    TestServer {
        addr,
        app,
        stop: Some(stop),
        handle,
        root,
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(app_dir) = self.root.parent() {
            let _ = std::fs::remove_dir_all(app_dir);
        }
    }
}

impl TestServer {
    async fn shutdown(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), &mut self.handle)
            .await
            .expect("server did not stop in time")
            .unwrap()
            .unwrap();
    }
}

/// Send one request with `Connection: close` and return the raw response.
async fn request(addr: std::net::SocketAddr, method: &str, path: &str) -> String {
    request_with(addr, method, path, "").await
}

/// Like [`request`], with extra header lines (each ending in `\r\n`).
async fn request_with(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req =
        format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{headers}\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(buf).unwrap()
}

#[tokio::test]
async fn health_endpoint() {
    let srv = start(ServerOptions::default()).await;

    let res = request(srv.addr, "GET", "/health").await;
    assert!(res.starts_with("HTTP/1.1 200 OK\r\n"), "{res}");
    assert!(
        res.to_ascii_lowercase()
            .contains("content-type: text/plain; charset=utf-8")
    );
    assert!(res.ends_with("\r\n\r\nok\n"), "{res}");

    let res = request(srv.addr, "HEAD", "/health").await;
    assert!(res.starts_with("HTTP/1.1 200 OK\r\n"), "{res}");
    assert!(
        res.to_ascii_lowercase().contains("content-length: 3\r\n"),
        "HEAD must report the GET length: {res:?}"
    );
    assert!(
        res.ends_with("\r\n\r\n"),
        "HEAD must not have a body: {res:?}"
    );

    let res = request(srv.addr, "DELETE", "/health").await;
    assert!(
        res.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
        "{res}"
    );
    assert!(
        res.to_ascii_lowercase().contains("allow: get, head"),
        "{res}"
    );

    srv.shutdown().await;
}

#[tokio::test]
async fn serves_static_files() {
    let srv = start(ServerOptions::default()).await;

    let res = request(srv.addr, "GET", "/css/app.css").await;
    assert!(res.starts_with("HTTP/1.1 200 OK\r\n"), "{res}");
    let lower = res.to_ascii_lowercase();
    assert!(
        lower.contains("content-type: text/css; charset=utf-8"),
        "{res}"
    );
    assert!(lower.contains("content-length: 7\r\n"), "{res}");
    assert!(lower.contains("last-modified: "), "{res}");
    assert!(res.ends_with("\r\n\r\nbody{}\n"), "{res}");

    let res = request(srv.addr, "HEAD", "/css/app.css").await;
    assert!(
        res.to_ascii_lowercase().contains("content-length: 7\r\n"),
        "{res}"
    );
    assert!(res.ends_with("\r\n\r\n"), "{res:?}");

    srv.shutdown().await;
}

#[tokio::test]
async fn never_serves_php_source_hidden_files_or_escaping_symlinks() {
    let srv = start(ServerOptions::default()).await;
    // Without a PHP backend, anything that is not static ends up as 501.
    for path in [
        "/index.php",
        "/.env",
        "/link.txt",
        "/../outside.txt",
        "/missing.css",
        "/",
    ] {
        let res = request(srv.addr, "GET", path).await;
        assert!(res.starts_with("HTTP/1.1 501 "), "{path}: {res}");
        assert!(!res.contains("secret") && !res.contains("SECRET") && !res.contains("outside"));
    }
    srv.shutdown().await;
}

#[tokio::test]
async fn shutdown_closes_idle_keep_alive_connections() {
    let srv = start(ServerOptions::default()).await;

    // Keep-alive connection that stays open after its response.
    let mut idle = TcpStream::connect(srv.addr).await.unwrap();
    idle.write_all(b"GET /health HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 512];
    let n = idle.read(&mut buf).await.unwrap();
    assert!(std::str::from_utf8(&buf[..n]).unwrap().contains("200 OK"));

    // Graceful shutdown must not wait for the full timeout on an idle client.
    let started = std::time::Instant::now();
    srv.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(5));

    // The server closes the idle connection.
    let n = tokio::time::timeout(Duration::from_secs(2), idle.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0);
}

#[tokio::test]
async fn shutdown_timeout_bounds_a_stuck_client() {
    let srv = start(ServerOptions {
        shutdown_timeout: Duration::from_millis(200),
        header_read_timeout: Duration::from_secs(60),
    })
    .await;

    // Half-sent request: the connection is busy, not idle.
    let mut stuck = TcpStream::connect(srv.addr).await.unwrap();
    stuck
        .write_all(b"GET /health HTTP/1.1\r\nHo")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = std::time::Instant::now();
    srv.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn header_read_timeout_drops_slow_clients() {
    let srv = start(ServerOptions {
        shutdown_timeout: Duration::from_secs(1),
        header_read_timeout: Duration::from_millis(200),
    })
    .await;

    let mut slow = TcpStream::connect(srv.addr).await.unwrap();
    slow.write_all(b"GET /health HTTP/1.1\r\n").await.unwrap();

    let mut buf = Vec::new();
    let res = tokio::time::timeout(Duration::from_secs(3), slow.read_to_end(&mut buf)).await;
    assert!(
        res.is_ok(),
        "server kept a slow client open past header_read_timeout"
    );

    srv.shutdown().await;
}

fn header<'a>(res: &'a str, name: &str) -> Option<&'a str> {
    let head = res.split("\r\n\r\n").next()?;
    head.split("\r\n").skip(1).find_map(|line| {
        let (n, v) = line.split_once(':')?;
        n.eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

#[tokio::test]
async fn metrics_endpoint_counts_requests() {
    let srv = start(ServerOptions::default()).await;

    request(srv.addr, "GET", "/css/app.css").await;
    request(srv.addr, "GET", "/missing").await;

    let res = request(srv.addr, "GET", "/metrics").await;
    assert!(res.starts_with("HTTP/1.1 200 OK\r\n"), "{res}");
    assert_eq!(
        header(&res, "content-type"),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    for expected in [
        r#"rphp_http_requests_total{handler="static",status="2xx"} 1"#,
        // Without PHP the fallback is 501.
        r#"rphp_http_requests_total{handler="php",status="5xx"} 1"#,
        // The scrape itself is still in flight.
        "rphp_http_requests_in_flight 1",
    ] {
        assert!(res.contains(&format!("{expected}\n")), "{expected}\n{res}");
    }
    // No PHP pool, no worker series.
    assert!(!res.contains("rphp_php_workers"), "{res}");

    let res = request(srv.addr, "POST", "/metrics").await;
    assert!(res.starts_with("HTTP/1.1 405 "), "{res}");

    srv.shutdown().await;
}

#[tokio::test]
async fn every_response_carries_a_request_id() {
    let srv = start(ServerOptions::default()).await;

    let a = request(srv.addr, "GET", "/health").await;
    let b = request(srv.addr, "GET", "/css/app.css").await;
    let (a, b) = (
        header(&a, "x-request-id").unwrap(),
        header(&b, "x-request-id").unwrap(),
    );
    assert_ne!(a, b);
    assert_eq!(a.len(), 33, "{a}");

    // A well-formed ID from the client (or a proxy in front) is kept...
    let res = request_with(srv.addr, "GET", "/health", "X-Request-Id: trace-42\r\n").await;
    assert_eq!(header(&res, "x-request-id"), Some("trace-42"));

    // ...anything that could garble a log line is replaced.
    let res = request_with(
        srv.addr,
        "GET",
        "/health",
        "X-Request-Id: evil\" level=error msg=\"forged\r\n",
    )
    .await;
    let id = header(&res, "x-request-id").unwrap();
    assert!(!id.contains("evil"), "{id}");

    srv.shutdown().await;
}

#[tokio::test]
async fn builtin_endpoints_can_be_handed_to_the_application() {
    let srv = start_app(ServerOptions::default(), false).await;
    // With an admin listener, /health and /metrics belong to the app (here:
    // no PHP backend, so 501).
    for path in ["/health", "/metrics"] {
        let res = request(srv.addr, "GET", path).await;
        assert!(res.starts_with("HTTP/1.1 501 "), "{path}: {res}");
    }
    srv.shutdown().await;
}

#[tokio::test]
async fn admin_listener_serves_health_and_metrics_only() {
    let app_srv = start(ServerOptions::default()).await;
    request(app_srv.addr, "GET", "/css/app.css").await;

    let admin = Arc::new(Admin {
        metrics: Arc::clone(&app_srv.app.metrics),
        pool: None,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = tokio::spawn(server::run(
        listener,
        admin,
        ServerOptions::default(),
        async {
            let _ = stopped.await;
        },
    ));

    let res = request(addr, "GET", "/health").await;
    assert!(res.starts_with("HTTP/1.1 200 OK\r\n"), "{res}");
    // Shares the application's metrics; admin requests are not counted.
    let res = request(addr, "GET", "/metrics").await;
    assert!(
        res.contains("rphp_http_requests_total{handler=\"static\",status=\"2xx\"} 1\n"),
        "{res}"
    );
    assert!(
        res.contains("rphp_http_requests_total{handler=\"builtin\",status=\"2xx\"} 0\n"),
        "{res}"
    );
    let res = request(addr, "GET", "/css/app.css").await;
    assert!(res.starts_with("HTTP/1.1 404 "), "{res}");

    stop.send(()).unwrap();
    handle.await.unwrap().unwrap();
    app_srv.shutdown().await;
}
