#!/usr/bin/env sh
# Run RPHP against a real PHP inside a container, without installing PHP on
# the host.
#
#   scripts/test-php.sh            build the image if needed, run all tests
#   scripts/test-php.sh serve      run `rphp serve` (reachable on the host)
#   scripts/test-php.sh shell      open a shell in the container
#   scripts/test-php.sh <cmd...>   run any command, e.g. `cargo clippy`
#
# Env: RPHP_IMAGE (default rphp-test:php84), CONTAINER (docker or podman).
set -eu

cd "$(dirname "$0")/.."
image="${RPHP_IMAGE:-rphp-test:php84}"
engine="${CONTAINER:-$(command -v podman >/dev/null 2>&1 && echo podman || echo docker)}"

if ! "$engine" image inspect "$image" >/dev/null 2>&1; then
    echo "building $image (first run only)..." >&2
    "$engine" build -t "$image" -f docker/test.Containerfile docker
fi

# Container builds go to target/container so they never clash with host
# builds; the Cargo registry is cached in a named volume.
run() {
    "$engine" run --rm "$@"
}
common="-v $PWD:/work -v rphp-cargo:/usr/local/cargo/registry -w /work \
    -e CARGO_TARGET_DIR=/work/target/container -e RUST_LOG -e RPHP_LOG_FORMAT"

cmd="${1:-test}"
# `shift` with no arguments aborts a POSIX shell, so only shift when present.
if [ $# -gt 0 ]; then shift; fi

case "$cmd" in
test)
    # shellcheck disable=SC2086
    run $common -e RPHP_REQUIRE_PHP=1 "$image" cargo test "$@"
    ;;
serve)
    # Host networking so 127.0.0.1:8080 (and the admin port) are reachable.
    # shellcheck disable=SC2086
    run -it --network host $common "$image" cargo run -- serve "$@"
    ;;
shell)
    # shellcheck disable=SC2086
    run -it --network host $common -e RPHP_REQUIRE_PHP=1 "$image" bash
    ;;
*)
    # shellcheck disable=SC2086
    run $common -e RPHP_REQUIRE_PHP=1 "$image" "$cmd" "$@"
    ;;
esac
