"""Runs the official Braintrust Python SDK against a real bt-emulator process.

Set BT_EMULATOR_URL to test an already-running emulator; otherwise the binary at
BT_EMULATOR_BIN (default: target/debug/bt-emulator) is started on an ephemeral port
with an in-memory SQLite database (override with BT_EMULATOR_DATABASE_URL), code
execution enabled, and prompts running against a local stub model.
"""

import http.server
import json
import os
import pathlib
import subprocess
import threading
import urllib.request
import uuid

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[2]
API_KEY = "bt-emulator-local"


class StubModel(http.server.BaseHTTPRequestHandler):
    """A deterministic OpenAI-compatible model: it echoes the last message, and an
    LLM classifier picks the choice named in the last message."""

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        last = request["messages"][-1]["content"]
        tools = request.get("tools") or []
        if tools and tools[0]["function"]["name"] == "select_choice":
            choices = tools[0]["function"]["parameters"]["properties"]["choice"]["enum"]
            choice = next((c for c in choices if c in last), choices[0])
            args = json.dumps({"choice": choice, "reasoning": "stub"})
            message = {"role": "assistant", "tool_calls": [{"id": "1", "type": "function", "function": {"name": "select_choice", "arguments": args}}]}
        else:
            message = {"role": "assistant", "content": f"echo: {last}"}
        body = json.dumps({"choices": [{"index": 0, "message": message, "finish_reason": "stop"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


@pytest.fixture(scope="session")
def model_url():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), StubModel)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}/v1"
    server.shutdown()


@pytest.fixture(scope="session")
def emulator_url(model_url):
    if url := os.environ.get("BT_EMULATOR_URL"):
        yield url.rstrip("/")
        return
    binary = os.environ.get("BT_EMULATOR_BIN", str(ROOT / "target/debug/bt-emulator"))
    db = os.environ.get("BT_EMULATOR_DATABASE_URL", "sqlite::memory:")
    proc = subprocess.Popen(
        [binary, "serve", "--listen", "127.0.0.1:0", "--database-url", db, "--llm-base-url", model_url, "--allow-code-execution"],
        stdout=subprocess.PIPE,
        text=True,
    )
    line = proc.stdout.readline().strip()
    assert line.startswith("listening on "), f"emulator failed to start: {line!r}"
    yield line.removeprefix("listening on ")
    proc.terminate()
    proc.wait(timeout=10)


@pytest.fixture(autouse=True)
def bt_env(emulator_url, monkeypatch):
    monkeypatch.setenv("BRAINTRUST_API_KEY", API_KEY)
    monkeypatch.setenv("BRAINTRUST_APP_URL", emulator_url)
    monkeypatch.setenv("BRAINTRUST_API_URL", emulator_url)
    import braintrust

    braintrust.login(app_url=emulator_url, api_key=API_KEY, force_login=True)
    yield
    braintrust.flush()


@pytest.fixture
def project_name():
    return f"e2e-{uuid.uuid4().hex[:12]}"


class Api:
    """Minimal REST client for reading state back out of the emulator."""

    def __init__(self, base):
        self.base = base

    def request(self, method, path, body=None):
        req = urllib.request.Request(
            self.base + path,
            method=method,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Authorization": f"Bearer {API_KEY}", "Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())

    def project_id(self, name):
        objs = self.request("GET", f"/v1/project?project_name={name}")["objects"]
        assert len(objs) == 1, objs
        return objs[0]["id"]

    def fetch(self, object_type, object_id):
        return self.request("POST", f"/v1/{object_type}/{object_id}/fetch", {})["events"]


@pytest.fixture
def api(emulator_url):
    return Api(emulator_url)
