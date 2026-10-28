# rphp/worker

PHP side of [RPHP](../../README.md)'s **experimental** persistent mode: a
loop that keeps one booted application alive and serves many requests from
it. Read [docs/persistent-mode.md](../../docs/persistent-mode.md) first: state
survives between requests.

Requires PHP 8.4+, `ext-json`; `ext-pcntl` is recommended (finish the current
request on SIGTERM).

## Plain handler

```php
use Rphp\Worker\{Request, Response, Worker};

$worker = Worker::fromEnvironment();
$worker->onReset(fn () => MyRequestContext::clear());   // after every request
$worker->run(fn (Request $r): Response => match ($r->path) {
    '/' => Response::text('hello'),
    '/api' => Response::json(['query' => $r->queryParams()]),
    default => Response::text('not found', 404),
});
```

`Request` exposes `method`, `uri`, `path`, `query`, `protocol`, `headers`
(lowercase name to list of values), `body`, `server` (`$_SERVER`-style) and
`timeoutMs`, plus `header()`, `queryParams()`, `cookies()` and
`parsedBody()` (URL-encoded forms). A `Response` body is a string or any
iterable of strings, streamed as it is produced.

## PSR-7

```php
use Nyholm\Psr7\Factory\Psr17Factory;
use Rphp\Worker\Psr7\Psr7Worker;
use Rphp\Worker\Worker;

$f = new Psr17Factory();
(new Psr7Worker(Worker::fromEnvironment(), $f, $f))
    ->run(fn ($request) => $app->handle($request));   // PSR-15 style
```

Needs `psr/http-message` ^2.0, `psr/http-factory` ^1.1 and any PSR-17
implementation.

## Running

The worker is started by RPHP (`worker.mode = "persistent"`,
`worker.script = "worker.php"`); running the script by hand fails because
`RPHP_WORKER_SOCKET` is not set. The wire protocol is documented in
[docs/worker-protocol.md](../../docs/worker-protocol.md).

Without Composer, `require 'php/worker/autoload.php'`.
