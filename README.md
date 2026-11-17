# bt-emulator

An offline emulator for the [Braintrust](https://www.braintrust.dev) API, written in Rust.

Point the Braintrust SDKs at `bt-emulator` and everything runs locally: no account, no network, no API key. That covers logging, experiments, datasets, summaries, and fetching data back.

> **Status:** early development. The official Python, JS and Go SDKs can log and trace
> (including OpenTelemetry/OTLP), run experiments and evals, read datasets back, store and
> load versioned prompts and functions, and invoke them against a local model you configure.
> Attachments are not implemented yet.

## Why

- **Offline development and CI:** run evals and tracing code without a Braintrust account or network access.
- **Deterministic tests:** start from a clean, local store for every run.
- **Fast:** a single Rust binary backed by SQLite or PostgreSQL.

## Goals

- Wire compatibility with the official Braintrust SDKs (Python, TypeScript and Go), including:
  - Login and org discovery (`/api/apikey/login`)
  - Project, experiment, and dataset registration
  - Span and event ingestion (`/logs3`), and OpenTelemetry traces (`/otel/v1/traces`, as the Go SDK and `braintrust.otel` export them)
  - The REST API (`/v1/project`, `/v1/experiment`, `/v1/dataset`, `/v1/project_logs`, …), including `insert`, `fetch`, `feedback`, and `summarize`
  - Versioned prompts and functions (`/v1/prompt`, `/v1/function`), as loaded by `load_prompt`/`loadPrompt`
  - Function invocation (`invoke`, remote scorers and tasks): prompts run against any OpenAI-compatible API you point it at; inline code runs locally if you allow it
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

### Invoking functions

Invoking a prompt (`braintrust.invoke`, `init_function` scorers, the Go SDK's function
tasks and scorers) needs a model. Point the emulator at any OpenAI-compatible API, such as
Ollama, vLLM, LiteLLM or OpenAI itself:

```bash
bt-emulator serve --llm-base-url http://localhost:11434/v1   # Ollama
bt-emulator serve --llm-base-url https://api.openai.com/v1 --llm-api-key "$OPENAI_API_KEY"
```

The prompt's own `model` is passed through. Inline code functions (Node or Python) run in
local `node`/`python3` processes only with `--allow-code-execution`. Leave that off when
others can reach the emulator, since it runs their code on your machine. See
[`docs/protocol.md` §16](docs/protocol.md) for the details.

## Testing

Development is test-first, and CI enforces 100% line and function coverage of the Rust code.

| Layer | What it checks | Command |
|---|---|---|
| Unit + API tests | Merge semantics, BTQL filters, summaries, every endpoint, on SQLite and Postgres | `just test-all` |
| Recorded traffic replay | Cassettes harvested from the official SDK repos (`compat/`), replayed against the emulator | `cargo test --test compat` |
| OpenAPI fuzzing | schemathesis generates valid and invalid requests from Braintrust's published spec and checks every response against it (`compat/openapi/`) | `just fuzz` |
| SDK end-to-end | The real Python, JS and Go SDKs against the binary, fully offline (prompts run against a local stub model) | `just e2e`, `just e2e-pg` |
| Everything CI runs | fmt, clippy, coverage gate on both backends | `just ci` |

Known-incompatible cassettes are listed in `compat/expected-failures.txt`; regenerate with
`just compat-bless`. The wire protocol the emulator implements is documented in
[`docs/protocol.md`](docs/protocol.md).

## Non-goals

- The Braintrust web UI
- Hosted features such as the AI proxy, online scoring, and automations (for now)

## Disclaimer

This is an independent, community project. It is not affiliated with or endorsed by Braintrust Data, Inc.
