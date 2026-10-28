<?php

declare(strict_types=1);

namespace Rphp\Worker\Psr7;

use Psr\Http\Message\ResponseInterface;
use Psr\Http\Message\ServerRequestFactoryInterface;
use Psr\Http\Message\ServerRequestInterface;
use Psr\Http\Message\StreamFactoryInterface;
use Rphp\Worker\Request;
use Rphp\Worker\Response;
use Rphp\Worker\Worker;

/**
 * Runs a PSR-15-style handler (ServerRequestInterface -> ResponseInterface)
 * in the persistent worker loop. Bring any PSR-17 implementation:
 *
 *     $f = new Nyholm\Psr7\Factory\Psr17Factory();
 *     (new Psr7Worker(Worker::fromEnvironment(), $f, $f))
 *         ->run(fn (ServerRequestInterface $r) => $app->handle($r));
 *
 * Requires psr/http-message ^2.0 and psr/http-factory ^1.1.
 */
final class Psr7Worker
{
    private const READ_CHUNK = 64 * 1024;

    public function __construct(
        private readonly Worker $worker,
        private readonly ServerRequestFactoryInterface $requests,
        private readonly StreamFactoryInterface $streams,
    ) {
    }

    /** @param callable(ServerRequestInterface): ResponseInterface $handler */
    public function run(callable $handler): void
    {
        $this->worker->run(fn (Request $request): Response => $this->toResponse(
            $handler($this->toServerRequest($request)),
        ));
    }

    public function toServerRequest(Request $request): ServerRequestInterface
    {
        $host = $request->header('host') ?? $request->server['SERVER_NAME'] . ':' . $request->server['SERVER_PORT'];
        $psr = $this->requests
            ->createServerRequest($request->method, $request->scheme . '://' . $host . $request->uri, $request->server)
            ->withProtocolVersion(substr($request->protocol, strlen('HTTP/')))
            ->withQueryParams($request->queryParams())
            ->withCookieParams($request->cookies())
            ->withBody($this->streams->createStream($request->body));
        foreach ($request->headers as $name => $values) {
            $psr = $psr->withHeader($name, $values);
        }
        $parsed = $request->parsedBody();
        if ($parsed !== null) {
            $psr = $psr->withParsedBody($parsed);
        }
        return $psr;
    }

    public function toResponse(ResponseInterface $response): Response
    {
        return new Response(
            $response->getStatusCode(),
            $response->getHeaders(),
            self::chunks($response),
        );
    }

    /** @return \Generator<string> */
    private static function chunks(ResponseInterface $response): \Generator
    {
        $body = $response->getBody();
        if ($body->isSeekable()) {
            $body->rewind();
        }
        while (!$body->eof()) {
            $chunk = $body->read(self::READ_CHUNK);
            if ($chunk === '') {
                break;
            }
            yield $chunk;
        }
    }
}
