// Starts (or attaches to) a bt-emulator for the official Braintrust JS SDK.
// BT_EMULATOR_URL: use a running emulator. Otherwise spawn BT_EMULATOR_BIN
// (default target/debug/bt-emulator) on an ephemeral port with in-memory SQLite.
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { randomUUID } from "node:crypto";
import path from "node:path";

const ROOT = path.resolve(import.meta.dirname, "../..");
export const API_KEY = "bt-emulator-local";

export async function startEmulator() {
  if (process.env.BT_EMULATOR_URL) {
    return { url: process.env.BT_EMULATOR_URL.replace(/\/$/, ""), stop() {} };
  }
  const bin = process.env.BT_EMULATOR_BIN ?? path.join(ROOT, "target/debug/bt-emulator");
  const db = process.env.BT_EMULATOR_DATABASE_URL ?? "sqlite::memory:";
  const child = spawn(bin, ["serve", "--listen", "127.0.0.1:0", "--database-url", db], {
    stdio: ["ignore", "pipe", "inherit"],
  });
  const line = await new Promise((resolve, reject) => {
    createInterface({ input: child.stdout }).once("line", resolve);
    child.once("exit", (code) => reject(new Error(`emulator exited with ${code}`)));
  });
  if (!line.startsWith("listening on ")) throw new Error(`emulator failed to start: ${line}`);
  return { url: line.slice("listening on ".length), stop: () => child.kill("SIGTERM") };
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
