<?php

declare(strict_types=1);

namespace Rphp\Worker;

/**
 * One HTTP request, as received by RPHP. Immutable; a new instance per
 * request. Superglobals ($_GET, $_POST, $_SERVER...) are NOT populated in
 * persistent mode: read everything from here.
 */
final class Request
{
    /**
     * @param array<string, list<string>> $headers lowercase names
     * @param array<string, string|int|float> $server $_SERVER-style values
     */
    public function __construct(
        public readonly string $method,
        public readonly string $uri,
        public readonly string $path,
        public readonly string $query,
        public readonly string $protocol,
        public readonly string $scheme,
        public readonly array $headers,
        public readonly string $body,
        public readonly array $server,
        /** Milliseconds left before RPHP times the request out. */
        public readonly int $timeoutMs,
    ) {
    }

    /**
     * Build from the protocol's JSON head.
     *
     * @param array<string, mixed> $head
     *
     * @internal
     */
    public static function fromHead(array $head, string $body): self
    {
        $headers = [];
        foreach ($head['headers'] ?? [] as [$name, $value]) {
            $headers[strtolower((string) $name)][] = (string) $value;
        }

        $now = microtime(true);
        $server = [
            'REQUEST_METHOD' => (string) $head['method'],
            'REQUEST_URI' => (string) $head['uri'],
            'QUERY_STRING' => (string) $head['query'],
            'SERVER_PROTOCOL' => (string) $head['protocol'],
            'REQUEST_SCHEME' => (string) $head['scheme'],
            'REMOTE_ADDR' => (string) $head['remote_addr'],
            'REMOTE_PORT' => (int) $head['remote_port'],
            'SERVER_ADDR' => (string) $head['server_addr'],
            'SERVER_PORT' => (int) $head['server_port'],
            'SERVER_NAME' => (string) $head['server_name'],
            'DOCUMENT_ROOT' => (string) $head['document_root'],
            'REQUEST_TIME' => (int) $now,
            'REQUEST_TIME_FLOAT' => $now,
        ];
        foreach ($headers as $name => $values) {
            $key = match ($name) {
                'content-type' => 'CONTENT_TYPE',
                'content-length' => 'CONTENT_LENGTH',
                default => 'HTTP_' . strtoupper(str_replace('-', '_', $name)),
            };
            // Same rule as classic mode: names with '_' could spoof '-' names.
            if (!str_contains($name, '_') && $name !== 'proxy') {
                $server[$key] = implode($name === 'cookie' ? '; ' : ', ', $values);
            }
        }
        if ($body !== '' || isset($headers['content-length'])) {
            $server['CONTENT_LENGTH'] = (string) strlen($body);
        }

        return new self(
            method: (string) $head['method'],
            uri: (string) $head['uri'],
            path: (string) $head['path'],
            query: (string) $head['query'],
            protocol: (string) $head['protocol'],
            scheme: (string) $head['scheme'],
            headers: $headers,
            body: $body,
            server: $server,
            timeoutMs: (int) ($head['timeout_ms'] ?? 0),
        );
    }

    /** All values of a header joined with ", ", or null if absent. */
    public function header(string $name): ?string
    {
        $values = $this->headers[strtolower($name)] ?? null;
        return $values === null ? null : implode(', ', $values);
    }

    /** @return array<array-key, mixed> */
    public function queryParams(): array
    {
        parse_str($this->query, $params);
        return $params;
    }

    /** @return array<string, string> */
    public function cookies(): array
    {
        $cookies = [];
        foreach ($this->headers['cookie'] ?? [] as $header) {
            foreach (explode(';', $header) as $pair) {
                $parts = explode('=', trim($pair), 2);
                if (count($parts) === 2 && $parts[0] !== '') {
                    $cookies[$parts[0]] = urldecode($parts[1]);
                }
            }
        }
        return $cookies;
    }

    /**
     * Form fields of an application/x-www-form-urlencoded body, else null.
     * Multipart bodies are not parsed yet (see docs/persistent-mode.md).
     *
     * @return array<array-key, mixed>|null
     */
    public function parsedBody(): ?array
    {
        $type = strtolower((string) $this->header('content-type'));
        if (!str_starts_with($type, 'application/x-www-form-urlencoded')) {
            return null;
        }
        parse_str($this->body, $fields);
        return $fields;
    }
}
