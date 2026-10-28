<?php

declare(strict_types=1);

// PSR-7 adapter fixture. Needs `composer install` in php/worker (dev
// dependencies provide nyholm/psr7); tests/persistent.rs skips it otherwise.

require __DIR__ . '/../../../php/worker/vendor/autoload.php';

use Nyholm\Psr7\Factory\Psr17Factory;
use Psr\Http\Message\ServerRequestInterface;
use Rphp\Worker\Psr7\Psr7Worker;
use Rphp\Worker\Worker;

$factory = new Psr17Factory();

(new Psr7Worker(Worker::fromEnvironment(), $factory, $factory))
    ->run(static function (ServerRequestInterface $request) use ($factory) {
        $payload = json_encode([
            'method' => $request->getMethod(),
            'uri' => (string) $request->getUri(),
            'query' => $request->getQueryParams(),
            'parsed' => $request->getParsedBody(),
            'cookies' => $request->getCookieParams(),
            'custom' => $request->getHeaderLine('X-Custom'),
            'body' => (string) $request->getBody(),
            'protocol' => $request->getProtocolVersion(),
        ], JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES);

        return $factory->createResponse(202)
            ->withHeader('Content-Type', 'application/json')
            ->withAddedHeader('Set-Cookie', 'p=1')
            ->withAddedHeader('Set-Cookie', 'q=2')
            ->withBody($factory->createStream($payload));
    });
