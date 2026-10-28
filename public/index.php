<?php

declare(strict_types=1);

// Development front controller, run through php-cgi by `rphp serve`.

header('Content-Type: text/plain; charset=utf-8');

echo "Hello from RPHP\n";
echo 'PHP ' . PHP_VERSION . ' (' . PHP_SAPI . ")\n";
echo 'Method: ' . ($_SERVER['REQUEST_METHOD'] ?? 'unknown') . "\n";
echo 'URI: ' . ($_SERVER['REQUEST_URI'] ?? 'unknown') . "\n";
