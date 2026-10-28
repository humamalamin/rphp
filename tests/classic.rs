//! Classic mode end to end: real `php-cgi` workers behind the HTTP server,
//! driven with the fixture in `tests/fixtures/native/`.
//!
//! Skipped when no PHP 8.4+ `php-cgi` is on PATH, unless `RPHP_REQUIRE_PHP=1`
//! (set in CI), in which case a missing interpreter fails the run.

use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use rphp::{
    backend::{
        Php,
        cgi::Script,
        classic::{ClassicPool, PoolConfig},
    },
    server::{self, App, ServerOptions},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};

struct Harness {
    addr: SocketAddr,
    pool: ClassicPool,
    stop: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<std::io::Result<()>>>,
}

struct Options {
    count: usize,
    max_requests: u64,
    request_timeout: Duration,
    max_body_bytes: u64,
    max_queue_size: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            count: 2,
            max_requests: 1000,
            request_timeout: Duration::from_secs(10),
            max_body_bytes: 1024 * 1024,
            max_queue_size: 64,
        }
    }
}

/// Resolve a usable php-cgi, or `None` to skip.
async fn php_cgi() -> Option<PathBuf> {
    let required = std::env::var_os("RPHP_REQUIRE_PHP").is_some_and(|v| v == "1");
    match rphp::php::probe("php-cgi", std::path::Path::new("/")).await {
        Ok(rt) if rt.version.is_fastcgi() => Some(rt.path),
        Ok(rt) if required => panic!("php-cgi is not the FastCGI SAPI: {}", rt.version),
        Err(err) if required => panic!("RPHP_REQUIRE_PHP=1 but php-cgi is unusable: {err}"),
        _ => {
            eprintln!("skipping: no PHP 8.4+ php-cgi on PATH (set RPHP_REQUIRE_PHP=1 to fail)");
            None
        }
    }
}

async fn start(options: Options) -> Option<Harness> {
    let binary = php_cgi().await?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/native/public")
        .canonicalize()
        .unwrap();

    let pool = ClassicPool::start(PoolConfig {
        binary,
        count: options.count,
        max_requests: options.max_requests,
        max_queue_size: options.max_queue_size,
        working_dir: root.clone(),
    })
    .await
    .expect("php-cgi workers start");

    let app = Arc::new(App {
        document_root: root.clone(),
        php: Some(Php {
            pool: pool.clone(),
            script: Script::new(root.clone(), root.join("index.php")),
            max_body_bytes: options.max_body_bytes,
            request_timeout: options.request_timeout,
        }),
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = tokio::spawn(server::run(
        listener,
        app,
        ServerOptions::default(),
        async {
            let _ = stopped.await;
        },
    ));
    Some(Harness {
        addr,
        pool,
        stop: Some(stop),
        handle: Some(handle),
    })
}

impl Harness {
    /// Ordered shutdown (HTTP, then PHP), then the exit-gate check: no
    /// php-cgi process the pool knew about may survive.
    async fn shutdown(mut self) {
        let pids = self.pool.worker_pids();
        let _ = self.stop.take().unwrap().send(());
        self.handle.take().unwrap().await.unwrap().unwrap();
        self.pool.shutdown(Duration::from_secs(5)).await;
        assert_no_survivors(&pids);
        assert!(self.pool.worker_pids().is_empty());
    }

    async fn send(&self, raw: Vec<u8>) -> Res {
        send_to(self.addr, raw).await
    }

    async fn get(&self, target: &str) -> Res {
        self.request("GET", target, &[], b"").await
    }

    async fn request(
        &self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Res {
        let mut raw =
            format!("{method} {target} HTTP/1.1\r\nHost: fixture.test\r\nConnection: close\r\n");
        for (name, value) in headers {
            raw.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() {
            raw.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        raw.push_str("\r\n");
        let mut raw = raw.into_bytes();
        raw.extend_from_slice(body);
        self.send(raw).await
    }
}

async fn send_to(addr: SocketAddr, raw: Vec<u8>) -> Res {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&raw).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut buf))
        .await
        .expect("response in time")
        .unwrap();
    Res::parse(&buf)
}

/// GET without borrowing the harness, for requests run in spawned tasks.
async fn get_at(addr: SocketAddr, target: &str) -> Res {
    let raw = format!("GET {target} HTTP/1.1\r\nHost: fixture.test\r\nConnection: close\r\n\r\n");
    send_to(addr, raw.into_bytes()).await
}

#[derive(Debug)]
struct Res {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Res {
    fn parse(raw: &[u8]) -> Self {
        let end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("no header end in {:?}", String::from_utf8_lossy(raw)));
        let head = std::str::from_utf8(&raw[..end]).unwrap();
        let mut lines = head.split("\r\n");
        let status = lines.next().unwrap()[9..12].parse().unwrap();
        let headers: Vec<(String, String)> = lines
            .map(|l| {
                let (n, v) = l.split_once(':').unwrap();
                (n.trim().to_ascii_lowercase(), v.trim().to_string())
            })
            .collect();
        let mut body = raw[end + 4..].to_vec();
        if headers
            .iter()
            .any(|(n, v)| n == "transfer-encoding" && v.contains("chunked"))
        {
            body = dechunk(&body);
        }
        Self {
            status,
            headers,
            body,
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("invalid JSON ({e}): {}", self.text()))
    }
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let nl = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .expect("chunk size line");
        let size =
            usize::from_str_radix(std::str::from_utf8(&data[..nl]).unwrap().trim(), 16).unwrap();
        data = &data[nl + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

#[tokio::test]
async fn method_query_headers_and_server_vars() {
    let Some(h) = start(Options::default()).await else {
        return;
    };

    let res = h
        .request(
            "GET",
            "/echo?name=rphp&list[]=a&list[]=b",
            &[
                ("X-Custom", "hello"),
                ("Cookie", "session=abc; theme=dark"),
                ("Proxy", "http://evil.test:3128"),
                ("X-Forwarded_For", "6.6.6.6"),
            ],
            b"",
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_eq!(res.header("content-type"), Some("application/json"));
    let j = res.json();
    assert_eq!(j["sapi"], "cgi-fcgi");
    assert_eq!(j["method"], "GET");
    assert_eq!(j["uri"], "/echo?name=rphp&list[]=a&list[]=b");
    assert_eq!(j["get"]["name"], "rphp");
    assert_eq!(j["get"]["list"], serde_json::json!(["a", "b"]));
    assert_eq!(j["script_name"], "/index.php");
    assert_eq!(j["remote_addr"], "127.0.0.1");
    assert_eq!(j["server_name"], "fixture.test");
    assert_eq!(j["headers"]["HTTP_X_CUSTOM"], "hello");
    assert_eq!(j["cookies"]["theme"], "dark");
    assert!(j["headers"].get("HTTP_PROXY").is_none(), "httpoxy: {j}");
    assert!(j["headers"].get("HTTP_X_FORWARDED_FOR").is_none(), "{j}");

    h.shutdown().await;
}

#[tokio::test]
async fn form_body_json_body_and_php_input() {
    let Some(h) = start(Options::default()).await else {
        return;
    };

    let res = h
        .request(
            "POST",
            "/echo",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            b"a=1&b=two+words",
        )
        .await;
    let j = res.json();
    assert_eq!(j["method"], "POST");
    assert_eq!(j["post"]["b"], "two words");
    assert_eq!(j["content_length"], "15");

    let payload = br#"{"hello":"world"}"#;
    let res = h
        .request(
            "PUT",
            "/echo",
            &[("Content-Type", "application/json")],
            payload,
        )
        .await;
    let j = res.json();
    assert_eq!(j["method"], "PUT");
    assert_eq!(j["input"], r#"{"hello":"world"}"#);
    assert_eq!(j["content_type"], "application/json");

    h.shutdown().await;
}

#[tokio::test]
async fn chunked_request_body_reaches_php_with_real_length() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let raw = b"POST /echo HTTP/1.1\r\nHost: fixture.test\r\nConnection: close\r\n\
                Transfer-Encoding: chunked\r\nContent-Type: text/plain\r\n\r\n\
                5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"
        .to_vec();
    let j = h.send(raw).await.json();
    assert_eq!(j["input"], "hello world");
    assert_eq!(j["content_length"], "11");
    h.shutdown().await;
}

#[tokio::test]
async fn status_redirect_cookies_and_repeated_headers() {
    let Some(h) = start(Options::default()).await else {
        return;
    };

    let res = h.get("/status").await;
    assert_eq!(res.status, 418);
    assert_eq!(res.text(), "teapot\n");

    let res = h.get("/redirect").await;
    assert_eq!(res.status, 302);
    assert_eq!(res.header("location"), Some("/target"));

    let res = h.get("/cookies").await;
    assert_eq!(res.status, 200);
    let cookies = res.all("set-cookie");
    assert_eq!(cookies.len(), 2, "{:?}", res.headers);
    assert!(cookies[0].starts_with("a=1"));
    assert!(cookies[1].starts_with("b=2") && cookies[1].contains("HttpOnly"));
    assert_eq!(res.all("x-multi"), vec!["one", "two"]);

    let res = h.get("/nope").await;
    assert_eq!(res.status, 404);

    h.shutdown().await;
}

#[tokio::test]
async fn multipart_upload() {
    let Some(h) = start(Options::default()).await else {
        return;
    };

    let file: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let boundary = "rphpBoundary7MA4YWxk";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"field\"\r\n\r\nvalue\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"doc\"; filename=\"data.bin\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&file);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let ct = format!("multipart/form-data; boundary={boundary}");
    let j = h
        .request("POST", "/upload", &[("Content-Type", &ct)], &body)
        .await
        .json();
    assert_eq!(j["field"], "value");
    assert_eq!(j["name"], "data.bin");
    assert_eq!(j["size"], 50_000);
    assert_eq!(j["error"], 0);
    assert_eq!(j["sha1"].as_str().unwrap().len(), 40);

    h.shutdown().await;
}

#[tokio::test]
async fn large_streamed_response() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let res = h.get("/big").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body.len(), 1024 * 1024);
    assert!(res.body.iter().all(|&b| b == b'x'));
    h.shutdown().await;
}

#[tokio::test]
async fn static_files_bypass_php_and_php_files_are_never_served() {
    let Some(h) = start(Options::default()).await else {
        return;
    };

    let res = h.get("/asset.txt").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.text(), "static asset\n");

    // Requesting the entry point's path runs it (front controller), it is
    // never sent as source.
    let res = h.get("/index.php").await;
    assert!(!res.text().contains("<?php"), "{}", res.text());

    h.shutdown().await;
}

#[tokio::test]
async fn php_environment_is_cleared() {
    // SAFETY: tests in this binary do not read this variable concurrently;
    // it only has to be visible when the workers are spawned below.
    unsafe { std::env::set_var("RPHP_TEST_SECRET", "leaked") };
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let j = h.get("/env").await.json();
    assert_eq!(j["secret"], false, "{j}");
    assert_eq!(j["server_secret"], false, "{j}");
    h.shutdown().await;
}

#[tokio::test]
async fn body_limit_returns_413() {
    let Some(h) = start(Options {
        max_body_bytes: 100,
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let res = h.request("POST", "/echo", &[], &[b'x'; 101]).await;
    assert_eq!(res.status, 413);
    let res = h.request("POST", "/echo", &[], &[b'x'; 100]).await;
    assert_eq!(res.status, 200);
    h.shutdown().await;
}

#[tokio::test]
async fn timeout_returns_504_and_worker_is_replaced() {
    let Some(h) = start(Options {
        count: 1,
        request_timeout: Duration::from_millis(1000),
        ..Options::default()
    })
    .await
    else {
        return;
    };

    let before: u32 = h.get("/pid").await.text().parse().unwrap();
    let started = std::time::Instant::now();
    let res = h.get("/sleep").await;
    assert_eq!(res.status, 504);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );

    // The only worker was killed and replaced; the next request succeeds.
    let res = h.get("/pid").await;
    assert_eq!(res.status, 200, "{}", res.text());
    let after: u32 = res.text().parse().unwrap();
    assert_ne!(before, after);
    assert!(
        !process_exists(before),
        "timed-out php-cgi {before} still running"
    );

    h.shutdown().await;
}

#[tokio::test]
async fn fatal_error_and_stderr_do_not_break_the_worker() {
    let Some(h) = start(Options {
        count: 1,
        ..Options::default()
    })
    .await
    else {
        return;
    };
    assert_eq!(h.get("/fatal").await.status, 500);
    assert_eq!(h.get("/stderr").await.text(), "logged\n");
    assert_eq!(h.get("/status").await.status, 418);
    h.shutdown().await;
}

#[tokio::test]
async fn workers_are_recycled_after_max_requests() {
    let Some(h) = start(Options {
        count: 1,
        max_requests: 2,
        ..Options::default()
    })
    .await
    else {
        return;
    };

    let mut pids = Vec::new();
    for _ in 0..6 {
        let res = h.get("/pid").await;
        assert_eq!(res.status, 200, "{}", res.text());
        pids.push(res.text().parse::<u32>().unwrap());
    }
    assert_eq!(pids[0], pids[1]);
    assert_ne!(pids[1], pids[2]);
    assert_eq!(pids[2], pids[3]);
    assert_ne!(pids[3], pids[4]);

    h.shutdown().await;
}

#[tokio::test]
async fn shutdown_leaves_no_php_processes() {
    let Some(h) = start(Options {
        count: 3,
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let mut pids = HashMap::new();
    for _ in 0..12 {
        let pid: u32 = h.get("/pid").await.text().parse().unwrap();
        *pids.entry(pid).or_insert(0) += 1;
    }
    h.shutdown().await;
    for pid in pids.keys() {
        assert!(!process_exists(*pid), "php-cgi {pid} survived shutdown");
    }
}

fn process_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

fn assert_no_survivors(pids: &[u32]) {
    for &pid in pids {
        assert!(!process_exists(pid), "php-cgi {pid} survived shutdown");
    }
}

fn kill9(pid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -9 {pid}");
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// --- Phase 3: supervision, backpressure and failure injection ---

#[tokio::test]
async fn worker_killed_mid_request_returns_502_and_is_replaced() {
    let Some(h) = start(Options {
        count: 1,
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let name = format!("rphp-hang-{}.pid", std::process::id());
    let pidfile = std::env::temp_dir().join(&name);
    let _ = std::fs::remove_file(&pidfile);

    let addr = h.addr;
    let target = format!("/hang?pidfile={name}");
    let pending = tokio::spawn(async move { get_at(addr, &target).await });

    wait_until("the worker to publish its PID", || pidfile.exists()).await;
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _ = std::fs::remove_file(&pidfile);
    let started = std::time::Instant::now();
    kill9(pid);

    let res = pending.await.unwrap();
    assert_eq!(res.status, 502, "{}", res.text());
    assert!(started.elapsed() < Duration::from_secs(3));

    let res = h.get("/pid").await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_ne!(res.text().parse::<u32>().unwrap(), pid);
    assert!(!process_exists(pid));

    h.shutdown().await;
}

#[tokio::test]
async fn worker_that_dies_while_idle_is_replaced_transparently() {
    let Some(h) = start(Options {
        count: 1,
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let pid = h.pool.worker_pids()[0];
    kill9(pid);
    wait_until("the killed worker to exit", || {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_or(true, |stat| stat.contains(") Z "))
    })
    .await;

    // The dead worker is noticed at checkout; the client never sees a 502.
    let res = h.get("/pid").await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_ne!(res.text().parse::<u32>().unwrap(), pid);

    h.shutdown().await;
}

#[tokio::test]
async fn exit_in_user_code_is_a_normal_response() {
    let Some(h) = start(Options {
        count: 1,
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let pid = h.get("/pid").await.text();
    let res = h.get("/exit").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.text(), "bye\n");
    // exit() ends the request, not the php-cgi process.
    assert_eq!(h.get("/pid").await.text(), pid);
    h.shutdown().await;
}

#[tokio::test]
async fn infinite_loop_times_out_and_worker_is_replaced() {
    let Some(h) = start(Options {
        count: 1,
        request_timeout: Duration::from_millis(800),
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let before: u32 = h.get("/pid").await.text().parse().unwrap();
    assert_eq!(h.get("/loop").await.status, 504);
    let res = h.get("/pid").await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_ne!(res.text().parse::<u32>().unwrap(), before);
    assert!(!process_exists(before));
    h.shutdown().await;
}

#[tokio::test]
async fn huge_output_streams_completely() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let res = h.get("/huge?mb=32").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body.len(), 32 * 1024 * 1024);
    h.shutdown().await;
}

#[tokio::test]
async fn full_queue_returns_503_with_retry_after() {
    let Some(h) = start(Options {
        count: 1,
        max_queue_size: 1,
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let addr = h.addr;
    // One request runs, one waits in the queue...
    let running = tokio::spawn(async move { get_at(addr, "/slow").await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let queued = tokio::spawn(async move { get_at(addr, "/slow").await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ...and the next is refused straight away.
    let started = std::time::Instant::now();
    let res = h.get("/slow").await;
    assert_eq!(res.status, 503, "{}", res.text());
    assert_eq!(res.header("retry-after"), Some("1"));
    assert!(started.elapsed() < Duration::from_millis(500));

    assert_eq!(running.await.unwrap().status, 200);
    assert_eq!(queued.await.unwrap().status, 200);
    h.shutdown().await;
}

#[tokio::test]
async fn shutdown_lets_in_flight_requests_finish() {
    let Some(h) = start(Options {
        count: 1,
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let addr = h.addr;
    let in_flight = tokio::spawn(async move { get_at(addr, "/slow").await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    h.shutdown().await;
    let res = in_flight.await.unwrap();
    assert_eq!(res.status, 200, "{}", res.text());
    assert_eq!(res.text(), "slow done\n");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn idle_workers_exit_promptly_on_sigterm() {
    let Some(h) = start(Options {
        count: 4,
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let pids = h.pool.worker_pids();
    let started = std::time::Instant::now();
    h.pool.shutdown(Duration::from_secs(5)).await;
    // Well under the grace period: SIGTERM, not the SIGKILL fallback.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_no_survivors(&pids);
    h.shutdown().await;
}

#[tokio::test]
async fn stuck_worker_is_killed_after_the_grace_period() {
    let Some(h) = start(Options {
        count: 1,
        request_timeout: Duration::from_secs(30),
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let pids = h.pool.worker_pids();
    let addr = h.addr;
    let stuck = tokio::spawn(async move { get_at(addr, "/loop").await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    h.pool.shutdown(Duration::from_millis(300)).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_no_survivors(&pids);

    // The aborted request ends with an error status, not a hang.
    let res = stuck.await.unwrap();
    assert!(res.status >= 500, "{}", res.status);
    h.shutdown().await;
}
