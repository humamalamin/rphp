# RPHP Runtime

Rust-powered PHP application server. RPHP aims to run PHP applications and
frameworks through a generic runtime interface, with PHP 8.4 as the minimum
supported interpreter version.

## Status

Phase 5 of the [roadmap](docs/roadmap.md). Implemented:

- `rphp.toml` configuration with validation (unknown keys rejected, all
  problems reported at once, paths checked before startup)
- PHP discovery and enforcement of PHP 8.4+ (`PHP_VERSION_ID >= 80400`)
- HTTP server with `/health`, graceful shutdown (Ctrl+C / SIGTERM) and a
  header read timeout
- Classic mode: every request that is not a static file runs the configured
  entry point through a pool of `php-cgi` FastCGI workers
- Static files from `document_root`, request body limit, request timeout
- Worker supervision: bounded wait queue (503 + `Retry-After`), crashed
  workers replaced, ordered shutdown (SIGTERM, then SIGKILL), no orphaned
  `php-cgi` processes
- Observability: text or JSON logs with an access log, request IDs
  (`X-Request-Id`, also visible to PHP), Prometheus `/metrics`, a `/health`
  that reflects worker availability, and an optional admin listener

- Persistent mode (**experimental**, opt-in): PHP CLI workers that boot the
  application once and serve many requests, with the
  [`rphp/worker`](php/worker/README.md) loop and a PSR-7 adapter. Read
  [docs/persistent-mode.md](docs/persistent-mode.md) before using it.

**Not implemented yet:** framework compatibility testing, benchmarks and
production hardening.

## Requirements

- Rust 1.85 or newer (edition 2024)
- PHP 8.4 or newer, with the `php-cgi` binary (classic mode uses FastCGI).
  On Debian/Ubuntu: `apt install php8.4-cgi`.
- A PHP release without known critical vulnerabilities: at least 8.4.27 or
  8.5.12 (PHP2Shell, CVE-2026-109618/109619). Older patch releases are
  refused at startup.
- Linux (other platforms are untested)

## Run

```bash
cargo run -- check   # validate config and PHP, print a report
cargo run -- serve   # validate, then start the server
```

The server refuses to start if the configuration is invalid or PHP is older
than 8.4. By default it listens on `127.0.0.1:8080` and serves the
`public/` directory next to the config file.

```bash
curl -i http://127.0.0.1:8080/health
```

`/health` answers `200 ok` while at least one PHP worker is running (idle or
busy) and `503` when none is (all being respawned, or shutting down).
`/metrics` serves Prometheus metrics: requests by handler and status class,
latency histogram, in-flight requests, failures by source (`http`, `server`,
`php`), worker states, queue depth, restarts and spawn failures. With
`[admin] enabled = true`, both move to a separate listener (default
`127.0.0.1:9180`) and the application receives those paths instead; do this
before binding the main listener to a public address. Any other path is served from `public/` if it is a static file, and
by `public/index.php` otherwise.

## Configuration

Copy [`rphp.example.toml`](rphp.example.toml) to `rphp.toml` and adjust, or
pass a path with `--config`. Every key is optional.

Logging: `RUST_LOG` sets verbosity (default `info`), `RPHP_LOG_FORMAT=json`
switches to one JSON object per line. Every request gets an ID: the
client's `X-Request-Id` if it is short and plain, a generated one otherwise.
It is returned in the response, passed to PHP as
`$_SERVER['HTTP_X_REQUEST_ID']`, and attached to every log line of that
request, including PHP's stderr. Query strings and bodies are never logged.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The PHP discovery tests use stand-in executables, so they pass without PHP
installed and can simulate PHP 8.3 and 8.4. The end-to-end tests
(`tests/classic.rs`, `tests/persistent.rs`) need `php-cgi` and the PHP CLI
8.4+ on `PATH`; they skip without them unless `RPHP_REQUIRE_PHP=1` is set (as
in CI). The PSR-7 test also needs `composer install` in `php/worker`.

Without PHP on the host, run everything in a container (Docker or Podman):

```bash
scripts/test-php.sh                  # all tests against PHP 8.4
scripts/test-php.sh serve            # rphp serve, reachable on 127.0.0.1:8080
scripts/test-php.sh cargo clippy --all-targets -- -D warnings
```

## Project documents

- [`SPEC.md`](SPEC.md): product and compatibility specification
- [`docs/implementation-plan.md`](docs/implementation-plan.md): decisions,
  tasks and exit gates per phase
- [`docs/architecture.md`](docs/architecture.md): architecture
- [`docs/roadmap.md`](docs/roadmap.md): phase checklist
- [`docs/persistent-mode.md`](docs/persistent-mode.md): persistent mode,
  what persists between requests, what does not work
- [`docs/worker-protocol.md`](docs/worker-protocol.md): persistent-mode wire
  protocol

## Design principles

1. Compatibility first, performance second.
2. Use the official PHP interpreter; do not implement a PHP interpreter.
3. Keep HTTP handling separate from PHP execution and worker lifecycle.
4. Persistent workers are opt-in until framework and request-isolation tests
   pass.
5. Publish benchmark methodology and results; do not assume Rust is
   automatically faster than PHP-FPM or Go.

## License

[MIT](LICENSE)
