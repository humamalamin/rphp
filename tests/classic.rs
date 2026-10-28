//! Classic mode end to end: real `php-cgi` workers behind the HTTP server,
//! driven with the fixture in `tests/fixtures/native/`.
//!
//! Skipped when no PHP 8.4+ `php-cgi` is on PATH, unless `RPHP_REQUIRE_PHP=1`
//! (set in CI), in which case a missing interpreter fails the run.

mod common;

use std::{collections::HashMap, path::PathBuf, time::Duration};

use common::{Harness, assert_no_survivors, get_at, kill9, process_exists, wait_until};
use rphp::{
    backend::{
        Php,
        cgi::Script,
        classic::{Classic, ClassicPool},
        pool::PoolConfig,
    },
    server::App,
};

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

async fn start(options: Options) -> Option<Harness> {
    let binary = common::require_php("php-cgi", "cgi-fcgi").await?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/native/public")
        .canonicalize()
        .unwrap();

    let kind = Classic {
        binary,
        working_dir: root.clone(),
    };
    let pool = ClassicPool::start(
        kind,
        PoolConfig {
            count: options.count,
            max_requests: options.max_requests,
            max_queue_size: options.max_queue_size,
            max_age: None,
        },
    )
    .await
    .expect("php-cgi workers start");

    let app = App::new(
        root.clone(),
        Some(Php {
            pool: pool.clone().into(),
            script: Script::new(root.clone(), root.join("index.php")),
            max_body_bytes: options.max_body_bytes,
            request_timeout: options.request_timeout,
        }),
    );
    Some(Harness::start(app, pool.into()).await)
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

// --- Phase 4: observability ---

/// Value of an unlabelled or fully labelled sample in a /metrics response.
fn metric(text: &str, series: &str) -> u64 {
    let prefix = format!("{series} ");
    text.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no sample {series} in\n{text}"))
        .parse()
        .unwrap()
}

#[tokio::test]
async fn request_id_reaches_php_and_the_response() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let res = h
        .request("GET", "/echo", &[("X-Request-Id", "upstream-7")], b"")
        .await;
    assert_eq!(res.header("x-request-id"), Some("upstream-7"));
    assert_eq!(res.json()["headers"]["HTTP_X_REQUEST_ID"], "upstream-7");

    let res = h.get("/echo").await;
    let id = res
        .header("x-request-id")
        .expect("generated ID")
        .to_string();
    assert_eq!(res.json()["headers"]["HTTP_X_REQUEST_ID"], id.as_str());
    h.shutdown().await;
}

#[tokio::test]
async fn metrics_track_workers_restarts_and_failures() {
    let Some(h) = start(Options {
        count: 1,
        max_requests: 2,
        // Long enough for /slow (1s), short enough for /loop to time out.
        request_timeout: Duration::from_millis(1500),
        ..Options::default()
    })
    .await
    else {
        return;
    };

    let text = h.get("/metrics").await.text();
    assert_eq!(metric(&text, "rphp_php_workers_configured"), 1);
    assert_eq!(metric(&text, r#"rphp_php_workers{state="idle"}"#), 1);

    // A running request shows up as a busy worker.
    let addr = h.addr;
    let slow = tokio::spawn(async move { get_at(addr, "/slow").await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let text = h.get("/metrics").await.text();
    assert_eq!(metric(&text, r#"rphp_php_workers{state="busy"}"#), 1);
    assert_eq!(slow.await.unwrap().status, 200);

    // Second request on this worker: it reaches max_requests and is recycled.
    assert_eq!(h.get("/pid").await.status, 200);
    // A timeout replaces the (new) worker and is counted as a PHP failure.
    assert_eq!(h.get("/loop").await.status, 504);
    assert_eq!(h.get("/pid").await.status, 200);

    let text = h.get("/metrics").await.text();
    assert_eq!(
        metric(
            &text,
            r#"rphp_php_worker_restarts_total{reason="recycled"}"#
        ),
        1
    );
    assert_eq!(
        metric(&text, r#"rphp_php_worker_restarts_total{reason="failed"}"#),
        1
    );
    assert_eq!(
        metric(
            &text,
            r#"rphp_request_failures_total{source="php",kind="timeout"}"#
        ),
        1
    );
    assert_eq!(
        metric(
            &text,
            r#"rphp_http_requests_total{handler="php",status="2xx"}"#
        ),
        3
    );
    assert_eq!(
        metric(
            &text,
            r#"rphp_http_requests_total{handler="php",status="5xx"}"#
        ),
        1
    );
    assert_eq!(
        metric(
            &text,
            r#"rphp_http_request_duration_seconds_count{handler="php"}"#
        ),
        4
    );
    h.shutdown().await;
}

#[tokio::test]
async fn health_reports_unavailable_without_workers() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let res = h.get("/health").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.text(), "ok\n");

    h.pool.shutdown(Duration::from_secs(5)).await;
    let res = h.get("/health").await;
    assert_eq!(res.status, 503, "{}", res.text());
    h.shutdown().await;
}
