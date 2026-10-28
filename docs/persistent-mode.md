# Persistent Mode (experimental)

In classic mode every request boots the application from scratch, like
PHP-FPM. In persistent mode a worker boots the application **once** and then
serves many requests, which removes the bootstrap cost from every request but
changes what a PHP application can assume. It is opt-in, marked experimental,
and only for applications written (or adapted) for it.

## Setup

1. Write a worker script with [`rphp/worker`](../php/worker/README.md):

   ```php
   <?php
   require __DIR__ . '/vendor/autoload.php';

   $app = require __DIR__ . '/bootstrap.php';     // once per worker

   Rphp\Worker\Worker::fromEnvironment()->run(
       fn (Rphp\Worker\Request $r): Rphp\Worker\Response => $app->handle($r),
   );
   ```

   PSR-7/PSR-15 applications can use `Rphp\Worker\Psr7\Psr7Worker` with any
   PSR-17 factory instead.

2. Configure RPHP:

   ```toml
   [php]
   binary = "php"           # the CLI, not php-cgi

   [worker]
   mode = "persistent"
   script = "worker.php"    # relative to this file
   max_requests = 500
   max_age_secs = 3600
   max_memory_mb = 128
   ```

A runnable example lives in [`examples/persistent/`](../examples/persistent/).
Static files are still served by RPHP from `document_root`, as in classic
mode. `server.entry_point` must still exist but is not used.

## What persists and what does not

| Between two requests on the same worker | Persists? |
|---|---|
| Static properties, globals, singletons, service containers | **Yes** |
| Open database/Redis connections, file handles | **Yes** (handle reconnects) |
| Variables captured by the handler closure | **Yes** |
| The `Request` object | No, a new one per request |
| `$_GET`, `$_POST`, `$_COOKIE`, `$_FILES`, `$_SERVER` | Never populated |
| Output buffers left open by the handler | No, closed and discarded |
| State reset in an `onReset()` hook | No |
| PHP's `memory_get_usage()` growth | Bounded only by recycling |

`tests/persistent.rs` checks static state, `onReset()`, output buffers and
`$_GET`, so a change in that behaviour fails CI.

**Security note.** Data from one user's request that the application keeps
in static or global state is visible to the next request on that worker. This
is the same class of bug as PHP's CVE-2026-109619 (stale per-request state
reused by a later request), but in application code. Clear per-request state
in `onReset()`, and keep `max_requests` finite.

## What does not work (yet)

| Feature | Status in persistent mode | Instead |
|---|---|---|
| `header()`, `setcookie()`, `http_response_code()` | Ignored (CLI SAPI) | Set them on the `Response` |
| `echo` / `print` in the handler | Discarded, with a warning in the log | Return the body in the `Response` |
| `exit()` / `die()` | **Ends the worker**; the client gets `502` | Return a `Response` |
| Superglobals (`$_GET`, `$_POST`...) | Not populated | `Request` / PSR-7 request |
| `multipart/form-data` (uploads) | Body passed raw, not parsed | Parse in the app; parsing is planned |
| Native sessions (`session_start()`) | Not supported | A PSR-15 session middleware |
| `register_shutdown_function()` | Runs when the worker exits, not per request | `onReset()` |
| Fatal errors (e.g. memory exhausted) | Worker dies, client gets `502`, worker replaced | — |
| Uncaught exceptions / `Error` | Client gets `500` (no details); reset hooks run and the worker is reused | Handle them in the app; clear state in `onReset()` |

## Framework compatibility

Nothing is claimed until a fixture proves it (roadmap phase 6).

| Application style | Expected |
|---|---|
| PSR-7/PSR-15 apps (Slim, Mezzio) via `Psr7Worker` | Likely, once tested |
| Symfony, Laravel | Need an adapter like Laravel Octane's; not provided |
| WordPress, apps relying on superglobals | No: use classic mode (or the phase 8 embedded SAPI) |

## Recycling

Long-running PHP leaks: caches grow, references linger. RPHP recycles a
worker (graceful: close the connection, SIGTERM, then SIGKILL after 5 s) when
any of these is reached, checked after each request:

- `worker.max_requests` requests served;
- `worker.max_age_secs` since the worker started (also checked while idle);
- `worker.max_memory_mb` of `memory_get_usage()` reported by the worker.

Do not cap worker memory with `RLIMIT_AS`/`RLIMIT_DATA`: allocation failures
reach the PHP2Shell (CVE-2026-109618) code path.
