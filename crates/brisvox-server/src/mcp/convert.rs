//! An MCP `tools/call` result as the tool engine's result: text for the LLM, or the error it
//! should read. Pure functions over the SDK's types, tested without a server.

use std::fmt::Write as _;

use brisvox_agent::tools::{ToolError, ToolOutput};
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};

/// Longest result handed to the engine, in bytes (under the engine's default bound of 64 KiB, so
/// the cut is ours, with a note, and not the engine's).
pub(crate) const MAX_RESULT_BYTES: usize = 60 * 1024;

/// Longest tool description sent to the LLM, in characters (a server's description is
/// untrusted text; a bounded one cannot flood the prompt).
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 1000;

/// What the engine reads when a tool answered nothing.
const NO_CONTENT: &str = "(the tool returned no content)";

/// What the engine reads when a tool reported an error without saying which.
const UNEXPLAINED_ERROR: &str = "the tool reported an error without a message";

/// `result` as a tool result.
///
/// - `isError: true` is the tool's error, with its text: the LLM reads it and may correct the
///   call.
/// - Structured content, when the tool gave some, is the result as JSON.
/// - Otherwise the text blocks, joined by line breaks. What cannot be spoken or read (images,
///   audio, binary resources) is replaced by a one-line note saying what it was.
///
/// A result over [`MAX_RESULT_BYTES`] is cut at a character boundary with a note.
pub(crate) fn tool_output(result: CallToolResult) -> Result<ToolOutput, ToolError> {
    let is_error = result.is_error.unwrap_or(false);
    let text = content_text(&result.content);
    if is_error {
        let message = if text.trim().is_empty() {
            UNEXPLAINED_ERROR.to_owned()
        } else {
            text
        };
        return Err(ToolError::new(cut(message)));
    }
    if let Some(structured) = result.structured_content
        && !structured.is_null()
    {
        // Compact JSON, as the engine sends it; one too large for the engine becomes text.
        return Ok(match serde_json::to_string(&structured) {
            Ok(json) if json.len() <= MAX_RESULT_BYTES => ToolOutput::Json(structured),
            Ok(json) => ToolOutput::Text(cut(json)),
            // Not serializable (cannot happen for parsed JSON): fall back to the text blocks.
            Err(_) => ToolOutput::Text(cut(text)),
        });
    }
    if text.is_empty() {
        return Ok(ToolOutput::Text(NO_CONTENT.to_owned()));
    }
    Ok(ToolOutput::Text(cut(text)))
}

/// The blocks as text, one line each.
fn content_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if !out.is_empty() {
            out.push('\n');
        }
        match block {
            ContentBlock::Text(text) => out.push_str(&text.text),
            ContentBlock::Image(image) => {
                let _ = write!(
                    out,
                    "[image of type {}, {} base64 characters, not shown]",
                    image.mime_type,
                    image.data.len()
                );
            }
            ContentBlock::Audio(audio) => {
                let _ = write!(
                    out,
                    "[audio of type {}, {} base64 characters, not shown]",
                    audio.mime_type,
                    audio.data.len()
                );
            }
            ContentBlock::Resource(embedded) => match &embedded.resource {
                ResourceContents::TextResourceContents { text, .. } => out.push_str(text),
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => {
                    let _ = write!(
                        out,
                        "[binary resource {uri} of type {}, {} base64 characters, not shown]",
                        mime_type.as_deref().unwrap_or("unknown"),
                        blob.len()
                    );
                }
                #[allow(unreachable_patterns)] // the SDK's enum may grow
                _ => out.push_str("[resource not shown]"),
            },
            ContentBlock::ResourceLink(link) => {
                let _ = write!(out, "[resource {}: {}]", link.name, link.uri);
            }
            #[allow(unreachable_patterns)] // the SDK's enum is non-exhaustive
            _ => out.push_str("[content not shown]"),
        }
        if out.len() > MAX_RESULT_BYTES.saturating_mul(2) {
            // Far past the bound: stop collecting.
            break;
        }
    }
    out
}

/// `text` cut to [`MAX_RESULT_BYTES`] at a character boundary, with a note when it was cut.
pub(crate) fn cut(mut text: String) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        return text;
    }
    let mut end = MAX_RESULT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n[cut: the result was longer than the agent reads]");
    text
}

/// `text` as a tool description: the first [`MAX_DESCRIPTION_CHARS`] characters.
pub(crate) fn description(text: &str) -> String {
    let mut out: String = text.chars().take(MAX_DESCRIPTION_CHARS).collect();
    if text.chars().count() > MAX_DESCRIPTION_CHARS {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use rmcp::model::{CallToolResult, ContentBlock};
    use serde_json::json;

    use super::*;

    fn ok(blocks: Vec<ContentBlock>) -> CallToolResult {
        CallToolResult::success(blocks)
    }

    #[test]
    fn text_blocks_join_by_lines() {
        let out = tool_output(ok(vec![
            ContentBlock::text("first"),
            ContentBlock::text("second"),
        ]))
        .unwrap();
        assert_eq!(out, ToolOutput::Text("first\nsecond".to_owned()));
    }

    #[test]
    fn structured_content_wins_over_text() {
        let mut result = ok(vec![ContentBlock::text("{\"n\":1}")]);
        result.structured_content = Some(json!({"n": 1}));
        assert_eq!(
            tool_output(result).unwrap(),
            ToolOutput::Json(json!({"n": 1}))
        );
    }

    #[test]
    fn a_null_structured_content_falls_back_to_text() {
        let mut result = ok(vec![ContentBlock::text("hi")]);
        result.structured_content = Some(json!(null));
        assert_eq!(tool_output(result).unwrap(), ToolOutput::Text("hi".into()));
    }

    #[test]
    fn is_error_is_the_tools_error_with_its_text() {
        let result = CallToolResult::error(vec![ContentBlock::text("no such order")]);
        let err = tool_output(result).unwrap_err();
        assert_eq!(err.message(), "no such order");
        let silent = CallToolResult::error(vec![]);
        assert_eq!(
            tool_output(silent).unwrap_err().message(),
            UNEXPLAINED_ERROR
        );
    }

    #[test]
    fn images_become_a_note_and_empty_results_say_so() {
        let out = tool_output(ok(vec![
            ContentBlock::text("chart:"),
            ContentBlock::image("AAAA", "image/png"),
        ]))
        .unwrap();
        let ToolOutput::Text(text) = out else {
            panic!("text expected")
        };
        assert!(
            text.starts_with("chart:\n[image of type image/png, 4 base64"),
            "{text}"
        );
        assert_eq!(
            tool_output(ok(vec![])).unwrap(),
            ToolOutput::Text(NO_CONTENT.to_owned())
        );
    }

    #[test]
    fn a_long_result_is_cut_at_a_character_boundary() {
        let long = "é".repeat(MAX_RESULT_BYTES);
        let ToolOutput::Text(text) = tool_output(ok(vec![ContentBlock::text(long)])).unwrap()
        else {
            panic!("text expected")
        };
        assert!(text.len() < MAX_RESULT_BYTES + 100);
        assert!(text.ends_with("agent reads]"));
        // Huge structured content becomes (cut) text, not an oversize value.
        let mut result = ok(vec![]);
        result.structured_content = Some(json!({"blob": "x".repeat(MAX_RESULT_BYTES * 2)}));
        assert!(matches!(tool_output(result).unwrap(), ToolOutput::Text(_)));
    }

    #[test]
    fn descriptions_are_bounded() {
        assert_eq!(description("short"), "short");
        let long = "a".repeat(MAX_DESCRIPTION_CHARS + 10);
        let out = description(&long);
        assert_eq!(out.chars().count(), MAX_DESCRIPTION_CHARS + 1);
        assert!(out.ends_with('…'));
    }
}
