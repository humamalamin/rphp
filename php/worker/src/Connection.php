<?php

declare(strict_types=1);

namespace Rphp\Worker;

/**
 * Blocking frame I/O on the worker's Unix socket.
 *
 * @internal
 */
final class Connection
{
    /** @param resource $stream */
    public function __construct(private $stream)
    {
        stream_set_blocking($this->stream, true);
        // An idle worker may wait for its next request indefinitely.
        stream_set_timeout($this->stream, 31_536_000);
    }

    public static function open(string $path): self
    {
        $stream = @stream_socket_client('unix://' . $path, $errno, $errstr, 5.0);
        if ($stream === false) {
            throw new \RuntimeException("rphp/worker: cannot connect to {$path}: {$errstr}");
        }
        return new self($stream);
    }

    public function send(int $type, int $id, string $payload = ''): void
    {
        if (strlen($payload) > Protocol::MAX_PAYLOAD) {
            throw new \LengthException('rphp/worker: frame payload too large');
        }
        $this->write(pack('a2CCNN', Protocol::MAGIC, Protocol::VERSION, $type, $id, strlen($payload)) . $payload);
    }

    /** @param array<string, mixed> $data */
    public function sendJson(int $type, int $id, array $data): void
    {
        $this->send($type, $id, json_encode($data, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES | JSON_INVALID_UTF8_SUBSTITUTE));
    }

    /**
     * Next frame, or null when RPHP closed the connection between frames.
     *
     * @return array{int, int, string}|null [type, request id, payload]
     */
    public function receive(): ?array
    {
        $header = $this->read(Protocol::HEADER_LEN, true);
        if ($header === null) {
            return null;
        }
        /** @var array{magic: string, version: int, type: int, id: int, len: int} $h */
        $h = unpack('a2magic/Cversion/Ctype/Nid/Nlen', $header);
        if ($h['magic'] !== Protocol::MAGIC || $h['version'] !== Protocol::VERSION) {
            throw new ProtocolError('rphp/worker: bad frame header');
        }
        if ($h['len'] > Protocol::MAX_PAYLOAD) {
            throw new ProtocolError("rphp/worker: frame of {$h['len']} bytes exceeds the limit");
        }
        $payload = $h['len'] === 0 ? '' : $this->read($h['len'], false);
        return [$h['type'], $h['id'], (string) $payload];
    }

    private function write(string $data): void
    {
        while ($data !== '') {
            $written = @fwrite($this->stream, $data);
            if ($written === false || $written === 0) {
                throw new ConnectionClosed('rphp/worker: connection closed while writing');
            }
            $data = substr($data, $written);
        }
    }

    private function read(int $length, bool $eofAllowed): ?string
    {
        $buffer = '';
        while (strlen($buffer) < $length) {
            $chunk = @fread($this->stream, $length - strlen($buffer));
            if ($chunk === false || $chunk === '') {
                if (feof($this->stream)) {
                    if ($eofAllowed && $buffer === '') {
                        return null;
                    }
                    throw new ConnectionClosed('rphp/worker: connection closed mid-frame');
                }
                // Only a read timeout is worth waiting through (an idle worker
                // may wait long for its next request). Anything else is a
                // broken socket: retrying would spin forever.
                if (stream_get_meta_data($this->stream)['timed_out'] ?? false) {
                    continue;
                }
                throw new ConnectionClosed('rphp/worker: read from RPHP failed');
            }
            $buffer .= $chunk;
        }
        return $buffer;
    }
}
