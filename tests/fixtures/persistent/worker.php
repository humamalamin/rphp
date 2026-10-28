<?php

declare(strict_types=1);

// Persistent-mode fixture driven by tests/persistent.rs. It boots once and
// serves many requests; each route exposes something the tests check about
// lifecycle and state isolation.

require __DIR__ . '/../../../php/worker/autoload.php';

use Rphp\Worker\Request;
use Rphp\Worker\Response;
use Rphp\Worker\Worker;

/** Survives between requests: that is what persistent mode is. */
final class Counter
{
    public static int $requests = 0;
}

/** Per-request state, cleared by the reset hook. */
final class RequestScoped
{
    public static ?string $user = null;
}

$bootedAt = microtime(true);
$leak = [];

$worker = Worker::fromEnvironment();
$worker->onReset(static function (): void {
    RequestScoped::$user = null;
});

$worker->run(static function (Request $r) use (&$leak, $bootedAt): Response {
    Counter::$requests++;

    switch ($r->path) {
        case '/echo':
            return Response::json([
                'method' => $r->method,
                'uri' => $r->uri,
                'query' => $r->queryParams(),
                'form' => $r->parsedBody(),
                'cookies' => $r->cookies(),
                'custom' => $r->header('x-custom'),
                'body_length' => strlen($r->body),
                'body_sha1' => sha1($r->body),
                'server' => [
                    'REMOTE_ADDR' => $r->server['REMOTE_ADDR'],
                    'SERVER_NAME' => $r->server['SERVER_NAME'],
                    'HTTP_X_CUSTOM' => $r->server['HTTP_X_CUSTOM'] ?? null,
                    'HTTP_PROXY' => $r->server['HTTP_PROXY'] ?? null,
                    'CONTENT_LENGTH' => $r->server['CONTENT_LENGTH'] ?? null,
                ],
                'timeout_ms' => $r->timeoutMs,
            ]);

        case '/state':
            $before = RequestScoped::$user;
            RequestScoped::$user = $r->queryParams()['user'] ?? null;
            return Response::json([
                'pid' => getmypid(),
                'requests' => Counter::$requests,
                'scoped_before' => $before,
                'get_superglobal' => $_GET,
                'ob_level' => ob_get_level(),
                'booted_at' => $bootedAt,
            ]);

        case '/leave-buffer':
            ob_start();
            echo 'stray output';
            return Response::text('ok');

        case '/leak':
            $leak[] = str_repeat('x', 4 * 1024 * 1024);
            return Response::json(['pid' => getmypid(), 'memory' => memory_get_usage()]);

        case '/throw':
            throw new RuntimeException('secret detail that must not reach the client');

        case '/undefined-function':
            // An Error (Throwable) in PHP 8: caught by the worker, like /throw.
            undefined_function_for_rphp_test();

        case '/fatal':
            // Memory exhaustion is a real fatal error: the process dies.
            ini_set('memory_limit', '16M');
            $blob = str_repeat('x', 32 * 1024 * 1024);
            return Response::text((string) strlen($blob));

        case '/exit':
            // exit() ends the whole worker process, not just the request.
            exit(3);

        case '/stream':
            return new Response(200, ['content-type' => 'text/plain'], (static function (): Generator {
                for ($i = 0; $i < 5; $i++) {
                    yield "chunk {$i}\n";
                }
            })());

        case '/cookies':
            return new Response(201, [
                'set-cookie' => ['a=1', 'b=2; HttpOnly'],
                'x-multi' => ['one', 'two'],
            ], 'created');

        case '/big':
            return new Response(200, [], str_repeat('z', 3 * 1024 * 1024));

        case '/slow':
            usleep(1_000_000);
            return Response::text("slow done\n");

        case '/sleep':
            sleep(5);
            return Response::text('too late');

        case '/env':
            return Response::json(['secret' => getenv('RPHP_TEST_SECRET')]);

        case '/noisy':
            $junk = str_repeat("\xff\xfe not utf-8 \xc3\x28\n", 5000);
            fwrite(STDERR, $junk);
            fwrite(STDOUT, $junk);
            return Response::text('ok');

        case '/pid':
            return Response::text((string) getmypid());

        default:
            return Response::text("no route for {$r->path}\n", 404);
    }
});
