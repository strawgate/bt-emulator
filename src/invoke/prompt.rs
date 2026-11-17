//! Prompt functions: render the template, call chat completions, shape the reply.

use serde_json::{Map, Value, json};

use super::{Call, InvokeError, Runtime, field};

/// Braintrust-only options that are not chat completion parameters.
const NOT_PARAMS: [&str; 1] = ["use_cache"];

pub async fn run(
    rt: &Runtime,
    function: &Map<String, Value>,
    call: &Call,
) -> Result<Value, InvokeError> {
    let Some(req) = rt.model_request(reqwest::Method::POST, "chat/completions") else {
        return Err(InvokeError::Unsupported(
            "prompt functions need a model: start bt-emulator with --llm-base-url pointing at an OpenAI-compatible API".into(),
        ));
    };
    let prompt_data = field(function, "prompt_data");
    // Running attached tools needs the server's tool loop, which is not public;
    // refuse rather than call the model without them.
    if prompt_data["tool_functions"]
        .as_array()
        .is_some_and(|t| !t.is_empty())
    {
        return Err(InvokeError::Unsupported(
            "prompts with tool_functions cannot be invoked in bt-emulator (tool calls are not run)"
                .into(),
        ));
    }
    let request = build_request(prompt_data, call)?;
    let req = req.json(&request);
    let failed = |e: reqwest::Error| InvokeError::Model(format!("model request failed: {e}"));
    let resp = req.send().await.map_err(failed)?;
    let status = resp.status();
    let text = resp.text().await.map_err(failed)?;
    if !status.is_success() {
        return Err(InvokeError::Model(format!(
            "model returned {status}: {text}"
        )));
    }
    let reply: Value = serde_json::from_str(&text)
        .map_err(|e| InvokeError::Model(format!("model returned invalid JSON: {e}")))?;
    let message = &reply["choices"][0]["message"];
    let name = function.get("name").cloned().unwrap_or(Value::Null);
    match &prompt_data["parser"] {
        Value::Null => Ok(output(message, &request)),
        parser => classify(parser, message, name),
    }
}

/// Template variables: the input, its fields when it is an object, and `metadata`.
fn variables(call: &Call) -> Value {
    let mut vars = Map::new();
    vars.insert("metadata".into(), call.metadata.clone());
    vars.insert("input".into(), call.input.clone());
    if let Value::Object(fields) = &call.input {
        vars.extend(fields.clone());
    }
    Value::Object(vars)
}

/// Render every string inside `value` as a template.
fn render_all(value: &Value, vars: &Value) -> Result<Value, InvokeError> {
    let bad = |e: String| InvokeError::Invalid(format!("invalid prompt template: {e}"));
    Ok(match value {
        Value::String(s) => Value::String(crate::mustache::render(s, vars).map_err(bad)?),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| render_all(v, vars))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), render_all(v, vars)?)))
                .collect::<Result<_, InvokeError>>()?,
        ),
        other => other.clone(),
    })
}

fn build_request(prompt_data: &Value, call: &Call) -> Result<Value, InvokeError> {
    let vars = variables(call);
    let prompt = &prompt_data["prompt"];
    let mut messages = match prompt["type"].as_str() {
        Some("completion") => vec![json!({"role": "user", "content": prompt["content"]})],
        _ => prompt["messages"].as_array().cloned().unwrap_or_default(),
    };
    messages = messages
        .iter()
        .map(|m| render_all(m, &vars))
        .collect::<Result<_, _>>()?;
    messages.extend(call.messages.iter().cloned());
    let options = &prompt_data["options"];
    let mut request = Map::new();
    for (key, value) in options["params"].as_object().cloned().unwrap_or_default() {
        if !NOT_PARAMS.contains(&key.as_str()) {
            request.insert(key, value);
        }
    }
    request.insert("model".into(), options["model"].clone());
    request.insert("messages".into(), Value::Array(messages));
    if let Some(tools) = prompt["tools"].as_str().filter(|t| !t.trim().is_empty()) {
        let rendered = render_all(&json!(tools), &vars)?;
        let tools: Value = serde_json::from_str(rendered.as_str().unwrap_or_default())
            .map_err(|e| InvokeError::Invalid(format!("prompt tools are not valid JSON: {e}")))?;
        request.insert("tools".into(), tools);
    }
    let parser = &prompt_data["parser"];
    if !parser.is_null() {
        request.insert("tools".into(), json!([select_choice(parser)]));
        request.insert(
            "tool_choice".into(),
            json!({"type": "function", "function": {"name": "select_choice"}}),
        );
    }
    Ok(Value::Object(request))
}

/// The tool an LLM classifier answers through, as autoevals defines it.
fn select_choice(parser: &Value) -> Value {
    let choices: Vec<&String> = parser["choice_scores"]
        .as_object()
        .map(|c| c.keys().collect())
        .unwrap_or_default();
    let mut properties = Map::new();
    let mut required = Vec::new();
    if parser["use_cot"] == true {
        properties.insert("reasoning".into(), json!({"type": "string", "description": "Write out in a step by step manner your reasoning to be sure that your conclusion is correct. Avoid simply stating the correct answer at the outset."}));
        required.push("reasoning");
    }
    properties.insert(
        "choice".into(),
        json!({"type": "string", "enum": choices, "description": "The choice"}),
    );
    required.push("choice");
    json!({"type": "function", "function": {
        "name": "select_choice",
        "description": "Call this function to select a choice.",
        "parameters": {"type": "object", "properties": properties, "required": required},
    }})
}

/// A classifier's verdict: the score its choice maps to.
fn classify(parser: &Value, message: &Value, name: Value) -> Result<Value, InvokeError> {
    let args = message["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .and_then(|a| serde_json::from_str::<Value>(a).ok())
        .ok_or_else(|| {
            InvokeError::Model(format!("model did not call select_choice: {message}"))
        })?;
    let choice = args["choice"].as_str().unwrap_or_default();
    let score = &parser["choice_scores"][choice];
    if !score.is_number() {
        return Err(InvokeError::Model(format!(
            "model chose unknown choice {choice:?}"
        )));
    }
    let mut metadata = json!({"choice": choice});
    if parser["use_cot"] == true && args["reasoning"].is_string() {
        metadata["rationale"] = args["reasoning"].clone();
    }
    Ok(json!({"name": name, "score": score, "metadata": metadata}))
}

/// A plain prompt's output: tool call arguments, parsed JSON when the prompt asked
/// for a JSON response format, or else the text.
fn output(message: &Value, request: &Value) -> Value {
    let parse = |s: &str| serde_json::from_str::<Value>(s).ok();
    if let Some(calls) = message["tool_calls"].as_array().filter(|c| !c.is_empty()) {
        let mut args: Vec<Value> = calls
            .iter()
            .map(|c| {
                let raw = c["function"]["arguments"].as_str().unwrap_or_default();
                parse(raw).unwrap_or_else(|| json!(raw))
            })
            .collect();
        return if args.len() == 1 {
            args.remove(0)
        } else {
            Value::Array(args)
        };
    }
    let content = message["content"].as_str().unwrap_or_default();
    let json_format = matches!(
        request["response_format"]["type"].as_str(),
        Some("json_object" | "json_schema")
    );
    match parse(content) {
        Some(parsed) if json_format => parsed,
        _ => json!(content),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_spread_object_inputs() {
        let call = Call {
            input: json!({"input": "q", "output": "a"}),
            metadata: json!({"m": 1}),
            ..Call::default()
        };
        assert_eq!(
            variables(&call),
            json!({"metadata": {"m": 1}, "input": "q", "output": "a"})
        );
        let call = Call {
            input: json!(5),
            ..Call::default()
        };
        assert_eq!(variables(&call), json!({"metadata": null, "input": 5}));
    }

    #[test]
    fn requests_render_messages_params_and_tools() {
        let prompt_data = json!({
            "options": {"model": "m", "params": {"temperature": 0, "use_cache": true}},
            "prompt": {"type": "chat", "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Q: {{input.q}}"}], "n": 1},
            ], "tools": "[{\"type\": \"function\", \"function\": {\"name\": \"{{tool}}\"}}]"},
        });
        let call = Call {
            input: json!({"q": "why", "tool": "t"}),
            messages: vec![json!({"role": "assistant", "content": "{{q}}"})],
            ..Call::default()
        };
        assert_eq!(
            build_request(&prompt_data, &call).unwrap(),
            json!({"model": "m", "temperature": 0, "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Q: why"}], "n": 1},
                {"role": "assistant", "content": "{{q}}"},
            ], "tools": [{"type": "function", "function": {"name": "t"}}]})
        );
        let completion = json!({"options": {"model": "m"}, "prompt": {"type": "completion", "content": "Say {{input}}", "tools": " "}});
        let call = Call {
            input: json!("hi"),
            ..Call::default()
        };
        assert_eq!(
            build_request(&completion, &call).unwrap(),
            json!({"model": "m", "messages": [{"role": "user", "content": "Say hi"}]})
        );
        let bad_tools = json!({"prompt": {"messages": [], "tools": "not json"}});
        assert!(
            matches!(build_request(&bad_tools, &call), Err(InvokeError::Invalid(e)) if e.contains("tools"))
        );
        let bad_template = json!({"prompt": {"messages": [{"content": [{"text": "{{#x}}"}]}]}});
        assert!(
            matches!(build_request(&bad_template, &call), Err(InvokeError::Invalid(e)) if e.contains("template"))
        );
    }

    #[test]
    fn classifiers_force_select_choice() {
        let prompt_data = json!({
            "options": {"model": "m"},
            "parser": {"type": "llm_classifier", "use_cot": true, "choice_scores": {"good": 1, "bad": 0}},
            "prompt": {"messages": []},
        });
        let request = build_request(&prompt_data, &Call::default()).unwrap();
        let params = &request["tools"][0]["function"]["parameters"];
        assert_eq!(params["required"], json!(["reasoning", "choice"]));
        assert_eq!(
            params["properties"]["choice"]["enum"],
            json!(["bad", "good"])
        );
        assert_eq!(request["tool_choice"]["function"]["name"], "select_choice");
        let no_cot = select_choice(&json!({"choice_scores": {"y": 1}}));
        assert_eq!(
            no_cot["function"]["parameters"]["required"],
            json!(["choice"])
        );
    }

    #[test]
    fn classifier_replies_become_scores() {
        let parser = json!({"use_cot": true, "choice_scores": {"good": 1, "bad": 0}});
        let call = |args: &str| json!({"tool_calls": [{"function": {"arguments": args}}]});
        assert_eq!(
            classify(
                &parser,
                &call(r#"{"choice": "good", "reasoning": "r"}"#),
                json!("S")
            )
            .unwrap(),
            json!({"name": "S", "score": 1, "metadata": {"choice": "good", "rationale": "r"}})
        );
        let no_cot = json!({"choice_scores": {"good": 1}});
        assert_eq!(
            classify(
                &no_cot,
                &call(r#"{"choice": "good", "reasoning": "r"}"#),
                json!("S")
            )
            .unwrap(),
            json!({"name": "S", "score": 1, "metadata": {"choice": "good"}})
        );
        assert!(
            matches!(classify(&parser, &call(r#"{"choice": "meh"}"#), json!("S")), Err(InvokeError::Model(e)) if e.contains("unknown choice"))
        );
        assert!(
            matches!(classify(&parser, &json!({"content": "good"}), json!("S")), Err(InvokeError::Model(e)) if e.contains("select_choice"))
        );
    }

    #[test]
    fn outputs_follow_the_response_format() {
        let json_format = json!({"response_format": {"type": "json_object"}});
        let text_format = json!({});
        let msg = |content: &str| json!({"content": content});
        assert_eq!(output(&msg(r#"{"a": 1}"#), &json_format), json!({"a": 1}));
        assert_eq!(output(&msg("not json"), &json_format), json!("not json"));
        assert_eq!(
            output(&msg(r#"{"a": 1}"#), &text_format),
            json!(r#"{"a": 1}"#)
        );
        assert_eq!(output(&json!({"content": null}), &text_format), json!(""));
        let tool = |args: &str| json!({"function": {"arguments": args}});
        assert_eq!(
            output(&json!({"tool_calls": [tool(r#"{"x": 1}"#)]}), &text_format),
            json!({"x": 1})
        );
        assert_eq!(
            output(
                &json!({"tool_calls": [tool("raw"), tool("[2]")], "content": "c"}),
                &text_format
            ),
            json!(["raw", [2]])
        );
        assert_eq!(
            output(&json!({"tool_calls": [], "content": "c"}), &text_format),
            json!("c")
        );
    }
}
