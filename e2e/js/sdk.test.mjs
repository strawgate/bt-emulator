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
