//! The YAML front-end of agent definitions (docs/CONFIG.md "YAML").
//!
//! A YAML definition is read into the very same [`AgentFile`] as a TOML one, so the cross-field
//! checks, the secret lookups and the mappings are shared, and so are their messages. The one
//! thing the schema needs from a front-end is the [`Spanned`](toml::Spanned) wrapper: a byte
//! range of the value in the source text, from which the checks compute line and column. TOML
//! fills it natively; here it is filled in two steps:
//! 1. [`saphyr_parser`] turns the text into events, each with its line and column. They become a
//!    small tree of scalars, sequences and mappings with byte ranges ([`Node`]). The builder
//!    rejects what has no TOML equivalent or would make positions ambiguous: anchors and aliases,
//!    tags, a second document, duplicate keys, keys that are not scalars, nesting deeper than
//!    [`MAX_DEPTH`], a lone carriage return (a line break YAML knows but positions do not count).
//! 2. A serde [`Deserializer`](de::Deserializer) over the tree ([`NodeDe`]) answers
//!    `Spanned<T>` requests with the node's byte range ([`serde_spanned::de`]) and tracks the
//!    key path, so every read error carries the key path and position of the innermost node it
//!    concerns.
//!
//! Scalars follow the YAML 1.2 core schema: `true`/`false`, integers (`300`, `+5`, `0x1F`,
//! `0o17`), floats (`0.7`, `1e3`, `.inf`), and `null`/`~`/nothing are typed; everything else, and
//! every quoted scalar, is a string. So `yes`/`no`/`on` are strings (no YAML 1.1 "Norway
//! problem"), and a value of the wrong type is an error with the same message as in TOML
//! (`invalid type: integer `300`, expected a string`). A null value is the same as leaving the
//! key out (an empty `turn:` is an empty `[turn]` table).

use std::cell::Cell;
use std::fmt;
use std::ops::Range;

use saphyr_parser::{Event, Marker, Parser, ScalarStyle, ScanError};
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Expected, IntoDeserializer, Unexpected, Visitor};
use serde_spanned::de::{SpannedDeserializer, is_spanned};

use super::error::{Issue, line_column};
use super::schema::AgentFile;

/// The YAML `text` as plain JSON data, without the schema's checks (the JSON Schema test
/// validates the data against the generated schema).
#[cfg(all(test, feature = "schema"))]
pub(crate) fn to_json(text: &str) -> Result<serde_json::Value, String> {
    let root = Builder::new(text).build().map_err(|e| e.message)?;
    let de = NodeDe {
        node: &root,
        path: String::new(),
    };
    serde_json::Value::deserialize(de).map_err(|e| e.to_string())
}

/// Deepest nesting of mappings and sequences a definition may use. The schema needs three
/// (`stt` > `deepgram` > `keyterms`); the limit bounds the builder's stack and the recursion of
/// dropping the tree.
pub(crate) const MAX_DEPTH: usize = 16;

/// Reads the YAML `text` into an [`AgentFile`]: syntax, types, ranges and unknown keys only
/// (like the TOML reader). The [`Issue`] has the key path and position of the first problem.
pub(crate) fn parse(text: &str) -> Result<AgentFile, Issue> {
    // The parser is third-party code fed with operator (later possibly tenant) input: should it
    // panic on some malformed text, that is a rejected definition, not a dead caller.
    let root = std::panic::catch_unwind(|| Builder::new(text).build()).unwrap_or_else(|_| {
        Err(Issue {
            key: String::new(),
            position: None,
            message: "the YAML parser failed on this text; check its syntax".to_owned(),
        })
    })?;
    let de = NodeDe {
        node: &root,
        path: String::new(),
    };
    AgentFile::deserialize(de).map_err(|e| e.into_issue(text))
}

/// A node of the YAML tree with its byte range in the source text.
#[derive(Debug)]
struct Node {
    /// What the node is.
    kind: Kind,
    /// Byte range in the text. A mapping or sequence that is the value of a key starts at the key
    /// (like a TOML table at its header), so a problem of the whole table points at its name.
    span: Range<usize>,
}

/// The three kinds of YAML nodes.
#[derive(Debug)]
enum Kind {
    /// A scalar; `plain` when it was written without quotes or block indicator (only plain
    /// scalars are typed by the core schema).
    Scalar { value: String, plain: bool },
    /// A sequence.
    Seq(Vec<Node>),
    /// A mapping, entries in file order.
    Map(Vec<Entry>),
}

/// One `key: value` entry of a mapping.
#[derive(Debug)]
struct Entry {
    /// The key text.
    key: String,
    /// Byte range of the key.
    key_span: Range<usize>,
    /// The value.
    value: Node,
}

/// A collection being built from parser events.
enum Frame {
    /// A sequence started at byte `start`.
    Seq { start: usize, items: Vec<Node> },
    /// A mapping started at byte `start`; `key` is the key whose value comes next.
    Map {
        start: usize,
        entries: Vec<Entry>,
        key: Option<(String, Range<usize>)>,
    },
}

/// Builds the [`Node`] tree from parser events.
struct Builder<'t> {
    /// The source text.
    text: &'t str,
    /// Where the YAML starts: after a byte order mark, which the parser would read as content.
    body_start: usize,
    /// The last marker turned into a byte offset: (character index from `body_start`, byte
    /// offset). Markers arrive almost in order, so [`offset`](Self::offset) walks only from here
    /// and reading a file stays linear in its size even when it is one long line.
    cursor: Cell<(usize, usize)>,
    /// The open collections, outermost first.
    stack: Vec<Frame>,
    /// The finished root node.
    root: Option<Node>,
    /// Documents seen so far.
    documents: usize,
}

impl<'t> Builder<'t> {
    /// A builder over `text`.
    fn new(text: &'t str) -> Self {
        let body_start = if text.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        };
        Self {
            text,
            body_start,
            cursor: Cell::new((0, body_start)),
            stack: Vec::new(),
            root: None,
            documents: 0,
        }
    }

    /// Byte offset of `marker`, clamped to the text. The parser's `index` counts characters from
    /// the start of its input (after the byte order mark), not bytes. It is mapped by walking
    /// from the previous marker, forwards or backwards, so the total work is the distance the
    /// markers move (about the text length), not the column of every event.
    fn offset(&self, marker: &Marker) -> usize {
        let target = marker.index();
        let (mut chars, mut byte) = self.cursor.get();
        if target >= chars {
            let mut ahead = self.text.get(byte..).unwrap_or("").chars();
            while chars < target {
                let Some(c) = ahead.next() else { break };
                byte = byte.saturating_add(c.len_utf8());
                chars = chars.saturating_add(1);
            }
        } else {
            let mut behind = self.text.get(self.body_start..byte).unwrap_or("").chars();
            while chars > target {
                let Some(c) = behind.next_back() else { break };
                byte = byte.saturating_sub(c.len_utf8());
                chars = chars.saturating_sub(1);
            }
        }
        self.cursor.set((chars, byte));
        byte
    }

    /// The key path of the node being built (`stt.deepgram.keyterms[2]`).
    fn path(&self) -> String {
        let mut path = String::new();
        for frame in &self.stack {
            match frame {
                Frame::Seq { items, .. } => {
                    path.push('[');
                    path.push_str(&items.len().to_string());
                    path.push(']');
                }
                Frame::Map {
                    key: Some((key, _)),
                    ..
                } => push_key(&mut path, key),
                Frame::Map { key: None, .. } => {}
            }
        }
        path
    }

    /// An issue of the node being built at byte `offset`.
    fn issue(&self, offset: usize, message: impl Into<String>) -> Issue {
        Issue {
            key: self.path(),
            position: Some(line_column(self.text, offset)),
            message: message.into(),
        }
    }

    /// The issue of a syntax error.
    fn scan_issue(&self, error: &ScanError) -> Issue {
        self.issue(
            self.offset(error.marker()),
            format!("YAML syntax error: {}", error.info()),
        )
    }

    /// Reads every event into the tree. An empty document (only comments) is an empty mapping.
    fn build(mut self) -> Result<Node, Issue> {
        // YAML reads a lone CR as a line break, but positions (here and in the shared checks)
        // count lines by LF, so every later position would be wrong. TOML rejects it too.
        if let Some(cr) = lone_carriage_return(self.text) {
            return Err(self.issue(
                cr,
                "a carriage return (`\\r`) must be followed by a line feed; save the file with \
                 LF or CRLF line ends",
            ));
        }
        let mut parser = Parser::new_from_str(self.text.get(self.body_start..).unwrap_or(""));
        while let Some(next) = parser.next_event() {
            let (event, span) = next.map_err(|e| self.scan_issue(&e))?;
            let start = self.offset(&span.start);
            let end = self.offset(&span.end);
            match event {
                Event::Nothing | Event::StreamStart | Event::DocumentEnd => {}
                Event::StreamEnd => break,
                Event::DocumentStart(_) => {
                    self.documents = self.documents.saturating_add(1);
                    if self.documents > 1 {
                        return Err(self.issue(
                            start,
                            "a definition file holds one YAML document; remove this `---` and \
                             what follows",
                        ));
                    }
                }
                Event::Alias(_) => {
                    return Err(self.issue(
                        start,
                        "YAML aliases (`*name`) are not supported in an agent definition; write \
                         the value out",
                    ));
                }
                Event::Scalar(value, style, anchor, tag) => {
                    self.check_properties(start, anchor, tag.is_some())?;
                    let plain = matches!(style, ScalarStyle::Plain);
                    let node = Node {
                        kind: Kind::Scalar {
                            value: value.into_owned(),
                            plain,
                        },
                        span: start..end,
                    };
                    self.push(node)?;
                }
                Event::SequenceStart(anchor, tag) => {
                    self.check_properties(start, anchor, tag.is_some())?;
                    self.open(
                        start,
                        Frame::Seq {
                            start,
                            items: Vec::new(),
                        },
                    )?;
                }
                Event::MappingStart(anchor, tag) => {
                    self.check_properties(start, anchor, tag.is_some())?;
                    self.open(
                        start,
                        Frame::Map {
                            start,
                            entries: Vec::new(),
                            key: None,
                        },
                    )?;
                }
                Event::SequenceEnd | Event::MappingEnd => self.close(end)?,
            }
        }
        if !self.stack.is_empty() {
            return Err(self.issue(
                self.text.len(),
                "the YAML document ends inside a collection",
            ));
        }
        Ok(self.root.unwrap_or(Node {
            kind: Kind::Map(Vec::new()),
            span: 0..0,
        }))
    }

    /// Rejects anchors (`&name`) and tags (`!!str`, `!name`).
    fn check_properties(&self, start: usize, anchor: usize, tagged: bool) -> Result<(), Issue> {
        if anchor != 0 {
            return Err(self.issue(
                start,
                "YAML anchors (`&name`) are not supported in an agent definition; write the \
                 value out",
            ));
        }
        if tagged {
            return Err(self.issue(
                start,
                "YAML tags (`!!str`, `!name`) are not supported in an agent definition; quote a \
                 value to make it a string",
            ));
        }
        Ok(())
    }

    /// Opens a collection at byte `start`.
    fn open(&mut self, start: usize, frame: Frame) -> Result<(), Issue> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(self.issue(
                start,
                format!("the definition is nested deeper than {MAX_DEPTH} levels"),
            ));
        }
        if let Some(Frame::Map { key: None, .. }) = self.stack.last() {
            return Err(self.issue(
                start,
                "a key must be a name, not a list or a mapping (YAML complex keys are not \
                 supported)",
            ));
        }
        self.stack.push(frame);
        Ok(())
    }

    /// Closes the innermost collection, which ends at byte `end`.
    fn close(&mut self, end: usize) -> Result<(), Issue> {
        let node = match self.stack.pop() {
            Some(Frame::Seq { start, items }) => Node {
                kind: Kind::Seq(items),
                span: start..end,
            },
            Some(Frame::Map { start, entries, .. }) => {
                if let Some(duplicate) = first_duplicate(&entries) {
                    let key = entries.get(duplicate).map_or("", |e| e.key.as_str());
                    let offset = entries.get(duplicate).map_or(start, |e| e.key_span.start);
                    let mut path = self.path();
                    push_key(&mut path, key);
                    return Err(Issue {
                        key: path,
                        position: Some(line_column(self.text, offset)),
                        message: format!("duplicate key `{key}`"),
                    });
                }
                Node {
                    kind: Kind::Map(entries),
                    span: start..end,
                }
            }
            None => return Err(self.issue(end, "unbalanced YAML collection end")),
        };
        self.push(node)
    }

    /// Adds a finished node to the innermost collection (or makes it the root).
    fn push(&mut self, mut node: Node) -> Result<(), Issue> {
        let error = match self.stack.last_mut() {
            None if self.root.is_none() => {
                self.root = Some(node);
                return Ok(());
            }
            None => "a definition file holds one YAML document",
            Some(Frame::Seq { items, .. }) => {
                items.push(node);
                return Ok(());
            }
            Some(Frame::Map { entries, key, .. }) => match key.take() {
                Some((key, key_span)) => {
                    if matches!(node.kind, Kind::Seq(_) | Kind::Map(_)) {
                        node.span.start = key_span.start;
                    }
                    entries.push(Entry {
                        key,
                        key_span,
                        value: node,
                    });
                    return Ok(());
                }
                None => match node.kind {
                    Kind::Scalar { value, .. } => {
                        *key = Some((value, node.span));
                        return Ok(());
                    }
                    // `open` rejects a collection in key position before it is built.
                    Kind::Seq(_) | Kind::Map(_) => "a key must be a name",
                },
            },
        };
        Err(self.issue(self.text.len(), error))
    }
}

/// Byte offset of the first `\r` that is not part of a `\r\n` pair, if any.
fn lone_carriage_return(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    bytes.iter().enumerate().find_map(|(i, &b)| {
        (b == b'\r' && bytes.get(i.saturating_add(1)) != Some(&b'\n')).then_some(i)
    })
}

/// Index of the later entry of the first duplicated key (by position), if any. Sorting keeps
/// this `O(n log n)` for a large mapping.
fn first_duplicate(entries: &[Entry]) -> Option<usize> {
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&a, &b| {
        let key = |i: usize| entries.get(i).map(|e| e.key.as_str());
        key(a).cmp(&key(b)).then(a.cmp(&b))
    });
    order
        .windows(2)
        .filter_map(|pair| match *pair {
            [a, b] if entries.get(a).map(|e| &e.key) == entries.get(b).map(|e| &e.key) => Some(b),
            _ => None,
        })
        .min()
}

/// Appends `key` to a dotted key path.
fn push_key(path: &mut String, key: &str) {
    if !path.is_empty() {
        path.push('.');
    }
    path.push_str(key);
}

/// `parent.key` as a new path.
fn child_path(parent: &str, key: &str) -> String {
    let mut path = parent.to_owned();
    push_key(&mut path, key);
    path
}

/// A read error: the message, then the key path and byte offset of the innermost node it
/// concerns (filled in by the node that sees it first).
#[derive(Debug)]
pub(crate) struct DeError {
    /// What is wrong (serde's standard wording, the same as the TOML reader's).
    message: String,
    /// Key path and byte offset.
    at: Option<(String, usize)>,
}

impl DeError {
    /// Locates the error at `path` and byte `offset` unless an inner node already did.
    fn locate(mut self, path: &str, offset: usize) -> Self {
        if self.at.is_none() {
            self.at = Some((path.to_owned(), offset));
        }
        self
    }

    /// The [`Issue`] for this error in `text`.
    fn into_issue(self, text: &str) -> Issue {
        let (key, position) = match self.at {
            Some((key, offset)) => (key, Some(line_column(text, offset))),
            None => (String::new(), None),
        };
        Issue {
            key,
            position,
            message: super::with_hints(self.message),
        }
    }
}

impl fmt::Display for DeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DeError {}

impl de::Error for DeError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self {
            message: msg.to_string(),
            at: None,
        }
    }

    /// serde's wording, except that a YAML null is called `null` (serde says "unit value").
    fn invalid_type(unexpected: Unexpected<'_>, expected: &dyn Expected) -> Self {
        match unexpected {
            Unexpected::Unit => {
                Self::custom(format_args!("invalid type: null, expected {expected}"))
            }
            other => Self::custom(format_args!("invalid type: {other}, expected {expected}")),
        }
    }
}

/// A plain scalar resolved by the YAML 1.2 core schema.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Resolved<'s> {
    /// `null`, `~` or nothing.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer that fits `i64`.
    Int(i64),
    /// An integer above `i64::MAX` that fits `u64`.
    UInt(u64),
    /// An integer out of the `u64` and `i64` range.
    BadInt,
    /// A float.
    Float(f64),
    /// A string.
    Str(&'s str),
}

/// Resolves a scalar; quoted and block scalars are always strings.
fn resolve(value: &str, plain: bool) -> Resolved<'_> {
    if !plain {
        return Resolved::Str(value);
    }
    match value {
        "" | "~" | "null" | "Null" | "NULL" => return Resolved::Null,
        "true" | "True" | "TRUE" => return Resolved::Bool(true),
        "false" | "False" | "FALSE" => return Resolved::Bool(false),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            return Resolved::Float(f64::INFINITY);
        }
        "-.inf" | "-.Inf" | "-.INF" => return Resolved::Float(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => return Resolved::Float(f64::NAN),
        _ => {}
    }
    let radix = |prefix: &str, radix: u32| {
        value.strip_prefix(prefix).and_then(|digits| {
            (!digits.is_empty() && digits.chars().all(|c| c.is_digit(radix)))
                .then(|| integer(u64::from_str_radix(digits, radix).ok()))
        })
    };
    if let Some(resolved) = radix("0x", 16).or_else(|| radix("0o", 8)) {
        return resolved;
    }
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    if !unsigned.is_empty() && unsigned.bytes().all(|b| b.is_ascii_digit()) {
        return match value.parse::<i64>() {
            Ok(n) => Resolved::Int(n),
            Err(_) => integer(value.strip_prefix('+').unwrap_or(value).parse().ok()),
        };
    }
    if is_core_float(unsigned)
        && let Ok(f) = value.parse::<f64>()
    {
        return Resolved::Float(f);
    }
    Resolved::Str(value)
}

/// An unsigned integer as `Int` when it fits `i64` (as TOML reports it), else `UInt`.
fn integer(n: Option<u64>) -> Resolved<'static> {
    match n {
        Some(n) => i64::try_from(n).map_or(Resolved::UInt(n), Resolved::Int),
        None => Resolved::BadInt,
    }
}

/// The core schema float without its sign: `( \. [0-9]+ | [0-9]+ ( \. [0-9]* )? ) ( [eE] [-+]?
/// [0-9]+ )?`.
fn is_core_float(s: &str) -> bool {
    let (mantissa, exponent) = match s.find(['e', 'E']) {
        Some(i) => (s.get(..i).unwrap_or(""), s.get(i.saturating_add(1)..)),
        None => (s, None),
    };
    let digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((whole, fraction)) => {
            digits(whole) && digits(fraction) && !(whole.is_empty() && fraction.is_empty())
        }
        None => !mantissa.is_empty() && digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    mantissa_ok && exponent_ok
}

/// A serde deserializer over one node; `path` is the node's key path.
struct NodeDe<'n> {
    /// The node.
    node: &'n Node,
    /// Its key path.
    path: String,
}

impl NodeDe<'_> {
    /// Whether the node is a null scalar.
    fn is_null(&self) -> bool {
        matches!(&self.node.kind, Kind::Scalar { value, plain } if resolve(value, *plain) == Resolved::Null)
    }

    /// Locates an error at this node.
    fn locate<T>(&self, result: Result<T, DeError>) -> Result<T, DeError> {
        result.map_err(|e| e.locate(&self.path, self.node.span.start))
    }
}

impl<'de> IntoDeserializer<'de, DeError> for NodeDe<'_> {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

/// An empty mapping (a null read as a table).
const NO_ENTRIES: &[Entry] = &[];

impl<'de> de::Deserializer<'de> for NodeDe<'_> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let result = match &self.node.kind {
            Kind::Map(entries) => visitor.visit_map(MapDe {
                entries: entries.iter(),
                parent: &self.path,
                value: None,
            }),
            Kind::Seq(items) => visitor.visit_seq(SeqDe {
                items: items.iter().enumerate(),
                parent: &self.path,
            }),
            Kind::Scalar { value, plain } => match resolve(value, *plain) {
                Resolved::Null => visitor.visit_unit(),
                Resolved::Bool(b) => visitor.visit_bool(b),
                Resolved::Int(n) => visitor.visit_i64(n),
                Resolved::UInt(n) => visitor.visit_u64(n),
                Resolved::Float(f) => visitor.visit_f64(f),
                Resolved::Str(s) => visitor.visit_str(s),
                Resolved::BadInt => Err(de::Error::custom(format_args!(
                    "integer `{value}` is out of range"
                ))),
            },
        };
        self.locate(result)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        if self.is_null() {
            let result = visitor.visit_none();
            self.locate(result)
        } else {
            let (path, offset) = (self.path.clone(), self.node.span.start);
            visitor
                .visit_some(self)
                .map_err(|e| e.locate(&path, offset))
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        if is_spanned(name) {
            let (path, span) = (self.path.clone(), self.node.span.clone());
            let start = span.start;
            return visitor
                .visit_map(SpannedDeserializer::new(self, span))
                .map_err(|e| e.locate(&path, start));
        }
        if self.is_null() {
            let result = visitor.visit_map(MapDe {
                entries: NO_ENTRIES.iter(),
                parent: &self.path,
                value: None,
            });
            return self.locate(result);
        }
        let _unused = fields;
        if let Kind::Seq(_) = self.node.kind {
            // serde's struct visitors also accept a sequence (fields by position); TOML has no
            // such form, so neither does YAML.
            let error = <DeError as de::Error>::invalid_type(Unexpected::Seq, &visitor);
            return self.locate(Err(error));
        }
        self.deserialize_any(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_struct("", &[], visitor)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        if self.is_null() {
            let result = visitor.visit_seq(SeqDe {
                items: [].iter().enumerate(),
                parent: &self.path,
            });
            return self.locate(result);
        }
        self.deserialize_any(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        if let Kind::Scalar { value, plain } = &self.node.kind
            && let Resolved::Str(s) = resolve(value, *plain)
        {
            let result = visitor.visit_enum(de::value::StrDeserializer::<DeError>::new(s));
            return self.locate(result);
        }
        self.deserialize_any(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        let (path, offset) = (self.path.clone(), self.node.span.start);
        visitor
            .visit_newtype_struct(self)
            .map_err(|e| e.locate(&path, offset))
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf unit
        unit_struct tuple tuple_struct identifier
    }
}

/// The entries of a mapping, for a struct (or `Spanned`) visitor.
struct MapDe<'n, 'p> {
    /// Entries not read yet.
    entries: std::slice::Iter<'n, Entry>,
    /// The mapping's key path.
    parent: &'p str,
    /// The value of the key just read, with its path.
    value: Option<(&'n Node, String)>,
}

impl<'de> de::MapAccess<'de> for MapDe<'_, '_> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, DeError> {
        let Some(entry) = self.entries.next() else {
            return Ok(None);
        };
        let path = child_path(self.parent, &entry.key);
        // Keys are always strings, as in TOML (`1:` is the key "1", not a field index).
        let key = seed
            .deserialize(KeyDe(&entry.key))
            .map_err(|e| e.locate(&path, entry.key_span.start))?;
        self.value = Some((&entry.value, path));
        Ok(Some(key))
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DeError> {
        match self.value.take() {
            Some((node, path)) => seed.deserialize(NodeDe { node, path }),
            None => Err(de::Error::custom("a value was read before its key")),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

/// The items of a sequence.
struct SeqDe<'n, 'p> {
    /// Items not read yet, with their index.
    items: std::iter::Enumerate<std::slice::Iter<'n, Node>>,
    /// The sequence's key path.
    parent: &'p str,
}

impl<'de> de::SeqAccess<'de> for SeqDe<'_, '_> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, DeError> {
        let Some((i, node)) = self.items.next() else {
            return Ok(None);
        };
        let path = format!("{}[{i}]", self.parent);
        seed.deserialize(NodeDe { node, path }).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len())
    }
}

/// A mapping key: always a string.
struct KeyDe<'k>(&'k str);

impl<'de> de::Deserializer<'de> for KeyDe<'_> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_str(self.0)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier
        ignored_any
    }
}

#[cfg(test)]
mod tests;
