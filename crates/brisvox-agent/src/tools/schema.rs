//! JSON Schema validation of tool arguments (TOOLS.md item 4).
//!
//! A tool's parameter schema is compiled once, at registration, with the `jsonschema` crate built
//! without its network features: a schema with a remote `$ref` fails to compile instead of
//! fetching anything. Every call's arguments are checked against the compiled schema, and the
//! problems are reported as a short, bounded list of path and reason for the LLM.

use jsonschema::Validator;
use serde_json::Value;

use super::bounded;

/// Issues reported to the LLM for one call; further problems only set [`Issues::truncated`].
const MAX_ISSUES: usize = 5;
/// Bytes of an issue's instance path (a JSON pointer) kept in the report.
const MAX_PATH_BYTES: usize = 128;
/// Bytes of an issue's reason kept in the report (the reason may quote the offending value).
const MAX_REASON_BYTES: usize = 200;

/// One problem with a call's arguments: where, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Issue {
    /// JSON pointer of the offending value (`/filter/tags/1`), or `(root)` for the whole object.
    pub(super) path: String,
    /// The validator's message, bounded to [`MAX_REASON_BYTES`].
    pub(super) reason: String,
}

/// The problems found in one call's arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Issues {
    /// At most [`MAX_ISSUES`] problems, in the validator's order.
    pub(super) items: Vec<Issue>,
    /// Whether problems beyond the reported ones exist.
    pub(super) truncated: bool,
}

impl Issues {
    /// Whether the arguments are valid.
    pub(super) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Compiles a tool's parameter schema (draft detected from `$schema`, 2020-12 by default).
///
/// # Errors
/// The compiler's message, bounded, when the schema is invalid or cannot be compiled offline.
pub(super) fn compile(schema: &Value) -> Result<Validator, String> {
    jsonschema::validator_for(schema).map_err(|error| bounded(&error.to_string(), MAX_REASON_BYTES))
}

/// Validates `args` against a compiled schema. Valid arguments give empty [`Issues`].
pub(super) fn issues(validator: &Validator, args: &Value) -> Issues {
    let mut found = Issues::default();
    // Valid arguments are the common case: `is_valid` stops at the first problem and builds no
    // error values, so it is much cheaper than collecting errors.
    if validator.is_valid(args) {
        return found;
    }
    for error in validator.iter_errors(args) {
        if found.items.len() == MAX_ISSUES {
            found.truncated = true;
            break;
        }
        let path = error.instance_path().to_string();
        let path = if path.is_empty() {
            "(root)".to_owned()
        } else {
            bounded(&path, MAX_PATH_BYTES)
        };
        found.items.push(Issue {
            path,
            reason: bounded(&error.to_string(), MAX_REASON_BYTES),
        });
    }
    found
}
