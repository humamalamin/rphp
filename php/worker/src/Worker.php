<?php

declare(strict_types=1);

namespace Rphp\Worker;

/**
 * The persistent worker loop. A worker script boots the application once,
 * then hands a request handler to run():
 *
 *     require __DIR__ . '/vendor/autoload.php';
 *     $app = bootstrap();                       // runs once per worker
 *     Rphp\Worker\Worker::fromEnvironment()
 *         ->run(fn (Request $r): Response => $app->handle($r));
 *
 * Everything the application keeps between requests (static properties,
 * globals, singletons, open connections) survives into the next request.
 * Register cleanup with onReset(). See docs/persistent-mode.md.
 */
final class Worker
{
    /** @var list<callable(): void> */
    private array $resetHooks = [];
    private bool $stopping = false;

    private function __construct(private readonly Connection $connection)
    {
    }

    /** Connect to RPHP using the environment it starts workers with. */
    public static function fromEnvironment(): self
    {
        $socket = getenv('RPHP_WORKER_SOCKET');
        if (!is_string($socket) || $socket === '') {
            throw new \RuntimeException(
                'rphp/worker: RPHP_WORKER_SOCKET is not set; this script must be started by '
                . 'rphp with worker.mode = "persistent"',
            );
        }
        if (getenv('RPHP_WORKER_PROTOCOL') !== (string) Protocol::VERSION) {
            throw new \RuntimeException('rphp/worker: this version of rphp/worker does not match the running rphp');
        }

        $worker = new self(Connection::open($socket));
        $worker->connection->sendJson(Protocol::HELLO, 0, [
            'protocol' => Protocol::VERSION,
            'pid' => getmypid(),
            'php' => PHP_VERSION,
        ]);
        if (function_exists('pcntl_signal')) {
            // On SIGTERM, finish the current request, then exit.
            pcntl_async_signals(true);
            pcntl_signal(SIGTERM, function () use ($worker): void {
                $worker->stopping = true;
            });
        }
        return $worker;
    }

    /** Run `$hook` after every request, e.g. to clear per-request state. */
    public function onReset(callable $hook): void
    {
        $this->resetHooks[] = $hook;
    }

    /**
     * Serve requests until RPHP closes the connection or asks the worker to
     * stop.
     *
     * @param callable(Request): Response $handler
     */
    public function run(callable $handler): void
    {
        try {
            while (!$this->stopping && ($next = $this->nextRequest()) !== null) {
                [$id, $request] = $next;
                $this->serve($id, $request, $handler);
            }
        } catch (ConnectionClosed) {
            // RPHP is recycling or stopping this worker.
        }
    }

    /** @return array{int, Request}|null */
    private function nextRequest(): ?array
    {
        $frame = $this->connection->receive();
        if ($frame === null) {
            return null;
        }
        [$type, $id, $payload] = $frame;
        if ($type !== Protocol::REQUEST) {
            throw new ProtocolError(sprintf('rphp/worker: expected REQUEST, got 0x%02x', $type));
        }
        /** @var array<string, mixed> $head */
        $head = json_decode($payload, true, 32, JSON_THROW_ON_ERROR);
        $expected = (int) ($head['body_length'] ?? 0);

        $body = '';
        while (true) {
            $frame = $this->connection->receive() ?? throw new ConnectionClosed('rphp/worker: closed mid-request');
            [$type, $frameId, $payload] = $frame;
            if ($frameId !== $id) {
                throw new ProtocolError('rphp/worker: frame for another request');
            }
            if ($type === Protocol::REQUEST_END) {
                break;
            }
            if ($type !== Protocol::REQUEST_BODY || strlen($body) + strlen($payload) > $expected) {
                throw new ProtocolError('rphp/worker: unexpected request body frame');
            }
            $body .= $payload;
        }
        return [$id, Request::fromHead($head, $body)];
    }

    /** @param callable(Request): Response $handler */
    private function serve(int $id, Request $request, callable $handler): void
    {
        $level = ob_get_level();
        ob_start();
        try {
            $response = $handler($request);
            if (!$response instanceof Response) {
                throw new \TypeError('rphp/worker: the handler must return ' . Response::class);
            }
        } catch (\Throwable $e) {
            $this->discardOutput($level);
            $this->connection->sendJson(Protocol::ERROR, $id, ['message' => self::describe($e)]);
            $this->reset();
            return;
        }
        $this->discardOutput($level);

        $this->connection->sendJson(Protocol::RESPONSE, $id, [
            'status' => $response->status,
            'headers' => $response->headerPairs(),
        ]);
        try {
            foreach ($response->chunks() as $chunk) {
                foreach (str_split((string) $chunk, Protocol::MAX_PAYLOAD) as $piece) {
                    if ($piece !== '') {
                        $this->connection->send(Protocol::RESPONSE_BODY, $id, $piece);
                    }
                }
            }
        } catch (ConnectionClosed $e) {
            throw $e;
        } catch (\Throwable $e) {
            // Mid-stream: RPHP aborts the response; this worker is reused.
            $this->connection->sendJson(Protocol::ERROR, $id, ['message' => self::describe($e)]);
            $this->reset();
            return;
        }

        $this->reset();
        $this->connection->sendJson(Protocol::RESPONSE_END, $id, ['memory' => memory_get_usage()]);
    }

    /** Drop output written outside the Response, and any buffers left open. */
    private function discardOutput(int $level): void
    {
        $discarded = 0;
        while (ob_get_level() > $level) {
            $discarded += strlen((string) ob_get_clean());
        }
        if ($discarded > 0) {
            fwrite(STDERR, "rphp/worker: discarded {$discarded} bytes of output written outside the Response\n");
        }
    }

    private function reset(): void
    {
        foreach ($this->resetHooks as $hook) {
            $hook();
        }
        gc_collect_cycles();
    }

    private static function describe(\Throwable $e): string
    {
        return sprintf('%s: %s in %s:%d', $e::class, $e->getMessage(), $e->getFile(), $e->getLine());
    }
}
