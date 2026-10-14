//! Builders for the JSON `data` of Messages API stream events, shaped like the examples in
//! `docs/providers/anthropic.md`.

use serde_json::{Value, json};

pub fn message_start(input_tokens: u64) -> String {
    json!({"type": "message_start", "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "content": [],
        "model": "claude-haiku-5-5", "stop_reason": null, "stop_sequence": null,
        "usage": {"input_tokens": input_tokens, "output_tokens": 1}}})
    .to_string()
}

pub fn message_start_with(usage: Value) -> String {
    json!({"type": "message_start", "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "content": [],
        "stop_reason": null, "stop_sequence": null, "usage": usage}})
    .to_string()
}

pub fn ping() -> String {
    json!({"type": "ping"}).to_string()
}

pub fn text_start(index: u32) -> String {
    json!({"type": "content_block_start", "index": index,
           "content_block": {"type": "text", "text": ""}})
    .to_string()
}

pub fn text_delta(index: u32, text: &str) -> String {
    json!({"type": "content_block_delta", "index": index,
           "delta": {"type": "text_delta", "text": text}})
    .to_string()
}

pub fn tool_start(index: u32, id: &str, name: &str) -> String {
    json!({"type": "content_block_start", "index": index,
           "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}})
    .to_string()
}

pub fn json_delta(index: u32, fragment: &str) -> String {
    json!({"type": "content_block_delta", "index": index,
           "delta": {"type": "input_json_delta", "partial_json": fragment}})
    .to_string()
}

pub fn thinking_start(index: u32) -> String {
    json!({"type": "content_block_start", "index": index,
           "content_block": {"type": "thinking", "thinking": "", "signature": ""}})
    .to_string()
}

pub fn thinking_delta(index: u32, text: &str) -> String {
    json!({"type": "content_block_delta", "index": index,
           "delta": {"type": "thinking_delta", "thinking": text}})
    .to_string()
}

pub fn signature_delta(index: u32, signature: &str) -> String {
    json!({"type": "content_block_delta", "index": index,
           "delta": {"type": "signature_delta", "signature": signature}})
    .to_string()
}

pub fn redacted_start(index: u32, data: &str) -> String {
    json!({"type": "content_block_start", "index": index,
           "content_block": {"type": "redacted_thinking", "data": data}})
    .to_string()
}

pub fn block_stop(index: u32) -> String {
    json!({"type": "content_block_stop", "index": index}).to_string()
}

pub fn message_delta(stop_reason: &str, output_tokens: u64) -> String {
    json!({"type": "message_delta",
           "delta": {"stop_reason": stop_reason, "stop_sequence": null},
           "usage": {"output_tokens": output_tokens}})
    .to_string()
}

pub fn message_delta_with(stop_reason: &str, usage: Value) -> String {
    json!({"type": "message_delta",
           "delta": {"stop_reason": stop_reason, "stop_sequence": null},
           "usage": usage})
    .to_string()
}

pub fn message_stop() -> String {
    json!({"type": "message_stop"}).to_string()
}

pub fn error_event(kind: &str) -> String {
    json!({"type": "error", "error": {"type": kind, "message": "details with sk-ant-SECRET-1234"}})
        .to_string()
}
