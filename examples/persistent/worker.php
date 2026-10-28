<?php

declare(strict_types=1);

// Minimal persistent-mode application. Everything above run() executes once
// per worker; the closure runs once per request. With Composer, require
// vendor/autoload.php instead.

require __DIR__ . '/../../php/worker/autoload.php';

use Rphp\Worker\Request;
use Rphp\Worker\Response;
use Rphp\Worker\Worker;

$bootedAt = date(DATE_ATOM);   // "expensive bootstrap", done once
$served = 0;                   // survives between requests (by design)

Worker::fromEnvironment()->run(function (Request $request) use ($bootedAt, &$served): Response {
    $served++;
    return Response::json([
        'hello' => $request->queryParams()['name'] ?? 'world',
        'path' => $request->path,
        'worker_pid' => getmypid(),
        'worker_booted_at' => $bootedAt,
        'requests_served_by_this_worker' => $served,
    ]);
});
