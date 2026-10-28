<?php

declare(strict_types=1);

namespace Rphp\Worker;

/**
 * Constants of RPHP's worker protocol, version 1. Must match
 * src/backend/protocol.rs and docs/worker-protocol.md.
 *
 * @internal
 */
final class Protocol
{
    public const MAGIC = 'RW';
    public const VERSION = 1;
    public const HEADER_LEN = 12;
    public const MAX_PAYLOAD = 1024 * 1024;

    public const HELLO = 0x01;
    public const REQUEST = 0x10;
    public const REQUEST_BODY = 0x11;
    public const REQUEST_END = 0x12;
    public const RESPONSE = 0x20;
    public const RESPONSE_BODY = 0x21;
    public const RESPONSE_END = 0x22;
    public const ERROR = 0x2f;

    private function __construct()
    {
    }
}
