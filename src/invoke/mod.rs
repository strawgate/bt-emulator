//! Running stored functions offline (docs/protocol.md §16). Prompt functions are
//! rendered and sent to an OpenAI-compatible chat completions endpoint the operator
//! configures; inline code functions run in a local `node` or `python3` process when
//! code execution is enabled. Nothing is ever sent to Braintrust.

mod code;
mod prompt;

use std::time::Duration;

use serde_json::{Map, Value};

/// Where invocations run. The default runs nothing: every invoke is rejected with an
/// explanation of which option enables it.
#[derive(Debug, Clone)]
pub struct Runtime {
    /// Base URL of an OpenAI-compatible API (e.g. `http://localhost:11434/v1`).
    pub llm_base_url: Option<String>,
    pub llm_api_key: Option<String>,
    /// Run inline code functions locally. Off by default: anyone who can reach the
    /// emulator could otherwise run arbitrary code on its host.
    pub allow_code: bool,
    pub node: String,
    pub python: String,
    pub code_timeout: Duration,
    pub(crate) http: reqwest::Client,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            llm_base_url: None,
            llm_api_key: None,
            allow_code: false,
            node: "node".into(),
            python: "python3".into(),
            code_timeout: Duration::from_secs(60),
            http: reqwest::Client::new(),
        }
    }
}

/// One invocation's arguments, after the function itself was resolved.
#[derive(Debug, Default)]
pub struct Call {
    pub input: Value,
    pub messages: Vec<Value>,
    pub metadata: Value,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InvokeError {
    /// The function cannot run here (unsupported kind, or the feature is disabled).
    Unsupported(String),
    /// The stored function is malformed (a template that does not parse).
    Invalid(String),
    /// The model failed: unreachable, an error status, or an unusable reply.
    Model(String),
    /// The function ran and failed: code error, timeout.
    Failed(String),
}

impl Runtime {
    /// A request to the configured model API (`<llm_base_url>/<path>`), with the
    /// model's own key; `None` when no model is configured.
    pub(crate) fn model_request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Option<reqwest::RequestBuilder> {
        let base = self.llm_base_url.as_deref()?;
        let req = self
            .http
            .request(method, format!("{}/{path}", base.trim_end_matches('/')));
        Some(match &self.llm_api_key {
            Some(key) => req.bearer_auth(key),
            None => req,
        })
    }

    /// Run `function` (a stored function's data) and return its output.
    pub async fn invoke(
        &self,
        function: &Map<String, Value>,
        call: &Call,
    ) -> Result<Value, InvokeError> {
        let data = field(function, "function_data");
        match data["type"].as_str().unwrap_or_default() {
            "prompt" => prompt::run(self, function, call).await,
            "code" if data["data"]["type"] == "inline" => {
                code::run(self, &data["data"], call).await
            }
            "code" => Err(InvokeError::Unsupported(
                "bundled code functions cannot run in bt-emulator; use inline code".into(),
            )),
            other => Err(InvokeError::Unsupported(format!(
                "bt-emulator cannot run {other:?} functions"
            ))),
        }
    }
}

/// A field of a stored object, `null` when absent.
fn field<'a>(object: &'a Map<String, Value>, key: &str) -> &'a Value {
    object.get(key).unwrap_or(&Value::Null)
}
