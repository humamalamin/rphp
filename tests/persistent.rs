//! Persistent mode end to end: real PHP CLI workers running the `rphp/worker`
//! loop (`php/worker`) with the fixture in `tests/fixtures/persistent/`.
//!
//! Skipped when no PHP 8.4+ CLI is on PATH, unless `RPHP_REQUIRE_PHP=1`.
//! The PSR-7 test also needs `composer install` in `php/worker`; it is skipped
//! without it unless `RPHP_REQUIRE_PSR7=1`.

mod common;

use std::{path::PathBuf, time::Duration};

use common::{Harness, assert_no_survivors, get_at, kill9, process_exists, wait_until};
use rphp::{
    backend::{
        Php,
        cgi::Script,
        persistent::{Persistent, PersistentPool},
        pool::PoolConfig,
    },
    server::App,
};

struct Options {
    script: &'static str,
    count: usize,
    max_requests: u64,
    max_age: Option<Duration>,
    max_memory: Option<u64>,
    request_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            script: "worker.php",
            count: 1,
            max_requests: 1000,
            max_age: None,
            max_memory: None,
            request_timeout: Duration::from_secs(10),
        }
    }
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/persistent")
        .canonicalize()
        .unwrap()
}

async fn php_cli() -> Option<PathBuf> {
    common::require_php("php", "cli").await
}

fn kind(binary: PathBuf, options: &Options) -> Persistent {
    let dir = fixture_dir();
    Persistent {
        binary,
        script: dir.join(options.script),
        working_dir: dir,
        max_memory: options.max_memory,
    }
}

fn pool_config(options: &Options) -> PoolConfig {
    PoolConfig {
        count: options.count,
        max_requests: options.max_requests,
        max_queue_size: 16,
        max_age: options.max_age,
    }
}

async fn start(options: Options) -> Option<Harness> {
    let binary = php_cli().await?;
    let pool = PersistentPool::start(kind(binary, &options), pool_config(&options))
        .await
        .expect("persistent workers start");
    let root = fixture_dir().join("public");
    let app = App::new(
        root.clone(),
        Some(Php {
            pool: pool.clone().into(),
            script: Script::new(root.clone(), root.join("index.php")),
            max_body_bytes: 8 * 1024 * 1024,
            request_timeout: options.request_timeout,
        }),
    );
    Some(Harness::start(app, pool.into()).await)
}

async fn pid(h: &Harness) -> u32 {
    let res = h.get("/pid").await;
    assert_eq!(res.status, 200, "{}", res.text());
    res.text().parse().unwrap()
}

// --- Request and response mapping ---

#[tokio::test]
async fn request_reaches_the_handler_intact() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let res = h
        .request(
            "POST",
            "/echo?a=1&list[]=x&list[]=y",
            &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Cookie", "session=abc; theme=dark"),
                ("X-Custom", "hello"),
                ("Proxy", "http://evil.test:3128"),
            ],
            b"name=two+words&n=2",
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.text());
    let j = res.json();
    assert_eq!(j["method"], "POST");
    assert_eq!(j["uri"], "/echo?a=1&list[]=x&list[]=y");
    assert_eq!(j["query"]["list"], serde_json::json!(["x", "y"]));
    assert_eq!(j["form"]["name"], "two words");
    assert_eq!(j["cookies"]["theme"], "dark");
    assert_eq!(j["custom"], "hello");
    assert_eq!(j["server"]["REMOTE_ADDR"], "127.0.0.1");
    assert_eq!(j["server"]["SERVER_NAME"], "fixture.test");
    assert_eq!(j["server"]["HTTP_X_CUSTOM"], "hello");
    assert_eq!(j["server"]["CONTENT_LENGTH"], "18");
    assert!(j["server"]["HTTP_PROXY"].is_null(), "httpoxy: {j}");
    assert!(j["timeout_ms"].as_u64().unwrap() > 0);
    h.shutdown().await;
}

#[tokio::test]
async fn large_bodies_cross_frame_boundaries() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    // 2 MiB request: several REQUEST_BODY frames.
    let body: Vec<u8> = (0..2 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let j = h.request("PUT", "/echo", &[], &body).await.json();
    assert_eq!(j["body_length"], 2 * 1024 * 1024);
    assert_eq!(j["body_sha1"].as_str().unwrap().len(), 40);

    // 3 MiB response: several RESPONSE_BODY frames.
    let res = h.get("/big").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body.len(), 3 * 1024 * 1024);
    assert!(res.body.iter().all(|&b| b == b'z'));
    h.shutdown().await;
}

#[tokio::test]
async fn status_repeated_headers_and_streamed_bodies() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let res = h.get("/cookies").await;
    assert_eq!(res.status, 201);
    assert_eq!(res.all("set-cookie"), vec!["a=1", "b=2; HttpOnly"]);
    assert_eq!(res.all("x-multi"), vec!["one", "two"]);
    assert_eq!(res.text(), "created");

    let res = h.get("/stream").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.text(), "chunk 0\nchunk 1\nchunk 2\nchunk 3\nchunk 4\n");

    assert_eq!(h.get("/nope").await.status, 404);
    // Static files never reach the worker.
    assert_eq!(
        h.get("/asset.txt").await.text(),
        "static from persistent fixture\n"
    );
    h.shutdown().await;
}

// --- Lifecycle and state isolation ---

#[tokio::test]
async fn application_state_persists_but_request_state_is_reset() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let first = h.get("/state?user=alice").await.json();
    let second = h.get("/state?user=bob").await.json();

    // Same process, booted once: this is what persistent mode buys.
    assert_eq!(first["pid"], second["pid"]);
    assert_eq!(first["booted_at"], second["booted_at"]);
    // Static state survives (documented behaviour)...
    assert_eq!(
        second["requests"].as_u64().unwrap(),
        first["requests"].as_u64().unwrap() + 1
    );
    // ...state registered with onReset() does not.
    assert!(second["scoped_before"].is_null(), "{second}");
    // Superglobals never carry request data in persistent mode.
    assert_eq!(second["get_superglobal"], serde_json::json!([]));
    h.shutdown().await;
}

#[tokio::test]
async fn output_buffers_and_stray_output_do_not_leak() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let baseline = h.get("/state").await.json()["ob_level"].clone();
    let res = h.get("/leave-buffer").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.text(), "ok", "stray echo must not reach the body");
    let after = h.get("/state").await.json()["ob_level"].clone();
    assert_eq!(
        baseline, after,
        "an unclosed ob_start() leaked into the next request"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn environment_is_cleared() {
    // SAFETY: only read by the workers spawned below.
    unsafe { std::env::set_var("RPHP_TEST_SECRET", "leaked") };
    let Some(h) = start(Options::default()).await else {
        return;
    };
    assert_eq!(h.get("/env").await.json()["secret"], false);
    h.shutdown().await;
}

// --- Failures ---

#[tokio::test]
async fn exception_is_500_without_details_and_worker_is_reused() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let before = pid(&h).await;
    for _ in 0..3 {
        let res = h.get("/throw").await;
        assert_eq!(res.status, 500);
        assert!(!res.text().contains("secret detail"), "{}", res.text());
    }
    // Exceptions must not force an application reboot per request.
    assert_eq!(pid(&h).await, before);
    h.shutdown().await;
}

#[tokio::test]
async fn non_utf8_output_does_not_wedge_the_worker() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    // Each request writes ~100 KiB of invalid UTF-8 to stderr and stdout.
    // The worker must keep serving; the forwarder logs it lossily instead of
    // giving up (which would close the pipe and silently drop later logs).
    for _ in 0..4 {
        let res = h.get("/noisy").await;
        assert_eq!(res.status, 200, "{}", res.text());
    }
    h.shutdown().await;
}

#[tokio::test]
async fn headers_too_large_for_a_frame_get_431() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    // ~360 KB of 0xFF bytes: accepted by hyper, but each becomes a 3-byte
    // U+FFFD in the JSON head, past the 1 MiB frame limit.
    let mut raw = b"GET /echo HTTP/1.1\r\nHost: fixture.test\r\nConnection: close\r\n".to_vec();
    for i in 0..6 {
        raw.extend_from_slice(format!("X-Big-{i}: ").as_bytes());
        raw.extend(std::iter::repeat_n(0xffu8, 60_000));
        raw.extend_from_slice(b"\r\n");
    }
    raw.extend_from_slice(b"\r\n");
    let res = h.send(raw).await;
    assert_eq!(res.status, 431, "{}", res.text());
    // The worker was never involved and still serves requests.
    assert_eq!(h.get("/pid").await.status, 200);
    h.shutdown().await;
}

#[tokio::test]
async fn php_errors_are_exceptions_too() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    // Calling an undefined function throws Error in PHP 8: a 500, not a crash.
    assert_eq!(h.get("/undefined-function").await.status, 500);
    h.shutdown().await;
}

#[tokio::test]
async fn fatal_error_and_exit_kill_the_worker_502_and_it_is_replaced() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    for route in ["/fatal", "/exit"] {
        let before = pid(&h).await;
        assert_eq!(h.get(route).await.status, 502, "{route}");
        assert_ne!(pid(&h).await, before, "{route}");
    }
    h.shutdown().await;
}

#[tokio::test]
async fn timeout_is_504_and_worker_is_replaced() {
    let Some(h) = start(Options {
        request_timeout: Duration::from_millis(800),
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let before = pid(&h).await;
    assert_eq!(h.get("/sleep").await.status, 504);
    assert_ne!(pid(&h).await, before);
    assert!(!process_exists(before));
    h.shutdown().await;
}

#[tokio::test]
async fn worker_killed_while_idle_is_replaced_transparently() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let before = pid(&h).await;
    kill9(before);
    wait_until("the killed worker to exit", || {
        std::fs::read_to_string(format!("/proc/{before}/stat"))
            .map_or(true, |stat| stat.contains(") Z "))
    })
    .await;
    let res = h.get("/pid").await;
    assert_eq!(res.status, 200, "{}", res.text());
    assert_ne!(res.text().parse::<u32>().unwrap(), before);
    h.shutdown().await;
}

#[tokio::test]
async fn a_script_that_is_not_a_worker_fails_startup_clearly() {
    let Some(binary) = php_cli().await else {
        return;
    };
    let options = Options {
        script: "not-a-worker.php",
        ..Options::default()
    };
    let err = PersistentPool::start(kind(binary, &options), pool_config(&options))
        .await
        .err()
        .expect("startup must fail");
    assert!(err.to_string().contains("exited during startup"), "{err}");
}

// --- Recycling ---

#[tokio::test]
async fn recycled_after_max_requests() {
    let Some(h) = start(Options {
        max_requests: 2,
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let mut pids = Vec::new();
    for _ in 0..5 {
        pids.push(pid(&h).await);
    }
    assert_eq!(pids[0], pids[1]);
    assert_ne!(pids[1], pids[2]);
    assert_eq!(pids[2], pids[3]);
    assert_ne!(pids[3], pids[4]);
    h.shutdown().await;
}

#[tokio::test]
async fn recycled_after_max_memory() {
    let Some(h) = start(Options {
        max_memory: Some(8 * 1024 * 1024),
        ..Options::default()
    })
    .await
    else {
        return;
    };
    // Each /leak keeps 4 MiB alive in the worker.
    let first = h.get("/leak").await.json();
    let second = h.get("/leak").await.json();
    assert_eq!(first["pid"], second["pid"]);
    assert!(second["memory"].as_u64().unwrap() >= 8 * 1024 * 1024);
    // The second request crossed the limit: the next one gets a fresh worker.
    let third = h.get("/leak").await.json();
    assert_ne!(second["pid"], third["pid"]);
    assert!(third["memory"].as_u64().unwrap() < 8 * 1024 * 1024);
    h.shutdown().await;
}

#[tokio::test]
async fn recycled_after_max_age() {
    let Some(h) = start(Options {
        max_age: Some(Duration::from_millis(500)),
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let before = pid(&h).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_ne!(pid(&h).await, before);
    h.shutdown().await;
}

// --- Shutdown ---

#[tokio::test]
async fn idle_workers_exit_promptly_and_leave_nothing_behind() {
    let Some(h) = start(Options {
        count: 3,
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let pids = h.pool.worker_pids();
    assert_eq!(pids.len(), 3);
    let started = std::time::Instant::now();
    h.pool.shutdown(Duration::from_secs(5)).await;
    // Closing the connection ends the loop: no SIGKILL fallback needed.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_no_survivors(&pids);
    h.shutdown().await;
}

#[tokio::test]
async fn shutdown_lets_an_in_flight_request_finish() {
    let Some(h) = start(Options::default()).await else {
        return;
    };
    let addr = h.addr;
    let in_flight = tokio::spawn(async move { get_at(addr, "/slow").await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.shutdown().await;
    let res = in_flight.await.unwrap();
    assert_eq!(res.status, 200, "{}", res.text());
    assert_eq!(res.text(), "slow done\n");
}

// --- PSR-7 adapter ---

#[tokio::test]
async fn psr7_adapter_round_trip() {
    let vendor = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("php/worker/vendor/autoload.php");
    if !vendor.exists() {
        let required = std::env::var_os("RPHP_REQUIRE_PSR7").is_some_and(|v| v == "1");
        assert!(
            !required,
            "RPHP_REQUIRE_PSR7=1 but php/worker/vendor is missing"
        );
        eprintln!("skipping: run `composer install` in php/worker for the PSR-7 test");
        return;
    }
    let Some(h) = start(Options {
        script: "psr7-worker.php",
        ..Options::default()
    })
    .await
    else {
        return;
    };
    let res = h
        .request(
            "POST",
            "/psr?x=1",
            &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("X-Custom", "hi"),
                ("Cookie", "c=3"),
            ],
            b"a=b",
        )
        .await;
    assert_eq!(res.status, 202, "{}", res.text());
    assert_eq!(res.all("set-cookie"), vec!["p=1", "q=2"]);
    let j = res.json();
    assert_eq!(j["method"], "POST");
    assert_eq!(j["uri"], "http://fixture.test/psr?x=1");
    assert_eq!(j["query"]["x"], "1");
    assert_eq!(j["parsed"]["a"], "b");
    assert_eq!(j["cookies"]["c"], "3");
    assert_eq!(j["custom"], "hi");
    assert_eq!(j["body"], "a=b");
    assert_eq!(j["protocol"], "1.1");

    // A Host the PSR-7 factory rejects (port out of range) is an exception:
    // 500, and the worker is kept.
    let raw = b"GET /psr HTTP/1.1\r\nHost: a:99999\r\nConnection: close\r\n\r\n".to_vec();
    assert_eq!(h.send(raw).await.status, 500);
    assert_eq!(h.get("/psr").await.status, 202);
    h.shutdown().await;
}
