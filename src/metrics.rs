//! Prometheus metrics, rendered in the text exposition format (0.0.4).
//!
//! Plain atomics, no registry and no dependency: the set of series is fixed
//! and small, and every label has a bounded set of values (no paths, no
//! status codes beyond their class), so cardinality cannot grow with traffic.

use std::{
    fmt::Write,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use hyper::StatusCode;

use crate::backend::pool::PoolStats;

/// `Content-Type` of [`Metrics::render`]'s output.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Upper bounds of the latency histogram buckets, in seconds.
const BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Which part of RPHP answered a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handler {
    /// `/health` and `/metrics`.
    Builtin,
    Static,
    Php,
}

impl Handler {
    const ALL: [Self; 3] = [Self::Builtin, Self::Static, Self::Php];

    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Static => "static",
            Self::Php => "php",
        }
    }
}

/// A request that did not get a normal answer, by where it went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Body larger than `max_body_bytes` (413).
    BodyTooLarge,
    /// The request body could not be read (400).
    MalformedBody,
    /// The client did not send the body before the deadline (408).
    BodyTimeout,
    /// All workers busy and the queue full (503).
    Overloaded,
    /// The pool is shutting down (503).
    ShuttingDown,
    /// PHP did not answer before the deadline (504).
    PhpTimeout,
    /// PHP crashed or sent an invalid response (502).
    PhpFailed,
    /// The application threw before responding (500, persistent mode).
    PhpException,
    /// Request headers too large to hand to a persistent worker (431).
    HeadersTooLarge,
}

impl Failure {
    const ALL: [Self; 9] = [
        Self::BodyTooLarge,
        Self::MalformedBody,
        Self::BodyTimeout,
        Self::Overloaded,
        Self::ShuttingDown,
        Self::PhpTimeout,
        Self::PhpFailed,
        Self::PhpException,
        Self::HeadersTooLarge,
    ];

    /// `http`: the client's request; `server`: RPHP's capacity or state;
    /// `php`: PHP execution.
    fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::BodyTooLarge => ("http", "body_too_large"),
            Self::MalformedBody => ("http", "malformed_body"),
            Self::BodyTimeout => ("http", "body_timeout"),
            Self::Overloaded => ("server", "overloaded"),
            Self::ShuttingDown => ("server", "shutting_down"),
            Self::PhpTimeout => ("php", "timeout"),
            Self::PhpFailed => ("php", "failed"),
            Self::PhpException => ("php", "exception"),
            Self::HeadersTooLarge => ("http", "headers_too_large"),
        }
    }
}

#[derive(Default)]
struct Histogram {
    /// Non-cumulative; the last slot is `+Inf`.
    buckets: [AtomicU64; BUCKETS.len() + 1],
    sum_micros: AtomicU64,
}

impl Histogram {
    fn observe(&self, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        let slot = BUCKETS
            .iter()
            .position(|&le| secs <= le)
            .unwrap_or(BUCKETS.len());
        self.buckets[slot].fetch_add(1, Ordering::Relaxed);
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
    }
}

pub struct Metrics {
    started: Instant,
    /// Indexed by [`Handler`], then status class (1xx..5xx).
    requests: [[AtomicU64; 5]; 3],
    latency: [Histogram; 3],
    in_flight: AtomicUsize,
    failures: [AtomicU64; Failure::ALL.len()],
    connection_errors: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            requests: Default::default(),
            latency: Default::default(),
            in_flight: AtomicUsize::new(0),
            failures: Default::default(),
            connection_errors: AtomicU64::new(0),
        }
    }

    /// Count a request as in flight until the guard is dropped.
    pub fn start_request(&self) -> InFlight<'_> {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlight(&self.in_flight)
    }

    /// Record a finished request. `elapsed` runs until the response head is
    /// ready; a streamed body may take longer.
    pub fn observe(&self, handler: Handler, status: StatusCode, elapsed: Duration) {
        let h = handler as usize;
        let class = usize::from(status.as_u16() / 100).clamp(1, 5) - 1;
        self.requests[h][class].fetch_add(1, Ordering::Relaxed);
        self.latency[h].observe(elapsed);
    }

    pub fn failure(&self, failure: Failure) {
        self.failures[failure as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub fn connection_error(&self) {
        self.connection_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// The metrics in Prometheus text format. Worker series are present only
    /// with a PHP pool.
    pub fn render(&self, pool: Option<PoolStats>) -> String {
        let mut out = String::with_capacity(4096);
        let o = &mut out;

        family(o, "rphp_build_info", "gauge", "RPHP version.");
        sample(
            o,
            "rphp_build_info",
            &[("version", env!("CARGO_PKG_VERSION"))],
            1,
        );
        family(
            o,
            "rphp_uptime_seconds",
            "gauge",
            "Seconds since the server started.",
        );
        let _ = writeln!(
            o,
            "rphp_uptime_seconds {:.3}",
            self.started.elapsed().as_secs_f64()
        );

        family(
            o,
            "rphp_http_requests_total",
            "counter",
            "HTTP requests answered, by handler and status class.",
        );
        for handler in Handler::ALL {
            for (class, count) in self.requests[handler as usize].iter().enumerate() {
                let status = ["1xx", "2xx", "3xx", "4xx", "5xx"][class];
                sample(
                    o,
                    "rphp_http_requests_total",
                    &[("handler", handler.label()), ("status", status)],
                    count.load(Ordering::Relaxed),
                );
            }
        }

        family(
            o,
            "rphp_http_request_duration_seconds",
            "histogram",
            "Time until the response head was ready, by handler.",
        );
        for handler in Handler::ALL {
            let hist = &self.latency[handler as usize];
            let mut cumulative = 0;
            for (i, bucket) in hist.buckets.iter().enumerate() {
                cumulative += bucket.load(Ordering::Relaxed);
                let le = BUCKETS.get(i).map_or("+Inf".to_string(), f64::to_string);
                sample(
                    o,
                    "rphp_http_request_duration_seconds_bucket",
                    &[("handler", handler.label()), ("le", &le)],
                    cumulative,
                );
            }
            let sum = hist.sum_micros.load(Ordering::Relaxed) as f64 / 1e6;
            let _ = writeln!(
                o,
                "rphp_http_request_duration_seconds_sum{{handler=\"{}\"}} {sum}",
                handler.label()
            );
            sample(
                o,
                "rphp_http_request_duration_seconds_count",
                &[("handler", handler.label())],
                cumulative,
            );
        }

        family(
            o,
            "rphp_http_requests_in_flight",
            "gauge",
            "HTTP requests being handled.",
        );
        sample(
            o,
            "rphp_http_requests_in_flight",
            &[],
            self.in_flight.load(Ordering::Relaxed) as u64,
        );

        family(
            o,
            "rphp_http_connection_errors_total",
            "counter",
            "HTTP connections that ended with a protocol or I/O error.",
        );
        sample(
            o,
            "rphp_http_connection_errors_total",
            &[],
            self.connection_errors.load(Ordering::Relaxed),
        );

        family(
            o,
            "rphp_request_failures_total",
            "counter",
            "Requests that failed, by source (http: the client's request, \
             server: RPHP capacity or state, php: PHP execution) and kind.",
        );
        for failure in Failure::ALL {
            let (source, kind) = failure.labels();
            sample(
                o,
                "rphp_request_failures_total",
                &[("source", source), ("kind", kind)],
                self.failures[failure as usize].load(Ordering::Relaxed),
            );
        }

        if let Some(pool) = pool {
            render_pool(o, pool);
        }
        out
    }
}

fn render_pool(o: &mut String, pool: PoolStats) {
    family(
        o,
        "rphp_php_workers",
        "gauge",
        "PHP workers by state (starting: being stopped or respawned).",
    );
    for (state, n) in [
        ("idle", pool.idle),
        ("busy", pool.busy),
        ("starting", pool.starting()),
    ] {
        sample(o, "rphp_php_workers", &[("state", state)], n as u64);
    }
    family(
        o,
        "rphp_php_workers_configured",
        "gauge",
        "Configured number of PHP workers (worker.count).",
    );
    sample(
        o,
        "rphp_php_workers_configured",
        &[],
        pool.configured as u64,
    );
    family(
        o,
        "rphp_php_queue_depth",
        "gauge",
        "Requests waiting for a free PHP worker.",
    );
    sample(o, "rphp_php_queue_depth", &[], pool.waiting as u64);
    family(
        o,
        "rphp_php_worker_restarts_total",
        "counter",
        "PHP workers replaced, by reason (recycled: reached max_requests, \
         max_age_secs or max_memory_mb; \
         failed: error, timeout or crash).",
    );
    sample(
        o,
        "rphp_php_worker_restarts_total",
        &[("reason", "recycled")],
        pool.recycled,
    );
    sample(
        o,
        "rphp_php_worker_restarts_total",
        &[("reason", "failed")],
        pool.failed,
    );
    family(
        o,
        "rphp_php_worker_spawn_failures_total",
        "counter",
        "Failed attempts to start a replacement PHP worker.",
    );
    sample(
        o,
        "rphp_php_worker_spawn_failures_total",
        &[],
        pool.spawn_failures,
    );
}

/// Decrements the in-flight gauge on drop.
pub struct InFlight<'a>(&'a AtomicUsize);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn family(o: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(o, "# HELP {name} {help}");
    let _ = writeln!(o, "# TYPE {name} {kind}");
}

/// One sample. Label values are internal constants, so no escaping is needed.
fn sample(o: &mut String, name: &str, labels: &[(&str, &str)], value: u64) {
    o.push_str(name);
    if !labels.is_empty() {
        o.push('{');
        for (i, (k, v)) in labels.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            let _ = write!(o, "{k}=\"{v}\"");
        }
        o.push('}');
    }
    let _ = writeln!(o, " {value}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line<'a>(text: &'a str, prefix: &str) -> &'a str {
        text.lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starting with {prefix:?} in\n{text}"))
    }

    #[test]
    fn counts_requests_by_handler_and_status_class() {
        let m = Metrics::new();
        m.observe(Handler::Php, StatusCode::OK, Duration::from_millis(3));
        m.observe(Handler::Php, StatusCode::CREATED, Duration::from_millis(3));
        m.observe(
            Handler::Php,
            StatusCode::BAD_GATEWAY,
            Duration::from_millis(3),
        );
        m.observe(Handler::Static, StatusCode::NOT_MODIFIED, Duration::ZERO);
        let text = m.render(None);
        assert_eq!(
            line(
                &text,
                r#"rphp_http_requests_total{handler="php",status="2xx"}"#
            ),
            r#"rphp_http_requests_total{handler="php",status="2xx"} 2"#
        );
        assert!(text.contains(r#"rphp_http_requests_total{handler="php",status="5xx"} 1"#));
        assert!(text.contains(r#"rphp_http_requests_total{handler="static",status="3xx"} 1"#));
        assert!(text.contains(r#"rphp_http_requests_total{handler="builtin",status="2xx"} 0"#));
        assert!(
            !text.contains("rphp_php_workers"),
            "no pool, no worker series"
        );
    }

    #[test]
    fn histogram_buckets_are_cumulative() {
        let m = Metrics::new();
        for ms in [1, 20, 20, 700, 60_000] {
            m.observe(Handler::Php, StatusCode::OK, Duration::from_millis(ms));
        }
        let text = m.render(None);
        let php = |le: &str| {
            let prefix =
                format!(r#"rphp_http_request_duration_seconds_bucket{{handler="php",le="{le}"}} "#);
            line(&text, &prefix)[prefix.len()..].parse::<u64>().unwrap()
        };
        assert_eq!(php("0.005"), 1);
        assert_eq!(php("0.025"), 3);
        assert_eq!(php("0.5"), 3);
        assert_eq!(php("1"), 4);
        assert_eq!(php("10"), 4);
        assert_eq!(php("+Inf"), 5);
        assert!(text.contains(r#"rphp_http_request_duration_seconds_count{handler="php"} 5"#));
        assert!(text.contains(r#"rphp_http_request_duration_seconds_sum{handler="php"} 60.741"#));
    }

    #[test]
    fn in_flight_gauge_follows_the_guard() {
        let m = Metrics::new();
        let guard = m.start_request();
        assert!(m.render(None).contains("rphp_http_requests_in_flight 1\n"));
        drop(guard);
        assert!(m.render(None).contains("rphp_http_requests_in_flight 0\n"));
    }

    #[test]
    fn failures_carry_their_source() {
        let m = Metrics::new();
        m.failure(Failure::PhpTimeout);
        m.failure(Failure::Overloaded);
        m.failure(Failure::Overloaded);
        let text = m.render(None);
        assert!(text.contains(r#"rphp_request_failures_total{source="php",kind="timeout"} 1"#));
        assert!(
            text.contains(r#"rphp_request_failures_total{source="server",kind="overloaded"} 2"#)
        );
        assert!(
            text.contains(r#"rphp_request_failures_total{source="http",kind="body_too_large"} 0"#)
        );
    }

    #[test]
    fn pool_series() {
        let text = Metrics::new().render(Some(PoolStats {
            configured: 4,
            idle: 1,
            busy: 2,
            waiting: 5,
            recycled: 7,
            failed: 3,
            spawn_failures: 1,
            closed: false,
        }));
        for expected in [
            r#"rphp_php_workers{state="idle"} 1"#,
            r#"rphp_php_workers{state="busy"} 2"#,
            r#"rphp_php_workers{state="starting"} 1"#,
            "rphp_php_workers_configured 4",
            "rphp_php_queue_depth 5",
            r#"rphp_php_worker_restarts_total{reason="recycled"} 7"#,
            r#"rphp_php_worker_restarts_total{reason="failed"} 3"#,
            "rphp_php_worker_spawn_failures_total 1",
        ] {
            assert!(
                text.contains(&format!("{expected}\n")),
                "{expected}\n{text}"
            );
        }
    }

    #[test]
    fn every_family_has_help_and_type() {
        let text = Metrics::new().render(Some(PoolStats {
            configured: 1,
            idle: 1,
            busy: 0,
            waiting: 0,
            recycled: 0,
            failed: 0,
            spawn_failures: 0,
            closed: false,
        }));
        for l in text.lines().filter(|l| !l.starts_with('#')) {
            let name = l.split(['{', ' ']).next().unwrap();
            let family = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|s| name.strip_suffix(s))
                .filter(|base| text.contains(&format!("# TYPE {base} histogram")))
                .unwrap_or(name);
            assert!(
                text.contains(&format!("# TYPE {family} ")),
                "no TYPE for {family}"
            );
        }
    }
}
