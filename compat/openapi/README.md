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

## Spec corrections

Where recorded SDK traffic (compat/corpus) shows the real API accepting input the spec
rejects, the emulator follows the recordings. `correct()` in `src/api/validate.rs`
applies these to the spec when it is loaded:

| Schema | Correction | Evidence |
|---|---|---|
| `FunctionTypeEnum`, `FunctionTypeEnumNullish` | adds `"prompt"` | Go SDK cassettes create prompts with `function_type: "prompt"` and get 200. |

## Accepted deviations (`baseline.json`)

Each entry in `baseline.json` is a deliberate difference from a literal reading of the
spec. Any failure not listed fails the run.

| Operations | Check | Why the emulator differs |
|---|---|---|
| `GET`/`POST .../fetch` (experiment, dataset, project_logs), `GET /v1/function`, `GET /v1/prompt` and their by-id reads | positive data rejected (400) | `version` is a transaction id (or its 16-hex display form). The spec types it as any string; the emulator rejects anything else instead of silently ignoring it. |
| `GET`, `PATCH`, `DELETE` on `/v1/function/{id}` and `/v1/prompt/{id}` | positive data rejected (400) | A missing function or prompt is a 400 `BadRequestError` ("Prompt does not exist or you do not have access"), as recorded from the real API in the Go SDK's cassettes. |
| `POST /v1/experiment`, `PATCH /v1/experiment/{id}`, `PATCH /v1/project/{id}` | positive data rejected (400) | In JSON Schema 2020-12, `format: uuid` is only an annotation, so `""` is technically valid. The emulator enforces formats, like the real API's validators. |
| `POST .../insert` (experiment, dataset, project_logs) | positive data rejected (400) | Same, for `format: date-time` on `created`. |
| `PATCH` on projects, experiments, datasets | undocumented status code (409) | Renaming onto an existing name returns 409 Conflict, which the spec does not list. |

Regenerate after an intentional change with
`just fuzz --baseline-update --baseline-prune` and update the table above.
