# bt-emulator

An offline emulator for the [Braintrust](https://www.braintrust.dev) API, written in Rust.

Point the Braintrust SDKs at `bt-emulator` and everything runs locally: no account, no network, no API key. That covers logging, experiments, datasets, summaries, and fetching data back.

> **Status:** early development. The official Python and JS SDKs can log, run experiments and
> evals, and read datasets back. Prompts, functions, attachments and OTLP ingest are not implemented yet.

## Why

- **Offline development and CI:** run evals and tracing code without a Braintrust account or network access.
- **Deterministic tests:** start from a clean, local store for every run.
- **Fast:** a single Rust binary backed by SQLite or PostgreSQL.

## Goals

- Wire compatibility with the official Braintrust SDKs (Python and TypeScript), including:
  - Login and org discovery (`/api/apikey/login`)
  - Project, experiment, and dataset registration
  - Span and event ingestion (`/logs3`)
  - The REST API (`/v1/project`, `/v1/experiment`, `/v1/dataset`, `/v1/project_logs`, …), including `insert`, `fetch`, `feedback`, and `summarize`
- Storage backends:
  - **SQLite:** zero-setup default
  - **PostgreSQL:** for shared or long-lived instances

## Usage

```bash
cargo run -- serve --listen 127.0.0.1:8000 --database-url sqlite://bt.db
# or: --database-url postgres://user@localhost/bt
```

```bash
export BRAINTRUST_API_URL=http://localhost:8000
export BRAINTRUST_APP_URL=http://localhost:8000
export BRAINTRUST_API_KEY=local
```

Then use the Braintrust SDK as usual. Any API key is accepted unless you start the
emulator with `--api-key <key>` (or `BT_EMULATOR_API_KEY`), which makes it reject others.

## Testing

Development is test-first, and CI enforces 100% line and function coverage of the Rust code.

| Layer | What it checks | Command |
|---|---|---|
| Unit + API tests | Merge semantics, BTQL filters, summaries, every endpoint, on SQLite and Postgres | `just test-all` |
| Recorded traffic replay | Cassettes harvested from the official SDK repos (`compat/`), replayed against the emulator | `cargo test --test compat` |
| OpenAPI fuzzing | schemathesis generates valid and invalid requests from Braintrust's published spec and checks every response against it (`compat/openapi/`) | `just fuzz` |
| SDK end-to-end | The real Python and JS SDKs against the binary, fully offline | `just e2e`, `just e2e-pg` |
| Everything CI runs | fmt, clippy, coverage gate on both backends | `just ci` |

Known-incompatible cassettes are listed in `compat/expected-failures.txt`; regenerate with
`just compat-bless`. The wire protocol the emulator implements is documented in
[`docs/protocol.md`](docs/protocol.md).

## Non-goals

- The Braintrust web UI
- Hosted features such as the AI proxy, online scoring, and automations (for now)

## Disclaimer

This is an independent, community project. It is not affiliated with or endorsed by Braintrust Data, Inc.
