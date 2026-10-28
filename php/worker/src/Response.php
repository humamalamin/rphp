<?php

declare(strict_types=1);

namespace Rphp\Worker;

/**
 * What a handler returns. `header()`, `setcookie()` and `echo` do not work in
 * persistent mode (the CLI SAPI sends no headers, and stray output is
 * discarded): put everything in the Response.
 */
final class Response
{
    /**
     * @param array<string, string|list<string>> $headers
     * @param string|iterable<string> $body a string, or chunks streamed as
     *        they are produced (e.g. a generator)
     */
    public function __construct(
        public readonly int $status = 200,
        public readonly array $headers = [],
        public readonly string|iterable $body = '',
    ) {
    }

    public static function text(string $body, int $status = 200): self
    {
        return new self($status, ['content-type' => 'text/plain; charset=utf-8'], $body);
    }

    public static function json(mixed $data, int $status = 200): self
    {
        return new self(
            $status,
            ['content-type' => 'application/json'],
            json_encode($data, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES),
        );
    }

    /**
     * @return list<array{string, string}>
     *
     * @internal
     */
    public function headerPairs(): array
    {
        $pairs = [];
        foreach ($this->headers as $name => $values) {
            foreach ((array) $values as $value) {
                $pairs[] = [(string) $name, (string) $value];
            }
        }
        return $pairs;
    }

    /**
     * @return iterable<string>
     *
     * @internal
     */
    public function chunks(): iterable
    {
        return is_string($this->body) ? [$this->body] : $this->body;
    }
}
