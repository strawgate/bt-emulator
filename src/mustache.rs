//! Mustache rendering as Braintrust prompts use it: no HTML escaping, and any
//! non-string value is substituted as compact JSON. Supports variables (`{{a.b}}`,
//! `{{{a}}}`, `{{&a}}`, `{{.}}`), sections, inverted sections and comments; not
//! partials or delimiter changes. Missing variables render as empty text.

use serde_json::Value;

enum Node {
    Text(String),
    Var(String),
    Section {
        name: String,
        inverted: bool,
        body: Vec<Node>,
    },
}

/// Render `template` against `data`.
pub fn render(template: &str, data: &Value) -> Result<String, String> {
    let mut rest = template;
    let nodes = parse(&mut rest, None)?;
    let mut out = String::new();
    render_nodes(&nodes, &mut vec![data], &mut out);
    Ok(out)
}

fn parse(rest: &mut &str, open: Option<&str>) -> Result<Vec<Node>, String> {
    let mut nodes = Vec::new();
    while let Some(start) = rest.find("{{") {
        nodes.push(Node::Text(rest[..start].to_owned()));
        let triple = rest[start..].starts_with("{{{");
        let (open_len, close) = if triple { (3, "}}}") } else { (2, "}}") };
        let after = &rest[start + open_len..];
        let end = after
            .find(close)
            .ok_or_else(|| format!("unclosed tag in template at byte {start}"))?;
        let tag = after[..end].trim();
        *rest = &after[end + close.len()..];
        if triple {
            nodes.push(Node::Var(tag.to_owned()));
            continue;
        }
        let (sigil, name) = tag.split_at(tag.chars().next().map_or(0, char::len_utf8));
        let name = name.trim();
        match sigil {
            "!" => {}
            "&" => nodes.push(Node::Var(name.to_owned())),
            "#" | "^" => {
                let body = parse(rest, Some(name))?;
                nodes.push(Node::Section {
                    name: name.to_owned(),
                    inverted: sigil == "^",
                    body,
                });
            }
            "/" if open == Some(name) => return Ok(nodes),
            "/" => return Err(format!("unexpected closing tag {{{{/{name}}}}}")),
            _ => nodes.push(Node::Var(tag.to_owned())),
        }
    }
    if let Some(name) = open {
        return Err(format!("unclosed section {{{{#{name}}}}}"));
    }
    if !rest.is_empty() {
        nodes.push(Node::Text((*rest).to_owned()));
    }
    Ok(nodes)
}

/// Resolve a dotted name against the context stack, innermost first.
fn lookup<'a>(stack: &[&'a Value], name: &str) -> Option<&'a Value> {
    let top = stack.last().copied();
    if name == "." {
        return top;
    }
    let mut parts = name.split('.');
    let first = parts.next().unwrap_or_default();
    let base = stack.iter().rev().find_map(|ctx| ctx.get(first))?;
    parts.try_fold(base, |v, part| match v {
        Value::Array(items) => part.parse::<usize>().ok().and_then(|i| items.get(i)),
        _ => v.get(part),
    })
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

fn render_nodes<'a>(nodes: &'a [Node], stack: &mut Vec<&'a Value>, out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Var(name) => match lookup(stack, name) {
                Some(Value::String(s)) => out.push_str(s),
                Some(Value::Null) | None => {}
                Some(other) => out.push_str(&other.to_string()),
            },
            Node::Section {
                name,
                inverted,
                body,
            } => {
                let value = lookup(stack, name);
                let on = value.is_some_and(truthy);
                if *inverted {
                    if !on {
                        render_nodes(body, stack, out);
                    }
                    continue;
                }
                match value {
                    Some(Value::Array(items)) => {
                        for item in items {
                            render_scoped(body, stack, item, out);
                        }
                    }
                    Some(v) if on => render_scoped(body, stack, v, out),
                    _ => {}
                }
            }
        }
    }
}

fn render_scoped<'a>(
    body: &'a [Node],
    stack: &mut Vec<&'a Value>,
    ctx: &'a Value,
    out: &mut String,
) {
    stack.push(ctx);
    render_nodes(body, stack, out);
    stack.pop();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r(template: &str, data: Value) -> String {
        render(template, &data).unwrap()
    }

    #[test]
    fn variables_are_not_escaped_and_non_strings_are_json() {
        let data = json!({"input": {"q": "<a & b>", "n": 5, "o": {"k": [1, 2]}}, "s": "x"});
        assert_eq!(r("Q: {{input.q}}", data.clone()), "Q: <a & b>");
        assert_eq!(
            r("{{input.n}}|{{{input.o}}}|{{& s}}", data.clone()),
            "5|{\"k\":[1,2]}|x"
        );
        assert_eq!(r("{{input.o.k.1}}", data.clone()), "2");
        assert_eq!(
            r("[{{missing}}][{{input.q.deep}}][{{input.o.k.x}}]", data),
            "[][][]"
        );
        assert_eq!(r("{{n}}", json!({"n": null})), "");
        assert_eq!(r("no tags", json!({})), "no tags");
        // A comment renders nothing, even when a key is spelled like it.
        assert_eq!(r("a{{!note}}b", json!({"!note": "x"})), "ab");
    }

    #[test]
    fn sections_iterate_scope_and_invert() {
        let data = json!({"nul": null, "items": [{"n": "a"}, {"n": "b"}], "t": true, "f": false, "o": {"n": "inner"}, "n": "outer", "e": [], "s": ""});
        assert_eq!(r("{{#items}}{{n}},{{/items}}", data.clone()), "a,b,");
        assert_eq!(
            r("{{#t}}yes {{n}}{{/t}}{{#f}}no{{/f}}", data.clone()),
            "yes outer"
        );
        assert_eq!(r("{{#o}}{{n}}{{/o}}", data.clone()), "inner");
        assert_eq!(
            r(
                "{{^e}}empty{{/e}}{{^t}}x{{/t}}{{^s}}blank{{/s}}{{^missing}}!{{/missing}}{{^nul}}?{{/nul}}",
                data.clone()
            ),
            "emptyblank!?"
        );
        assert_eq!(
            r("{{#items}}{{.}}{{/items}}", json!({"items": [1, "two"]})),
            "1two"
        );
        assert_eq!(r("a{{! a comment }}b{{#missing}}x{{/missing}}", data), "ab");
    }

    #[test]
    fn malformed_templates_are_errors() {
        assert!(
            render("{{oops", &json!({}))
                .unwrap_err()
                .contains("unclosed tag")
        );
        assert!(
            render("{{#a}}x", &json!({}))
                .unwrap_err()
                .contains("unclosed section")
        );
        assert!(
            render("x{{/a}}", &json!({}))
                .unwrap_err()
                .contains("unexpected closing")
        );
        assert!(
            render("{{#a}}{{/b}}", &json!({}))
                .unwrap_err()
                .contains("unexpected closing")
        );
    }
}
