//! The Grafana dashboard (`deploy/grafana/brisvox.json`) against what `/metrics` really exports.
//!
//! The exposition is rendered with every optional family present (pool pressure, provider
//! budgets, the resilience layer, the Smart Turn executor, usage), parsed, and every metric a
//! panel or variable query names is checked against it: the family exists, histograms are used
//! through `_bucket`, `_sum` or `_count`, `rate()` and `increase()` are applied only to counters
//! and histograms, counters and histogram series are never read without one of them, every
//! `histogram_quantile()` over buckets aggregates `by (le, ...)`, and the labels and label values of selectors and `by (...)` clauses occur on
//! that metric. A dashboard that asks for a metric the server does not export therefore fails
//! here, not in front of an operator.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use brisvox_core::SimClock;
use serde_json::Value;

use super::exposition::{Family, parse};
use super::tests::{FakeResilience, models};
use super::*;
use crate::admission::{AdmissionPolicy, AdmissionSettings};
use crate::session::{ManagerConfig, SessionManager};

/// Labels Prometheus adds to every scraped series; the server does not export them.
const TARGET_LABELS: [&str; 2] = ["instance", "job"];
/// Histogram series suffixes.
const SUFFIXES: [&str; 3] = ["_bucket", "_sum", "_count"];

/// `deploy/grafana/brisvox.json`; `None` in a packaged crate, which does not carry `deploy/`.
fn dashboard_path() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    root.join("Cargo.toml")
        .is_file()
        .then(|| root.join("deploy/grafana/brisvox.json"))
}

fn dashboard() -> Option<Value> {
    let path = dashboard_path()?;
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    Some(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

/// The exposition of a server that has everything switched on.
async fn full_exposition() -> Vec<Family> {
    let pool = brisvox_inference::InferencePool::new(
        &brisvox_inference::PoolConfig {
            threads: 1,
            queue_capacity: 4,
            name: "brisvox-dashboard-test".to_owned(),
        },
        SimClock::shared(),
    )
    .unwrap();
    let settings = AdmissionSettings {
        provider_limits: BTreeMap::from([("deepgram".to_owned(), 2)]),
        ..AdmissionSettings::default()
    };
    let policy = AdmissionPolicy::new(&settings, Some(Arc::new(pool.handle())));
    let manager =
        SessionManager::with_admission(ManagerConfig::new(4).unwrap(), SimClock::shared(), policy)
            .unwrap();
    let obs = Observability::new();
    assert!(obs.set_resilience_source(Arc::new(FakeResilience)));
    assert!(obs.set_smart_turn_pool(pool.handle()));
    obs.metrics().register_usage(&models("gpt-test"));
    let text = obs.render(&manager);
    pool.shutdown();
    parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

/// Every `expr`, `query` and `definition` string in the dashboard.
fn queries(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                match (key.as_str(), v) {
                    ("expr" | "query" | "definition", Value::String(s)) => out.push(s.clone()),
                    _ => queries(v, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| queries(v, out)),
        _ => {}
    }
}

/// A metric name found in a query, with the text around it.
struct Use<'a> {
    name: &'a str,
    /// The query text before the name.
    before: &'a str,
    /// The matchers of the selector right after the name, if there is one.
    selector: Option<&'a str>,
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The `brisvox_*` names of `expr`, in order.
fn uses(expr: &str) -> Vec<Use<'_>> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(i) = expr[from..].find("brisvox_") {
        let start = from + i;
        let end = expr[start..]
            .find(|c: char| !is_name_char(c))
            .map_or(expr.len(), |n| start + n);
        let standalone = expr[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_name_char(c));
        if standalone {
            let selector = expr[end..].strip_prefix('{').and_then(|rest| {
                // Label values are quoted and never contain a brace here.
                rest.find('}').map(|close| &rest[..close])
            });
            found.push(Use {
                name: &expr[start..end],
                before: &expr[..start],
                selector,
            });
        }
        from = end;
    }
    found
}

/// `(label, operator, value)` of each matcher in a selector body.
fn matchers(selector: &str) -> Vec<(&str, &str, &str)> {
    let mut out = Vec::new();
    let mut rest = selector.trim();
    while !rest.is_empty() {
        let op_at = rest.find(['=', '!']).expect("a matcher has an operator");
        let label = rest[..op_at].trim();
        let after = &rest[op_at..];
        let op_len =
            if after.starts_with("=~") || after.starts_with("!~") || after.starts_with("!=") {
                2
            } else {
                1
            };
        let (op, after) = after.split_at(op_len);
        let after = after
            .trim_start()
            .strip_prefix('"')
            .expect("a quoted value");
        let close = after.find('"').expect("a closed quote");
        out.push((label, op, &after[..close]));
        rest = after[close + 1..]
            .trim_start()
            .trim_start_matches(',')
            .trim_start();
    }
    out
}

/// The label names of the `by (...)` clauses in `expr`.
fn by_labels(expr: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = expr[from..].find("by (") {
        let start = from + i + "by (".len();
        let Some(close) = expr[start..].find(')') else {
            break;
        };
        out.extend(
            expr[start..start + close]
                .split(',')
                .map(str::trim)
                .filter(|l| !l.is_empty()),
        );
        from = start + close;
    }
    out
}

/// The family a series name belongs to, and whether it is a histogram series.
fn resolve<'a>(families: &'a [Family], name: &str) -> Result<(&'a Family, bool), String> {
    if let Some(family) = families.iter().find(|f| f.name == name) {
        return if family.kind == "histogram" {
            Err(format!(
                "{name} is a histogram: query its _bucket, _sum or _count"
            ))
        } else {
            Ok((family, false))
        };
    }
    for suffix in SUFFIXES {
        if let Some(base) = name.strip_suffix(suffix)
            && let Some(family) = families.iter().find(|f| f.name == base)
            && family.kind == "histogram"
        {
            return Ok((family, true));
        }
    }
    Err(format!("the server does not export {name}"))
}

fn label_values<'a>(family: &'a Family, label: &str) -> BTreeSet<&'a str> {
    family
        .samples
        .iter()
        .filter_map(|s| s.label(label))
        .collect()
}

fn has_label(family: &Family, label: &str) -> bool {
    family.samples.iter().any(|s| s.label(label).is_some())
}

/// The text between the parentheses of each `histogram_quantile(` call in `expr`.
fn quantile_calls(expr: &str) -> Vec<&str> {
    const CALL: &str = "histogram_quantile(";
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = expr[from..].find(CALL) {
        let start = from + i + CALL.len();
        let mut depth = 1_usize;
        let mut end = expr.len();
        for (n, c) in expr[start..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + n;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push(&expr[start..end]);
        from = start;
    }
    out
}

/// Problems of one query, empty when it is sound.
fn check_query(families: &[Family], expr: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut resolved = Vec::new();
    for used in uses(expr) {
        let (family, series) = match resolve(families, used.name) {
            Ok(found) => found,
            Err(e) => {
                problems.push(e);
                continue;
            }
        };
        resolved.push(family);
        let range_function = used.before.ends_with("rate(") || used.before.ends_with("increase(");
        if range_function && family.kind == "gauge" {
            problems.push(format!("{}: rate() or increase() of a gauge", used.name));
        }
        if range_function && family.kind == "histogram" && !series {
            problems.push(format!("{}: a histogram needs a series suffix", used.name));
        }
        // A counter or a histogram series read raw is a total since start, not a rate. Variable
        // queries only list label values.
        let cumulative = family.kind == "counter" || (family.kind == "histogram" && series);
        if cumulative && !range_function && !expr.starts_with("label_values(") {
            problems.push(format!(
                "{}: a cumulative series without rate() or increase()",
                used.name
            ));
        }
        for (label, op, value) in used.selector.map(matchers).unwrap_or_default() {
            if TARGET_LABELS.contains(&label) {
                continue;
            }
            if !has_label(family, label) {
                problems.push(format!("{}: no label {label}", used.name));
                continue;
            }
            // Template variables and regexes with operators are not checked further.
            if value.contains('$')
                || !value.chars().all(|c| is_name_char(c) || c == '|')
                || op.starts_with('!')
            {
                continue;
            }
            let known = label_values(family, label);
            for wanted in value.split('|') {
                if !known.contains(wanted) {
                    problems.push(format!(
                        "{}: no sample with {label}={wanted:?} (have {known:?})",
                        used.name
                    ));
                }
            }
        }
    }
    for call in quantile_calls(expr) {
        if call.contains("_bucket") && !by_labels(call).contains(&"le") {
            problems.push(format!(
                "histogram_quantile over buckets without `le` in its by (...): {call}"
            ));
        }
    }
    for label in by_labels(expr) {
        if label == "le" || TARGET_LABELS.contains(&label) {
            continue;
        }
        if !resolved.iter().any(|f| has_label(f, label)) {
            problems.push(format!(
                "by ({label}): no metric of the query has that label"
            ));
        }
    }
    problems
}

#[tokio::test(start_paused = true)]
async fn every_metric_and_label_the_dashboard_queries_is_exported() {
    let Some(dashboard) = dashboard() else {
        return;
    };
    let families = full_exposition().await;
    let mut all = Vec::new();
    queries(&dashboard, &mut all);
    let mut problems = Vec::new();
    for expr in &all {
        for problem in check_query(&families, expr) {
            problems.push(format!("{problem}\n    in {expr}"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    let named: BTreeSet<&str> = all
        .iter()
        .flat_map(|e| uses(e))
        .filter_map(|u| {
            resolve(&families, u.name)
                .ok()
                .map(|(f, _)| f.name.as_str())
        })
        .collect();
    assert!(
        named.len() >= 20,
        "the dashboard covers {} metric families only: {named:?}",
        named.len()
    );
}

#[test]
fn the_checker_rejects_what_would_break_a_panel() {
    let family = |name: &str, kind: &str, labels: &[(&str, &str)]| Family {
        name: name.to_owned(),
        help: String::new(),
        kind: kind.to_owned(),
        samples: vec![exposition::Sample {
            name: name.to_owned(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            value: 0.0,
        }],
    };
    let families = [
        family("brisvox_c_total", "counter", &[("reason", "full")]),
        family("brisvox_g", "gauge", &[]),
        family("brisvox_h_seconds", "histogram", &[("stage", "response")]),
    ];
    let sound = [
        "sum by (reason) (rate(brisvox_c_total{instance=~\"$instance\", reason=~\"full\"}[$__rate_interval]))",
        "brisvox_g",
        "histogram_quantile(0.95, sum by (le, stage) (rate(brisvox_h_seconds_bucket{stage=\"response\"}[1m])))",
        "sum(increase(brisvox_c_total[1h])) / 60",
        "sum(rate(brisvox_h_seconds_sum[1m])) / sum(rate(brisvox_h_seconds_count[1m]))",
        "label_values({__name__=~\"brisvox_c_total|brisvox_g\"}, reason)",
    ];
    for expr in sound {
        assert_eq!(check_query(&families, expr), Vec::<String>::new(), "{expr}");
    }
    for (expr, why) in [
        ("brisvox_missing_total", "unknown metric"),
        ("rate(brisvox_g[1m])", "rate of a gauge"),
        ("rate(brisvox_h_seconds[1m])", "histogram without suffix"),
        ("brisvox_h_seconds_bucket_x", "unknown suffix"),
        (
            "sum by (provider) (rate(brisvox_c_total[1m]))",
            "label the metric lacks",
        ),
        (
            "brisvox_c_total{provider=\"x\"}",
            "selector label the metric lacks",
        ),
        (
            "brisvox_c_total{reason=\"nope\"}",
            "label value that never occurs",
        ),
        (
            "brisvox_h_seconds_bucket{stage=~\"response|nope\"}",
            "one wrong alternative",
        ),
        (
            "histogram_quantile(0.95, sum by (stage) (rate(brisvox_h_seconds_bucket[1m])))",
            "quantile that drops le",
        ),
        (
            "histogram_quantile(0.95, rate(brisvox_h_seconds_bucket[1m]))",
            "quantile without any by",
        ),
        ("brisvox_c_total", "counter read raw"),
        (
            "sum(brisvox_c_total{reason=\"full\"})",
            "counter summed raw",
        ),
        (
            "sum by (le) (brisvox_h_seconds_bucket)",
            "histogram buckets read raw",
        ),
        ("brisvox_h_seconds_count", "histogram count read raw"),
    ] {
        assert!(
            !check_query(&families, expr).is_empty(),
            "accepted ({why}): {expr}"
        );
    }
}

/// The dashboard is importable (declares its datasource input and variables), has no
/// hard-coded datasource and gives every panel a title, a description and a unit.
#[test]
fn the_dashboard_is_importable_and_every_panel_is_documented() {
    let Some(dashboard) = dashboard() else {
        return;
    };
    let inputs = dashboard["__inputs"].as_array().expect("__inputs");
    assert!(
        inputs
            .iter()
            .any(|i| i["name"] == "DS_PROMETHEUS" && i["pluginId"] == "prometheus"),
        "a Prometheus datasource input"
    );
    let variables: Vec<&str> = dashboard["templating"]["list"]
        .as_array()
        .expect("templating")
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
    assert_eq!(variables, ["datasource", "instance", "provider"]);

    // A datasource reference is the variable (or Grafana's own annotation source): never a uid.
    fn datasources(value: &Value, found: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, v) in map {
                    if key == "datasource" {
                        found.push(v["uid"].as_str().unwrap_or_default().to_owned());
                    } else {
                        datasources(v, found);
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|v| datasources(v, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    datasources(&dashboard, &mut found);
    assert!(found.len() > 20, "{} datasource references", found.len());
    for uid in found {
        assert!(
            uid == "${datasource}" || uid == "-- Grafana --",
            "hard-coded datasource uid {uid:?}"
        );
    }

    let mut ids = BTreeSet::new();
    let mut panels = 0;
    for panel in dashboard["panels"].as_array().expect("panels") {
        let title = panel["title"].as_str().unwrap_or_default();
        assert!(!title.is_empty(), "a panel without a title: {panel}");
        assert!(
            ids.insert(panel["id"].as_u64().expect("an id")),
            "duplicate id"
        );
        if panel["type"] == "row" {
            continue;
        }
        panels += 1;
        let description = panel["description"].as_str().unwrap_or_default();
        assert!(
            description.ends_with('.') && description.len() > 60,
            "{title}: say how to read the panel and what is bad"
        );
        assert!(
            panel["fieldConfig"]["defaults"]["unit"].as_str().is_some()
                || panel["type"] == "state-timeline",
            "{title}: no unit"
        );
        assert!(
            !panel["targets"].as_array().expect("targets").is_empty(),
            "{title}: no query"
        );
    }
    assert!((15..=30).contains(&panels), "{panels} panels");
}
