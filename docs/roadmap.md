# RPHP Roadmap

Planning estimate for one developer; dates are not commitments. The detailed
tasks and exit gates for each phase are in
[`implementation-plan.md`](implementation-plan.md).

## Phase 0 — Specification and baseline (week 1)
- [x] Finalize PHP >= 8.4 policy (fixed minimum, not configurable)
- [x] Choose supported OS for MVP: Linux
- [x] Choose classic-mode backend: FastCGI to `php-cgi`
- [x] Define configuration (`rphp.example.toml`)
- [ ] Create native PHP fixture and rough PHP-FPM baseline

## Phase 1 — Rust HTTP shell (weeks 2–3)
- [x] HTTP server and `/health` (GET, HEAD)
- [x] Configuration parsing and validation
- [x] PHP binary discovery and version check
- [x] Graceful shutdown (Ctrl+C, SIGTERM) and resilient accept loop
- [x] Structured logging
- [x] Unit and integration tests for config, version, CLI, HTTP
- [x] CI green on PHP 8.4 and 8.5

## Phase 2 — Classic PHP execution over FastCGI (weeks 3–5)
- [x] FastCGI record codec with bounded lengths
- [x] `php-cgi` process pool, no shell, cleared environment
- [x] Request environment mapping (CGI/1.1 variables, httpoxy guard)
- [x] Response mapping (status, headers, cookies, streaming body)
- [x] Body size, request timeout and response header limits
- [x] Static file serving
- [x] Native PHP fixture integration tests

## Phase 3 — Supervision and backpressure (weeks 5–6)
- [x] Bounded queue and 503 backpressure
- [x] Worker crash detection and replacement
- [x] Ordered graceful shutdown of PHP processes
- [x] Failure-injection tests, no orphaned processes

## Phase 4 — Observability (week 7)
- [x] JSON logs and request IDs
- [x] Prometheus metrics
- [x] Health reflects worker availability; admin listener

## Phase 5 — Persistent worker prototype (weeks 8–10)
- [ ] Framed worker protocol (see `worker-protocol.md`)
- [ ] PHP-side PSR-7 worker loop (`rphp/worker`)
- [ ] Recycling by count, age, memory
- [ ] State leakage tests
- [ ] Explicit compatibility documentation

## Phase 6 — Framework fixtures (weeks 10–12)
- [ ] Native PHP
- [ ] Slim
- [ ] Symfony
- [ ] Laravel
- [ ] CodeIgniter
- [ ] WordPress smoke tests (classic mode)

## Phase 7 — Hardening and benchmarks (weeks 12–14)
- [ ] Load tests at multiple concurrency levels
- [ ] p50/p95/p99 latency, throughput, CPU, RAM
- [ ] Compare against PHP-FPM and FrankenPHP
- [ ] Fuzz protocol decoders
- [ ] Security review and resource limits
- [ ] Installation and deployment documentation

## Phase 8 — Research: embedded PHP SAPI
- [ ] `libphp` FFI prototype for superglobal-compatible persistent mode
