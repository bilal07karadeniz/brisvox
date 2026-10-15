//! Fake OpenAI-compatible Chat Completions (`POST /v1/chat/completions`).
//!
//! `stream: true` answers with Server-Sent Events in the shape of OpenAI's `chat.completion.chunk`
//! objects: a role chunk and the first token `llm_first_token` after the request, the following
//! tokens `llm_inter_token` apart (a word or a punctuation mark per token), a chunk with
//! `finish_reason`, a usage chunk when `stream_options.include_usage` is set, and `[DONE]`.
//! `stream: false` answers with one `chat.completion` object after the same total delay.
//! The reply is chosen like the in-process mocks do: the last user message is matched against the
//! fixture's utterances (exact, then prefix) and answered in the language of the match; a message
//! it cannot match gets the English reply. Tool calls are not produced (the load test has no
//! tools); requests that carry `tools` are answered with text.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep_until, timeout};

use crate::config::{Config, TokenSplit};
use crate::script;
use crate::server::{Body, Shared, full};

/// Largest request body read (a conversation of a long call stays far below this).
const MAX_BODY: usize = 4 * 1024 * 1024;
/// Time to receive the request body.
const BODY_TIMEOUT: Duration = Duration::from_secs(15);

/// A timed Server-Sent Event.
type Event = (Duration, Bytes);

/// The text of the last user message (`content` is a string or a list of parts).
pub fn last_user_text(request: &Value) -> String {
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        return match message.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
    }
    String::new()
}

/// The reply to `user_text`.
pub fn reply_for<'a>(cfg: &'a Config, script: &[script::Utterance], user_text: &str) -> &'a str {
    match script::find(script, user_text) {
        Some(u) if u.lang == "tr" => &cfg.reply_tr,
        _ => &cfg.reply_en,
    }
}

fn chunk(
    id: &str,
    model: &str,
    created: u64,
    seq: u64,
    choices: &Value,
    usage: Option<&Value>,
) -> Bytes {
    // OpenAI pads every chunk without usage with a random string against side channels; the
    // size matters. The usage chunk has `usage` instead.
    let tail = match usage {
        Some(usage) => json!({"usage": usage}),
        None => json!({"obfuscation": format!("{:016x}", seq.wrapping_mul(0x9E37_79B9_7F4A_7C15))}),
    };
    let v = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "service_tier": "default",
        "system_fingerprint": "fp_fake000000",
        "choices": choices,
        "extra": tail,
    });
    // Flatten `extra` into the object (a member of its own would not be OpenAI's shape).
    let mut v = v;
    if let Some(object) = v.as_object_mut()
        && let Some(Value::Object(extra)) = object.remove("extra")
    {
        object.extend(extra);
    }
    Bytes::from(format!(
        "data: {v}

"
    ))
}

/// The timed events of a streamed reply.
pub fn stream_events(
    cfg: &Config,
    id: &str,
    model: &str,
    created: u64,
    reply: &str,
    include_usage: bool,
) -> Vec<Event> {
    let tokens: Vec<String> = match cfg.llm_tokens {
        TokenSplit::Words => script::word_tokens(reply),
        TokenSplit::Single => vec![reply.to_owned()],
    };
    let mut events: Vec<Event> = Vec::with_capacity(tokens.len() + 4);
    let mut at = cfg.llm_first_token;
    let mut seq = 0_u64;
    let mut next_seq = || {
        seq += 1;
        seq
    };
    events.push((
        at,
        chunk(id, model, created, next_seq(), &json!([{"index": 0, "delta": {"role": "assistant", "content": "", "refusal": null}, "logprobs": null, "finish_reason": null}]), None),
    ));
    for token in &tokens {
        events.push((
            at,
            chunk(id, model, created, next_seq(), &json!([{"index": 0, "delta": {"content": token}, "logprobs": null, "finish_reason": null}]), None),
        ));
        at += cfg.llm_inter_token;
    }
    events.push((
        at,
        chunk(
            id,
            model,
            created,
            next_seq(),
            &json!([{"index": 0, "delta": {}, "logprobs": null, "finish_reason": "stop"}]),
            None,
        ),
    ));
    if include_usage {
        events.push((
            at,
            chunk(
                id,
                model,
                created,
                next_seq(),
                &json!([]),
                Some(&usage(tokens.len())),
            ),
        ));
    }
    events.push((at, Bytes::from_static(b"data: [DONE]\n\n")));
    events
}

fn usage(completion_tokens: usize) -> Value {
    let prompt = 180_usize;
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt + completion_tokens,
        "prompt_tokens_details": {"cached_tokens": 0, "audio_tokens": 0},
        "completion_tokens_details": {
            "reasoning_tokens": 0, "audio_tokens": 0,
            "accepted_prediction_tokens": 0, "rejected_prediction_tokens": 0
        },
    })
}

/// The body that plays `events` in real time, keeping `guard` (the open-request count) alive
/// until the client has read everything or went away.
fn timed_body(events: Vec<Event>, guard: crate::metrics::GaugeGuard) -> Body {
    let start = Instant::now();
    let items = stream::unfold(
        (events.into_iter(), guard),
        move |(mut it, guard)| async move {
            let (offset, bytes) = it.next()?;
            sleep_until(start + offset).await;
            Some((Ok::<_, Infallible>(Frame::data(bytes)), (it, guard)))
        },
    );
    StreamBody::new(items).boxed_unsync()
}

fn json_response(status: StatusCode, value: &Value) -> Response<Body> {
    let mut response = Response::new(full(Bytes::from(value.to_string())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

fn error(status: StatusCode, message: &str) -> Response<Body> {
    json_response(
        status,
        &json!({"error": {"message": message, "type": "invalid_request_error", "param": null, "code": null}}),
    )
}

/// Answers a request to `/v1/chat/completions`.
pub async fn chat(request: Request<Incoming>, shared: Arc<Shared>) -> Response<Body> {
    if request.method() == Method::HEAD {
        // `warm()` of the brisvox adapter: a HEAD to learn the route and open the connection.
        return Response::new(
            Full::new(Bytes::new())
                .map_err(|never| match never {})
                .boxed_unsync(),
        );
    }
    if request.method() != Method::POST {
        return error(StatusCode::METHOD_NOT_ALLOWED, "POST only");
    }
    let open = shared.metrics.llm_requests.enter();
    let body = match timeout(
        BODY_TIMEOUT,
        Limited::new(request.into_body(), MAX_BODY).collect(),
    )
    .await
    {
        Ok(Ok(collected)) => collected.to_bytes(),
        _ => return error(StatusCode::BAD_REQUEST, "unreadable request body"),
    };
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        return error(StatusCode::BAD_REQUEST, "request body is not JSON");
    };
    let cfg = &shared.config;
    let user = last_user_text(&parsed);
    let reply = reply_for(cfg, &shared.script, &user).to_owned();
    let model = parsed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("fake-model")
        .to_owned();
    let id = format!("chatcmpl-{}", shared.next_request_id().replace('-', ""));
    let created = shared.unix_seconds();
    let stream_requested = parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !stream_requested {
        let tokens = script::word_tokens(&reply).len();
        let total = cfg.llm_first_token + cfg.llm_inter_token * u32::try_from(tokens).unwrap_or(0);
        tokio::time::sleep(total).await;
        return json_response(
            StatusCode::OK,
            &json!({
                "id": id, "object": "chat.completion", "created": created, "model": model,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": reply, "refusal": null},
                              "logprobs": null, "finish_reason": "stop"}],
                "usage": usage(tokens),
                "service_tier": "default", "system_fingerprint": "fp_fake000000",
            }),
        );
    }
    let include_usage = parsed
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let events = stream_events(cfg, &id, &model, created, &reply, include_usage);
    let mut response = Response::new(timed_body(events, open));
    let headers = response.headers_mut();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    headers.insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-cache"),
    );
    response
}

/// `GET /v1/models`: a list with one model.
pub fn models() -> Response<Body> {
    json_response(
        StatusCode::OK,
        &json!({"object": "list", "data": [{"id": "fake-model", "object": "model", "created": 0, "owned_by": "fake"}]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script() -> Vec<script::Utterance> {
        script::parse(
            r#"{"segments":[{"start":0,"end":16000,"lang":"en","text":"Hello there friend."},
                {"start":0,"end":16000,"lang":"tr","text":"Merhaba dostum nasilsin"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn the_last_user_message_is_found_in_both_content_shapes() {
        let a = json!({"messages":[{"role":"system","content":"s"},{"role":"user","content":"one"},{"role":"assistant","content":"x"},{"role":"user","content":"two"}]});
        assert_eq!(last_user_text(&a), "two");
        let b = json!({"messages":[{"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}]});
        assert_eq!(last_user_text(&b), "a b");
        assert_eq!(last_user_text(&json!({})), "");
    }

    #[test]
    fn the_reply_follows_the_matched_utterance() {
        let cfg = Config::default();
        let s = script();
        assert_eq!(reply_for(&cfg, &s, "hello there friend"), "Okay, got it.");
        assert_eq!(reply_for(&cfg, &s, "Merhaba dostum nasilsin"), cfg.reply_tr);
        assert_eq!(reply_for(&cfg, &s, "unknown"), "Okay, got it.");
    }

    #[test]
    fn the_stream_has_the_openai_shape_and_pacing() {
        let cfg = Config::default();
        let events = stream_events(&cfg, "chatcmpl-1", "m", 1, "Okay, got it.", true);
        // role, 5 tokens, finish, usage, DONE
        assert_eq!(events.len(), 9);
        assert_eq!(events[0].0, Duration::from_millis(350));
        assert_eq!(events[1].0, Duration::from_millis(350));
        assert_eq!(events[2].0, Duration::from_millis(380));
        let text: String = events
            .iter()
            .filter_map(|(_, b)| std::str::from_utf8(b).ok())
            .filter_map(|s| s.strip_prefix("data: "))
            .filter_map(|s| serde_json::from_str::<Value>(s.trim()).ok())
            .filter_map(|v| {
                v.pointer("/choices/0/delta/content")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(text, "Okay, got it.");
        let last = std::str::from_utf8(&events[8].1).unwrap();
        assert_eq!(last, "data: [DONE]\n\n");
        let usage_chunk = std::str::from_utf8(&events[7].1).unwrap();
        assert!(usage_chunk.contains("\"usage\""));
        assert!(usage_chunk.contains("\"choices\":[]"));
        // Without include_usage there is no usage chunk.
        assert_eq!(
            stream_events(&cfg, "i", "m", 1, "Okay, got it.", false).len(),
            8
        );
        // One token mode.
        let single = Config {
            llm_tokens: TokenSplit::Single,
            ..Config::default()
        };
        assert_eq!(
            stream_events(&single, "i", "m", 1, "Okay, got it.", false).len(),
            4
        );
    }
}
