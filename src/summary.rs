//! Experiment summaries: per-score and per-metric aggregates over traces, with an
//! optional comparison experiment. Traces are matched across experiments by their
//! root span's `input` (Braintrust's default comparison key).
//!
//! The output keys are exactly what the Python SDK's `ScoreSummary` and
//! `MetricSummary` dataclasses accept; any extra key makes the SDK raise.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value, json};

const COUNT_METRICS: [&str; 5] = [
    "llm_calls",
    "tool_calls",
    "errors",
    "llm_errors",
    "tool_errors",
];
const TOKEN_METRICS: [&str; 7] = [
    "prompt_tokens",
    "prompt_cached_tokens",
    "prompt_cache_creation_tokens",
    "completion_tokens",
    "completion_reasoning_tokens",
    "total_tokens",
    "estimated_cost",
];

#[derive(Debug, Default)]
struct Trace {
    key: String,
    scores: BTreeMap<String, f64>,
    metrics: BTreeMap<&'static str, f64>,
}

fn is_root(row: &Value) -> bool {
    row["span_parents"].as_array().is_none_or(Vec::is_empty)
}

fn traces(rows: &[Value]) -> Vec<Trace> {
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for row in rows {
        let root = row["root_span_id"]
            .as_str()
            .or(row["id"].as_str())
            .unwrap_or_default();
        groups.entry(root.to_owned()).or_default().push(row);
    }
    let mut out = Vec::new();
    for spans in groups.values() {
        let Some(root) = spans.iter().find(|r| is_root(r)) else {
            continue;
        };
        let mut t = Trace {
            key: root["input"].to_string(),
            ..Trace::default()
        };
        // Eval logs each scorer's result on its own child span, so a trace's score is
        // the mean over every span in the trace that reports it.
        let mut score_sums: BTreeMap<String, (f64, f64)> = BTreeMap::new();
        for span in spans {
            for (name, value) in span["scores"].as_object().into_iter().flatten() {
                if let Some(v) = value.as_f64() {
                    let e = score_sums.entry(name.clone()).or_default();
                    *e = (e.0 + v, e.1 + 1.0);
                }
            }
        }
        t.scores = score_sums
            .into_iter()
            .map(|(k, (sum, n))| (k, sum / n))
            .collect();
        if let (Some(start), Some(end)) = (
            root["metrics"]["start"].as_f64(),
            root["metrics"]["end"].as_f64(),
        ) {
            t.metrics.insert("duration", end - start);
        }
        for name in COUNT_METRICS {
            t.metrics.insert(name, 0.0);
        }
        for span in spans {
            let ty = span["span_attributes"]["type"].as_str();
            let errored = !span["error"].is_null();
            let mut bump = |name: &'static str| *t.metrics.get_mut(name).unwrap() += 1.0;
            match ty {
                Some("llm") => bump("llm_calls"),
                Some("tool") => bump("tool_calls"),
                _ => {}
            }
            if errored {
                bump("errors");
                match ty {
                    Some("llm") => bump("llm_errors"),
                    Some("tool") => bump("tool_errors"),
                    _ => {}
                }
            }
            for name in TOKEN_METRICS {
                if let Some(v) = span["metrics"][name].as_f64() {
                    *t.metrics.entry(name).or_default() += v;
                }
            }
        }
        out.push(t);
    }
    out
}

/// The value a trace reports for a score or metric. Helpers take `&dyn Fn` rather
/// than generics so each is compiled (and coverage-counted) exactly once.
type Getter<'a> = &'a dyn Fn(&Trace) -> Option<f64>;

/// Mean of `value` over the traces that report it.
fn mean(traces: &[Trace], value: Getter) -> Option<f64> {
    let values: Vec<f64> = traces.iter().filter_map(value).collect();
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

fn unit(metric: &str) -> &'static str {
    match metric {
        "duration" => "s",
        m if m.ends_with("tokens") => "tok",
        "estimated_cost" => "$",
        _ => "",
    }
}

/// Count matched traces where the value went up / down.
fn changes(current: &[Trace], baseline: &HashMap<&str, &Trace>, value: Getter) -> (u64, u64) {
    let (mut up, mut down) = (0, 0);
    for t in current {
        if let (Some(cur), Some(base)) = (
            value(t),
            baseline.get(t.key.as_str()).and_then(|b| value(b)),
        ) {
            if cur > base {
                up += 1;
            } else if cur < base {
                down += 1;
            }
        }
    }
    (up, down)
}

pub fn summarize(
    rows: &[Value],
    comparison: Option<&[Value]>,
) -> (Map<String, Value>, Map<String, Value>) {
    let current = traces(rows);
    let baseline_traces = comparison.map(traces).unwrap_or_default();
    let mut baseline: HashMap<&str, &Trace> = HashMap::new();
    for t in &baseline_traces {
        baseline.entry(t.key.as_str()).or_insert(t);
    }
    let has_comparison = comparison.is_some();

    let mut scores = Map::new();
    let names: std::collections::BTreeSet<&String> =
        current.iter().flat_map(|t| t.scores.keys()).collect();
    for name in names {
        let get = |t: &Trace| t.scores.get(name).copied();
        let score = mean(&current, &get).unwrap();
        let (up, down) = changes(&current, &baseline, &get);
        let mut s = json!({"name": name, "score": score, "improvements": up, "regressions": down});
        if let Some(base) = mean(&baseline_traces, &get).filter(|_| has_comparison) {
            s["diff"] = json!(score - base);
        }
        scores.insert(name.clone(), s);
    }

    let mut metrics = Map::new();
    // Every metric is always reported, like the real API (0 when nothing measured it).
    let names: std::collections::BTreeSet<&'static str> = COUNT_METRICS
        .into_iter()
        .chain(TOKEN_METRICS)
        .chain(["duration"])
        .collect();
    for name in names {
        let get = |t: &Trace| t.metrics.get(name).copied();
        let metric = mean(&current, &get).unwrap_or(0.0);
        // Lower is better for every metric (latency, tokens, cost, errors).
        let (up, down) = changes(&current, &baseline, &get);
        let mut m = json!({"name": name, "metric": metric, "unit": unit(name), "improvements": down, "regressions": up});
        if let Some(base) = mean(&baseline_traces, &get).filter(|_| has_comparison) {
            m["diff"] = json!(metric - base);
        }
        metrics.insert(name.to_owned(), m);
    }
    (scores, metrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(input: i64, scores: Value, start: f64, end: f64) -> Value {
        json!({"id": format!("r{input}"), "root_span_id": format!("t{input}"), "span_parents": null,
               "input": input, "scores": scores, "metrics": {"start": start, "end": end}})
    }

    fn child(trace: i64, ty: &str, error: Value, tokens: f64) -> Value {
        json!({"id": format!("c{trace}{ty}"), "root_span_id": format!("t{trace}"), "span_parents": ["x"],
               "span_attributes": {"type": ty}, "error": error, "metrics": {"total_tokens": tokens}})
    }

    #[test]
    fn scores_and_metrics_without_comparison() {
        let rows = vec![
            root(1, json!({"acc": 1.0, "skip": null}), 0.0, 2.0),
            root(2, json!({"acc": 0.0}), 0.0, 4.0),
            child(1, "llm", json!("boom"), 10.0),
            child(1, "tool", json!("bad"), 0.0),
            child(2, "task", json!("task failed"), 30.0),
            json!({"id": "orphan", "root_span_id": "t9", "span_parents": ["missing"]}),
        ];
        let (scores, metrics) = summarize(&rows, None);
        assert_eq!(
            scores["acc"],
            json!({"name": "acc", "score": 0.5, "improvements": 0, "regressions": 0})
        );
        assert!(!scores.contains_key("skip"));
        assert_eq!(
            metrics["duration"],
            json!({"name": "duration", "metric": 3.0, "unit": "s", "improvements": 0, "regressions": 0})
        );
        assert_eq!(metrics["total_tokens"]["metric"], 20.0);
        assert_eq!(metrics["total_tokens"]["unit"], "tok");
        assert_eq!(metrics["llm_calls"]["metric"], 0.5);
        assert_eq!(metrics["tool_calls"]["metric"], 0.5);
        assert_eq!(metrics["errors"]["metric"], 1.5);
        assert_eq!(metrics["llm_errors"]["metric"], 0.5);
        assert_eq!(metrics["tool_errors"]["unit"], "");
    }

    #[test]
    fn comparison_produces_diffs_improvements_and_regressions() {
        let base = vec![
            root(1, json!({"acc": 0.0}), 0.0, 5.0),
            root(2, json!({"acc": 1.0}), 0.0, 1.0),
            root(3, json!({"acc": 0.5}), 0.0, 1.0),
        ];
        let cur = vec![
            root(1, json!({"acc": 1.0}), 0.0, 1.0),
            root(2, json!({"acc": 0.0}), 0.0, 3.0),
            root(3, json!({"acc": 0.5}), 0.0, 1.0),
            root(4, json!({"new": 1.0}), 0.0, 1.0),
        ];
        let (scores, metrics) = summarize(&cur, Some(&base));
        assert_eq!(scores["acc"]["diff"], 0.0);
        assert_eq!(
            (
                scores["acc"]["improvements"].as_u64(),
                scores["acc"]["regressions"].as_u64()
            ),
            (Some(1), Some(1))
        );
        assert!(scores["new"].get("diff").is_none());
        // Lower duration is an improvement.
        assert_eq!(
            (
                metrics["duration"]["improvements"].as_u64(),
                metrics["duration"]["regressions"].as_u64()
            ),
            (Some(1), Some(1))
        );
        assert_eq!(metrics["duration"]["diff"], 1.5 - 7.0 / 3.0);
    }

    #[test]
    fn scores_from_scorer_child_spans_count_toward_their_trace() {
        let mut scorer = child(1, "score", Value::Null, 0.0);
        scorer["scores"] = json!({"exact": 1.0, "fuzzy": 0.5});
        let mut second = child(1, "score2", Value::Null, 0.0);
        second["scores"] = json!({"fuzzy": 1.0});
        let rows = vec![
            root(1, Value::Null, 0.0, 1.0),
            scorer,
            second,
            root(2, json!({"exact": 0.0}), 0.0, 1.0),
        ];
        let (scores, _) = summarize(&rows, None);
        assert_eq!(scores["exact"]["score"], 0.5);
        assert_eq!(scores["fuzzy"]["score"], 0.75);
    }

    #[test]
    fn roots_without_timing_have_no_duration() {
        let rows = vec![json!({"id": "r", "root_span_id": "r", "input": 1, "scores": {"s": 1.0}})];
        let (_, metrics) = summarize(&rows, None);
        assert_eq!(metrics["duration"]["metric"], 0.0);
    }

    #[test]
    fn units_and_empty_experiments() {
        assert_eq!(unit("estimated_cost"), "$");
        let (scores, metrics) = summarize(&[], Some(&[]));
        assert!(scores.is_empty());
        assert_eq!(
            metrics["llm_calls"],
            json!({"name": "llm_calls", "metric": 0.0, "unit": "", "improvements": 0, "regressions": 0})
        );
    }
}
