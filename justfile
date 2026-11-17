set dotenv-load := false

pg_dir := justfile_directory() / "target/pgdata"
pg_port := "54329"
pg_url := "postgres://postgres@127.0.0.1:" + pg_port + "/postgres"

default:
    @just --list

# Format the code.
fmt:
    cargo fmt --all

# Run clippy with warnings denied.
lint:
    cargo clippy --all-targets -- -D warnings

# Run tests. Postgres tests run when BT_TEST_POSTGRES_URL is set (see `just pg-start`).
test *args:
    cargo test {{args}}

# Run tests against both backends using the local throwaway Postgres.
test-all *args: pg-start
    BT_TEST_POSTGRES_URL={{pg_url}} cargo test {{args}}

# Enforce 100% line and function coverage.
cov:
    cargo llvm-cov --fail-under-lines 100 --fail-under-functions 100 --show-missing-lines

# Coverage across both backends.
cov-all: pg-start
    BT_TEST_POSTGRES_URL={{pg_url}} just cov

# Everything CI runs. Uses BT_TEST_POSTGRES_URL if set, else a local throwaway cluster.
ci:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo fmt --all -- --check
    just lint
    if [ -z "${BT_TEST_POSTGRES_URL:-}" ]; then just pg-start; export BT_TEST_POSTGRES_URL={{pg_url}}; fi
    BT_REQUIRE_POSTGRES=1 just cov

# Start a throwaway local Postgres cluster (uses local postgres binaries, no Docker).
pg-start:
    #!/usr/bin/env bash
    set -euo pipefail
    if pg_ctl -D "{{pg_dir}}" status >/dev/null 2>&1; then exit 0; fi
    if [ ! -d "{{pg_dir}}" ]; then initdb -D "{{pg_dir}}" -U postgres --auth=trust >/dev/null; fi
    pg_ctl -D "{{pg_dir}}" -o "-p {{pg_port}} -k /tmp -c listen_addresses=127.0.0.1" -l "{{pg_dir}}/log" -w start >/dev/null
    echo "postgres running at {{pg_url}}"

# Stop the throwaway Postgres cluster.
pg-stop:
    pg_ctl -D "{{pg_dir}}" -m fast stop

# Run the emulator.
serve *args:
    cargo run -- serve {{args}}

# Run the official Braintrust SDKs (Python + JS) against the emulator binary.
e2e: e2e-py e2e-js

e2e-py:
    cargo build
    cd e2e/python && uv run pytest -q

e2e-js:
    cargo build
    cd e2e/js && npm ci --silent && npm test

# e2e against a Postgres-backed emulator.
e2e-pg: pg-start
    #!/usr/bin/env bash
    set -euo pipefail
    db="bt_e2e_$(date +%s)"
    psql -h 127.0.0.1 -p {{pg_port}} -U postgres -qc "CREATE DATABASE $db"
    BT_EMULATOR_DATABASE_URL="postgres://postgres@127.0.0.1:{{pg_port}}/$db" just e2e

# Re-harvest recorded Braintrust traffic from upstream repos (compat/sources.toml).
compat-harvest *args:
    uv run -q --with pyyaml python -I compat/harvest.py {{args}}

# Rewrite compat/expected-failures.txt from the current replay results.
compat-bless:
    BT_COMPAT_BLESS=1 cargo test --test compat -- --nocapture
