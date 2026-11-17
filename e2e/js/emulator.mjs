// Starts (or attaches to) a bt-emulator for the official Braintrust JS SDK.
// BT_EMULATOR_URL: use a running emulator. Otherwise spawn BT_EMULATOR_BIN
// (default target/debug/bt-emulator) on an ephemeral port with in-memory SQLite,
// code execution enabled, and prompts running against a local stub model.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { createInterface } from "node:readline";
import { randomUUID } from "node:crypto";
import path from "node:path";

const ROOT = path.resolve(import.meta.dirname, "../..");
export const API_KEY = "bt-emulator-local";

// A deterministic OpenAI-compatible model: it echoes the last message, and an LLM
// classifier picks the choice named in the last message.
async function startStubModel() {
  const server = createServer(async (req, res) => {
    let raw = "";
    for await (const chunk of req) raw += chunk;
    const request = JSON.parse(raw);
    const last = request.messages.at(-1).content;
    const tool = request.tools?.[0]?.function;
    let message;
    if (tool?.name === "select_choice") {
      const choices = tool.parameters.properties.choice.enum;
      const choice = choices.find((c) => last.includes(c)) ?? choices[0];
      const args = JSON.stringify({ choice, reasoning: "stub" });
      message = { role: "assistant", tool_calls: [{ id: "1", type: "function", function: { name: "select_choice", arguments: args } }] };
    } else {
      message = { role: "assistant", content: `echo: ${last}` };
    }
    res.setHeader("Content-Type", "application/json");
    res.end(JSON.stringify({ choices: [{ index: 0, message, finish_reason: "stop" }] }));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { url: `http://127.0.0.1:${server.address().port}/v1`, stop: () => server.close() };
}

export async function startEmulator() {
  if (process.env.BT_EMULATOR_URL) {
    return { url: process.env.BT_EMULATOR_URL.replace(/\/$/, ""), stop() {} };
  }
  const model = await startStubModel();
  const bin = process.env.BT_EMULATOR_BIN ?? path.join(ROOT, "target/debug/bt-emulator");
  const db = process.env.BT_EMULATOR_DATABASE_URL ?? "sqlite::memory:";
  const args = ["serve", "--listen", "127.0.0.1:0", "--database-url", db, "--llm-base-url", model.url, "--allow-code-execution"];
  const child = spawn(bin, args, {
    stdio: ["ignore", "pipe", "inherit"],
  });
  const line = await new Promise((resolve, reject) => {
    createInterface({ input: child.stdout }).once("line", resolve);
    child.once("exit", (code) => reject(new Error(`emulator exited with ${code}`)));
  });
  if (!line.startsWith("listening on ")) throw new Error(`emulator failed to start: ${line}`);
  const stop = () => {
    child.kill("SIGTERM");
    model.stop();
  };
  return { url: line.slice("listening on ".length), stop };
}

export function projectName() {
  return `e2e-${randomUUID().slice(0, 12)}`;
}

export function api(base) {
  const request = async (method, p, body) => {
    const res = await fetch(base + p, {
      method,
      headers: { Authorization: `Bearer ${API_KEY}`, "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (!res.ok) throw new Error(`${method} ${p}: ${res.status} ${await res.text()}`);
    return res.json();
  };
  return {
    request,
    async projectId(name) {
      const { objects } = await request("GET", `/v1/project?project_name=${name}`);
      if (objects.length !== 1) throw new Error(`expected 1 project, got ${objects.length}`);
      return objects[0].id;
    },
    async fetch(objectType, id) {
      return (await request("POST", `/v1/${objectType}/${id}/fetch`, {})).events;
    },
  };
}
