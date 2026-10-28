# Changelog

## Unreleased

### Added
- `rphp serve`, `check`, `version`, `help` commands with `--config`.
- `rphp.toml` loading with defaults, strict unknown-key rejection, and
  validation of host, port, limits, document root and entry point.
- PHP discovery on `PATH` (no shell) and enforcement of PHP 8.4+.
- Graceful shutdown on Ctrl+C and SIGTERM with a drain timeout.
- Header read timeout; `HEAD /health`; `405` with `Allow` for other methods.
- Unit and integration tests, CI workflow, `rphp.example.toml`.

### Changed
- Classic mode targets FastCGI to `php-cgi` instead of the PHP CLI.
- Default `php.binary` is `php-cgi`.
- `worker.max_requests` applies to classic mode (process recycling).

### Removed
- `php.min_version`. The PHP 8.4 minimum is fixed and not configurable.

### Fixed
- LICENSE was missing the MIT liability paragraph.
- Accept errors (e.g. too many open files) no longer stop the server.
- Shutdown no longer drops in-flight connections immediately.
