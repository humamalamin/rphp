<?php

declare(strict_types=1);

// Front controller exercised by tests/classic.rs. Every route reports what
// PHP saw, so the tests can check RPHP's request and response mapping.

$path = parse_url($_SERVER['REQUEST_URI'] ?? '/', PHP_URL_PATH);

function json(array $data): void
{
    header('Content-Type: application/json');
    echo json_encode($data, JSON_UNESCAPED_SLASHES | JSON_THROW_ON_ERROR);
}

switch ($path) {
    case '/echo':
        $headers = [];
        foreach ($_SERVER as $key => $value) {
            if (str_starts_with($key, 'HTTP_')) {
                $headers[$key] = $value;
            }
        }
        json([
            'sapi' => PHP_SAPI,
            'method' => $_SERVER['REQUEST_METHOD'],
            'uri' => $_SERVER['REQUEST_URI'],
            'query_string' => $_SERVER['QUERY_STRING'],
            'get' => $_GET,
            'post' => $_POST,
            'input' => file_get_contents('php://input'),
            'content_type' => $_SERVER['CONTENT_TYPE'] ?? null,
            'content_length' => $_SERVER['CONTENT_LENGTH'] ?? null,
            'script_name' => $_SERVER['SCRIPT_NAME'],
            'php_self' => $_SERVER['PHP_SELF'],
            'remote_addr' => $_SERVER['REMOTE_ADDR'],
            'server_name' => $_SERVER['SERVER_NAME'],
            'headers' => $headers,
            'cookies' => $_COOKIE,
        ]);
        break;

    case '/status':
        http_response_code(418);
        echo "teapot\n";
        break;

    case '/redirect':
        header('Location: /target');
        break;

    case '/cookies':
        setcookie('a', '1');
        setcookie('b', '2', ['httponly' => true]);
        header('X-Multi: one');
        header('X-Multi: two', false);
        echo "cookies set\n";
        break;

    case '/upload':
        $file = $_FILES['doc'] ?? null;
        json([
            'field' => $_POST['field'] ?? null,
            'name' => $file['name'] ?? null,
            'size' => $file['size'] ?? null,
            'error' => $file['error'] ?? null,
            'sha1' => $file ? sha1_file($file['tmp_name']) : null,
        ]);
        break;

    case '/env':
        json([
            'secret' => getenv('RPHP_TEST_SECRET'),
            'server_secret' => $_SERVER['RPHP_TEST_SECRET'] ?? false,
        ]);
        break;

    case '/pid':
        echo getmypid();
        break;

    case '/big':
        $chunk = str_repeat('x', 1024);
        for ($i = 0; $i < 1024; $i++) {
            echo $chunk;
        }
        break;

    case '/stderr':
        error_log('rphp fixture stderr line');
        echo "logged\n";
        break;

    case '/sleep':
        sleep(5);
        echo "too late\n";
        break;

    case '/slow':
        usleep(1_000_000);
        echo "slow done\n";
        break;

    case '/hang':
        // Publish the worker PID so a test can kill -9 it mid-request.
        $name = basename((string) ($_GET['pidfile'] ?? 'rphp-hang.pid'));
        file_put_contents(sys_get_temp_dir() . '/' . $name, (string) getmypid());
        sleep(10);
        echo "survived\n";
        break;

    case '/loop':
        while (true) {
        }

    case '/exit':
        echo "bye\n";
        exit(3);

    case '/huge':
        $mb = (int) ($_GET['mb'] ?? 1);
        $chunk = str_repeat('y', 64 * 1024);
        for ($i = 0; $i < $mb * 16; $i++) {
            echo $chunk;
            flush();
        }
        break;

    case '/fatal':
        http_response_code(500);
        undefined_function_for_rphp_test();
        break;

    default:
        http_response_code(404);
        echo "no route for $path\n";
}
