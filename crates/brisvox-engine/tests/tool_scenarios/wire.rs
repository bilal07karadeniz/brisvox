//! Loopback servers that speak the streaming wire format of each HTTP adapter, so the real
//! adapters (`OpenAiCompatibleLlm`, `AnthropicLlm`, `GeminiLlm`) assemble the scripted responses
//! the way they assemble a vendor's: tool-call fragments of parallel calls interleaved by `index`
//! (OpenAI-compatible), one content block per call (Anthropic), whole `functionCall` parts in one
//! chunk (Gemini).
//!
//! HTTP/1.1 over a `TcpListener`, chunked `text/event-stream`, the connection closed after each
//! response. One scripted response is served per `POST`, in order; the request bodies are kept.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One tool call of a scripted model response.
#[derive(Debug, Clone)]
pub struct ScriptedCall {
    /// The provider's call id.
    pub id: String,
    pub name: String,
    /// Complete JSON arguments.
    pub args: String,
}

/// One scripted model response: text deltas, then tool calls (provider-neutral).
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub text: Vec<String>,
    pub calls: Vec<ScriptedCall>,
}

impl Turn {
    pub fn say(text: &str) -> Self {
        Self {
            text: split_words(text),
            calls: Vec::new(),
        }
    }

    pub fn call(name: &str, args: &str) -> Self {
        Self::default().and_call(name, args)
    }

    pub fn calls(calls: &[(&str, &str)]) -> Self {
        calls.iter().fold(Self::default(), |turn, (name, args)| {
            turn.and_call(name, args)
        })
    }

    pub fn and_call(mut self, name: &str, args: &str) -> Self {
        let n = self.calls.len() + 1;
        self.calls.push(ScriptedCall {
            id: format!("call_{n}"),
            name: name.to_owned(),
            args: args.to_owned(),
        });
        self
    }

    pub fn and_say(mut self, text: &str) -> Self {
        self.text = split_words(text);
        self
    }
}

/// `"a b c"` as the deltas `"a "`, `"b "`, `"c"` (the way a model streams words).
fn split_words(text: &str) -> Vec<String> {
    text.split_inclusive(' ').map(str::to_owned).collect()
}

/// Which wire format a server speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    OpenAi,
    Anthropic,
    Gemini,
}

/// Cuts `text` into pieces of at most `n` chars (ASCII-safe: the arguments of the scenarios are).
fn fragments(text: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(n).map(|c| c.iter().collect()).collect()
}

fn data(value: &Value) -> String {
    format!("data: {value}\n\n")
}

fn event(kind: &str, value: &Value) -> String {
    format!("event: {kind}\ndata: {value}\n\n")
}

/// The SSE chunks of `turn`, one HTTP chunk each.
pub fn encode(dialect: Dialect, turn: &Turn) -> Vec<String> {
    match dialect {
        Dialect::OpenAi => openai(turn),
        Dialect::Anthropic => anthropic(turn),
        Dialect::Gemini => gemini(turn),
    }
}

fn openai(turn: &Turn) -> Vec<String> {
    let delta = |d: Value| {
        data(
            &json!({"id": "chatcmpl-1", "object": "chat.completion.chunk",
                     "model": "scripted", "choices": [{"index": 0, "delta": d}]}),
        )
    };
    let mut out = vec![delta(json!({"role": "assistant", "content": ""}))];
    for t in &turn.text {
        out.push(delta(json!({"content": t})));
    }
    if !turn.calls.is_empty() {
        let headers: Vec<Value> = turn
            .calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                json!({"index": i, "id": c.id, "type": "function",
                       "function": {"name": c.name, "arguments": ""}})
            })
            .collect();
        // The first call opens alone; the others open in one delta (both shapes occur).
        out.push(delta(json!({"tool_calls": [headers[0].clone()]})));
        if headers.len() > 1 {
            out.push(delta(json!({"tool_calls": headers[1..]})));
        }
        // Argument fragments of the calls interleave by `index`.
        let frags: Vec<Vec<String>> = turn.calls.iter().map(|c| fragments(&c.args, 7)).collect();
        let longest = frags.iter().map(Vec::len).max().unwrap_or(0);
        for k in 0..longest {
            for (i, f) in frags.iter().enumerate() {
                if let Some(fragment) = f.get(k) {
                    out.push(delta(json!({"tool_calls": [
                        {"index": i, "function": {"arguments": fragment}}]})));
                }
            }
        }
    }
    let finish = if turn.calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    out.push(data(
        &json!({"id": "chatcmpl-1", "object": "chat.completion.chunk",
        "model": "scripted", "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}),
    ));
    out.push(data(
        &json!({"id": "chatcmpl-1", "object": "chat.completion.chunk",
        "model": "scripted", "choices": [],
        "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}}),
    ));
    out.push("data: [DONE]\n\n".to_owned());
    out
}

fn anthropic(turn: &Turn) -> Vec<String> {
    let mut out = vec![event(
        "message_start",
        &json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "content": [],
            "model": "scripted", "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 100, "output_tokens": 1}}}),
    )];
    out.push(event("ping", &json!({"type": "ping"})));
    let mut index = 0u32;
    if !turn.text.is_empty() {
        out.push(event(
            "content_block_start",
            &json!({"type": "content_block_start", "index": index,
                    "content_block": {"type": "text", "text": ""}}),
        ));
        for t in &turn.text {
            out.push(event(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": index,
                        "delta": {"type": "text_delta", "text": t}}),
            ));
        }
        out.push(event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
        index += 1;
    }
    for call in &turn.calls {
        out.push(event(
            "content_block_start",
            &json!({"type": "content_block_start", "index": index,
                    "content_block": {"type": "tool_use", "id": call.id,
                                      "name": call.name, "input": {}}}),
        ));
        // Anthropic opens the argument stream with an empty fragment.
        let mut frags = vec![String::new()];
        frags.extend(fragments(&call.args, 7));
        for fragment in frags {
            out.push(event(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": fragment}}),
            ));
        }
        out.push(event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
        index += 1;
    }
    let stop = if turn.calls.is_empty() {
        "end_turn"
    } else {
        "tool_use"
    };
    out.push(event(
        "message_delta",
        &json!({"type": "message_delta",
                "delta": {"stop_reason": stop, "stop_sequence": null},
                "usage": {"output_tokens": 20}}),
    ));
    out.push(event("message_stop", &json!({"type": "message_stop"})));
    out
}

fn gemini(turn: &Turn) -> Vec<String> {
    let chunk = |parts: Value, finish: Option<&str>| {
        let mut candidate = json!({"content": {"role": "model", "parts": parts}, "index": 0});
        if let Some(finish) = finish {
            candidate["finishReason"] = json!(finish);
        }
        let mut chunk = json!({"candidates": [candidate], "modelVersion": "scripted",
                               "responseId": "r1"});
        if finish.is_some() {
            chunk["usageMetadata"] = json!({"promptTokenCount": 100, "candidatesTokenCount": 20,
                                            "totalTokenCount": 120});
        }
        data(&chunk)
    };
    let mut out = Vec::new();
    for t in &turn.text {
        out.push(chunk(json!([{"text": t}]), None));
    }
    if !turn.calls.is_empty() {
        // All calls of the response arrive whole, in one chunk; the first carries the thought
        // signature that the next request must echo (Gemini 3).
        let parts: Vec<Value> = turn
            .calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let args: Value = serde_json::from_str(&c.args).unwrap_or_else(|_| json!({}));
                let mut part = json!({"functionCall": {"name": c.name, "args": args}});
                if i == 0 {
                    part["thoughtSignature"] = json!(SIGNATURE);
                }
                part
            })
            .collect();
        out.push(chunk(json!(parts), None));
    }
    out.push(chunk(json!([]), Some("STOP")));
    out
}

/// The thought signature of the Gemini calls.
pub const SIGNATURE: &str = "c2lnbmF0dXJlLTE=";

// ---- the server --------------------------------------------------------------------------------

#[derive(Default)]
struct State {
    turns: Mutex<VecDeque<Vec<String>>>,
    requests: Mutex<Vec<Value>>,
    dialect: Option<Dialect>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The server. Dropping it stops accepting connections.
pub struct WireServer {
    pub addr: SocketAddr,
    state: Arc<State>,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for WireServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl WireServer {
    /// Starts a server of `dialect` that answers its next `POST`s with `turns`, in order. A
    /// request beyond them gets an empty answer.
    pub async fn start(dialect: Dialect, turns: &[Turn]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(State {
            turns: Mutex::new(turns.iter().map(|t| encode(dialect, t)).collect()),
            requests: Mutex::default(),
            dialect: Some(dialect),
        });
        let accept = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let state = Arc::clone(&state);
                    tokio::spawn(serve(socket, state));
                }
            }
        });
        Self {
            addr,
            state,
            accept,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The JSON bodies of the `POST`s received, in order.
    pub fn requests(&self) -> Vec<Value> {
        lock(&self.state.requests).clone()
    }
}

/// Reads one request; `(method, body)`, or `None` when the client closed first.
async fn read_request(socket: &mut TcpStream) -> Option<(String, String)> {
    let mut carry: Vec<u8> = Vec::new();
    let head_end = loop {
        if let Some(pos) = carry.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let mut buf = [0u8; 4096];
        let n = socket.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        carry.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&carry[..head_end]).into_owned();
    let method = head.split(' ').next()?.to_owned();
    let length: usize = head
        .split("\r\n")
        .filter_map(|l| l.split_once(':'))
        .find(|(n, _)| n.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    let start = head_end + 4;
    while carry.len() < start + length {
        let mut buf = [0u8; 4096];
        let n = socket.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        carry.extend_from_slice(&buf[..n]);
    }
    let body = String::from_utf8_lossy(&carry[start..start + length]).into_owned();
    Some((method, body))
}

async fn write_chunk(socket: &mut TcpStream, data: &[u8]) -> bool {
    let mut frame = format!("{:x}\r\n", data.len()).into_bytes();
    frame.extend_from_slice(data);
    frame.extend_from_slice(b"\r\n");
    socket.write_all(&frame).await.is_ok() && socket.flush().await.is_ok()
}

async fn serve(mut socket: TcpStream, state: Arc<State>) {
    let _ = socket.set_nodelay(true);
    let Some((method, body)) = read_request(&mut socket).await else {
        return;
    };
    if method != "POST" {
        // A warm-up probe.
        let head = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = socket.write_all(head.as_bytes()).await;
        return;
    }
    lock(&state.requests).push(serde_json::from_str(&body).unwrap_or(Value::Null));
    let chunks = lock(&state.turns)
        .pop_front()
        .unwrap_or_else(|| encode(state.dialect.unwrap_or(Dialect::OpenAi), &Turn::default()));
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for chunk in chunks {
        if !write_chunk(&mut socket, chunk.as_bytes()).await {
            return;
        }
    }
    let _ = socket.write_all(b"0\r\n\r\n").await;
    let _ = socket.shutdown().await;
}

// ---- reading a request body back --------------------------------------------------------------

/// The tool results the last request carried, in the order the provider's wire has them: for
/// OpenAI one `tool` message per result, for Anthropic the `tool_result` blocks of one user
/// message, for Gemini the `functionResponse` parts of one user content. Each as its JSON text.
pub fn tool_results_on_wire(dialect: Dialect, body: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match dialect {
        Dialect::OpenAi => {
            for m in body["messages"].as_array().into_iter().flatten() {
                if m["role"] == "tool" {
                    out.push(m["content"].as_str().unwrap_or_default().to_owned());
                }
            }
        }
        Dialect::Anthropic => {
            for m in body["messages"].as_array().into_iter().flatten() {
                for b in m["content"].as_array().into_iter().flatten() {
                    if b["type"] == "tool_result" {
                        out.push(b["content"].to_string());
                    }
                }
            }
        }
        Dialect::Gemini => {
            for c in body["contents"].as_array().into_iter().flatten() {
                for p in c["parts"].as_array().into_iter().flatten() {
                    if let Some(r) = p.get("functionResponse") {
                        out.push(r["response"].to_string());
                    }
                }
            }
        }
    }
    out
}

/// How the tool results of a request are grouped on the wire, in order: the number of results
/// that sit together. The providers need the results of one parallel response together: OpenAI
/// as consecutive `tool` messages, Anthropic as the `tool_result` blocks of one user message,
/// Gemini as the `functionResponse` parts of one content.
pub fn result_groups_on_wire(dialect: Dialect, body: &Value) -> Vec<usize> {
    let mut groups = Vec::new();
    match dialect {
        Dialect::OpenAi => {
            let mut run = 0;
            for m in body["messages"].as_array().into_iter().flatten() {
                if m["role"] == "tool" {
                    run += 1;
                } else if run > 0 {
                    groups.push(run);
                    run = 0;
                }
            }
            if run > 0 {
                groups.push(run);
            }
        }
        Dialect::Anthropic => {
            for m in body["messages"].as_array().into_iter().flatten() {
                let n = m["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == "tool_result")
                    .count();
                if n > 0 {
                    groups.push(n);
                }
            }
        }
        Dialect::Gemini => {
            for c in body["contents"].as_array().into_iter().flatten() {
                let n = c["parts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|p| p.get("functionResponse").is_some())
                    .count();
                if n > 0 {
                    groups.push(n);
                }
            }
        }
    }
    groups
}
