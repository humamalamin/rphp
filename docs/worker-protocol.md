# Worker Protocol Notes (Draft)

This document is a design proposal, not an implemented protocol.

## Scope

This protocol is for **persistent mode only** (roadmap phase 5). Classic mode
does not need it: it uses FastCGI to `php-cgi`, which already provides
framing, request IDs, bounded record lengths and separate stdout/stderr
streams. This document is finalised when phase 5 starts, so that its design
follows real requirements from the PHP-side worker loop.

## Requirements

- Explicit framing; never rely on arbitrary reads matching message boundaries.
- Maximum frame size.
- Request ID for correlating request and response.
- Message type and protocol version.
- Clear distinction between application response and worker/runtime error.
- Timeout and cancellation behavior.
- stdout reserved for protocol data when using stdio transport; diagnostics go to stderr.
- Validate every length and field before allocation or dispatch.

## Candidate envelope

A future binary frame could contain:
- magic bytes
- protocol version
- message type
- payload length
- request identifier
- payload bytes

The payload format could initially use JSON for development simplicity, then be benchmarked against a more compact encoding. HTTP bodies should not be base64-encoded by default; the protocol should support byte payloads or separate body frames.

## Request fields to consider

- method
- URI/path and query
- normalized headers
- body bytes
- remote address
- scheme/host information
- request deadline
- configured document root and entry point

## Response fields to consider

- status
- headers
- body bytes
- cookies through `Set-Cookie`
- application error metadata kept separate from public response details

## Important limitation

A protocol alone does not make PHP CLI persistent. Persistent mode requires PHP-side code that remains alive, receives framed requests, establishes a fresh request context, invokes the application, and emits the response.
