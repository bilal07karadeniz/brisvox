//! The wire dialects of the mock server: one per preset, each rendering a scripted contract
//! response the way (as far as `docs/providers/openai-compatible.md` knows) that vendor does.
//! Everything the docs mark UNVERIFIED is spread over the dialects on purpose, so the adapter is
//! tested against every shape it must accept.

use std::time::Duration;

use brisvox_providers::openai_compatible::Preset;
use brisvox_sim::contract::{Fault, Response};
use serde_json::{Value, json};

use crate::mock::{Script, Step};

/// How tool calls are streamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolShape {
    /// First delta with id, name and empty arguments, then argument fragments (OpenAI, Together,
    /// Fireworks).
    Fragmented,
    /// Id, name and the whole arguments in one delta (Ollama, vLLM `hf` parser).
    Whole,
}

/// Where the mock puts usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageWire {
    /// A trailing chunk with `choices: []` and a top-level `usage`.
    Trailing,
    /// A trailing chunk with `choices: []` and `x_groq.usage`.
    XGroqTrailing,
    /// A top-level `usage` on the chunk with `finish_reason`.
    OnFinish,
    /// A trailing chunk with `usage` whose choice repeats `finish_reason` with an empty delta
    /// (OpenRouter: "exactly once in the final chunk before the [DONE] message").
    TrailingWithFinish,
    /// Inside the choice of the finish chunk.
    InChoice,
    /// None at all.
    Never,
}

/// One vendor's way of speaking the protocol.
#[derive(Debug, Clone, Copy)]
pub struct Dialect {
    pub preset: Preset,
    pub tools: ToolShape,
    pub usage: UsageWire,
    /// `finish_reason` of a plain answer.
    pub stop_word: &'static str,
    /// `finish_reason` of an answer that called tools.
    pub tool_word: &'static str,
    /// Name of the reasoning field the mock sends before the answer (never spoken).
    pub reasoning_field: Option<&'static str>,
    /// Omit `id` from tool calls.
    pub missing_ids: bool,
    /// Send `"index": null` on tool-call deltas.
    pub null_index: bool,
    /// Send `[DONE]` after the last chunk.
    pub done_marker: bool,
}

impl Dialect {
    pub fn of(preset: Preset) -> Self {
        let base = Self {
            preset,
            tools: ToolShape::Fragmented,
            usage: UsageWire::Trailing,
            stop_word: "stop",
            tool_word: "tool_calls",
            reasoning_field: None,
            missing_ids: false,
            null_index: false,
            done_marker: true,
        };
        match preset {
            // Groq: `x_groq.usage` (placement UNVERIFIED), reasoning in `reasoning`.
            Preset::Groq => Self {
                usage: UsageWire::XGroqTrailing,
                reasoning_field: Some("reasoning"),
                ..base
            },
            // Together: no usage, the Together-only `eos`, `reasoning_content` or `reasoning`.
            Preset::Together => Self {
                usage: UsageWire::Never,
                stop_word: "eos",
                reasoning_field: Some("reasoning_content"),
                ..base
            },
            // Fireworks: usage on the finish chunk.
            Preset::Fireworks => Self {
                usage: UsageWire::OnFinish,
                reasoning_field: Some("reasoning_content"),
                ..base
            },
            // vLLM: trailing usage, and `stop` kept for a tool call of a named tool_choice.
            Preset::Vllm => Self {
                tool_word: "stop",
                ..base
            },
            // OpenRouter: usage in one last chunk that repeats the finish reason.
            Preset::OpenRouter => Self {
                usage: UsageWire::TrailingWithFinish,
                reasoning_field: Some("reasoning"),
                ..base
            },
            // Ollama: whole calls, trailing usage with `timings`, reasoning in `reasoning`.
            Preset::Ollama => Self {
                tools: ToolShape::Whole,
                reasoning_field: Some("reasoning"),
                ..base
            },
        }
    }

    pub fn all() -> Vec<Self> {
        Preset::ALL.iter().map(|p| Self::of(*p)).collect()
    }

    pub fn with(self, f: impl FnOnce(&mut Self)) -> Self {
        let mut copy = self;
        f(&mut copy);
        copy
    }
}

/// The usage the mock reports.
pub fn usage_json() -> Value {
    json!({"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17,
           "prompt_tokens_details": {"cached_tokens": 8}})
}

fn chunk(choices: Value) -> Value {
    json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "m",
           "choices": choices})
}

fn halves(text: &str) -> Vec<String> {
    let mid = text.len() / 2;
    if mid == 0 || !text.is_char_boundary(mid) {
        return vec![text.to_owned()];
    }
    let (a, b) = text.split_at(mid);
    vec![a.to_owned(), b.to_owned()]
}

/// The chunks of a response in `dialect`: the counted ones (text deltas, tool-call deltas, the
/// finish chunk) and what follows them (usage chunk, `[DONE]`). Leading uncounted chunks (role,
/// reasoning) come first.
pub struct Rendered {
    pub head: Vec<String>,
    pub counted: Vec<String>,
    pub tail: Vec<String>,
}

pub fn render(d: &Dialect, response: &Response) -> Rendered {
    let mut head = vec![
        chunk(
            json!([{"index": 0, "delta": {"role": "assistant", "content": ""},
                      "finish_reason": null}]),
        )
        .to_string(),
    ];
    if let Some(field) = d.reasoning_field {
        let mut delta = serde_json::Map::new();
        delta.insert(field.to_owned(), json!("thinking about the weather"));
        head.push(chunk(json!([{"index": 0, "delta": delta, "finish_reason": null}])).to_string());
    }
    let mut counted = Vec::new();
    for text in &response.text {
        counted.push(
            chunk(json!([{"index": 0, "delta": {"content": text}, "finish_reason": null}]))
                .to_string(),
        );
    }
    for (n, call) in response.tool_calls.iter().enumerate() {
        let index = if d.null_index { Value::Null } else { json!(n) };
        let mut open = json!({"index": index, "type": "function",
                              "function": {"name": call.name}});
        if !d.missing_ids {
            open["id"] = json!(call.id);
        }
        match d.tools {
            ToolShape::Whole => {
                open["function"]["arguments"] = json!(call.arguments);
                counted.push(
                    chunk(json!([{"index": 0, "delta": {"tool_calls": [open]},
                                  "finish_reason": null}]))
                    .to_string(),
                );
            }
            ToolShape::Fragmented => {
                open["function"]["arguments"] = json!("");
                counted.push(
                    chunk(json!([{"index": 0, "delta": {"tool_calls": [open]},
                                  "finish_reason": null}]))
                    .to_string(),
                );
                for fragment in halves(&call.arguments) {
                    counted.push(
                        chunk(json!([{"index": 0, "delta": {"tool_calls": [
                            {"index": index, "function": {"arguments": fragment}}]},
                            "finish_reason": null}]))
                        .to_string(),
                    );
                }
            }
        }
    }
    let word = if response.tool_calls.is_empty() {
        d.stop_word
    } else {
        d.tool_word
    };
    let mut finish = chunk(json!([{"index": 0, "delta": {}, "finish_reason": word}]));
    let mut tail = Vec::new();
    match d.usage {
        UsageWire::OnFinish => finish["usage"] = usage_json(),
        UsageWire::InChoice => finish["choices"][0]["usage"] = usage_json(),
        UsageWire::Trailing => {
            let mut usage = chunk(json!([]));
            usage["usage"] = usage_json();
            if d.preset == Preset::Ollama {
                usage["timings"] = json!({"prompt_ms": 3, "predicted_ms": 9});
            }
            tail.push(usage.to_string());
        }
        UsageWire::TrailingWithFinish => {
            let mut usage = chunk(json!([{"index": 0, "delta": {"content": ""},
                                          "finish_reason": word}]));
            usage["usage"] = usage_json();
            tail.push(usage.to_string());
        }
        UsageWire::XGroqTrailing => {
            let mut usage = chunk(json!([]));
            usage["x_groq"] = json!({"id": "req_1", "usage": usage_json()});
            tail.push(usage.to_string());
        }
        UsageWire::Never => {}
    }
    counted.push(finish.to_string());
    if d.done_marker {
        tail.push("[DONE]".to_owned());
    }
    Rendered {
        head,
        counted,
        tail,
    }
}

/// Time between two counted chunks of a text response: slow enough that a 200-delta response is
/// still streaming when the cancel check hangs up.
pub const PACE: Duration = Duration::from_millis(15);

/// The wire script for a contract response and its faults, in `dialect`.
pub fn script_for(d: &Dialect, response: &Response) -> Script {
    let mut reject = None;
    let mut pre_delay = Duration::ZERO;
    for fault in &response.faults {
        match fault {
            Fault::Reject { status } => reject = Some(*status),
            Fault::ConnectStall { duration } => pre_delay = *duration,
            _ => {}
        }
    }
    if let Some(status) = reject {
        let body = json!({"error": {"message": "Incorrect API key provided: sk-abc***",
            "type": "invalid_request_error", "code": "invalid_api_key"}})
        .to_string();
        let mut script = Script::status(status, &body);
        if status == 429 {
            script = script.header("Retry-After", "1");
        }
        script.pre_delay = pre_delay;
        return script;
    }
    let rendered = render(d, response);
    let mut steps: Vec<Step> = rendered.head.into_iter().map(Step::event).collect();
    let text_events = response.text.len();
    for (i, event) in rendered.counted.iter().enumerate() {
        for fault in &response.faults {
            match fault {
                Fault::Stall {
                    before_event,
                    duration,
                } if *before_event == i => steps.push(Step::Sleep(*duration)),
                Fault::Disconnect { after_events } if *after_events == i => steps.push(Step::Close),
                Fault::Protocol { after_events } if *after_events == i => {
                    steps.push(Step::raw(b"data: {this is not json\n\n".to_vec()));
                }
                _ => {}
            }
        }
        steps.push(Step::counted(event.clone()));
        if i < text_events {
            steps.push(Step::Sleep(PACE));
        }
    }
    steps.extend(rendered.tail.into_iter().map(Step::event));
    let mut script = Script::stream(steps);
    script.pre_delay = pre_delay;
    script
}
