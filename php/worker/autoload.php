<?php

declare(strict_types=1);

// Autoloader for using rphp/worker without Composer (e.g. RPHP's own tests).
// With Composer, use vendor/autoload.php instead.

spl_autoload_register(static function (string $class): void {
    $prefix = 'Rphp\\Worker\\';
    if (str_starts_with($class, $prefix)) {
        $file = __DIR__ . '/src/' . str_replace('\\', '/', substr($class, strlen($prefix))) . '.php';
        if (is_file($file)) {
            require $file;
        }
    }
});
