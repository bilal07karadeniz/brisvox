//! Conversion of a tool's JSON Schema to the OpenAPI subset of
//! `functionDeclarations[].parameters`.
//!
//! Brisvox tools carry arbitrary JSON Schema. The `parameters` field of a Gemini function
//! declaration takes an OpenAPI-style `Schema` (upper-case `type`, `nullable`, `anyOf`, no
//! `$ref`). `docs/providers/gemini.md` does not list the fields of that subset (UNVERIFIED), so
//! the conversion keeps only the classic core and drops the rest rather than risk a 400 that
//! would fail every tool of the request. [`SchemaMode::JsonSchema`](super::SchemaMode) sends the
//! schema untouched as `parametersJsonSchema` instead.
//!
//! # Kept
//! `type` (upper-cased; a type list becomes the one non-null type plus `nullable`, or `anyOf`
//! when several remain; a missing type is inferred from `properties`, `items`, `enum`; a schema
//! with no type information at all becomes `STRING`), `description`, `properties`, `required`
//! (only names that are properties), `items`, `enum` (strings only; `const` is a one-value
//! enum), `nullable`, `anyOf` (also from `oneOf`; a `null` member becomes `nullable`),
//! `minimum`, `maximum`, `minItems`, `maxItems`, and `format` for `date-time` (strings),
//! `int32`/`int64` (integers) and `float`/`double` (numbers).
//!
//! # Rewritten
//! Local `$ref`s (`#/$defs/..`, `#/definitions/..`) are inlined; `allOf` members are merged
//! (properties and required united, other keys: the first wins). An `enum` of non-strings is
//! dropped from the schema and listed in the description instead ("Allowed values: ...").
//!
//! # Dropped
//! `$schema`, `$id`, `$defs`, `definitions`, `$comment`, `title`, `default`, `examples`,
//! `additionalProperties`, `patternProperties`, `pattern`, `minLength`, `maxLength`,
//! `exclusiveMinimum`, `exclusiveMaximum`, `multipleOf`, `uniqueItems`, `if`/`then`/`else`,
//! `not`, `dependentSchemas`, `propertyNames`, and every other unknown keyword.
//!
//! # Degraded (and logged as a warning by the caller, with the tool name)
//! Gemini rejects `OBJECT` without `properties` (UNVERIFIED for nested nodes, known for the root),
//! and a free-form value has no representation in the subset, so such nodes cannot be sent as
//! they are and one bad property must not fail every tool of the request:
//! - A nested object without properties (`{"type":"object","additionalProperties":true}`)
//!   becomes `STRING` whose description ends with "Pass a JSON object encoded as a string."
//!   The model then sends a JSON **string** for that argument; the adapter turns it back into
//!   the object before the call reaches the tool ([`restore_objects`], applied to the
//!   arguments of every call of a tool that has such a node), so the tool's own schema
//!   validation passes. (The root without properties is declared without `parameters`.)
//! - A schema with no type information at all (`{}` or only a description) becomes `STRING`:
//!   a parameter that accepts numbers or objects is then offered as a string.
//!
//! # Refused
//! External `$ref`s, an unknown `type`, a root that is not an object, a schema nested deeper
//! than [`MAX_DEPTH`] levels or larger than [`MAX_NODES`] nodes after inlining (a recursive
//! `$ref`). The node budget is charged by both passes (inlining `$ref`/`allOf` and
//! conversion; each pass has its own budget of [`MAX_NODES`]), so a small schema whose
//! `$ref`/`allOf` fan out exponentially fails fast instead of pinning a thread.

use serde_json::{Map, Value};

/// Deepest nesting of a schema after inlining `$ref`s.
pub(crate) const MAX_DEPTH: usize = 24;

/// Most schema nodes one tool may expand to (bounds the work of a recursive or fan-out
/// `$ref`).
pub(crate) const MAX_NODES: usize = 10_000;

/// The `description` suffix of a free-form object sent as a string.
const OBJECT_AS_STRING_NOTE: &str = "Pass a JSON object encoded as a string.";

/// The `degraded` note of a nested free-form object sent as a string.
const NESTED_OBJECT_DEGRADED: &str = "a nested free-form object was sent as STRING";

/// Conversion state: the node budgets and what had to be degraded.
struct Ctx {
    /// Nodes the conversion pass may still visit.
    budget: usize,
    /// Nodes the `$ref`/`allOf` inlining pass may still visit (counted separately, so a legal
    /// schema is not charged twice).
    inline_budget: usize,
    degraded: Vec<&'static str>,
}

impl Ctx {
    fn degrade(&mut self, what: &'static str) {
        if !self.degraded.contains(&what) {
            self.degraded.push(what);
        }
    }
}

/// The result of converting a tool's root schema.
#[derive(Debug)]
pub(crate) struct Converted {
    /// The `parameters` value; `None` for an object without properties: Gemini rejects `OBJECT`
    /// with empty `properties`, so such a tool is declared without `parameters`.
    pub(crate) parameters: Option<Value>,
    /// What was degraded (module docs, "Degraded"); deduplicated, in first-seen order.
    pub(crate) degraded: Vec<&'static str>,
    /// Whether `parameters` holds a nested free-form object sent as a string, so the arguments
    /// of the tool's calls need [`restore_objects`].
    pub(crate) has_object_strings: bool,
}

/// Converts the root `schema` of a tool to a Gemini `Schema`.
///
/// # Errors
/// A description of what is not supported (see the module docs, "Refused"); the caller adds
/// the tool name.
pub(crate) fn convert_parameters(schema: &Value) -> Result<Converted, String> {
    let defs = Defs::of(schema);
    let mut ctx = Ctx {
        budget: MAX_NODES,
        inline_budget: MAX_NODES,
        degraded: Vec::new(),
    };
    let root = normalize(schema, &defs, 0, &mut ctx)?;
    let converted = convert(&root, &defs, 0, &mut ctx)?;
    if converted.get("type").and_then(Value::as_str) != Some("OBJECT") {
        return Err("the root schema must be an object (type \"object\")".to_owned());
    }
    let has_properties = converted
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|p| !p.is_empty());
    let has_object_strings = ctx.degraded.contains(&NESTED_OBJECT_DEGRADED);
    Ok(Converted {
        parameters: has_properties.then_some(Value::Object(converted)),
        degraded: ctx.degraded,
        has_object_strings,
    })
}

/// The definitions a `$ref` may point to.
struct Defs<'a> {
    defs: Option<&'a Map<String, Value>>,
    definitions: Option<&'a Map<String, Value>>,
}

impl<'a> Defs<'a> {
    fn of(root: &'a Value) -> Self {
        Self {
            defs: root.get("$defs").and_then(Value::as_object),
            definitions: root.get("definitions").and_then(Value::as_object),
        }
    }

    fn resolve(&self, reference: &str) -> Result<&'a Value, String> {
        let (table, name) = if let Some(name) = reference.strip_prefix("#/$defs/") {
            (self.defs, name)
        } else if let Some(name) = reference.strip_prefix("#/definitions/") {
            (self.definitions, name)
        } else {
            return Err("only local $ref (#/$defs/.. or #/definitions/..) is supported".to_owned());
        };
        table
            .and_then(|t| t.get(name))
            .ok_or_else(|| "a $ref points to a definition that does not exist".to_owned())
    }
}

/// `node` with its `$ref` inlined and its `allOf` merged, as a map.
fn normalize(
    node: &Value,
    defs: &Defs<'_>,
    depth: usize,
    ctx: &mut Ctx,
) -> Result<Map<String, Value>, String> {
    if depth > MAX_DEPTH {
        return Err(format!(
            "the schema nests deeper than {MAX_DEPTH} levels (recursive $ref?)"
        ));
    }
    // Every visit is charged, so a `$ref`/`allOf` DAG that fans out (not a cycle, so the depth
    // limit alone would allow 8^12 visits) is cut off after MAX_NODES.
    ctx.inline_budget = ctx
        .inline_budget
        .checked_sub(1)
        .ok_or_else(|| format!("the schema expands to more than {MAX_NODES} nodes"))?;
    let Some(object) = node.as_object() else {
        return Err("a sub-schema is not a JSON object".to_owned());
    };
    let mut merged = object.clone();
    if let Some(reference) = merged.remove("$ref") {
        let Some(reference) = reference.as_str() else {
            return Err("$ref is not a string".to_owned());
        };
        let target = normalize(defs.resolve(reference)?, defs, depth + 1, ctx)?;
        // Siblings of the $ref override the target (a description next to a $ref).
        for (key, value) in target {
            merged.entry(key).or_insert(value);
        }
    }
    if let Some(all_of) = merged.remove("allOf") {
        let Some(members) = all_of.as_array() else {
            return Err("allOf is not an array".to_owned());
        };
        for member in members {
            let member = normalize(member, defs, depth + 1, ctx)?;
            merge_into(&mut merged, member);
        }
    }
    Ok(merged)
}

/// Merges `member` into `base`: `properties` and `required` are united, any other key is taken
/// from `member` only when `base` has none.
fn merge_into(base: &mut Map<String, Value>, member: Map<String, Value>) {
    for (key, value) in member {
        match (key.as_str(), base.get_mut(&key)) {
            ("properties", Some(Value::Object(existing))) => {
                if let Value::Object(more) = value {
                    for (name, schema) in more {
                        existing.entry(name).or_insert(schema);
                    }
                }
            }
            ("required", Some(Value::Array(existing))) => {
                if let Value::Array(more) = value {
                    for name in more {
                        if !existing.contains(&name) {
                            existing.push(name);
                        }
                    }
                }
            }
            (_, Some(_)) => {}
            (_, None) => {
                base.insert(key, value);
            }
        }
    }
}

fn gemini_type(json_type: &str) -> Result<&'static str, String> {
    Ok(match json_type {
        "string" => "STRING",
        "number" => "NUMBER",
        "integer" => "INTEGER",
        "boolean" => "BOOLEAN",
        "array" => "ARRAY",
        "object" => "OBJECT",
        "null" => "NULL",
        _ => return Err("a schema has an unknown \"type\"".to_owned()),
    })
}

/// Converts a normalised schema.
fn convert(
    schema: &Map<String, Value>,
    defs: &Defs<'_>,
    depth: usize,
    ctx: &mut Ctx,
) -> Result<Map<String, Value>, String> {
    if depth > MAX_DEPTH {
        return Err(format!(
            "the schema nests deeper than {MAX_DEPTH} levels (recursive $ref?)"
        ));
    }
    ctx.budget = ctx
        .budget
        .checked_sub(1)
        .ok_or_else(|| format!("the schema expands to more than {MAX_NODES} nodes"))?;
    let mut out = Map::new();
    let mut nullable = schema.get("nullable").and_then(Value::as_bool) == Some(true);

    // The declared types, without `null`.
    let mut types: Vec<&'static str> = Vec::new();
    match schema.get("type") {
        Some(Value::String(t)) => types.push(gemini_type(t)?),
        Some(Value::Array(list)) => {
            for item in list {
                let Some(t) = item.as_str() else {
                    return Err("a \"type\" list holds a non-string".to_owned());
                };
                types.push(gemini_type(t)?);
            }
        }
        _ => {}
    }
    if types.contains(&"NULL") && types.len() > 1 {
        nullable = true;
        types.retain(|t| *t != "NULL");
    }
    types.dedup();

    // anyOf / oneOf: a `null` member becomes `nullable`; a single remaining member is merged.
    let members = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array);
    let mut alternatives: Vec<Map<String, Value>> = Vec::new();
    for member in members.into_iter().flatten() {
        let member = normalize(member, defs, depth + 1, ctx)?;
        let member = convert(&member, defs, depth + 1, ctx)?;
        if member.get("type").and_then(Value::as_str) == Some("NULL") && members_len(members) > 1 {
            nullable = true;
        } else {
            alternatives.push(member);
        }
    }
    if types.len() > 1 && alternatives.is_empty() {
        for t in types.drain(..) {
            let mut member = Map::new();
            member.insert("type".to_owned(), Value::String(t.to_owned()));
            alternatives.push(member);
        }
    }
    let mut merged_alternative = false;
    if alternatives.len() == 1 {
        if let Some(only) = alternatives.pop() {
            out = only;
            merged_alternative = true;
        }
    } else if !alternatives.is_empty() {
        out.insert(
            "anyOf".to_owned(),
            Value::Array(alternatives.into_iter().map(Value::Object).collect()),
        );
    }

    // Enum values (`const` is a one-value enum).
    let enum_values: Option<Vec<Value>> = match (schema.get("enum"), schema.get("const")) {
        (Some(Value::Array(values)), _) => Some(values.clone()),
        (None, Some(value)) => Some(vec![value.clone()]),
        _ => None,
    };

    // The type: declared, else inferred.
    let inferred = if schema.contains_key("properties") {
        Some("OBJECT")
    } else if schema.contains_key("items") {
        Some("ARRAY")
    } else if enum_values
        .as_ref()
        .is_some_and(|v| !v.is_empty() && v.iter().all(Value::is_string))
    {
        Some("STRING")
    } else {
        None
    };
    let kind: Option<String> = match types.first() {
        Some(t) => Some((*t).to_owned()),
        None if out.contains_key("anyOf") => None,
        None if merged_alternative => out.get("type").and_then(Value::as_str).map(str::to_owned),
        None => {
            if inferred.is_none() {
                ctx.degrade("a schema without type information was sent as STRING");
            }
            Some(inferred.unwrap_or("STRING").to_owned())
        }
    };
    if let Some(kind) = &kind {
        out.insert("type".to_owned(), Value::String(kind.clone()));
    }
    let kind = kind.as_deref();

    let mut description = schema
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut string_note: Option<&str> = None;

    match kind {
        Some("OBJECT") => {
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                let mut converted = Map::new();
                for (name, property) in properties {
                    let property = normalize(property, defs, depth + 1, ctx)?;
                    converted.insert(
                        name.clone(),
                        Value::Object(convert(&property, defs, depth + 1, ctx)?),
                    );
                }
                let required: Vec<Value> = schema
                    .get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|name| name.as_str().is_some_and(|n| converted.contains_key(n)))
                    .cloned()
                    .collect();
                if !converted.is_empty() {
                    out.insert("properties".to_owned(), Value::Object(converted));
                }
                if !required.is_empty() {
                    out.insert("required".to_owned(), Value::Array(required));
                }
            }
            if depth > 0 && !out.contains_key("properties") {
                // A nested object with nothing to describe: see the module docs, "Degraded".
                ctx.degrade(NESTED_OBJECT_DEGRADED);
                out.insert("type".to_owned(), Value::String("STRING".to_owned()));
                out.remove("required");
                string_note = Some(OBJECT_AS_STRING_NOTE);
            }
        }
        Some("ARRAY") => {
            let items = match schema.get("items") {
                Some(items) => {
                    let items = normalize(items, defs, depth + 1, ctx)?;
                    convert(&items, defs, depth + 1, ctx)?
                }
                None => {
                    let mut any = Map::new();
                    any.insert("type".to_owned(), Value::String("STRING".to_owned()));
                    any
                }
            };
            out.insert("items".to_owned(), Value::Object(items));
            for key in ["minItems", "maxItems"] {
                if let Some(n) = schema.get(key).filter(|n| n.is_u64()) {
                    out.insert(key.to_owned(), n.clone());
                }
            }
        }
        Some("INTEGER" | "NUMBER") => {
            for key in ["minimum", "maximum"] {
                if let Some(n) = schema.get(key).filter(|n| n.is_number()) {
                    out.insert(key.to_owned(), n.clone());
                }
            }
        }
        _ => {}
    }

    if let (Some(format), Some(kind)) = (schema.get("format").and_then(Value::as_str), kind) {
        let supported = matches!(
            (kind, format),
            ("STRING", "date-time")
                | ("INTEGER", "int32" | "int64")
                | ("NUMBER", "float" | "double")
        );
        if supported {
            out.insert("format".to_owned(), Value::String(format.to_owned()));
        }
    }

    if let Some(values) = enum_values.filter(|v| !v.is_empty()) {
        if kind == Some("STRING") && values.iter().all(Value::is_string) {
            out.insert("enum".to_owned(), Value::Array(values));
        } else {
            // Gemini's enum is for strings: keep the information for the model as text.
            let listed: Vec<String> = values.iter().map(Value::to_string).collect();
            let note = format!("Allowed values: {}.", listed.join(", "));
            description = Some(match description {
                Some(text) => format!("{text} {note}"),
                None => note,
            });
        }
    }
    if let Some(note) = string_note {
        description = Some(match description {
            Some(text) => format!("{text} {note}"),
            None => note.to_owned(),
        });
    }
    if let Some(description) = description {
        out.insert("description".to_owned(), Value::String(description));
    }
    if nullable {
        out.insert("nullable".to_owned(), Value::Bool(true));
    }
    Ok(out)
}

/// Whether `schema` (a converted node) is a free-form object that was sent as a string.
fn is_object_string(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("STRING")
        && schema
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|d| d.ends_with(OBJECT_AS_STRING_NOTE))
}

/// Turns the JSON-encoded strings the model sent for free-form objects back into objects.
///
/// `schema` is the converted `parameters` of the tool (see "Degraded" in the module docs),
/// `value` the call's arguments. The walk follows `properties`, `items` and `anyOf` and only
/// replaces a string that parses as a JSON **object**; anything else is left untouched (the
/// tool's own validation then reports it). Returns whether anything was replaced. The depth
/// of the walk is bounded by the schema, which [`MAX_DEPTH`] bounds.
pub(crate) fn restore_objects(schema: &Value, value: &mut Value) -> bool {
    if is_object_string(schema) {
        if let Value::String(text) = value
            && let Ok(object @ Value::Object(_)) = serde_json::from_str::<Value>(text)
        {
            *value = object;
            return true;
        }
        return false;
    }
    let mut changed = false;
    match value {
        Value::Object(map) => {
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (name, sub_schema) in properties {
                    if let Some(sub_value) = map.get_mut(name) {
                        changed |= restore_objects(sub_schema, sub_value);
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    changed |= restore_objects(item_schema, item);
                }
            }
        }
        _ => {}
    }
    if let Some(alternatives) = schema.get("anyOf").and_then(Value::as_array) {
        for alternative in alternatives {
            changed |= restore_objects(alternative, value);
        }
    }
    changed
}

fn members_len(members: Option<&Vec<Value>>) -> usize {
    members.map_or(0, Vec::len)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn convert_text(schema: &Value) -> Option<Value> {
        convert_parameters(schema).unwrap().parameters
    }

    #[test]
    fn a_plain_object_schema_is_upper_cased_and_trimmed() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Weather",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "city": {"type": "string", "description": "City name", "minLength": 1, "pattern": "^[A-Z]"},
                "days": {"type": "integer", "minimum": 1, "maximum": 7, "default": 1},
                "unit": {"type": "string", "enum": ["c", "f"]},
                "tags": {"type": "array", "items": {"type": "string"}, "minItems": 1, "uniqueItems": true}
            },
            "required": ["city", "ghost"]
        });
        assert_eq!(
            convert_text(&schema),
            Some(json!({
                "type": "OBJECT",
                "properties": {
                    "city": {"type": "STRING", "description": "City name"},
                    "days": {"type": "INTEGER", "minimum": 1, "maximum": 7},
                    "unit": {"type": "STRING", "enum": ["c", "f"]},
                    "tags": {"type": "ARRAY", "items": {"type": "STRING"}, "minItems": 1}
                },
                "required": ["city"]
            }))
        );
    }

    #[test]
    fn an_object_without_properties_has_no_parameters() {
        assert_eq!(convert_text(&json!({"type": "object"})), None);
        assert_eq!(
            convert_text(&json!({"type": "object", "properties": {}})),
            None
        );
    }

    #[test]
    fn a_nested_free_form_object_is_degraded_to_a_described_string_not_sent_bare() {
        let schema = json!({"type": "object", "properties": {
            "metadata": {"type": "object", "additionalProperties": true, "description": "Extra data."},
            "bare": {"type": "object"},
            "empty": {"type": "object", "properties": {}, "required": ["x"]},
            "list": {"type": "array", "items": {"type": "object"}},
            "kept": {"type": "object", "properties": {"a": {"type": "integer"}}}
        }, "required": ["metadata"]});
        let converted = convert_parameters(&schema).unwrap();
        let out = converted.parameters.unwrap();
        let p = &out["properties"];
        assert_eq!(
            p["metadata"],
            json!({"type": "STRING",
                   "description": "Extra data. Pass a JSON object encoded as a string."})
        );
        assert_eq!(
            p["bare"],
            json!({"type": "STRING", "description": "Pass a JSON object encoded as a string."})
        );
        assert_eq!(p["empty"]["type"], json!("STRING"));
        assert!(p["empty"].get("required").is_none());
        assert_eq!(p["list"]["items"]["type"], json!("STRING"));
        // An object that has properties is untouched.
        assert_eq!(p["kept"]["type"], json!("OBJECT"));
        fn bare_objects(node: &Value) -> usize {
            match node {
                Value::Object(map) => {
                    let bare = map.get("type") == Some(&json!("OBJECT"))
                        && !map.get("properties").is_some_and(|p| p.is_object());
                    usize::from(bare) + map.values().map(bare_objects).sum::<usize>()
                }
                Value::Array(items) => items.iter().map(bare_objects).sum(),
                _ => 0,
            }
        }
        assert_eq!(bare_objects(&out), 0, "no bare OBJECT anywhere: {out}");
        assert_eq!(
            converted.degraded,
            vec!["a nested free-form object was sent as STRING"]
        );
    }

    #[test]
    fn the_root_without_properties_is_not_a_degradation() {
        let converted = convert_parameters(&json!({"type": "object"})).unwrap();
        assert!(converted.parameters.is_none());
        assert!(converted.degraded.is_empty());
    }

    #[test]
    fn nullable_types_and_anyof_are_normalised() {
        let schema = json!({"type": "object", "properties": {
            "a": {"type": ["string", "null"]},
            "b": {"anyOf": [{"type": "integer"}, {"type": "null"}]},
            "c": {"oneOf": [{"type": "string"}, {"type": "number"}]},
            "d": {"type": ["string", "number"]},
            "e": {"type": "string", "nullable": true}
        }});
        let out = convert_text(&schema).unwrap();
        let p = &out["properties"];
        assert_eq!(p["a"], json!({"type": "STRING", "nullable": true}));
        assert_eq!(p["b"], json!({"type": "INTEGER", "nullable": true}));
        assert_eq!(
            p["c"],
            json!({"anyOf": [{"type": "STRING"}, {"type": "NUMBER"}]})
        );
        assert_eq!(
            p["d"],
            json!({"anyOf": [{"type": "STRING"}, {"type": "NUMBER"}]})
        );
        assert_eq!(p["e"], json!({"type": "STRING", "nullable": true}));
    }

    #[test]
    fn local_refs_are_inlined_and_all_of_is_merged() {
        let schema = json!({
            "type": "object",
            "$defs": {
                "Address": {"type": "object", "properties": {"zip": {"type": "string"}}, "required": ["zip"]},
                "Named": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}
            },
            "properties": {
                "home": {"$ref": "#/$defs/Address", "description": "Home address"},
                "person": {"allOf": [{"$ref": "#/$defs/Named"}, {"properties": {"age": {"type": "integer"}}, "required": ["age"]}]}
            }
        });
        let out = convert_text(&schema).unwrap();
        assert_eq!(
            out["properties"]["home"],
            json!({"type": "OBJECT", "description": "Home address",
                   "properties": {"zip": {"type": "STRING"}}, "required": ["zip"]})
        );
        assert_eq!(
            out["properties"]["person"],
            json!({"type": "OBJECT",
                   "properties": {"name": {"type": "STRING"}, "age": {"type": "INTEGER"}},
                   "required": ["name", "age"]})
        );
    }

    #[test]
    fn a_non_string_enum_moves_into_the_description_and_const_is_an_enum() {
        let schema = json!({"type": "object", "properties": {
            "n": {"type": "integer", "enum": [1, 2, 3], "description": "Pick one."},
            "k": {"const": "fixed"}
        }});
        let out = convert_text(&schema).unwrap();
        assert_eq!(
            out["properties"]["n"],
            json!({"type": "INTEGER", "description": "Pick one. Allowed values: 1, 2, 3."})
        );
        assert_eq!(
            out["properties"]["k"],
            json!({"type": "STRING", "enum": ["fixed"]})
        );
    }

    #[test]
    fn only_documented_formats_survive_and_untyped_schemas_become_strings() {
        let schema = json!({"type": "object", "properties": {
            "when": {"type": "string", "format": "date-time"},
            "mail": {"type": "string", "format": "email"},
            "big": {"type": "integer", "format": "int64"},
            "any": {}
        }});
        let converted = convert_parameters(&schema).unwrap();
        assert_eq!(
            converted.degraded,
            vec!["a schema without type information was sent as STRING"],
            "the type-less `any` is reported"
        );
        let out = converted.parameters.unwrap();
        let p = &out["properties"];
        assert_eq!(p["when"], json!({"type": "STRING", "format": "date-time"}));
        assert_eq!(p["mail"], json!({"type": "STRING"}));
        assert_eq!(p["big"], json!({"type": "INTEGER", "format": "int64"}));
        assert_eq!(p["any"], json!({"type": "STRING"}));
    }

    #[test]
    fn arrays_without_items_get_string_items() {
        let schema = json!({"type": "object", "properties": {"xs": {"type": "array"}}});
        let out = convert_text(&schema).unwrap();
        assert_eq!(
            out["properties"]["xs"],
            json!({"type": "ARRAY", "items": {"type": "STRING"}})
        );
    }

    #[test]
    fn unsupported_schemas_are_refused_with_a_reason() {
        let external = json!({"type": "object", "properties": {"a": {"$ref": "http://x/y"}}});
        assert!(
            convert_parameters(&external)
                .unwrap_err()
                .contains("local $ref")
        );
        let missing = json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/none"}}});
        assert!(
            convert_parameters(&missing)
                .unwrap_err()
                .contains("does not exist")
        );
        let unknown = json!({"type": "object", "properties": {"a": {"type": "quark"}}});
        assert!(
            convert_parameters(&unknown)
                .unwrap_err()
                .contains("unknown")
        );
        let not_object = json!({"type": "string"});
        assert!(
            convert_parameters(&not_object)
                .unwrap_err()
                .contains("root")
        );
        let not_schema = json!({"type": "object", "properties": {"a": true}});
        assert!(convert_parameters(&not_schema).is_err());
    }

    #[test]
    fn a_recursive_ref_is_refused_not_expanded_forever() {
        let schema = json!({
            "type": "object",
            "$defs": {"Node": {"type": "object", "properties": {
                "left": {"$ref": "#/$defs/Node"}, "right": {"$ref": "#/$defs/Node"}}}},
            "properties": {"root": {"$ref": "#/$defs/Node"}}
        });
        let error = convert_parameters(&schema).unwrap_err();
        assert!(
            error.contains("nests deeper") || error.contains("more than"),
            "{error}"
        );
    }

    #[test]
    fn a_ref_and_all_of_fan_out_is_cut_off_by_the_node_budget() {
        // Six levels of eight `$ref` members each: no cycle (so the depth limit never trips),
        // but 8^6 = 262 144 visits of the inlining pass for about 1 KiB of schema text.
        let mut defs = serde_json::Map::new();
        for k in 0..6 {
            let members: Vec<Value> = (0..8)
                .map(|_| json!({"$ref": format!("#/$defs/d{}", k + 1)}))
                .collect();
            defs.insert(format!("d{k}"), json!({"allOf": members}));
        }
        defs.insert(
            "d6".to_owned(),
            json!({"type": "object", "properties": {"a": {"type": "string"}}}),
        );
        let schema = json!({"$defs": defs, "$ref": "#/$defs/d0"});
        let error = convert_parameters(&schema).unwrap_err();
        assert!(error.contains("more than"), "{error}");
        // Within the budget the same shape is fine: two levels of eight are 73 visits.
        let mut defs = serde_json::Map::new();
        let members: Vec<Value> = (0..8).map(|_| json!({"$ref": "#/$defs/d1"})).collect();
        defs.insert("d0".to_owned(), json!({"allOf": members}));
        defs.insert(
            "d1".to_owned(),
            json!({"type": "object", "properties": {"a": {"type": "string"}}}),
        );
        let schema = json!({"$defs": defs, "$ref": "#/$defs/d0"});
        assert!(convert_parameters(&schema).unwrap().parameters.is_some());
    }

    #[test]
    fn strings_sent_for_free_form_objects_are_turned_back_into_objects() {
        let schema = json!({"type": "object", "properties": {
            "metadata": {"type": "object", "additionalProperties": true},
            "name": {"type": "string"},
            "items": {"type": "array", "items": {"type": "object"}},
            "inner": {"type": "object", "properties": {"extra": {"type": "object"}}},
            "either": {"anyOf": [{"type": "object"}, {"type": "integer"}]}
        }});
        let converted = convert_parameters(&schema).unwrap();
        assert!(converted.has_object_strings);
        let parameters = converted.parameters.unwrap();
        let mut args = json!({
            "metadata": "{\"a\": 1, \"b\": [true]}",
            "name": "{\"not\": \"an object parameter\"}",
            "items": ["{\"k\": \"v\"}", "{\"k\": \"w\"}"],
            "inner": {"extra": "{\"deep\": null}"},
            "either": "{\"x\": 2}"
        });
        assert!(restore_objects(&parameters, &mut args));
        assert_eq!(
            args,
            json!({
                "metadata": {"a": 1, "b": [true]},
                // A string parameter is a string, whatever it contains.
                "name": "{\"not\": \"an object parameter\"}",
                "items": [{"k": "v"}, {"k": "w"}],
                "inner": {"extra": {"deep": null}},
                "either": {"x": 2}
            })
        );
        // Not JSON, not an object, or already an object: left alone, nothing reported.
        let mut args = json!({"metadata": "not json", "items": ["[1]", {"k": 1}], "either": 3});
        let before = args.clone();
        assert!(!restore_objects(&parameters, &mut args));
        assert_eq!(args, before);
        // A schema without such a node reports nothing to restore.
        let plain =
            convert_parameters(&json!({"type": "object", "properties": {"a": {"type": "string"}}}))
                .unwrap();
        assert!(!plain.has_object_strings);
    }
}
