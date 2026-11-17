//! Inline code functions, run in a local `node` or `python3` process.
//!
//! The runner script reads `{code, input, metadata}` as JSON on stdin, defines the
//! user's `handler`, calls it and writes the JSON result to stdout. As in Braintrust,
//! a TypeScript handler receives the input as its first argument and `{metadata}` as
//! its second; a Python handler receives an object input's fields as keyword
//! arguments (only those it declares, unless it takes `**kwargs`).

use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::{Call, InvokeError, Runtime};

const NODE_RUNNER: &str = r#"
const req = JSON.parse(require("fs").readFileSync(0, "utf8"));
const handler = new Function("require", req.code + "\n;return handler;")(require);
Promise.resolve()
  .then(() => handler(req.input, { metadata: req.metadata }))
  .then(
    (r) => process.stdout.write(JSON.stringify(r === undefined ? null : r)),
    (e) => { process.stderr.write(String((e && e.stack) || e)); process.exit(1); },
  );
"#;

const PYTHON_RUNNER: &str = r#"
import asyncio, inspect, json, sys
req = json.load(sys.stdin)
ns = {}
exec(req["code"], ns)
handler = ns["handler"]
args = req["input"]
if isinstance(args, dict):
    params = inspect.signature(handler).parameters
    if not any(p.kind == p.VAR_KEYWORD for p in params.values()):
        args = {k: v for k, v in args.items() if k in params}
    result = handler(**args)
else:
    result = handler(args)
if inspect.isawaitable(result):
    async def wait():
        return await result
    result = asyncio.run(wait())
sys.stdout.write(json.dumps(result))
"#;

pub async fn run(rt: &Runtime, data: &Value, call: &Call) -> Result<Value, InvokeError> {
    if !rt.allow_code {
        return Err(InvokeError::Unsupported(
            "code functions run only when bt-emulator is started with --allow-code-execution"
                .into(),
        ));
    }
    let (program, runner) = match data["runtime_context"]["runtime"].as_str() {
        Some("node") => (&rt.node, NODE_RUNNER),
        Some("python") => (&rt.python, PYTHON_RUNNER),
        other => {
            return Err(InvokeError::Unsupported(format!(
                "bt-emulator cannot run code for runtime {other:?}"
            )));
        }
    };
    let flag = if runner == NODE_RUNNER { "-e" } else { "-c" };
    let mut child = tokio::process::Command::new(program)
        .args([flag, runner])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(io_failed)?;
    let request = json!({"code": data["code"], "input": call.input, "metadata": call.metadata});
    let mut stdin = child.stdin.take().expect("stdin is piped");
    // A runner that exits early closes its stdin; its exit status reports why.
    let _ = stdin.write_all(request.to_string().as_bytes()).await;
    drop(stdin);
    let out = tokio::time::timeout(rt.code_timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            InvokeError::Failed(format!(
                "code function timed out after {:?}",
                rt.code_timeout
            ))
        })?
        .map_err(io_failed)?;
    if !out.status.success() {
        return Err(InvokeError::Failed(format!(
            "code function failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| InvokeError::Failed(format!("code function returned invalid JSON: {e}")))
}

fn io_failed(e: std::io::Error) -> InvokeError {
    InvokeError::Failed(format!("cannot run code function: {e}"))
}
