# RPHP Implementation Plan

This plan turns [`roadmap.md`](roadmap.md) into concrete, testable work. Each
phase ends with an exit gate: the phase is done when the gate's tests pass in
CI, not when the code exists. Estimates assume one developer and are not
commitments.

## Key decisions

These were decided before writing code because they shape everything after.

| # | Decision | Why |
|---|----------|-----|
| D1 | **Classic mode talks FastCGI to `php-cgi`**, not `php` (CLI) over stdin/stdout. | The CLI SAPI does not populate HTTP superglobals, does not emit `header()` / `setcookie()` output (`headers_list()` is empty under CLI), and has no `php://input` or multipart handling. `php-cgi` gives correct HTTP semantics from PHP itself, and FastCGI already provides framing, request IDs, and separate stdout/stderr streams. |
| D2 | **`php-cgi` processes are pooled and reused**, recycled after `worker.max_requests` (passed as `PHP_FCGI_MAX_REQUESTS`). | Spawning a process per request would be slower than PHP-FPM. Request isolation is still provided by the CGI SAPI, which resets request state between requests. |
| D3 | **No custom IPC protocol until persistent mode.** | FastCGI covers classic mode. The draft in `worker-protocol.md` is only needed for the persistent worker, and is designed then, with real requirements. |
| D4 | **Persistent mode goal: superglobal-compatible**, reached in two steps: a PSR-7 worker loop first (phase 5), then an embedded `libphp` SAPI as a research track (phase 8). | A PSR-7 loop is achievable early but excludes apps that rely on superglobals (e.g. WordPress). Embedding is what makes broad compatibility possible, but it needs FFI, ZTS builds and much more risk. |
| D5 | **PHP 8.4 minimum is policy, not configuration.** `php.min_version` was removed; unknown config keys are rejected. | A configurable minimum would let users silently run unsupported versions. |
| D6 | **Linux first**, single crate with modules, Rust 1.85+ (edition 2024). | Keeps the surface small until boundaries are proven. |

## Positioning

FrankenPHP and RoadRunner already exist and are supported by Laravel Octane.
RPHP must state what it offers in return. Working hypothesis, to be confirmed
or dropped by phase 7 benchmarks:

* lower memory footprint and no GC pauses in the server process;
* first-class integration with the Rust ecosystem (Tower middleware, rustls);
* a small, auditable codebase with strict defaults.

If benchmarks do not support the first point, the README must say so.

## Phase 0 — Baseline (done in this revision)

- [x] Fix scaffold defects: truncated MIT license, non-graceful shutdown,
      accept errors that killed the server, empty `repository` key, missing
      MSRV.
- [x] Record decisions D1–D6 in SPEC and architecture docs.
- [ ] Native PHP fixture and PHP-FPM baseline numbers (moved to phase 7 tooling,
      but record a first rough baseline now to sanity-check later results).

## Phase 1 — Startup and HTTP shell (done in this revision)

Deliverables:

- [x] `rphp serve | check | version | help`, `--config <path>`.
- [x] `rphp.toml` loading with defaults, `deny_unknown_fields`, all validation
      errors reported at once, relative paths resolved against the config file.
- [x] Document root and entry point validation: relative, `.php`, no `..`,
      must stay inside the root after symlink resolution.
- [x] PHP discovery on `PATH` without a shell; `<php> -v` probe with timeout and
      `kill_on_drop`; `PHP_VERSION_ID >= 80400` enforced; SAPI reported.
- [x] Server binds only after config and PHP pass.
- [x] Graceful shutdown on Ctrl+C and SIGTERM with a bounded drain timeout.
- [x] Accept loop survives per-connection errors and backs off on `EMFILE`.
- [x] Header read timeout (slowloris guard).
- [x] `/health` supports GET and HEAD; other methods get 405 with `Allow`.

Exit gate:

- [x] `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` green in
      CI on Linux with PHP 8.4.
- [x] `rphp check` rejects PHP 8.3 and accepts PHP 8.4 (covered by
      `tests/php_probe.rs` with stand-in binaries).

## Phase 2 — Classic execution over FastCGI (implemented; exit gate pending CI)

Module: `src/fastcgi/` (protocol codec), `src/backend/classic.rs` (pool).

1. **FastCGI codec.** Records (`BEGIN_REQUEST`, `PARAMS`, `STDIN`, `STDOUT`,
   `STDERR`, `END_REQUEST`), name-value pair encoding, 64 KiB record limit,
   padding. Property tests for encode/decode round-trips and malformed input.
   Treat all bytes from PHP as untrusted: validate lengths before allocating.
2. **Process pool.** Spawn `worker.count` `php-cgi -b <unix socket>` processes
   (or one socket per process), with `PHP_FCGI_CHILDREN=0` and
   `PHP_FCGI_MAX_REQUESTS=<max_requests>`. Use a cleared environment plus an
   explicit allowlist. Respawn on exit with exponential backoff.
3. **Request mapping.** `SCRIPT_FILENAME` (always the configured entry point
   for front-controller apps), `REQUEST_METHOD`, `REQUEST_URI`,
   `QUERY_STRING`, `CONTENT_TYPE`, `CONTENT_LENGTH`, `SERVER_*`,
   `REMOTE_ADDR`, `HTTPS`, and `HTTP_*` headers. Drop the `Proxy` header
   (httpoxy) and drop header names containing `_`, so `X-Foo_Bar` cannot
   spoof `X-Foo-Bar` once both become `HTTP_X_FOO_BAR`.
4. **Response mapping.** Parse CGI headers from `STDOUT` (`Status:`,
   `Location:`, multiple `Set-Cookie`), stream the body, forward `STDERR` to
   logs at warn level without request bodies.
5. **Limits.** `max_body_bytes` (413), `request_timeout_ms` (504 and kill or
   recycle the process), cap on response header size (502).
6. **Static files.** Serve existing files under `document_root` directly;
   never pass `.php` files other than the entry point unless configured.
7. Make a non-`cgi-fcgi` SAPI a startup error (currently a warning).

Exit gate: fixture `tests/fixtures/native/` exercising method, query,
headers, body, status, redirects, multiple cookies, uploads, `php://input`,
and a script that exceeds the timeout. All pass.

Implementation notes:

- One Unix-socket connection per request, no `FCGI_KEEP_CONN`; sockets live in
  a private `0700` directory under the temp dir.
- RPHP counts requests per worker and respawns it after `max_requests`
  (php-cgi exits by itself at the same count). Errors and timeouts kill and
  replace the worker. Respawn failures back off exponentially up to 30s.
- Request bodies are buffered (bounded by `max_body_bytes`) so that
  `CONTENT_LENGTH` is exact, including for chunked uploads. Responses stream.
- The request deadline (`request_timeout_ms`) covers reading the body,
  waiting for a worker and the whole PHP exchange. Before the response head:
  504. After it: the stream is aborted.
- Response head limit: 64 KiB (502 beyond that).
- Static files: regular files inside the document root after symlink
  resolution; never PHP source extensions or hidden files (except
  `.well-known`). Everything else goes to the entry point.
- Known gaps, deferred: no pass-through of application environment variables
  (cleared environment only), no ETag or Range for static files, `/health`
  shadows an application route of the same path.

## Phase 3 — Supervision and backpressure (implemented; exit gate pending CI)

- Bounded queue (`max_queue_size`); 503 with `Retry-After` when full.
- Acquire-a-worker timeout counts against the request deadline.
- One request per process at a time (FastCGI multiplexing off).
- Crash detection mid-request returns 502 and the process is replaced.
- Shutdown order: stop accepting → drain HTTP → stop PHP processes (SIGTERM,
  then SIGKILL after timeout).
- Failure-injection tests: `kill -9` a worker mid-request, PHP fatal error,
  `exit()` in user code, infinite loop, huge output.

Exit gate: failure-injection suite green; no orphaned `php-cgi` processes after
any test (checked by the test harness).

Implementation notes:

- `max_queue_size` bounds requests *waiting* for a worker (running requests
  are bounded by `count`). A full queue answers `503` with `Retry-After: 1`,
  checked once before the body is read and again, binding, at checkout.
- A checked-out worker is held by a lease; dropping it without finishing (a
  cancelled request, an aborted task) replaces the worker, so the pool never
  shrinks.
- A worker that died while idle, or whose socket refuses connections, is
  replaced at checkout and the request moves to the next worker: nothing was
  sent to PHP yet, so the retry is safe. A crash mid-request is a 502.
- Shutdown: HTTP drain (`shutdown_timeout_ms`), then SIGTERM to every worker
  (php-cgi finishes its current request and exits), then SIGKILL after the
  same timeout.
- On Linux, workers get `PR_SET_PDEATHSIG=SIGKILL`, so they also die if RPHP
  itself is killed with SIGKILL.
- Not done: no per-worker memory limit. Do not add one with
  `RLIMIT_AS`/`RLIMIT_DATA`: allocation failures reach the PHP2Shell
  (CVE-2026-109618) code path.

## Phase 4 — Observability (implemented; exit gate pending CI)

- Structured JSON logs (`RPHP_LOG_FORMAT=json`), request ID propagated to PHP
  as `HTTP_X_REQUEST_ID`.
- `/metrics` (Prometheus text): requests, latency histogram, active and idle
  workers, queue depth, restarts, errors by class (HTTP vs PHP).
- `/health` reports degraded when no worker is available. Health and metrics
  move to a separate admin listener before any non-loopback deployment.

Exit gate: request ID visible in PHP and in the response; metrics reflect
worker states, recycling, failures and request outcomes against real
`php-cgi`; `/health` is 503 without workers.

Implementation notes:

- `RPHP_LOG_FORMAT=text|json` (default text; anything else is a startup
  error). JSON is one object per line, event fields flattened, plus the
  current span, which carries `request_id`. PHP stderr from a FastCGI
  request is logged inside that span.
- Access log: one event per request, target `rphp::access`, with method,
  path (never the query string: it often carries tokens), status,
  `duration_ms` and handler. `/health` and `/metrics` log at debug.
- Request ID: a client-supplied `X-Request-Id` is kept if it is a single
  value of at most 128 characters from `[A-Za-z0-9._:/+=-]`; otherwise a new
  one is generated (random per-process prefix plus a counter, no new
  dependency). It replaces the request header, so PHP sees the same value,
  and is returned on every response.
- Metrics are rendered by hand from atomics (no `prometheus` crate). Labels
  are bounded: `handler` (`builtin`, `static`, `php`), status class, failure
  `source` (`http`, `server`, `php`) and `kind`. Latency runs until the
  response head is ready, not until a streamed body ends.
- Health: 200 while at least one worker is idle or busy (all busy is not
  unhealthy; requests queue), 503 when every worker is being respawned or
  the pool is shutting down.
- `[admin] enabled = true` serves `/health` and `/metrics` on their own
  listener (default `127.0.0.1:9180`) and hands those paths on the main
  listener to the application, closing the phase 2 gap. Without it, a
  non-loopback `server.host` produces a startup warning.
- Not done: per-worker metrics (by worker ID), OpenTelemetry/trace context
  (`traceparent`) propagation, log sampling.

## Phase 5 — Persistent worker prototype, PSR-7 (3 weeks)

- Finalise `worker-protocol.md` (framed, versioned, bounded) and implement it
  over a Unix socket.
- Composer package `rphp/worker` (PHP side): loop, PSR-7 request building,
  response emission, per-request `gc_collect_cycles()` and reset hooks.
- Recycling by request count, age and memory (`memory_get_usage`).
- State-leak test suite: static properties, globals, singletons, sessions,
  output buffers, `register_shutdown_function`.
- Remains opt-in and marked experimental.

## Phase 6 — Framework fixtures (2 weeks)

Native, Slim, Symfony, Laravel, CodeIgniter in classic mode; Slim, Symfony
and Laravel in persistent mode; WordPress smoke test in classic mode. Each
fixture is a CI job with a pinned lockfile. Results go into
`docs/compatibility.md`.

## Phase 7 — Benchmarks and hardening (2 weeks)

- Scripted, reproducible runs (`bench/`) against PHP-FPM + Nginx and
  FrankenPHP, at several concurrency levels; p50/p95/p99, throughput, CPU,
  RSS. Publish hardware, versions and raw results.
- Fuzz the FastCGI and worker protocol decoders (`cargo fuzz`).
- Security review: environment handling, path traversal, header injection,
  resource limits. Document secret handling.

## Phase 8 — Research: embedded PHP SAPI (open-ended)

Prototype a custom SAPI over `libphp` (ZTS) via FFI to get superglobal
compatibility in persistent mode, as FrankenPHP does from Go. Go/no-go
decided on stability and benchmark data; not on the MVP path.

## MVP definition

The MVP is phases 1–4 plus the native and Slim fixtures from phase 6 and the
first benchmark run from phase 7. It matches the acceptance criteria in
`SPEC.md` §7.

## Working agreements

- Every phase lands behind tests; CI runs on PHP 8.4 and 8.5.
- No performance claim in docs without a linked benchmark run.
- Breaking config changes are listed in `CHANGELOG.md` until 1.0.
