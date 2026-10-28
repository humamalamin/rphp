# RPHP Runtime

Rust-powered PHP application server. RPHP aims to run PHP applications and
frameworks through a generic runtime interface, with PHP 8.4 as the minimum
supported interpreter version.

## Status

Phase 3 of the [roadmap](docs/roadmap.md). Implemented:

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

**Not implemented yet:** metrics and structured logs,
persistent mode, framework compatibility testing and production hardening.

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

`/health` reports that the Rust server is up. It does not yet check PHP
workers. Any other path is served from `public/` if it is a static file, and
by `public/index.php` otherwise.

## Configuration

Copy [`rphp.example.toml`](rphp.example.toml) to `rphp.toml` and adjust, or
pass a path with `--config`. Every key is optional. Set `RUST_LOG` to change
log verbosity (default `info`).

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The PHP discovery tests use stand-in executables, so they pass without PHP
installed and can simulate PHP 8.3 and 8.4. The classic-mode tests in
`tests/classic.rs` need `php-cgi` 8.4+ on `PATH`; they skip without it unless
`RPHP_REQUIRE_PHP=1` is set (as in CI).

## Project documents

- [`SPEC.md`](SPEC.md): product and compatibility specification
- [`docs/implementation-plan.md`](docs/implementation-plan.md): decisions,
  tasks and exit gates per phase
- [`docs/architecture.md`](docs/architecture.md): architecture
- [`docs/roadmap.md`](docs/roadmap.md): phase checklist
- [`docs/worker-protocol.md`](docs/worker-protocol.md): persistent-mode IPC
  design notes

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
