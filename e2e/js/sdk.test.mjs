import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import * as braintrust from "braintrust";
import { API_KEY, api, projectName, startEmulator } from "./emulator.mjs";

let emu;
let rest;

before(async () => {
  emu = await startEmulator();
  rest = api(emu.url);
  process.env.BRAINTRUST_API_KEY = API_KEY;
  process.env.BRAINTRUST_APP_URL = emu.url;
  process.env.BRAINTRUST_API_URL = emu.url;
  await braintrust.login({ appUrl: emu.url, apiKey: API_KEY, forceLogin: true });
});

after(() => emu?.stop());

test("logger spans round trip", async () => {
  const project = projectName();
  const logger = braintrust.initLogger({ projectName: project, asyncFlush: false });
  await logger.traced(
    async (root) => {
      root.log({ input: { q: "hi" }, output: "hello", metadata: { k: "v" } });
      await root.traced((child) => child.log({ output: "nested" }), { name: "child" });
    },
    { name: "root" },
  );
  await logger.flush();

  const events = await rest.fetch("project_logs", await rest.projectId(project));
  const byName = Object.fromEntries(events.map((e) => [e.span_attributes.name, e]));
  assert.deepEqual(byName.root.input, { q: "hi" });
  assert.equal(byName.root.output, "hello");
  assert.deepEqual(byName.child.span_parents, [byName.root.span_id]);
  assert.equal(byName.child.root_span_id, byName.root.root_span_id);
});

test("experiment log and summarize", async () => {
  const project = projectName();
  const exp = braintrust.init({ project, experiment: "baseline" });
  exp.log({ input: 1, output: 2, expected: 2, scores: { exact: 1 } });
  exp.log({ input: 2, output: 3, expected: 4, scores: { exact: 0 } });
  const summary = await exp.summarize();
  assert.equal(summary.projectName, project);
  assert.equal(summary.experimentName, "baseline");
  assert.equal(summary.scores.exact.score, 0.5);
});

test("dataset insert, update, delete, iterate", async () => {
  const project = projectName();
  const ds = braintrust.initDataset({ project, dataset: "golden" });
  const keep = ds.insert({ input: { x: 1 }, expected: 2 });
  const drop = ds.insert({ input: { x: 9 }, expected: 0 });
  ds.update({ id: keep, metadata: { reviewed: true } });
  ds.delete(drop);
  await ds.flush();

  const rows = [];
  for await (const row of braintrust.initDataset({ project, dataset: "golden" })) rows.push(row);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].id, keep);
  assert.deepEqual(rows[0].metadata, { reviewed: true });
});

test("Eval with dataset end to end", async () => {
  const project = projectName();
  const ds = braintrust.initDataset({ project, dataset: "math" });
  for (let x = 0; x < 5; x++) ds.insert({ input: x, expected: x * 2 });
  await ds.flush();

  const result = await braintrust.Eval(project, {
    data: braintrust.initDataset({ project, dataset: "math" }),
    task: (x) => (x < 4 ? x * 2 : -1),
    scores: [({ output, expected }) => ({ name: "exact", score: output === expected ? 1 : 0 })],
    experimentName: "doubling",
  });
  assert.equal(result.summary.experimentName, "doubling");
  assert.equal(result.summary.scores.exact.score, 0.8);
  assert.equal(result.results.length, 5);
});

test("loadPrompt reads the latest and a pinned version", async () => {
  const project = projectName();
  const { id: projectId } = await rest.request("POST", "/v1/project", { name: project });
  const save = (greeting) =>
    rest.request("PUT", "/v1/prompt", {
      project_id: projectId,
      name: "Greeter",
      slug: "greeter",
      prompt_data: {
        options: { model: "gpt-4o-mini" },
        prompt: { type: "chat", messages: [{ role: "user", content: `${greeting} {{name}}` }] },
      },
    });
  const v1 = await save("Hello");
  await save("Howdy");

  const latest = await braintrust.loadPrompt({ projectName: project, slug: "greeter", noTrace: true });
  assert.deepEqual(latest.build({ name: "Ada" }).messages, [{ role: "user", content: "Howdy Ada" }]);

  const pinned = await braintrust.loadPrompt({ projectName: project, slug: "greeter", version: v1._xact_id, noTrace: true });
  assert.equal(pinned.build({ name: "Ada" }).messages[0].content, "Hello Ada");
});

test("invoke runs prompt and inline code functions", async () => {
  const project = projectName();
  const { id: projectId } = await rest.request("POST", "/v1/project", { name: project });
  const create = (slug, fields) => rest.request("PUT", "/v1/function", { project_id: projectId, name: slug, slug, ...fields });
  await create("greet", {
    function_data: { type: "prompt" },
    prompt_data: {
      options: { model: "stub" },
      prompt: { type: "chat", messages: [{ role: "user", content: "Say hello to {{input}}" }] },
    },
  });
  await create("double", {
    function_data: {
      type: "code",
      data: { type: "inline", runtime_context: { runtime: "node", version: "22" }, code: "function handler(x) { return x * 2; }" },
    },
  });

  assert.equal(await braintrust.invoke({ projectName: project, slug: "greet", input: "world" }), "echo: Say hello to world");
  const stream = await braintrust.invoke({ projectName: project, slug: "greet", input: "Ada", stream: true });
  assert.equal(await stream.finalValue(), "echo: Say hello to Ada");
  assert.equal(await braintrust.invoke({ projectName: project, slug: "double", input: 21 }), 42);
});

test("Eval with a remote LLM classifier", async () => {
  const project = projectName();
  const { id: projectId } = await rest.request("POST", "/v1/project", { name: project });
  await rest.request("PUT", "/v1/function", {
    project_id: projectId,
    name: "judge",
    slug: "judge",
    function_type: "scorer",
    function_data: { type: "prompt" },
    prompt_data: {
      options: { model: "stub" },
      parser: { type: "llm_classifier", use_cot: true, choice_scores: { correct: 1, wrong: 0 } },
      prompt: { type: "chat", messages: [{ role: "user", content: "{{output}} vs {{expected}}: {{metadata.verdict}}" }] },
    },
  });
  // The stub model picks the choice its prompt names, here via the case metadata.
  const result = await braintrust.Eval(project, {
    experimentName: "remote-judge",
    data: [
      { input: "a", expected: "A", metadata: { verdict: "correct" } },
      { input: "b", expected: "X", metadata: { verdict: "wrong" } },
    ],
    task: (input) => input.toUpperCase(),
    scores: [braintrust.initFunction({ projectName: project, slug: "judge" })],
  });
  assert.deepEqual(result.results.map((r) => Object.values(r.scores)[0]).sort(), [0, 1]);
});

test("attachments upload and read back", async () => {
  const project = projectName();
  const logger = braintrust.initLogger({ projectName: project, asyncFlush: false });
  const audio = new braintrust.Attachment({
    data: new Blob([new TextEncoder().encode("RIFF....WAVE")], { type: "audio/wav" }),
    filename: "clip.wav",
    contentType: "audio/wav",
  });
  logger.log({ input: { audio }, output: "ok" });
  await logger.flush();

  const [row] = await rest.fetch("project_logs", await rest.projectId(project));
  const reference = row.input.audio;
  assert.equal(reference.type, "braintrust_attachment");
  const attachment = new braintrust.ReadonlyAttachment(reference);
  assert.deepEqual(await attachment.status(), { upload_status: "done" });
  assert.equal(await (await attachment.data()).text(), "RIFF....WAVE");
});
