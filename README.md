# bt-emulator

An offline emulator for the [Braintrust](https://www.braintrust.dev) API, written in Rust.

Point the Braintrust SDKs at `bt-emulator` and everything runs locally: no account, no network, no API key. That covers logging, experiments, datasets, summaries, and fetching data back.

> **Status:** early development. Not usable yet.

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

## Usage (planned)

```bash
bt-emulator serve --port 8000 --database sqlite://bt.db
```

```bash
export BRAINTRUST_API_URL=http://localhost:8000
export BRAINTRUST_APP_URL=http://localhost:8000
export BRAINTRUST_API_KEY=local
```

Then use the Braintrust SDK as usual.

## Non-goals

- The Braintrust web UI
- Hosted features such as the AI proxy, online scoring, and automations (for now)

## Disclaimer

This is an independent, community project. It is not affiliated with or endorsed by Braintrust Data, Inc.
