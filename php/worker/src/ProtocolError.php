<?php

declare(strict_types=1);

namespace Rphp\Worker;

/** RPHP sent something this worker does not understand. */
final class ProtocolError extends \RuntimeException
{
}
