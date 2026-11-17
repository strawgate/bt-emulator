# OpenAPI conformance fuzzing

`spec.json` is Braintrust's published OpenAPI spec, vendored from
[braintrustdata/braintrust-openapi](https://github.com/braintrustdata/braintrust-openapi)
at commit `374478b4a6214e5cf1f32551703bdbc4cea5a7b3` under the Apache-2.0 license (`LICENSE`).

It is used twice:

1. **At runtime**, the emulator validates every `/v1/*` request's query parameters and
   JSON body against the spec's schemas (`src/api/validate.rs`), rejecting invalid
   input with 400 as the real API does.
2. **In tests**, `just fuzz` runs [schemathesis](https://github.com/schemathesis/schemathesis)
   (MIT) against a local emulator started on `127.0.0.1`. It generates valid and invalid
   requests for every implemented operation, checks responses against the spec, and runs
   stateful multi-request chains. It never contacts Braintrust.

## Accepted deviations (`baseline.json`)

Each entry in `baseline.json` is a deliberate difference from a literal reading of the
spec. Any failure not listed fails the run.

| Operations | Check | Why the emulator differs |
|---|---|---|
| `GET`/`POST .../fetch` (experiment, dataset, project_logs) | positive data rejected (400) | `version` is a transaction id. The spec types it as any string; the emulator rejects non-numeric values instead of silently ignoring them. |
| `POST /v1/experiment`, `PATCH /v1/experiment/{id}`, `PATCH /v1/project/{id}` | positive data rejected (400) | In JSON Schema 2020-12, `format: uuid` is only an annotation, so `""` is technically valid. The emulator enforces formats, like the real API's validators. |
| `POST .../insert` (experiment, dataset, project_logs) | positive data rejected (400) | Same, for `format: date-time` on `created`. |
| `PATCH` on projects, experiments, datasets | undocumented status code (409) | Renaming onto an existing name returns 409 Conflict, which the spec does not list. |

Regenerate after an intentional change with
`just fuzz --baseline-update --baseline-prune` and update the table above.
