# Recorded-traffic compatibility corpus

`corpus/` holds HTTP interactions harvested by `harvest.py` from the cassettes that the
official Braintrust SDKs commit to their own repos. The sources are pinned in `sources.toml`,
and only permissively licensed sources are harvested. `tests/compat.rs` replays each
cassette against a fresh emulator on every backend and checks status codes, the keys and
types of every recorded JSON value, and that strings the client sent come back where the
recording echoed them.

The harness reproduces the recording's environment, never its answers:
- **Pre-existing objects.** Projects, experiments, datasets, functions and base
  experiments that a response shows but the cassette never created are created through the
  public API first. Only names, parents and settings are seeded, never rows.
- **One org.** The emulator is started with the recording org's name, and every recorded
  org id maps to the emulator's.
- **Rejected keys.** A cassette whose logins were all rejected runs against an emulator
  started with `--api-key` set to a different key.
- **Function invocations.** Invokes run against a stub model that replies with the
  recorded output: a `select_choice` tool call for classifiers, text otherwise. Inline code
  really runs, in local `node`.

## What does not replay (`expected-failures.txt`)

| Cassettes | Why |
|---|---|
| `test_register_sandbox_*` (2) | `/function/sandbox-list` runs the user's eval file in a remote (Modal) sandbox to list its evaluators. The emulator has no sandbox. |
| `auth__login_retry`, `eval__auto_init` | Server-side fault injection: a login that returns 500 twice and then 200, and logins that alternate between keys the recording strips. |
| `test_dataset_internal_btql_limit_caps_total_results`, `test_span_filters_backend_parity` | The BTQL response's `schema` is the server's internal row schema. The emulator returns the spec's event schema; a few nested `classifications` details differ in the server's revision. No SDK reads it. |
| `test_eval_with_dataset_id_completes_successfully` | Reads dataset rows that existed before the recording. Seeding rows would replay the answer, so the harness does not. |
| `btql__trace_spans` | The spans it queries were written over OTLP, which the Ruby SDK's VCR does not record. |
| `test_git_metadata_is_preserved_in_braintrust_data` | The recorded fetch returns fields (`context`, ids) that the recorded `/logs3` request never sent, so the request was recorded from a different run. |
