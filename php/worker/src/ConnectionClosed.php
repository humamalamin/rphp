<?php

declare(strict_types=1);

namespace Rphp\Worker;

/** RPHP closed the connection: the worker should exit. */
final class ConnectionClosed extends \RuntimeException
{
}
