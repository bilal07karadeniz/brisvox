//! A strict parser of the Prometheus text exposition format 0.0.4, used to prove that
//! `/metrics` is well-formed (tests, and anyone embedding the server who wants to check a
//! scrape). It accepts what [`Prom`](super::metrics::Prom) writes and rejects what a real
//! scraper would reject: samples before their `# TYPE`, a family split in two, bad names or
//! escapes, non-numeric values, histograms whose buckets shrink or whose `+Inf` bucket is not
//! the count.

use std::collections::{BTreeMap, HashSet};

/// One sample line.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// The sample name (a histogram's carries `_bucket`, `_sum` or `_count`).
    pub name: String,
    /// The labels, in file order, values unescaped.
    pub labels: Vec<(String, String)>,
    /// The value.
    pub value: f64,
}

impl Sample {
    /// The value of label `name`.
    pub fn label(&self, name: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// One metric family.
#[derive(Debug, Clone, PartialEq)]
pub struct Family {
    /// The family name.
    pub name: String,
    /// The `# HELP` text, unescaped.
    pub help: String,
    /// The `# TYPE` word (`counter`, `gauge`, `histogram`, ...).
    pub kind: String,
    /// The samples, in file order.
    pub samples: Vec<Sample>,
}

impl Family {
    /// The sample called exactly `name` whose labels include all of `labels`.
    pub fn find(&self, name: &str, labels: &[(&str, &str)]) -> Option<&Sample> {
        self.samples.iter().find(|s| {
            s.name == name
                && labels
                    .iter()
                    .all(|(k, v)| s.label(k).is_some_and(|x| x == *v))
        })
    }
}

fn valid_metric_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == ':')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

fn valid_label_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_value(text: &str) -> Result<f64, String> {
    match text {
        "+Inf" => Ok(f64::INFINITY),
        "-Inf" => Ok(f64::NEG_INFINITY),
        "NaN" => Ok(f64::NAN),
        other => other
            .parse::<f64>()
            .map_err(|_| format!("{other:?} is not a number")),
    }
}

/// Parses `name{labels} value` into a sample.
fn parse_sample(line: &str) -> Result<Sample, String> {
    let name_end = line
        .find(['{', ' '])
        .ok_or_else(|| format!("no value in {line:?}"))?;
    let (name, mut rest) = line.split_at(name_end);
    if !valid_metric_name(name) {
        return Err(format!("bad metric name {name:?}"));
    }
    let mut labels = Vec::new();
    if let Some(after) = rest.strip_prefix('{') {
        rest = after;
        loop {
            if let Some(after) = rest.strip_prefix('}') {
                rest = after;
                break;
            }
            let eq = rest.find('=').ok_or("label without =")?;
            let (label, after) = rest.split_at(eq);
            if !valid_label_name(label) {
                return Err(format!("bad label name {label:?}"));
            }
            let mut chars = after
                .strip_prefix("=\"")
                .ok_or("label value is not quoted")?
                .char_indices();
            let mut value = String::new();
            let end = loop {
                let (i, c) = chars.next().ok_or("unterminated label value")?;
                match c {
                    '"' => break i,
                    '\\' => match chars.next().ok_or("dangling backslash")?.1 {
                        '\\' => value.push('\\'),
                        '"' => value.push('"'),
                        'n' => value.push('\n'),
                        other => return Err(format!("bad escape \\{other}")),
                    },
                    c => value.push(c),
                }
            };
            labels.push((label.to_owned(), value));
            // `after` starts with `="`: skip it, the value and the closing quote.
            rest = after
                .get(2 + end + 1..)
                .ok_or("label value overruns the line")?;
            if let Some(after) = rest.strip_prefix(',') {
                rest = after;
            } else if !rest.starts_with('}') {
                return Err(format!("expected , or }} in {line:?}"));
            }
        }
    }
    let value = rest
        .strip_prefix(' ')
        .ok_or_else(|| format!("no space before the value in {line:?}"))?;
    if value.contains(' ') {
        return Err(format!("timestamps are not written: {line:?}"));
    }
    Ok(Sample {
        name: name.to_owned(),
        labels,
        value: parse_value(value)?,
    })
}

fn unescape_help(text: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                other => return Err(format!("bad escape in HELP: {other:?}")),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// Parses and validates `text`.
///
/// # Errors
/// A description of the first rule the document breaks.
pub fn parse(text: &str) -> Result<Vec<Family>, String> {
    if !text.is_empty() && !text.ends_with('\n') {
        return Err("the document does not end with a newline".to_owned());
    }
    let mut families: Vec<Family> = Vec::new();
    let mut seen = HashSet::new();
    let mut pending_help: Option<(String, String)> = None;
    for (n, line) in text.lines().enumerate() {
        let at = |e: String| format!("line {}: {e}", n + 1);
        if line.is_empty() {
            return Err(at("empty line".to_owned()));
        }
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let (name, help) = rest.split_once(' ').unwrap_or((rest, ""));
            if !valid_metric_name(name) {
                return Err(at(format!("bad family name {name:?}")));
            }
            pending_help = Some((name.to_owned(), unescape_help(help).map_err(at)?));
        } else if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest
                .split_once(' ')
                .ok_or_else(|| at("TYPE without a kind".to_owned()))?;
            if !seen.insert(name.to_owned()) {
                return Err(at(format!("family {name} is written twice")));
            }
            if !matches!(
                kind,
                "counter" | "gauge" | "histogram" | "summary" | "untyped"
            ) {
                return Err(at(format!("unknown type {kind:?}")));
            }
            let help = match pending_help.take() {
                Some((help_name, help)) if help_name == name => help,
                _ => String::new(),
            };
            families.push(Family {
                name: name.to_owned(),
                help,
                kind: kind.to_owned(),
                samples: Vec::new(),
            });
        } else if line.starts_with('#') {
            continue;
        } else {
            let sample = parse_sample(line).map_err(at)?;
            let family = families
                .last_mut()
                .ok_or_else(|| at("sample before any # TYPE".to_owned()))?;
            let belongs = if family.kind == "histogram" {
                ["_bucket", "_sum", "_count"]
                    .iter()
                    .any(|suffix| sample.name == format!("{}{suffix}", family.name))
            } else {
                sample.name == family.name
            };
            if !belongs {
                return Err(at(format!(
                    "sample {} is outside its family {}",
                    sample.name, family.name
                )));
            }
            if family.kind == "counter" && (sample.value.is_nan() || sample.value < 0.0) {
                return Err(at(format!("counter {} is negative or NaN", sample.name)));
            }
            family.samples.push(sample);
        }
    }
    for family in families.iter().filter(|f| f.kind == "histogram") {
        check_histogram(family)?;
    }
    Ok(families)
}

/// Buckets of one label set must not shrink, must end at `+Inf`, and `+Inf` must equal `_count`.
fn check_histogram(family: &Family) -> Result<(), String> {
    /// The `(le, cumulative)` buckets and the `_count` of one label set.
    type Series = (Vec<(f64, f64)>, Option<f64>);
    let mut series: BTreeMap<Vec<(String, String)>, Series> = BTreeMap::new();
    for sample in &family.samples {
        let key: Vec<(String, String)> = sample
            .labels
            .iter()
            .filter(|(k, _)| k != "le")
            .cloned()
            .collect();
        let entry = series.entry(key).or_default();
        if sample.name.ends_with("_bucket") {
            let le = sample
                .label("le")
                .ok_or_else(|| format!("{}: bucket without le", family.name))
                .and_then(parse_value)?;
            entry.0.push((le, sample.value));
        } else if sample.name.ends_with("_count") {
            entry.1 = Some(sample.value);
        }
    }
    for (labels, (buckets, count)) in series {
        let mut last = (f64::NEG_INFINITY, 0.0);
        for (le, value) in &buckets {
            if *le <= last.0 || *value < last.1 {
                return Err(format!(
                    "{}{labels:?}: buckets are not increasing and cumulative",
                    family.name
                ));
            }
            last = (*le, *value);
        }
        if last.0 != f64::INFINITY {
            return Err(format!("{}{labels:?}: no +Inf bucket", family.name));
        }
        if count != Some(last.1) {
            return Err(format!(
                "{}{labels:?}: +Inf bucket {} is not the count {count:?}",
                family.name, last.1
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_well_formed_document() {
        let text = "# HELP a_total things\n# TYPE a_total counter\na_total{x=\"1\"} 3\n\
                    # HELP h seconds\n# TYPE h histogram\n\
                    h_bucket{le=\"0.1\"} 1\nh_bucket{le=\"+Inf\"} 2\nh_sum 0.5\nh_count 2\n";
        let families = parse(text).unwrap();
        assert_eq!(families.len(), 2);
        assert_eq!(
            families[0].find("a_total", &[("x", "1")]).map(|s| s.value),
            Some(3.0)
        );
    }

    #[test]
    fn rejects_what_a_scraper_would() {
        for (bad, why) in [
            ("a_total 1\n", "sample before TYPE"),
            ("# TYPE a counter\na 1", "no trailing newline"),
            ("# TYPE a counter\na -1\n", "negative counter"),
            ("# TYPE a gauge\nb 1\n", "sample outside its family"),
            ("# TYPE a gauge\na{x=1} 1\n", "unquoted label"),
            ("# TYPE a gauge\na{x=\"\\q\"} 1\n", "bad escape"),
            ("# TYPE a gauge\na one\n", "not a number"),
            ("# TYPE a gauge\n# TYPE a gauge\n", "family twice"),
            (
                "# TYPE h histogram\nh_bucket{le=\"1\"} 2\nh_bucket{le=\"+Inf\"} 1\nh_sum 1\nh_count 1\n",
                "shrinking buckets",
            ),
            (
                "# TYPE h histogram\nh_bucket{le=\"+Inf\"} 2\nh_sum 1\nh_count 3\n",
                "+Inf is not the count",
            ),
        ] {
            assert!(parse(bad).is_err(), "accepted ({why}): {bad:?}");
        }
    }
}
