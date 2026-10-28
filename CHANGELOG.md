# Changelog

## Unreleased

### Added
- Observability (phase 4): `RPHP_LOG_FORMAT=json` for one JSON object per
  log line; an access log (`rphp::access`) without query strings; request
  IDs from `X-Request-Id` (validated) or generated, returned on every
  response, passed to PHP as `HTTP_X_REQUEST_ID` and attached to every log
  line of the request, including PHP stderr.
- `GET /metrics` in Prometheus text format: requests by handler and status
  class, latency histogram, in-flight requests, connection errors, failures
  by source (`http`, `server`, `php`), PHP worker states, queue depth,
  restarts by reason and spawn failures.
- `[admin]` section: `enabled`, `host`, `port` (default `127.0.0.1:9180`).
  When enabled, `/health` and `/metrics` are served only there and the
  application receives those paths. `rphp check` reports it, and a
  non-loopback `server.host` without it produces a warning.
- Backpressure: `worker.max_queue_size` bounds requests waiting for a worker;
  beyond it RPHP answers `503` with `Retry-After: 1`.
- Supervision: workers that crash (mid-request: 502) or die while idle
  (transparent retry on another worker) are replaced; a cancelled request
  can no longer shrink the pool.
- Ordered shutdown: HTTP drain, then SIGTERM to PHP workers, then SIGKILL
  after `server.shutdown_timeout_ms`. On Linux, workers also die if RPHP is
  killed (`PR_SET_PDEATHSIG`).
- Failure-injection tests (`kill -9` mid-request and while idle, `exit()`,
  infinite loop, 32 MiB output, full queue, shutdown) with a harness check
  that no `php-cgi` process survives.
- PHP releases with a known critical vulnerability are rejected at startup
  (`rphp check` and `rphp serve`) with the CVE IDs, the fixed release and a
  link. First entry: PHP2Shell (CVE-2026-109618, CVE-2026-109619), fixed in
  PHP 8.4.27 and 8.5.12. The list lives in `src/php/advisory.rs`.
- Classic mode: requests run through a pool of `php-cgi` FastCGI workers
  (`worker.count`), recycled after `worker.max_requests` and replaced after
  errors or timeouts. PHP starts with a cleared environment plus an allowlist.
- FastCGI codec, CGI/1.1 request mapping (httpoxy guard, `_` header names
  dropped) and response mapping (status, redirects, multiple cookies,
  streaming bodies, PHP stderr logged at warn).
- Limits: `server.max_body_bytes` (413), `worker.request_timeout_ms` (504),
  64 KiB response header cap (502).
- Static files under `document_root` are served directly; PHP sources and
  hidden files never are.
- End-to-end tests against real `php-cgi` (`tests/classic.rs`, fixture in
  `tests/fixtures/native/`).
- `rphp serve`, `check`, `version`, `help` commands with `--config`.
- `rphp.toml` loading with defaults, strict unknown-key rejection, and
  validation of host, port, limits, document root and entry point.
- PHP discovery on `PATH` (no shell) and enforcement of PHP 8.4+.
- Graceful shutdown on Ctrl+C and SIGTERM with a drain timeout.
- Header read timeout; `HEAD /health`; `405` with `Allow` for other methods.
- Unit and integration tests, CI workflow, `rphp.example.toml`.

### Changed
- `/health` answers `503` when no PHP worker is running (all being
  respawned) or the pool is shutting down.
- An invalid `RPHP_LOG_FORMAT` is a startup error (exit code 2).
- A PHP binary without the FastCGI SAPI is now a startup error (was a
  warning).
- `HEAD /health` reports the same `Content-Length` as `GET`.
- Classic mode targets FastCGI to `php-cgi` instead of the PHP CLI.
- Default `php.binary` is `php-cgi`.
- `worker.max_requests` applies to classic mode (process recycling).

### Removed
- `php.min_version`. The PHP 8.4 minimum is fixed and not configurable.

### Fixed
- Flaky PHP probe tests (`ETXTBSY` when another test forked while a stand-in
  binary was being written).
- LICENSE was missing the MIT liability paragraph.
- Accept errors (e.g. too many open files) no longer stop the server.
- Shutdown no longer drops in-flight connections immediately.
