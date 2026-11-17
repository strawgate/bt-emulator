# Braintrust SDK <-> API wire protocol (for bt-emulator)

## TL;DR

- **Two API families have to be served.** The current Python SDK (0.50.0, `clones/py/py/src/braintrust/version.py:1`) uses the REST endpoints under **`/v1/*`** for registration, fetch and summarize. The JS SDK (npm braintrust 3.44.0) and older Python SDKs still use the **legacy app endpoints**: `/api/project/register`, `/api/experiment/register`, `/api/dataset/register`, `/api/experiment/get`, `/api/base_experiment/get_id`, `/experiment-comparison2`, `/dataset-summary`. All SDKs write rows through **`POST /logs3`**. The Go SDK uses `/v1/*` REST and exports spans over OTLP (`/otel/v1/traces`).
- **Hosts.** `app_url` (`BRAINTRUST_APP_URL`) serves `/api/apikey/login` and the `/api/*` legacy calls. `api_url` serves everything else. `BRAINTRUST_API_URL` overrides the `api_url` that login returns, in both Python and JS. There are no path collisions between the two, so one server can serve both if the two env vars point at it.
- **Auth.** Every SDK request sends `Authorization: Bearer <key>`. Signed-URL uploads are the exception and send no auth.
- **Rows.** `/logs3` takes `{"rows":[...],"api_version":2}`. Rows are addressed by `experiment_id`, `dataset_id`, or `project_id` + `log_id:"g"` (project logs).
  - The server assigns **one `_xact_id` per request**. Format: `(0x0DE1<<48) | (unix_secs<<16) | seq16`, sent as a decimal string.
  - The server keeps **one current row per id**: `_is_merge` deep-merges into the existing row, a row without it replaces the existing row, and `_object_delete` removes it.
- **Reads.**
  - Python reads experiments and datasets with `POST /v1/{experiment|dataset}/{id}/fetch`. The body is `{limit, cursor?, version?}` and the response is `{events, cursor?}`.
  - JS reads them with `POST /btql`. The response is `{data, schema, cursor?, ...}`.
  - Cassettes show the **cursor is present iff the page is non-empty**, and the SDKs loop until they get a page with no cursor. Pages are newest first.
- **Create semantics** (from cassettes):
  - `POST /v1/project` and `POST /v1/dataset` are get-or-create by name. They return the existing object **unchanged**.
  - `POST /v1/experiment` with `ensure_new` false or absent returns the existing experiment of that name. With `ensure_new:true`, if the name is taken, it creates `name-<8 hex>`.
- **Summaries are parsed strictly.** Python builds `ScoreSummary(**v)`, `MetricSummary(**v)` and `DataSummary(new_records, **raw)` directly from the response, so the emulator must emit **exactly** the documented keys. An extra key raises a TypeError.

Conventions: `py:` = `clones/py/py/src/braintrust/`, `js:` = `js/package/dist/chunk-XQRKU3GF.js` (the JS core chunk) unless noted, and `go:` = `clones/go/`. Cassette paths are relative to their clone root. Real examples are trimmed.

---

## 0. Transport-level facts

| Fact | Evidence |
|---|---|
| Python legacy `HTTPConnection` sets `Authorization: Bearer <token>` as a session header and joins paths with `_urljoin(base, path)` | py:api/_transport.py:152-184 |
| Python generated client (`/v1/*`) sends `Authorization: Bearer <key>` on every op | py:api/_service.py:61-71 |
| Python `/logs3` is posted with `data=bytes` and **no Content-Type** header. The server must parse the body as JSON whatever the header says. | py:logger.py:1591. Cassettes: 17 of 18 `/logs3` requests have no Content-Type. |
| JS always sends `Accept: application/json` and `Content-Type: application/json` | js:6727-6760 |
| JS `post_json`/`get_json` parse the response as JSON. Every endpoint (including `/logs3`) must therefore return a JSON body on 2xx. | js:6800-6831, 8491-8496 |
| Python `/btql` and JS `/btql` send `Accept-Encoding: gzip`. A plain response is fine. | py:logger.py:3749-3751, js:10812 |
| Query arrays are repeated-key form (`ids=a&ids=b`). Booleans are `true`/`false`. | py:api/_service.py:142-159 |
| Python generated-client retries: 408/429/500/502/503/504 for SAFE_READ and IDEMPOTENT_WRITE ops. POST /v1/experiment, PATCH and DELETE are `NONE` (1 attempt). | py:api/policies.py:8, 55-60; py:api/_generated/experiments.py (POST_EXPERIMENT retry_mode NONE) |
| A non-200 on any generated op raises `BraintrustHTTPError` (a subclass of `BraintrustAPIError`) | py:api/_service.py:122-132, api/errors.py:22-26 |
| IDs: row `id` = uuid4. `span_id` = 16 hex chars. `root_span_id` = 32 hex chars (OTEL mode, the default). With `BRAINTRUST_LEGACY_IDS`, both are uuid4 and `root_span_id == span_id` for root spans. The server must treat all of them as opaque strings. | py:id_gen.py:8-64, logger.py:5180-5195 |
| **JS validates `Project.id` and `Project.org_id` as UUIDs (zod)**, so the emulator must mint real UUIDs | js:3078-3087; dist/index.js:14460-14476 |

---

## 1. Login / org discovery

### `POST {app_url}/api/apikey/login`

- Python: no body (`Content-Length: 0`), header `Authorization: Bearer <key>` (py:api/auth.py:92-97). Cassette: `py/src/braintrust/api/cassettes/test_high_level_experiment_uses_generated_resources.yaml` (request body `null`).
- JS: `Content-Type: application/json` plus Bearer, no body (js:9767-9774).
- Go: `client.POST(ctx, "/api/apikey/login", nil)` (go:internal/auth/auth.go:92). Go cassettes recorded it against `api.braintrust.dev`, because the test app URL equals the API URL.

Response (real):
```json
{"org_info":[{"id":"5abfae3a-7aa7-4653-a9c8-b3efcb18f584","name":"Braintrust SDKs",
  "api_url":"https://api.braintrust.dev",
  "git_metadata":{"collect":"some","fields":["commit","branch","tag","dirty","author_name","author_email","commit_message","commit_time"]},
  "is_universal_api":null,"proxy_url":"https://api.braintrust.dev","realtime_url":"wss://realtime.braintrustapi.com"}]}
```

Fields the SDKs read:

| field | required | used by |
|---|---|---|
| `org_info` (list) | yes. Python raises if it is missing or not a list. | py:api/auth.py:98-102 |
| `org_info[].id`, `.name` (strings) | yes. Python raises if either is not a string. | py:api/auth.py:30-34 |
| `api_url` | yes, unless `BRAINTRUST_API_URL` is set. Precedence: env > org value. | py:api/auth.py:106-115; js:10485 |
| `proxy_url` | optional (env `BRAINTRUST_PROXY_URL` overrides) | py:api/auth.py:107; js:10486 |
| `is_universal_api` | optional, coerced with `bool()` | py:api/auth.py:51 |
| `git_metadata` | optional. Python builds `GitMetadataSettings(**git_metadata)`, so keys must be limited to `collect` and `fields`. Recommend `null` for offline use so no git calls/diffs run. | py:logger.py:2706-2708 |
| `realtime_url` | ignored | — |

Org selection: the first org, or the one whose `name == org_name` (env `BRAINTRUST_ORG_NAME`). Otherwise the SDK raises "Organization X not found" (py:api/auth.py:131-139). An empty list raises "This user is not part of any organizations".

Errors: a non-2xx makes Python raise `ValueError("Invalid API key ...: [status] body")` (py:logger.py:2652-2656). For Go, 401/403 are an auth error and other statuses are retried (go:internal/auth/auth.go:93-110).

`TEST_API_KEY` short-circuit: Python `"___TEST_API_KEY__"` (py:logger.py:266, 2624-2648) and JS `"___TEST_API_KEY__THIS_IS_NOT_REAL___"` (js:12534) skip login entirely. This is not useful for the emulator.

### `GET {api_url}/ping`
Python `user_info()` only (py:logger.py:982-985). No high-level flow calls it. JS `ping()` checks `status===200` (js:6693-6699). Return `{}` (shape unknown; nothing reads fields).

---

## 2. `GET {api_url}/version`

Called once per background logger before the first flush. Python: `conn.get_json("version")` → `info.get("logs3_payload_max_bytes")` (py:logger.py:1344-1376). JS: js:8224-8226. A failure is only printed and the defaults are used.

Real response:
```json
{"version":"2.11.0","date_version":"20260819","ff_version":46,"commit":"e27b1434...","deployment_mode":"lambda",
 "brainstore_default":"force","has_logs2":true,"universal":true,"code_execution":true,
 "logs3_payload_max_bytes":5242880, ...}
```
(`py/src/braintrust/api/cassettes/test_experiment_summarize_with_real_backend[True].yaml`)

SDK reads only `logs3_payload_max_bytes`:
- If present and > 0: max request size = min(override, value) and `can_use_overflow=true`.
- If absent: `DEFAULT_MAX_REQUEST_SIZE = 6 MiB` (py:logger.py:141; js:5821) and **overflow is disabled**.

**Emulator recommendation:** return `{"version":"bt-emulator", ...}` **without** `logs3_payload_max_bytes`, or with a very large value. Then the overflow path (section 9) is never used, and the emulator must accept bodies of at least 6 MiB.

---

## 3. Projects

### 3.1 `/v1/project` (api_url)
Operation metadata: py:api/_generated/projects.py:19-128. Python callers:
- logger: `POST /v1/project {"name": project or "Global", "org_name": <org>}`, plus optional `project_group_name` (py:logger.py:2116-2121)
- experiment: py:logger.py:1888-1893
- dataset: py:logger.py:2047-2050
- `Project.lazy_init`: py:logger.py:6240-6249

| Op | Request | Response | Notes |
|---|---|---|---|
| `POST /v1/project` | `{name: str, org_name?: str, description?: str, project_group_name?: str}` (py:api/_generated/models/projects.py:16-29). Go sends just `{"name":...}`. | `Project` | **Get-or-create by (org, name). Returns the existing project unchanged.** Evidence: `py/src/braintrust/api/cassettes/test_projects_end_to_end_with_real_backend.yaml`. A 2nd POST without `description` returned the PATCHed description and the same id. Retry mode IDEMPOTENT_WRITE. |
| `GET /v1/project` | query: `limit, starting_after, ending_before, ids (repeat), project_name, org_name`. Go also sends `org_id` (go:api/projects/projects.go:98-99). | `{"objects":[Project...]}` | Excludes deleted rows |
| `GET /v1/project/{id}` | — | `Project` | A non-UUID id gives 400 (zod array text), see §11 |
| `PATCH /v1/project/{id}` | `{name?, description?, settings?, only_if_unset?, user_id?}` (models/projects.py:201-215) | `Project` (updated) | |
| `DELETE /v1/project/{id}` | — | `Project` with `deleted_at` set | soft delete |

`Project` object (real, from the cassette above):
```json
{"id":"7cb22019-3d25-4b51-98a1-63c46abd8bf3","org_id":"5abfae3a-...","name":"python-sdk-generated-projects-vcr",
 "description":"created by the Python SDK VCR test","created":"2026-08-19T15:34:34.696Z","deleted_at":null,
 "user_id":"c1f71e19-...","settings":null}
```
Python reads `id` and `name` (py:logger.py:1893-1900, 2122-2125). JS zod requires uuid `id`, uuid `org_id`, and `name` (js:3078-3087).

### 3.2 Legacy project endpoints (app_url): JS SDK

| Op | Request | Response the SDK reads | Evidence |
|---|---|---|---|
| `POST /api/project/register` | `{project_name, org_id, project_group_name?}`. Undefined keys are dropped by JSON.stringify. | `{"project": Project}`. Reads `project.id`, `project.name`. `ProjectNameIdMap` zod-parses `{project: Project}`. | js:9288-9304; dist/index.js:14456-14463 |
| `GET /api/project?id=<id>` | — | JS reads `response.name` (and `response.project` as fullInfo). **Uncertain:** the likely shape is a bare `Project`. Return a Project with an extra `"project": Project` key to satisfy both readings. | js:9308-9319 |
| `POST /api/project/get` | `{id}` | non-empty array of `Project` | dist/index.js:14471-14476 |

---

## 4. Experiments

### 4.1 `POST {api}/v1/experiment` (create/register): Python and Go

Python body construction is at py:logger.py:1895-1964:
```text
project_id: str (required)
ensure_new: bool                   = (update is not True)   # i.e. default true for braintrust.init()
name?: str
description?: str
repo_info?: {commit,branch,tag,dirty,author_name,author_email,commit_message,commit_time,git_diff}
base_exp_id?: str                  # explicit, or resolved from base_experiment name via GET /v1/experiment
ancestor_commits?: [sha...]        # only when git collect != none and no base given
dataset_id?: str, dataset_version?: str   # dataset_version = Dataset.version (max _xact_id string)
parameters_id?, parameters_version?, public?: bool, metadata?: obj, tags?: [str]
```
The schema also allows `internal_metadata: {dataset_filter}` (py:api/_generated/models/experiments.py:270-320). JS sends it.

Real request (`py/src/braintrust/api/cassettes/test_experiment_summarize_with_real_backend[True].yaml`):
```json
{"project_id":"c70d5397-...","ensure_new":false,"name":"generated-experiments-candidate",
 "repo_info":{"commit":null,"branch":null,"tag":null,"dirty":null,"author_name":null,"author_email":null,"commit_message":null,"commit_time":null,"git_diff":null},
 "base_exp_id":"067c3728-...","public":false}
```
Response = `Experiment`:
```json
{"id":"b28b747b-...","project_id":"c70d5397-...","name":"generated-experiments-candidate","description":null,
 "created":"2026-08-18T01:12:52.485Z","repo_info":{...},"commit":null,"base_exp_id":"067c3728-...","deleted_at":null,
 "dataset_id":null,"dataset_version":null,"internal_metadata":null,"parameters_id":null,"parameters_version":null,
 "public":false,"user_id":"c1f71e19-...","metadata":null,"tags":null}
```
Python reads `id` and `name`, and keeps the rest as `full_info` (py:logger.py:1966-1969). `public` is required (bool) in the schema (models/experiments.py:383).

**Name and ensure_new semantics** (Go and Python cassettes):

| Case | Result | Evidence |
|---|---|---|
| name new, any ensure_new | created with that exact name | `go:eval/testdata/cassettes/TestWorkflowExperimentIntegration.yaml` (`ensure_new:true` → `TestWorkflowExperimentIntegration`) |
| name exists, ensure_new false/absent | **returns the existing experiment** (same id, unchanged) | `go:openapi/testdata/cassettes/TestGeneratedExperimentLifecycle.yaml` (2 POSTs → same id `dcc8b67f`); `go:eval/testdata/cassettes/TestEval_Integration_UpdateFlag.yaml` |
| name exists, ensure_new true | new experiment named `<name>-<8 lowercase hex>` | `go:openapi/testdata/cassettes/TestGeneratedExperimentEnsureNew.yaml` (`ensure-new-test` → `ensure-new-test-8f33f3db`); UpdateFlag: `…-ea8ebef6` → `…-ea8ebef6-758d0fa9` |
| name absent | server generates a name. Only legacy evidence exists: `"<branch>-<unix_secs>"` (e.g. `matt/re-1761833612`, from `py/src/braintrust/devserver/cassettes/test_eval_with_dataset_id_completes_successfully.yaml`). **Uncertain** what is used without a branch. Suggest `experiment-<unix_secs>` (or `<repo_info.branch>-<unix_secs>`). | |

The Go SDK sends `ensure_new: !update` with `omitempty`, so `update=true` omits the key (go:api/experiments/experiments.go:76-88, types.go:38). The default server value must therefore be **false**.

`base_exp_id` when absent: the server may pick one from `ancestor_commits` (most recent experiment whose `repo_info.commit` is in the list). **Uncertain** about the exact algorithm. For the emulator it is enough to store `base_exp_id` as given. Summarize falls back to "most recent prior experiment in the project" (docstring py:logger.py:4838).

### 4.2 Lookup/list/get/patch/delete
- `GET /v1/experiment?limit&starting_after&ending_before&ids&experiment_name&project_name&project_id&org_name` → `{"objects":[Experiment...]}` (py:api/_generated/experiments.py GET_EXPERIMENT).
  - Python `init(open=True)` sends `experiment_name`, `project_name|project_id`, `org_name`. It reads `objects[0].id`, `.name`, `.project_id`. Empty `objects` → `ValueError` (py:logger.py:1856-1879).
  - Python base-experiment-by-name sends `experiment_name`, `project_id`, `org_name` and uses `objects[0].id`. If empty it warns and continues (py:logger.py:1924-1935).
  - Real: `GET /v1/experiment?experiment_name=high-level-generated-experiment&project_name=python-sdk-high-level-experiments-vcr&org_name=Braintrust%20SDKs` → `{"objects":[{...}]}` (`py/src/braintrust/api/cassettes/test_high_level_experiment_uses_generated_resources.yaml`).
  - Go: `GET /v1/experiment?limit=100&project_id=…` returns newest first (`go:openapi/testdata/cassettes/TestGeneratedExperimentLifecycle.yaml`).
- `GET /v1/experiment/{id}` → `Experiment`. After delete (or when not found): **403** `{"Code":"ForbiddenError","Message":"Missing read access to experiment id <id>, or the experiment does not exist [...]"}` (same Go cassette). JS calls this to get the comparison name (js:11084).
- `PATCH /v1/experiment/{id}` (`PatchExperiment` fields, models/experiments.py:412-454) → `Experiment`.
- `DELETE /v1/experiment/{id}` → `Experiment` with `deleted_at`.

### 4.3 Legacy experiment endpoints (app_url): JS SDK and old Python
- **`POST /api/experiment/register`** (js:8838-8910). Body:
  - `{project_name?, project_id?, org_id, update?: bool, experiment_name?, description?, repo_info?, base_exp_id? | base_experiment?(name) | ancestor_commits?, dataset_id?, dataset_version?, internal_metadata?: {dataset_filter}, parameters_id?, parameters_version?, public?, metadata?, tags?}`
  - Old Python sends nulls explicitly. Real keys: `ancestor_commits, dataset_id, dataset_version, org_id, project_id, project_name, public, repo_info, update` (`py/src/braintrust/devserver/cassettes/test_eval_with_dataset_id_completes_successfully.yaml`).
  - Response: `{"project": Project, "experiment": Experiment}`. JS reads `project.id/name` and `experiment.id/name/created` (js:8906-8919).
  - Semantics: `update:true` → reuse the existing experiment. Otherwise always create, deduplicating the name. Name absent → generated.
  - If `base_experiment` (a name) is not found, the error body must contain the text `base experiment`. JS then retries without it (js:8896-8904).
- **`POST /api/experiment/get`** `{project_name?, project_id?, org_name, experiment_name}` → **array** of Experiment. JS reads `[0].id/name/project_id`, and an empty array is an error (js:8783-8810).
- **`POST /api/base_experiment/get_id`** (both SDKs). Body `{"id": <experiment_id>}`.
  - 200: `{"id","project_id","name","base_exp_id","base_exp_name"}`. Real: `{"id":"735629b8-...","project_id":"0e748405-...","name":"matt/re-1761833612","base_exp_id":"699db184-...","base_exp_name":"matt/re-1761833608"}`.
  - **No base → return HTTP 400.** Python returns `None` on 400 (py:logger.py:4815-4829) and JS returns `null` (js:11030-11050). A 200 with `null` works for Python but crashes JS (`base["base_exp_id"]` on null). Real 400 body (bad uuid): zod array text (`py/src/braintrust/cassettes/test_scorer_spans_have_purpose_attribute.yaml`).
  - Python calls it only from `Eval` with `BaseExperiment()` data (py:framework.py:2002).

---

## 5. Datasets

### 5.1 `/v1/dataset` (api_url)
- **`POST /v1/dataset`**
  - Body `{project_id, name, description?, metadata?, tags?}` (models/datasets.py:16-33). Python: `{project_id, name: name or "logs", description?, metadata?}` (py:logger.py:2051-2056).
  - Response `Dataset`. **Get-or-create by (project_id, name) and return the existing one unchanged.** Evidence: `go:eval/testdata/cassettes/TestDatasetAPI_Query_Integration.yaml`. The request had `description`, but the response has `description:null` and an older `created`.
  - Real: `{"id":"e0753a78-...","project_id":"8bc899ea-...","name":"logs","description":"created through braintrust.init_dataset","created":"2026-08-18T17:43:50.112Z","deleted_at":null,"user_id":"...","tags":null,"metadata":null,"url_slug":"logs"}` (`py/src/braintrust/api/cassettes/test_high_level_dataset_uses_generated_resources.yaml`). `url_slug` is required in the schema (models/datasets.py:79).
- `GET /v1/dataset/{id}` → `Dataset`. Python `init_dataset(dataset_id=...)` calls this and then `GET /v1/project/{project_id}` (py:logger.py:2040-2042).
- **`GET /v1/dataset`**
  - Query: `limit, starting_after, ending_before, ids, dataset_name, project_name, project_id, org_name`. Go also sends `id` and `version` (go:api/datasets/datasets.go:134-147).
  - Response `{"objects":[...]}`.
  - **Order: newest created first.** `starting_after=<id>` returns the items after that id in this order (`go:openapi/testdata/cassettes/TestGeneratedDatasetQueryFilters/project_and_pagination.yaml`).
  - A deleted dataset with `id=` filter → `{"objects":[]}` (`go:openapi/testdata/cassettes/TestGeneratedDatasetLifecycle.yaml`).
- `PATCH /v1/dataset/{id}` `{name?, description?, metadata?, tags?}` → `Dataset`. `DELETE /v1/dataset/{id}` → `Dataset` with `deleted_at`.

### 5.2 Legacy dataset endpoints (app_url): JS SDK and old Python
- **`POST /api/dataset/register`** `{org_id, project_name?, project_id?, dataset_name, description?, metadata?}`
  - Response `{"project": Project, "dataset": Dataset, "found_existing": bool}`. Real response includes `"found_existing":true` (`py/src/braintrust/devserver/cassettes/test_eval_with_dataset_id_completes_successfully.yaml`).
  - JS reads `project.id/name` and `dataset.id/name` (js:9193-9213). Get-or-create.
- **`POST /api/self/get_object_info`** `{object_type:"dataset", object_ids:[id]}` → `[{object_id, object_name, parent_cols:{project:{id,name}}}]` (zod js:5826-5835, call js:9165-9190). Used when JS `initDataset({datasetId})`.
- **`GET {api}/dataset-summary?dataset_id=<id>`** → `{"total_records": number}` (zod, js:12009-12026).
- `GET {api}/environment-object/dataset/{id}/{env}?org_name=` → `{object_version}`. Only used with `environment=` (py:logger.py:5681-5690; js:9031). Low priority.

---

## 6. `POST {api_url}/logs3`: ingestion (all writes from Python/JS)

### 6.1 Envelope
`'{"rows": ' + json_array + ', "api_version": 2}'` (py:logger.py:1119-1121, DATA_API_VERSION=2 at :138; js:7866-7868). Rows are batched (≤100 rows default, ≤ max request size). Python sends batches concurrently from a thread pool, and waves sequentially (py:logger.py:1376-1407). **The server must therefore not assume cross-request ordering inside a wave.** The client already merged same-id rows inside a wave (§6.4).

Response (real): `{"ids":["generated-experiments-base-row"],"xact_id":"1000197715909279737"}`. Python only checks `resp.ok`; JS requires valid JSON. On failure Python retries `BRAINTRUST_NUM_RETRIES` (2) + 1 = 3 times with backoff. **413 is not retried and the batch is dropped** (py:logger.py:1594-1627).

### 6.2 Row shapes actually sent

Object addressing (exactly one of):
- experiment row: `experiment_id` (py:span_identifier_v3.py:201-202)
- project logs: `project_id` + `log_id: "g"` (py:span_identifier_v3.py:203-204; js:995)
- playground logs: `prompt_session_id` + `log_id: "x"`
- dataset row: `dataset_id` (py:logger.py:5761-5765)

**Span row** (Experiment/Logger/Span). The first write for a span is a replace, and later writes are `_is_merge: true` (py:logger.py:5197-5203, 5243-5251). Real (`[True]` summarize cassette):
```json
{"context":{"caller_filename":"...","caller_functionname":"pytest_pyfunc_call","caller_lineno":167,
   "span_origin":{"instrumentation":{"name":"braintrust-python-logger"},"name":"braintrust.sdk.python","version":"0.34.0"}},
 "created":"2026-08-19T18:46:04.996084+00:00","expected":"correct","experiment_id":"b28b747b-...",
 "id":"generated-experiments-candidate-row","input":{"question":"What is the answer?"},
 "metrics":{"completion_tokens":10,"end":1787165164.996673,"start":1787165164.995522},"output":"correct",
 "root_span_id":"b4009b5cf5a2884619885a6467e93204","scores":{"accuracy":1},
 "span_attributes":{"exec_counter":4,"name":"root","type":"eval"},"span_id":"2f4c4a510964b778","span_parents":null}
```
Typical end-of-span merge row (`py/src/braintrust/integrations/harbor/cassettes/0.16.0/test_atif_import_round_trips_with_real_sdks.yaml`): `{"_is_merge":true,"experiment_id":..,"id":..,"metrics":{"end":1767225603},"root_span_id":..,"span_id":..,"span_parents":null}`.

User fields: `input, output, expected, error, scores{name:number|null}, metadata{}, metrics{start,end,tokens,...}, tags[], span_attributes{name,type,purpose?,exec_counter}, context{}, origin{object_type,object_id,id,_xact_id,created?}`.

**Dataset rows** (py:logger.py:5739-5767, 5852-5876):
- insert: `{id, input, expected, tags, metadata?, created: iso, dataset_id}`. No `_is_merge`, so it replaces (upsert). No span ids.
- update: `{id, ...non-null fields, _is_merge: true, dataset_id}`.
- delete: `{id, created, _object_delete: true, dataset_id}`.

**Feedback** (py:logger.py:4164-4240):
- score/expected/tags update: `{id: <target row id>, scores?, expected?, tags?, <object ids>, _audit_source: "external"|"app"|"api", _audit_metadata: <metadata|null>, _is_merge: true}`
- comment: a **separate new row** `{id: uuid4, created, origin: {id: <target row id>}, comment: {text}, <object ids>, _audit_source, _audit_metadata}`
- JS is equivalent.

**update_span** (py:logger.py:4243-4284): `{id, ...fields, root_span_id?, span_id?, <object ids>, _is_merge: true}`.

**Eval rows with a dataset** (py:framework.py:1725-1749) carry `origin: {object_type:"dataset", object_id, id, _xact_id, created?}`. Real: `"origin":{"object_type":"dataset","object_id":"f1423e15-...","id":"34dc9bc0-...","created":"2025-10-29T17:04:33.010Z","_xact_id":"1000196050791148460"}`.

### 6.3 Reserved control fields (py:db_fields.py)
`_xact_id, _object_delete, created, id, _is_merge, _merge_paths, _array_delete, _audit_source, _audit_metadata, _parent_id, _async_scoring_control, _skip_async_scoring`. The emulator must strip these from stored rows. It should map `_audit_source`/`_audit_metadata` into the `audit_data` entry. Neither SDK emits `_merge_paths` or `_array_delete` in normal flows: they only appear in pass-through lists (py:span_customizer.py:85-92; js:394). Implement them anyway: a `_merge_paths` path is replaced, not deep-merged.

### 6.4 Client-side pre-merge (what arrives)
Rows with the same key `(org_id, project_id, experiment_id, dataset_id, prompt_session_id, log_id, id)` inside one flush are collapsed (py:merge_row_batch.py:15-27, 59-147):
- A merge row is merged into the earlier row.
- `created, span_id, root_span_id, span_parents, _parent_id` keep the **first** row's values (py:merge_row_batch.py:31-38).
- If the earlier row was a replace, the result stays a replace (`_is_merge` deleted, :139-142).

The server still sees cross-flush merges (e.g. start row in flush 1, `metrics.end` merge in flush 2).

### 6.5 Server merge semantics to replicate
State model: one **current row per (object, id)**, plus history for `version` reads and `audit_data`.
1. Assign one `_xact_id` per `/logs3` (or `/v1/.../insert`) request. All rows in the request share it. Evidence: 6 rows → one `xact_id` (`py/src/braintrust/cassettes/test_span_filters_backend_parity.yaml`).
2. `_object_delete: true` → the row disappears from reads.
3. `_is_merge: true` and an existing row → deep merge (`util.merge_dicts` semantics: dicts recurse, other values replace, and top-level `tags` take a set-union unless listed in `_merge_paths`; py:util.py:58-102).
   - **Uncertain:** whether the server does the `tags` union exactly as the client does. The client implements it to mirror the server.
   - `created` keeps the original value. Evidence: merged read-back row keeps the first write's `created`, `_xact_id` = latest write's xact, and `metrics` = `{"end":..,"start":..}` (harbor 0.16.0 cassette).
4. `_is_merge: true` and no existing row → insert as new (Go workflow insert with `_is_merge:true` was stored and read back; `go:eval/testdata/cassettes/TestWorkflowExperimentIntegration.yaml`).
5. Otherwise replace the whole row.
6. Row `_xact_id` = xact of the latest write that touched it.
7. `audit_data` appends `{"_xact_id", "audit_data":{"action":"upsert"} | {"action":"merge","from":null,"path":[...],"to":...}, "metadata":{}, "source":"api"}`. Nothing in the SDKs reads it, except the prompt-version helper (py:logger.py:6976-6980, js:12603-12606), which filters `audit_data.action in (upsert, merge)`.
8. Derived fields on read:
   - `project_id` (from the parent object; experiment and dataset rows include it although never sent)
   - `is_root` = `span_parents` null/empty
   - `span_id`/`root_span_id` default: if missing (dataset rows, `/v1` inserts) set both to a fresh uuid4. Real dataset fetch shows `span_id == root_span_id == <random uuid>` ≠ id.
   - `created` default to server now if missing
   - `created` format on read is ISO with ms and `Z` (`"2026-08-18T17:32:54.877Z"`). The client sent `"...54.876745+00:00"` and the server rounded it.
   - all schema columns present, with null if absent

### 6.6 `_xact_id` format
- 64-bit decimal string `= (0x0DE1 << 48) | (unix_seconds << 16) | seq16`.
- Verified by decoding cassette values: `1000197715909279737` → secs 1787165165 (2026-08-19T18:46:05, request time 18:46:04), seq 65529.
- Constant from py:xact_ids.py:1. It must be strictly increasing (the SDK compares versions as **strings**: `max(str(_xact_id))`, py:logger.py:3784-3789). Same width keeps lexicographic order equal to numeric order.
- `prettify_xact` (py:xact_ids.py:10-12) is only used for display/prompt versions.

---

## 7. Reading data back

### 7.1 `POST {api}/v1/experiment/{id}/fetch` and `POST {api}/v1/dataset/{id}/fetch`: Python SDK and Go

| | |
|---|---|
| Request | `{"limit": int, "cursor"?: str, "version"?: str}`. Python sends `limit` = 1000 by default (`batch_size`), plus `cursor` from the previous page, plus `version` (dataset only, when pinned) (py:logger.py:4582-4612 experiment, 5692-5722 dataset). Go uses `limit:100` or `2` (go cassettes). Schema also allows deprecated `max_xact_id`, `max_root_span_id` (models/common.py:755-760). |
| Response | `{"events":[row...], "cursor"?: str}`. Python loops until `cursor` is falsy. `events` must be a list or Python raises (py:logger.py:4596-4600). |
| Cursor rule | **Non-empty page → include `cursor`. Empty page → omit it.** All 23 non-empty fetch pages had a cursor, even when `len < limit`, and all 14 empty pages had none (computed over every py and go cassette). The cursor is opaque (`"aoSZ1pNdAAA"`). |
| Order | newest first: descending `_xact_id`, and within one xact reverse insertion order. Go inserted q1,q2,q3 in one call and fetched q3 first (`go:eval/testdata/cassettes/TestDatasetAPI_Query_Integration.yaml`). |
| `version` | snapshot read: the row state as of writes with `_xact_id <= version`. |
| GET variants | `GET .../fetch?limit&max_xact_id&max_root_span_id&version` exist (py:api/_generated/experiments.py GET_EXPERIMENT_ID_FETCH) but no SDK high-level path uses them. Real: one GET dataset fetch appears in py cassettes. |

Experiment event (real, `py/src/braintrust/api/cassettes/test_high_level_experiment_uses_generated_resources.yaml`):
```json
{"_pagination_key":"p07675815845862375424","_xact_id":"1000197715907835377",
 "audit_data":[{"_xact_id":"1000197715907835377","audit_data":{"action":"upsert"},"metadata":{},"source":"api"}],
 "classifications":null,"comments":null,"context":null,"created":"2026-08-19T18:45:42.956Z","error":null,"expected":null,
 "experiment_id":"6a0fc93e-...","facets":null,"id":"high-level-generated-experiment-row","input":{"question":"What is the answer?"},
 "is_root":true,"metadata":null,"metrics":null,"origin":null,"output":"42","project_id":"fd57931d-...",
 "root_span_id":"high-level-generated-experiment-row","scores":{"accuracy":1},
 "span_attributes":{"created_by_api_key_id":"...","created_by_user_id":"..."},"span_id":"high-level-generated-experiment-row",
 "span_parents":null,"tags":null}
```
Dataset event keys: `_pagination_key,_xact_id,audit_data,classifications,comments,created,dataset_id,expected,facets,id,input,is_root,metadata,origin,project_id,root_span_id,span_id,tags`. Required by schema: `_xact_id, created, dataset_id, id, project_id, root_span_id, span_id` (models/datasets.py:208-274). Experiment events add `context,error,experiment_id,metrics,output,scores,span_attributes,span_parents` (models/experiments.py:476-567).

Fields the SDK depends on:
- `ExperimentDatasetIterator` (`exp.as_dataset()`, Eval on `BaseExperiment`) indexes `root_span_id`, `span_id`, `id`, `_xact_id` (KeyError if missing). It keeps only rows with `root_span_id == span_id` (py:logger.py:4556-4578).
- Eval with a Dataset reads `id`, `_xact_id`, `created`, `input`, `expected`, `metadata`, `tags`, `origin` (py:framework.py:1725-1749).
- `Dataset.version` = max `_xact_id` over the fetched rows (py:logger.py:3784-3789).
- `_pagination_key` (`"p" + 20 digits`) is not read by any SDK.

### 7.2 `POST {api}/btql`: JS object fetch, Python with `_internal_btql`, Python trace span fetch

Literal request (Python ObjectFetcher, py:logger.py:3727-3752; real from `py/src/braintrust/cassettes/test_dataset_internal_btql_limit_caps_total_results.yaml`):
```json
{"query":{"select":[{"op":"star"}],
          "from":{"op":"function","name":{"op":"ident","name":["dataset"]},"args":[{"op":"literal","value":"93e21c91-..."}]},
          "cursor":null,"limit":1, "...": "plus any _internal_btql keys e.g. filter"},
 "use_columnstore":false,"brainstore_realtime":true,"query_source":"py_sdk_object_fetcher_dataset",
 "version":"<xact>"            // top-level, only if pinned
}
```
- JS is the same, but `query_source: "js_sdk_object_fetcher_<type>"`, `cursor` is omitted (undefined) on the first page, and `limit` is capped by `_internal_btql.limit` (js:10768-10835).
- `name` ∈ `experiment | dataset | project_logs | project_prompts` (`project_prompts` with `"audit_log": true`, py:logger.py:6932-6966).
- Filter ops used by SDKs: `and, or, eq, ne, in, isnull, isnotnull` over `{"op":"ident","name":[path...]}` and `{"op":"literal","value":...}` (py:trace.py:146-170, 228-275). Real filter from `py/src/braintrust/cassettes/test_span_filters_backend_parity.yaml`:
  `{"op":"and","children":[{"op":"eq","left":{"op":"ident","name":["root_span_id"]},"right":{"op":"literal","value":"span-filters-root"}},{"op":"or","children":[{"op":"isnull","expr":{"op":"ident","name":["span_attributes","purpose"]}},{"op":"ne",...,"right":{"op":"literal","value":"scorer"}}]},{"op":"in","left":{"op":"ident","name":["span_attributes","type"]},"right":{"op":"literal","value":["tool"]}}]}`

Response (real keys): `{"data":[row...], "schema":{...JSON schema...}, "cursor"?: str, "realtime_state":{"type":"on","minimum_xact_id":..,"read_bytes":..,"actual_xact_id":..}, "freshness_state":{...}, "warnings":[]}`.
- **SDK reads only `data` and `cursor`** (py:logger.py:3756-3766; js:10817-10830).
- Rows are identical to §7.1 events.
- The cursor rule is the same (18 of 18 non-empty pages had a cursor, 20 of 20 empty pages had none).
- Python caps total rows with `_internal_btql.limit` (py:logger.py:3711-3716, 3759-3761).

---

## 8. Summaries

### 8.1 `GET {api}/v1/experiment/{id}/summarize?summarize_scores=true[&comparison_experiment_id=<id>]`: Python
- Called by `Experiment.summarize()` after flush (py:logger.py:4831-4900). Not called when `summarize_scores=False`. Eval calls `summarize(False)` at start (no request, py:framework.py:1303-1308) and the full summarize at the end (py:framework.py:1546).
- If `comparison_experiment_id` is omitted, the server uses `base_exp_id`, else the most recent prior experiment in the project (docstring py:logger.py:4838).
- Response (real, `py/src/braintrust/api/cassettes/test_experiment_summarize_with_real_backend[True].yaml`):
```json
{"project_name":"python-sdk-generated-experiments-vcr","project_url":"https://www.braintrust.dev/app/Braintrust%20SDKs/p/...",
 "experiment_name":"generated-experiments-candidate","experiment_url":"...","comparison_experiment_name":"generated-experiments-base",
 "scores":{"accuracy":{"name":"accuracy","score":1,"diff":1,"improvements":1,"regressions":0}},
 "metrics":{"llm_calls":{"name":"llm_calls","metric":0,"unit":"","diff":0,"improvements":0,"regressions":0},
            "completion_tokens":{"name":"completion_tokens","metric":10,"unit":"tok","diff":-10,"improvements":1,"regressions":0},
            "duration":{"name":"duration","metric":0.0011510848999023438,"unit":"s","diff":-0.8208298683166504,"improvements":1,"regressions":0}, ...}}
```
- Metric keys observed: `llm_calls, tool_calls, errors, llm_errors, tool_errors, prompt_tokens, prompt_cached_tokens, prompt_cache_creation_tokens, prompt_cache_creation_5m_tokens, prompt_cache_creation_1h_tokens, completion_tokens, completion_reasoning_tokens, total_tokens, duration`.
- **Python reads `comparison_experiment_name`, `scores`, `metrics` only** (py:logger.py:4867-4878). It builds `ScoreSummary(_longest_score_name=…, **v)` and `MetricSummary(…, **v)`. The dataclasses accept **only** `{name, score, improvements, regressions, diff?}` and `{name, metric, unit, improvements, regressions, diff?}` (py:logger.py:6656-6672, 6694-6711). **Any other key raises TypeError.**
- A `BraintrustAPIError` (non-200) is caught and turned into a warning and a "comparison could not be retrieved" result. It is not fatal.

Semantics inferred from the cassette (scores: candidate 1 vs base 0; tokens 10 vs 20; duration):
- `score` = mean of that score over rows that have it (root/eval spans).
- `diff` = current − comparison. It is omitted when there is no comparison.
- `improvements`/`regressions` = count of matched pairs (matched by `input`, the default comparison key) where the value went up/down. For metrics like tokens/duration, **lower is better**: tokens 10 vs 20 → `improvements:1`.
- `duration` = mean of root `metrics.end - metrics.start`. Verified: 0.996673 − 0.995522 = 0.0011511. `completion_tokens`/`total_tokens` = per-trace sum, then mean (**uncertain**: only single-row evidence). Units: `""`, `"tok"`, `"s"`.
- **Uncertain:** exact aggregation for multi-span traces and the `errors`/`llm_calls` counting (likely count of spans with `span_attributes.type=="llm"`/`"tool"` and non-null `error` per trace, then mean).

### 8.2 `GET {api}/experiment-comparison2?experiment_id=<id>[&base_experiment_id=<id>]`: JS (and old Python)
JS flow (js:11055-11106):
1. If no explicit comparison id, `POST /api/base_experiment/get_id`. Otherwise `GET /v1/experiment/{cmp}` for its `name`.
2. `GET /experiment-comparison2` (3 retries).
3. Read `scores` and `metrics`. Errors are caught and turned into `{}`.

Real response: `{"scores":{"scorer":{"name":"scorer","score":0,"improvements":0,"regressions":0}},"metrics":{"llm_calls":{"name":"llm_calls","metric":0,"unit":"","improvements":0,"regressions":0},...,"duration":{"name":"duration","metric":0.00044,"unit":"s","improvements":0,"regressions":0}}}` (`py/src/braintrust/devserver/cassettes/test_eval_with_dataset_id_completes_successfully.yaml`). Use the same computation as §8.1.

### 8.3 `GET {api}/v1/dataset/{id}/summarize?summarize_data=true`: Python
Real: `{"project_name":"...","project_url":"...","dataset_name":"logs","dataset_url":"...","data_summary":{"total_records":1}}` (`py/src/braintrust/api/cassettes/test_high_level_dataset_uses_generated_resources.yaml`).
- Python reads only `data_summary` and builds `DataSummary(new_records=…, **raw)`. **`data_summary` must contain only `total_records`** (py:logger.py:5889-5894, 6867-6872).
- `total_records` = count of live (non-deleted) rows.

---

## 9. Overflow / large payloads (note only)
Overflow is enabled only if `/version` returns `logs3_payload_max_bytes > 0` and a batch exceeds it (py:logger.py:1556-1560).
1. `POST /logs3/overflow` with `{"content_type":"application/json","size_bytes":N,"rows":[{"object_ids":{...},"has_comment":bool,"is_delete":bool,"input_row":{"byte_size":n}}]}`. Expected response: `{"method":"PUT"|"POST","signedUrl":url,"headers":{..}(PUT),"fields":{..}(POST),"key":str}` (py:logger.py:1491-1527; zod js:7859-7865).
2. Upload the payload to `signedUrl`.
3. `POST /logs3` with `{"rows":{"type":"logs3_overflow","key":key,"size_bytes":N},"api_version":2}` (py:logger.py:1124-1128).

**Recommendation:** omit `logs3_payload_max_bytes` so overflow never triggers (§2).

Attachments (only if rows contain `Attachment` objects):
- `POST {api}/attachment` `{key, filename, content_type, org_id}` → `{"signedUrl": str, "headers": {...}}`. Headers must be a dict (py:logger.py:3879-3903).
- Then the SDK sends `PUT <signedUrl>` with no auth.
- Then `POST {api}/attachment/status` `{key, org_id, status:{upload_status:"done"|"error", error_message?}}` → JSON, e.g. `{"status":"success"}`.
- Read: `GET {api}/attachment?filename&content_type&org_id&key|url` → `{"downloadUrl": str, "status": {"upload_status":"done"}}` (py:logger.py:4108-4131).
- Real examples are in `py/src/braintrust/integrations/pipecat/cassettes/latest/*.yaml`. The emulator can point `signedUrl` at itself.

---

## 10. `/v1/.../insert` and `/v1/.../feedback` (REST writes)
- `POST /v1/{experiment|dataset}/{id}/insert` `{"events":[InsertEvent...]}` → `{"row_ids":[...]}`. This is one xact per request, with ids generated (uuid4) when absent and returned in input order.
  - Real: Go dataset insert of 3 rows without ids → 3 uuids (`go:eval/testdata/cassettes/TestDatasetAPI_Query_Integration.yaml`).
  - Python high-level insert: `{"events":[{"id":"high-level-generated-row","input":{...},"expected":"42"}]}` → `{"row_ids":["high-level-generated-row"]}`.
  - Events accept the same control fields as logs3 (`_is_merge`, `_merge_paths`, `_object_delete`, `_parent_id`, `_array_delete`) (models/experiments.py:586-694). Used by Go (workflow eval) and by Python only via the raw generated client.
- `POST /v1/{experiment|dataset}/{id}/feedback` `{"feedback":[{id, scores?, expected?, comment?, metadata?, source?, tags?}]}` (models/experiments.py:89-116, 398-399). One appears in a py cassette (`/v1/dataset/{id}/feedback`). Implement it as a merge row plus a comment row (same as §6.2 feedback). Response shape not verified in this pass.

---

## 11. Errors the SDKs depend on

| Situation | Real server | SDK behaviour | Emulator |
|---|---|---|---|
| bad/non-UUID id in path | 400, body = zod issue array text + `[user_email=..]...` suffix (`go:api/testdata/cassettes/TestCompatibilityRegisterAndErrors/server_error.yaml`) | raises | 400 `[{"code":"invalid_string","message":"Invalid uuid","path":["id"]}]` |
| missing/deleted object GET | 403 `{"Code":"ForbiddenError","Message":"Missing read access to experiment id X, or the experiment does not exist ..."}` (`go:openapi/testdata/cassettes/TestGeneratedExperimentLifecycle.yaml`) | raises | 403 same shape (likewise for project/dataset; only experiment is evidenced) |
| list with no match | 200 `{"objects":[]}` | Python `init(open=True)` → ValueError "not found" | same |
| base experiment absent | 400 | Python `None` / JS `null` | **400** |
| legacy register: base_experiment name missing | error text containing "base experiment" | JS retries without it (js:8896-8904) | 400 with that text |
| `/logs3` 413 | — | dropped, no retry | avoid; accept large bodies |
| project group mismatch | 409 (docstring py:logger.py:2160) | raises | optional |
| summarize failure | — | warning only | — |

---

## 12. Go SDK notes
- REST CRUD is entirely `/v1/*` (Go cassette tally): `POST /v1/project` ×63, `POST /v1/experiment` ×25, `POST /v1/dataset/{id}/fetch` ×16, `POST /v1/dataset` ×13, `POST /v1/dataset/{id}/insert` ×10, `DELETE /v1/dataset/{id}` ×8, `GET /v1/dataset?…`, `GET /v1/project?…`, `GET /v1/experiment?limit&project_id`, `GET/DELETE /v1/experiment/{id}`, `/v1/experiment/{id}/insert|fetch`.
  - Also `/v1/function` (list `?project_name&slug&limit`, `PUT` upsert, `/{id}/invoke`), `/v1/prompt`, and `/v1/project_automation` (out of scope here; invoke needs an LLM).
- Extra query params Go sends that are not in the Python spec: `org_id` (project list), `id` and `version` (dataset list) (go:api/projects/projects.go:98-99, go:api/datasets/datasets.go:143-147). Unknown params must be tolerated.
- **Go span ingestion uses OTLP/HTTP `POST {api_url}/otel/v1/traces`** (protobuf, gzip, `Authorization: Bearer`, parent via the `x-bt-parent` header) (go:trace/trace.go:884-905; py:http_headers.py:4). It does not use `/logs3`. Go eval cassettes contain no OTLP calls, so this path has no recorded examples. It is required for Go tracing/eval results, but it is a separate workstream.

---

## 13. Priority (minimum endpoint sets)

| Scenario | Python 0.50 | JS 3.44 |
|---|---|---|
| **(a)** `init_logger(...).log(...)` + flush | `POST app /api/apikey/login` → `POST /v1/project` → `GET /version` → `POST /logs3` (rows with `project_id`+`log_id:"g"`) | login → `POST app /api/project/register` → `/version` → `/logs3` |
| **(b)** `init(project=…)` log + `summarize()` | (a)'s login, `/v1/project`, then `POST /v1/experiment`, `/version`, `/logs3`, `GET /v1/experiment/{id}/summarize`. With a `base_experiment` name, also `GET /v1/experiment?experiment_name&project_id&org_name`. | login → `POST /api/experiment/register` → `/version` → `/logs3` → `POST /api/base_experiment/get_id` → `GET /experiment-comparison2` (or `GET /v1/experiment/{id}` when an explicit comparison id is given) |
| **(c)** `init_dataset` insert + iterate | login → `/v1/project` → `POST /v1/dataset` → `/version` → `/logs3` (dataset rows) → `POST /v1/dataset/{id}/fetch` (paged; cursor rule!). With `dataset_id=`: `GET /v1/dataset/{id}` + `GET /v1/project/{id}`. Optional `GET /v1/dataset/{id}/summarize`. | login → `POST /api/dataset/register` → `/logs3` → `POST /btql` (`dataset`) → optional `GET /dataset-summary` |
| **(d)** `Eval(...)` with a dataset | (c) + `Dataset.version` (fetch, cached) + `POST /v1/project` + `POST /v1/experiment` (with `dataset_id`/`dataset_version`, `ensure_new:true` → name dedupe) + `/logs3` (rows with `origin`) + `GET /v1/experiment/{id}/summarize`. Trace-aware scorers may also call `POST /btql` with filter (`experiment`). `BaseExperiment()` data needs `/api/base_experiment/get_id` + `GET /v1/experiment` + experiment fetch. | (c) + (b) |
| **(e)** Go REST CRUD (cassettes) | `POST/GET(list,id)/PATCH/DELETE /v1/project`, `/v1/experiment`, `/v1/dataset` + `/insert`, `/fetch`. List filters: `limit, starting_after, ending_before, ids/id, *_name, project_id, org_id, org_name`. Newest first. Deleted excluded. Get-or-create + ensure_new rules (§3.1, §4.1, §5.1). 400/403 errors (§11). | — |

Suggested build order:
1. login + `/version` + `/v1/project` + `/logs3` store with merge semantics
2. `/v1/experiment` create/list/get + `/v1/{experiment,dataset}/{id}/fetch` with the cursor rule
3. `/v1/dataset` + summarize endpoints
4. legacy `/api/*` register/get + `/btql` (JS)
5. REST insert, feedback, patch/delete
6. attachments, overflow, OTLP

## 14. Open uncertainties
- Server-generated experiment name when `name` is absent and no git branch is known.
- The exact summarize aggregation (multi-span traces, error/llm/tool counts, token sum vs mean). The comparison key is `input` by default (project setting `comparison_key`, models/projects.py:161).
- `GET /api/project?id=` response shape (JS reads `.name` and `.project`).
- Whether the server applies a `tags` set-union on merge (the client does, py:util.py:77-90).
- 403 vs 404 for missing projects/datasets (only the experiment 403 is recorded).
- `/v1/.../feedback` response body and `/ping` response body (no SDK flow reads either).
