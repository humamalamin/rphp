# Worker Protocol, version 1

Wire protocol between RPHP and persistent-mode PHP workers (`php/worker`).
Classic mode does not use it: it speaks FastCGI to `php-cgi`.

Implementations: `src/backend/protocol.rs` (Rust, codec) and
`php/worker/src/Connection.php` (PHP). Both must change together; bump
`VERSION` for any incompatible change.

## Transport

- One Unix stream socket per worker, in a private `0700` directory. RPHP
  listens, starts `php <worker.script>` with `RPHP_WORKER_SOCKET` (path),
  `RPHP_WORKER_ID` and `RPHP_WORKER_PROTOCOL` (`1`) in an otherwise cleared
  environment, and accepts exactly one connection.
- The connection is accepted only if `SO_PEERCRED` shows it comes from that
  worker's PID. The socket file is removed right after.
- The connection lives as long as the worker and carries one request at a
  time. stdout and stderr are not part of the protocol; RPHP logs them.

## Frames

Every frame is a 12-byte big-endian header and a payload:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 2 | magic `RW` |
| 2 | 1 | version (`1`) |
| 3 | 1 | frame type |
| 4 | 4 | request id (`u32`; `0` for `HELLO`) |
| 8 | 4 | payload length (`u32`, at most 1 MiB) |

JSON payloads (heads, trailers, errors) are limited to 64 KiB. Bodies are raw
bytes, never base64. Both sides check the length in the header before
allocating, and reject bad magic, unknown versions and oversized frames by
closing the connection (RPHP then replaces the worker).

| Type | Name | Direction | Payload |
|------|------|-----------|---------|
| `0x01` | `HELLO` | worker → RPHP | `{"protocol":1,"pid":…,"php":"8.4.27"}` |
| `0x10` | `REQUEST` | RPHP → worker | request head (JSON, below) |
| `0x11` | `REQUEST_BODY` | RPHP → worker | body bytes (≤ 256 KiB per frame) |
| `0x12` | `REQUEST_END` | RPHP → worker | empty |
| `0x20` | `RESPONSE` | worker → RPHP | `{"status":200,"headers":[["name","value"],…]}` |
| `0x21` | `RESPONSE_BODY` | worker → RPHP | body bytes |
| `0x22` | `RESPONSE_END` | worker → RPHP | `{"memory": memory_get_usage()}` |
| `0x2f` | `ERROR` | worker → RPHP | `{"message":"…"}` (logged, never sent to the client) |

## Exchange

```text
worker                                   RPHP
  │──── HELLO ─────────────────────────────▶│   once, after connecting
  │◀─── REQUEST (id n) ─────────────────────│
  │◀─── REQUEST_BODY (id n) × k ────────────│   k ≥ 0
  │◀─── REQUEST_END (id n) ─────────────────│
  │       handler runs                       │
  │──── RESPONSE (id n) ───────────────────▶│
  │──── RESPONSE_BODY (id n) × m ──────────▶│   streamed to the client
  │──── RESPONSE_END (id n) ───────────────▶│
```

- The worker reads the whole request before running the handler, so RPHP may
  write the request completely before reading.
- Request ids increase per connection; every frame of an exchange carries
  the request's id. Anything else is a protocol violation.
- `ERROR` before `RESPONSE`: the client gets `500`. `ERROR` after
  `RESPONSE`: the response stream is aborted. Either way the worker has run
  its reset hooks and is reused, so clients cannot force reboots by
  triggering exceptions.
- Connection closed, malformed frame, wrong order or deadline exceeded: the
  client gets `502` (or `504` for the deadline) and the worker is killed.

### Request head

```json
{
  "method": "POST", "uri": "/path?a=1", "path": "/path", "query": "a=1",
  "protocol": "HTTP/1.1", "scheme": "http",
  "headers": [["host", "example.test"], ["cookie", "a=1"]],
  "remote_addr": "127.0.0.1", "remote_port": 51000,
  "server_addr": "127.0.0.1", "server_port": 8080, "server_name": "example.test",
  "document_root": "/srv/app/public", "body_length": 18, "timeout_ms": 29998
}
```

Header names are lowercase; repeated headers are repeated pairs, in order.
Header values that are not UTF-8 are passed lossily.

## Lifecycle

- RPHP recycles a worker after `max_requests` requests, after `max_age_secs`,
  or when `RESPONSE_END.memory` reaches `max_memory_mb`: it closes the
  connection and sends SIGTERM. The worker's loop sees EOF and returns; the
  process gets a grace period before SIGKILL.
- On shutdown, idle workers are closed the same way. A busy worker with
  `ext-pcntl` finishes its request after SIGTERM, then exits.
