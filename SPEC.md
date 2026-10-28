# RPHP Runtime Specification v0.2

**Status:** Draft
**Project type:** Rust-powered PHP application server
**Minimum PHP version:** 8.4 (fixed; not configurable)

Changes from v0.1: classic mode uses FastCGI to `php-cgi` instead of the PHP
CLI; `php.min_version` removed; `worker.max_requests` now applies to classic
mode; positioning section added; persistent-mode strategy stated.

## 1. Vision

RPHP is a general-purpose PHP application server implemented in Rust. It
provides HTTP serving, PHP process management, request/response adaptation,
worker supervision, observability, and deployment ergonomics without being
tied to one PHP framework.

RPHP is not a new PHP language implementation. It uses the official PHP
interpreter as its execution engine.

## 2. Positioning

Established PHP application servers already exist (FrankenPHP and RoadRunner,
both written in Go, both supported by Laravel Octane). RPHP aims to offer:

- a lower memory footprint and no garbage-collector pauses in the server
  process;
- integration with the Rust ecosystem (e.g. Tower middleware, rustls);
- a small, auditable codebase with strict, secure defaults.

These are hypotheses until benchmarks (§7, criterion 8) support them. If they
do not, the documentation must say so.

## 3. Goals

- Require PHP 8.4 or newer.
- Run native PHP applications and common PHP frameworks through a generic
  entry-point model.
- Keep HTTP networking separate from PHP execution.
- Provide a compatibility-oriented classic execution mode.
- Provide an opt-in persistent-worker mode after lifecycle and isolation
  behavior are tested.
- Manage worker health, timeouts, restarts, graceful shutdown, and configurable
  limits.
- Provide repeatable benchmarks against PHP-FPM and existing PHP application
  servers.
- Offer clear startup errors when PHP or required extensions are unavailable.

## 4. Non-goals for the initial MVP

- Implementing a PHP parser, compiler, or interpreter from scratch.
- Guaranteeing compatibility with every PHP application, extension, SAPI
  assumption, or web-server configuration.
- Replacing all Caddy, Nginx, or Apache features in the first release.
- Claiming performance superiority before reproducible benchmarks exist.
- Embedding the PHP engine into the Rust process in the MVP (research track
  only; see `docs/implementation-plan.md`, phase 8).

## 5. Compatibility policy

### PHP versions

- Minimum supported interpreter: PHP 8.4 (`PHP_VERSION_ID >= 80400`).
- The minimum is project policy and cannot be lowered by configuration.
- Older versions are rejected at startup with an actionable message.
- Newer PHP versions are CI and compatibility-testing targets.
- The PHP executable is configurable (name on `PATH` or path).

### Frameworks and application styles

Planned compatibility test targets, not a claim of current support:

- Native PHP front-controller applications
- Laravel
- Symfony
- CodeIgniter
- Slim
- WordPress

Compatibility is established through tests, not framework names alone.
Applications that depend on a specific SAPI, `.htaccess`, server modules, or
unsupported extensions may require configuration or remain unsupported.

### Execution modes

**Classic mode (default)**
- RPHP speaks FastCGI to a pool of `php-cgi` processes.
- The CGI SAPI provides standard request semantics: superglobals,
  `header()`, `setcookie()`, `php://input`, uploads, sessions.
- Request state is reset by PHP between requests; processes are recycled
  after `worker.max_requests` requests (`PHP_FCGI_MAX_REQUESTS`).
- Framework bootstrap still runs on every request, as with PHP-FPM.

Why not the PHP CLI: the CLI SAPI does not populate HTTP superglobals and
does not emit headers set by `header()` or `setcookie()`. Emulating a web SAPI
in userland would be a permanent source of compatibility bugs.

**Persistent-worker mode (experimental, opt-in)**
- PHP worker processes boot the application once and handle many requests.
- First implementation: a PHP-side PSR-7 worker loop. Apps must use an
  adapter and must not rely on superglobals.
- Superglobal-compatible persistent mode requires an embedded SAPI and is a
  research track, not an MVP commitment.
- Requires request-state isolation, resource cleanup, exception handling and
  memory-growth tests before it can be recommended.
- Worker recycling, timeouts, crash recovery, and bounded concurrency are
  required.
- Rejected by configuration validation until it is implemented.

## 6. Functional requirements

### Startup and configuration
- Parse configuration with useful validation errors; reject unknown keys;
  report all problems at once.
- Resolve relative paths against the configuration file's directory.
- Resolve the configured PHP binary without invoking a shell.
- Verify `PHP_VERSION_ID >= 80400`.
- Report PHP version, SAPI and executable path in diagnostics.
- Validate document root and entry point before accepting traffic. The entry
  point must be a relative `.php` path that stays inside the document root,
  including after symlink resolution.
- Bind the listener only after all checks pass.

### HTTP behavior
- Correctly handle request method, path, query string, headers, body, and
  content type.
- Preserve response status, headers, multiple cookies, and body.
- Enforce request body size limits and a header read timeout.
- Handle malformed requests safely.
- Avoid logging secrets or request bodies by default.

### Process and worker management
- Enforce request deadlines and worker limits.
- Bound queues and apply backpressure (503 when full).
- Recover from worker crashes.
- Graceful shutdown on SIGINT and SIGTERM: stop accepting, drain in-flight
  requests up to a timeout, then stop PHP processes.
- Recycle workers by request count (and, in persistent mode, by age and
  memory).
- A worker processes one request at a time unless its execution model
  explicitly supports concurrency safely.

### Observability
- Structured logs.
- Health endpoint (`GET` and `HEAD /health`).
- Metrics for request count, latency, active workers, queue depth, restarts,
  and errors.
- Diagnostics that distinguish HTTP server failures from PHP execution
  failures.

### Security
- Bind to loopback by default; warn when binding elsewhere.
- Never execute arbitrary filesystem paths; only the configured entry point
  (and explicitly configured scripts) run as PHP.
- Apply configurable request, body, header and time limits.
- Pass arguments directly to process APIs; never build shell strings.
- Start PHP with a cleared environment plus an explicit allowlist.
- Drop the `Proxy` request header (httpoxy).
- Treat FastCGI and worker IPC input as untrusted; validate lengths and fields
  before allocation.

## 7. Initial acceptance criteria

The MVP is not complete until:
1. Startup rejects PHP versions below 8.4. *(done)*
2. The configured PHP binary is resolved and reported. *(done)*
3. `/health` returns a successful response while the server is healthy.
   *(done; worker-aware health in phase 4)*
4. A native PHP front-controller fixture serves requests through `php-cgi`.
5. HTTP method, query, headers, body, status, cookies, and response headers
   are tested.
6. Worker timeout and crash behavior are tested; no orphaned processes.
7. Persistent mode cannot be enabled by default before isolation tests pass.
   *(enforced: currently rejected)*
8. Benchmark comparisons include PHP-FPM and FrankenPHP, with published
   methodology.
9. Known compatibility limitations are documented.

## 8. CLI

```text
rphp serve [--config rphp.toml]   validate, then serve
rphp check [--config rphp.toml]   validate and print a report
rphp version
rphp help
```

Without `--config`, `./rphp.toml` is used when present, otherwise built-in
defaults.

## 9. Configuration

See [`rphp.example.toml`](rphp.example.toml) for every key and its default.

```toml
[server]
host = "127.0.0.1"
port = 8080
document_root = "public"
entry_point = "index.php"
max_body_bytes = 8388608
shutdown_timeout_ms = 10000
header_read_timeout_ms = 30000

[php]
binary = "php-cgi"

[worker]
mode = "classic"
count = 4
max_requests = 1000
request_timeout_ms = 30000
max_queue_size = 256
```

`worker.max_requests` recycles PHP processes in both modes. `worker.mode =
"persistent"` is rejected until roadmap phase 5.
