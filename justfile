set positional-arguments

default:
    @just --list

build:
    cargo build --locked --workspace

check:
    cargo check --locked --workspace --all-targets

format:
    cargo fmt --all
    nixfmt flake.nix

lint:
    cargo fmt --all -- --check
    nixfmt --check flake.nix
    cargo clippy --locked --workspace --all-targets -- -D warnings

test:
    cargo test --locked --workspace

smoke: build
    python3 tests/smoke.py "${CARGO_TARGET_DIR:-target}/debug/geata"

faults: build
    python3 tests/acme_faults.py "${CARGO_TARGET_DIR:-target}/debug/geata"

demo *args:
    python3 examples/traffic-demo/demo.py {{args}}

demo-check binary="result/bin/geata":
    python3 examples/traffic-demo/check.py "$1"
    node examples/traffic-demo/check-ui.cjs

cashu:
    cargo test --locked --workspace --test cashu

rate-limits: build
    python3 tests/rate_limits.py "${CARGO_TARGET_DIR:-target}/debug/geata"

regressions: build
    python3 tests/regressions.py "${CARGO_TARGET_DIR:-target}/debug/geata"

integration: build
    python3 tests/smoke.py "${CARGO_TARGET_DIR:-target}/debug/geata" --pebble-bin "$(dirname "$(command -v pebble)")"

quick-check: lint test

final-check:
    nix flake check path:. -L
