# RPHP Architecture (Draft)

## Layering

1. **HTTP layer** (`server`): accepts connections, timeouts, routing of
   built-in endpoints, graceful shutdown.
2. **Runtime core** (`config`, `lib.rs` preflight): configuration, validation,
   deadlines, lifecycle, and coordination.
3. **Execution interface**: framework-agnostic request/response contract
   between the HTTP layer and a backend.
4. **Execution backends**:
   - classic: FastCGI client talking to a pool of `php-cgi` processes;
   - persistent (later): framed worker protocol to a PHP-side worker loop.
5. **Worker supervisor**: process health, queueing, restart, recycling, and
   shutdown ordering.
6. **Observability**: structured logs, health, and metrics.

```text
client ──HTTP──▶ server ──▶ queue ──▶ backend ──FastCGI──▶ php-cgi pool
                   │                    │
                   └── /health          └── supervisor (spawn, recycle, kill)
```

## Core architectural rule

Framework-specific logic does not live in the HTTP server or the scheduler.
RPHP passes a normalized request to a PHP entry point and translates the
PHP response back to HTTP.

## Startup sequence

1. Parse CLI arguments.
2. Load `rphp.toml` (or defaults); reject unknown keys; resolve relative
   paths against the config file's directory; collect all validation errors.
3. Resolve the PHP binary on `PATH` without a shell; run `<php> -v` with a
   timeout; enforce `PHP_VERSION_ID >= 80400`; record the SAPI.
4. Only then bind the listener.

## Modes

### Classic mode: FastCGI to php-cgi

`php-cgi` implements the FastCGI SAPI, so PHP itself handles superglobals,
headers, cookies, uploads, `php://input` and sessions, and resets request
state between requests. RPHP keeps `worker.count` processes alive and
recycles each after `worker.max_requests` requests.

The PHP CLI is deliberately not used: under the CLI SAPI, HTTP superglobals
are not populated and `header()` / `setcookie()` produce no output
(`headers_list()` is empty). Emulating a web SAPI in userland would be a
permanent source of compatibility bugs.

FastCGI also provides what the IPC layer needs: explicit record framing with
bounded lengths, request IDs, and separate stdout and stderr streams.

### Persistent mode

A worker initializes the application once and processes many requests. This
requires a PHP-side loop and a defined request lifecycle. Leaving
`php index.php` running does not create a persistent HTTP worker.

Two strategies, in order:

1. **PSR-7 worker loop** (phase 5): PHP receives framed requests, builds a
   PSR-7 request, calls the application, emits the response. Requires an
   adapter and does not support code that reads superglobals.
2. **Embedded SAPI** (phase 8, research): link `libphp` (ZTS) and implement a
   SAPI in Rust that resets superglobals per request, the approach FrankenPHP
   takes from Go. Broadest compatibility, highest complexity.

Persistent mode must address:
- request globals and environment reset
- output buffer cleanup
- exception and fatal error boundaries
- session and database resource lifecycle
- per-request dependency state
- memory growth and worker recycling
- cancellation and request timeout semantics

## Shutdown sequence

1. SIGINT or SIGTERM received.
2. Stop accepting connections; close the listener.
3. Signal open connections to finish (idle keep-alive connections close
   immediately).
4. Wait up to `server.shutdown_timeout_ms`, then drop what remains.
5. (Phase 3) Stop PHP processes: SIGTERM, then SIGKILL after a timeout.

## Module layout

Single crate until boundaries are stable.

| Module | Status |
|--------|--------|
| `cli` | implemented |
| `config` | implemented |
| `php` (discovery, version) | implemented |
| `server` | implemented |
| `fastcgi` | phase 2 |
| `backend` (classic pool) | phase 2 |
| `supervisor` | phase 3 |
| `observability` | phase 4 |
| `protocol` (persistent worker) | phase 5 |
