//! Errors of agent definition loading: every problem names the file, the key path and the
//! line and column, and never contains a secret value.

use std::fmt;
use std::ops::Range;

use toml::Spanned;
use toml::de::{DeTable, DeValue};

/// One problem in an agent definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// The key path, e.g. `stt.deepgram.endpointing_ms` or `agent.tools[1]`; empty for a
    /// problem of the whole document (a syntax error before any key, or an engine rule that
    /// spans several sections).
    pub key: String,
    /// 1-based line and column (in characters) of the offending text, when known.
    pub position: Option<(usize, usize)>,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some((line, column)) = self.position {
            write!(f, "{line}:{column}: ")?;
        }
        if !self.key.is_empty() {
            write!(f, "`{}`: ", self.key)?;
        }
        f.write_str(&self.message)
    }
}

/// An agent definition that could not be loaded: the file (or `<string>`) and every problem
/// found, in file order. A syntax or type error stops reading, so it comes alone; the
/// cross-field checks and the environment lookups all report together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The file the definition came from, as given to the loader.
    pub source_name: String,
    /// The problems, at least one.
    pub issues: Vec<Issue>,
}

impl ConfigError {
    /// An error with one issue that has no position.
    pub(crate) fn single(source_name: &str, key: &str, message: String) -> Self {
        Self {
            source_name: source_name.to_owned(),
            issues: vec![Issue {
                key: key.to_owned(),
                position: None,
                message,
            }],
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.issues.len();
        write!(
            f,
            "invalid agent definition {} ({n} problem{})",
            self.source_name,
            if n == 1 { "" } else { "s" }
        )?;
        for issue in &self.issues {
            write!(f, "\n  {}:{issue}", self.source_name)?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigError {}

/// 1-based line and character column of byte `offset` in `text` (clamped to the text).
pub(crate) fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let mut end = offset.min(text.len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let before = text.get(..end).unwrap_or("");
    let line = before.matches('\n').count().saturating_add(1);
    let line_start = before.rfind('\n').map_or(0, |i| i.saturating_add(1));
    let column = before
        .get(line_start..)
        .map_or(0, |s| s.chars().count())
        .saturating_add(1);
    (line, column)
}

/// The key path of the most specific key or value of `text` whose span contains `offset`.
/// `None` when the text does not parse as TOML or nothing contains the offset.
pub(crate) fn key_path_at(text: &str, offset: usize) -> Option<String> {
    let doc = DeTable::parse(text).ok()?;
    let mut best: Option<(usize, String)> = None;
    search_table(doc.get_ref(), offset, "", &mut best);
    best.map(|(_, path)| path)
}

/// Records `path` as the answer when `span` contains `offset` and is no wider than the best one.
fn consider(span: &Range<usize>, offset: usize, path: &str, best: &mut Option<(usize, String)>) {
    // An empty span (end of input) contains its start.
    let contains = span.contains(&offset) || (span.is_empty() && span.start == offset);
    if !contains {
        return;
    }
    let width = span.len();
    if best.as_ref().is_none_or(|(w, _)| width <= *w) {
        *best = Some((width, path.to_owned()));
    }
}

/// Walks every entry of `table` (children are searched even when the parent's span does not
/// contain the offset: a table defined by a `[header]` may be spanned by its header only).
fn search_table(
    table: &DeTable<'_>,
    offset: usize,
    prefix: &str,
    best: &mut Option<(usize, String)>,
) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.get_ref().to_string()
        } else {
            format!("{prefix}.{}", key.get_ref())
        };
        consider(&key.span(), offset, &path, best);
        search_value(value, offset, &path, best);
    }
}

/// [`search_table`] for one value.
fn search_value(
    value: &Spanned<DeValue<'_>>,
    offset: usize,
    path: &str,
    best: &mut Option<(usize, String)>,
) {
    consider(&value.span(), offset, path, best);
    match value.get_ref() {
        DeValue::Table(table) => search_table(table, offset, path, best),
        DeValue::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                search_value(item, offset, &format!("{path}[{i}]"), best);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_column_count_characters() {
        let text = "a = 1\nşş = 2\n";
        assert_eq!(line_column(text, 0), (1, 1));
        assert_eq!(line_column(text, 6), (2, 1));
        // `=` after two two-byte characters and a space: byte 11, character column 4.
        assert_eq!(line_column(text, 11), (2, 4));
        assert_eq!(line_column(text, 1_000), (3, 1));
    }

    #[test]
    fn key_path_finds_the_innermost_entry() {
        let text =
            "[stt]\nprovider = \"x\"\n[stt.deepgram]\nmodel = \"m\"\ntools = [\"a\", \"b\"]\n";
        let at = |needle: &str| text.find(needle).unwrap();
        assert_eq!(
            key_path_at(text, at("\"x\"")).as_deref(),
            Some("stt.provider")
        );
        assert_eq!(
            key_path_at(text, at("\"m\"")).as_deref(),
            Some("stt.deepgram.model")
        );
        assert_eq!(
            key_path_at(text, at("\"b\"")).as_deref(),
            Some("stt.deepgram.tools[1]")
        );
        assert_eq!(
            key_path_at(text, at("model")).as_deref(),
            Some("stt.deepgram.model")
        );
    }
}
