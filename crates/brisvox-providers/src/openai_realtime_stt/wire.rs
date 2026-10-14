//! The messages of the OpenAI Realtime transcription protocol and its dialects
//! (`docs/providers/openai-realtime-stt.md`): what is sent to open a session, how audio and a
//! commit are encoded, and how the server's events map to transcripts and to the answer of a
//! `Finalize`.

use std::borrow::Cow;
use std::io::Write as _;
use std::time::Duration;

use base64::display::Base64Display;
use base64::engine::general_purpose::STANDARD;
use brisvox_core::frame::SampleRate;
use bytes::{BufMut, Bytes, BytesMut};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};

use super::PROVIDER;
use super::config::Dialect;
use crate::error::{Error, Result};
use crate::wsstt::{Actions, Closed, Trim, Wire, default_closed};

/// The client event that turns the buffered audio into a transcript.
pub(super) const COMMIT: &str = r#"{"type":"input_audio_buffer.commit"}"#;

/// vLLM: ends the audio of the generation that is running.
pub(super) const COMMIT_FINAL: &str = r#"{"type":"input_audio_buffer.commit","final":true}"#;

/// Sent audio kept for replay after the server ended a turn by itself.
const SERVER_TURN_TAIL: Duration = Duration::from_secs(1);

/// Most timed-out commits whose late answers are still recognised and dropped.
const MAX_EXPIRED: usize = 8;

fn protocol(detail: impl Into<String>) -> Error {
    Error::Protocol {
        provider: PROVIDER,
        detail: detail.into(),
    }
}

/// What the wire needs to know about one session.
#[derive(Debug, Clone)]
pub(super) struct SessionParams {
    pub(super) dialect: Dialect,
    pub(super) model: Option<String>,
    pub(super) language: Option<String>,
    pub(super) prompt: Option<String>,
    pub(super) punctuation: bool,
    pub(super) replaces: bool,
    pub(super) min_commit: Duration,
    pub(super) rate: SampleRate,
}

// ---------------------------------------------------------------------------------------------
// Client events
// ---------------------------------------------------------------------------------------------

/// The session message(s) sent right after the connect. OpenAI and NVIDIA NIM configure the
/// session explicitly (transcription mode, PCM format, no server VAD for OpenAI); Speaches is
/// configured by its URL; vLLM gets its model and the start of the first generation.
pub(super) fn session_messages(p: &SessionParams) -> Vec<String> {
    match p.dialect {
        Dialect::OpenAi => {
            let mut transcription = serde_json::Map::new();
            if let Some(model) = &p.model {
                transcription.insert("model".into(), json!(model));
            }
            if let Some(language) = &p.language {
                // `gpt-live-transcribe` takes `languages` and refuses the singular field.
                let live = p
                    .model
                    .as_deref()
                    .is_some_and(|m| m.starts_with("gpt-live"));
                if live {
                    transcription.insert("languages".into(), json!([language]));
                } else {
                    transcription.insert("language".into(), json!(language));
                }
            }
            if let Some(prompt) = &p.prompt {
                transcription.insert("prompt".into(), json!(prompt));
            }
            vec![
                json!({
                    "type": "session.update",
                    "session": {
                        "type": "transcription",
                        "audio": {"input": {
                            "format": {"type": "audio/pcm", "rate": p.rate.hz()},
                            "transcription": Value::Object(transcription),
                            "turn_detection": Value::Null,
                        }},
                    },
                })
                .to_string(),
            ]
        }
        Dialect::Nim => {
            let mut transcription = serde_json::Map::new();
            if let Some(language) = &p.language {
                transcription.insert("language".into(), json!(language));
            }
            if let Some(model) = &p.model {
                transcription.insert("model".into(), json!(model));
            }
            if let Some(prompt) = &p.prompt {
                transcription.insert("prompt".into(), json!(prompt));
            }
            vec![
                json!({
                    "type": "transcription_session.update",
                    "session": {
                        "modalities": ["text"],
                        "input_audio_format": "pcm16",
                        "input_audio_transcription": Value::Object(transcription),
                        "input_audio_params": {"sample_rate_hz": p.rate.hz(), "num_channels": 1},
                        "recognition_config": {"enable_automatic_punctuation": p.punctuation},
                    },
                })
                .to_string(),
            ]
        }
        Dialect::Speaches => Vec::new(),
        Dialect::Vllm => {
            let model = p.model.as_deref().unwrap_or_default();
            vec![
                json!({"type": "session.update", "model": model}).to_string(),
                COMMIT.to_owned(),
            ]
        }
    }
}

/// Encodes `pcm` (PCM16 LE) as an `input_audio_buffer.append` event. The JSON is built in `out`
/// and handed out as a frozen slice of it: the slice goes to the socket without a copy and the
/// buffer's allocation is reclaimed once the socket dropped it, so the steady state does not
/// allocate per frame.
///
/// # Errors
/// [`Error::Protocol`] when the buffer could not be written (cannot happen for a `BytesMut`).
pub(super) fn append_audio(pcm: &[u8], out: &mut BytesMut) -> Result<Bytes> {
    const HEAD: &[u8] = br#"{"type":"input_audio_buffer.append","audio":""#;
    const TAIL: &[u8] = br#""}"#;
    let encoded = base64::encoded_len(pcm.len(), true).unwrap_or(0);
    out.reserve(HEAD.len() + encoded + TAIL.len());
    let written = {
        let mut writer = out.writer();
        writer
            .write_all(HEAD)
            .and_then(|()| write!(writer, "{}", Base64Display::new(pcm, &STANDARD)))
            .and_then(|()| writer.write_all(TAIL))
    };
    written.map_err(|_| protocol("an audio frame could not be encoded"))?;
    Ok(out.split().freeze())
}

// ---------------------------------------------------------------------------------------------
// Server events
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct Incoming<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Cow<'a, str>,
    #[serde(borrow, default)]
    item_id: Cow<'a, str>,
    #[serde(borrow, default)]
    delta: Cow<'a, str>,
    #[serde(borrow, default)]
    transcript: Cow<'a, str>,
    #[serde(borrow, default)]
    text: Cow<'a, str>,
    #[serde(default)]
    error: Option<Value>,
    /// vLLM puts the error code beside the message.
    #[serde(borrow, default)]
    code: Cow<'a, str>,
}

/// The code and the message of an `error` event: an object `{type, code, message}` (OpenAI,
/// NIM, Speaches) or a string with a sibling `code` (vLLM).
fn error_parts(incoming: &Incoming<'_>) -> (String, String) {
    match &incoming.error {
        Some(Value::Object(object)) => {
            let get = |key: &str| {
                object
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let code = get("code");
            let code = if code.is_empty() { get("type") } else { code };
            (code, get("message"))
        }
        Some(Value::String(message)) => (incoming.code.to_string(), message.clone()),
        _ => (incoming.code.to_string(), String::new()),
    }
}

/// An error code no retry or commit can fix.
fn fatal_status(code: &str) -> Option<u16> {
    match code {
        "invalid_api_key" | "authentication_error" | "unauthorized" => Some(401),
        "model_not_found" | "model_not_available" => Some(404),
        "insufficient_quota" | "rate_limit_exceeded" => Some(429),
        _ => None,
    }
}

/// A commit waiting for its transcript.
#[derive(Debug, Default)]
struct Awaiting {
    /// The item id from `input_audio_buffer.committed`, once seen.
    item: Option<String>,
}

/// Per-session protocol state.
#[derive(Debug)]
pub(super) struct RealtimeWire {
    p: SessionParams,
    /// Encoding buffer for audio frames (see [`append_audio`]).
    scratch: BytesMut,
    /// Wire audio appended since the last commit (or generation start).
    appended: usize,
    /// The least a commit may hold, in wire bytes.
    min_commit: usize,
    /// The hypothesis so far (accumulated deltas) of the item in progress.
    interim: String,
    /// The item the deltas belong to.
    item: String,
    /// The last interim sent to the caller.
    emitted: String,
    /// The commit/finalize in flight.
    awaiting: Option<Awaiting>,
    /// Item ids of timed-out commits: their late transcripts are dropped.
    expired_items: Vec<String>,
    /// Timed-out commits whose item id was never seen: the next transcripts are dropped.
    expired_unknown: usize,
}

impl RealtimeWire {
    pub(super) fn new(p: SessionParams) -> Self {
        let min_commit = usize::try_from(p.rate.hz())
            .unwrap_or(0)
            .saturating_mul(2)
            .saturating_mul(usize::try_from(p.min_commit.as_millis()).unwrap_or(0))
            / 1000;
        Self {
            p,
            scratch: BytesMut::new(),
            appended: 0,
            min_commit,
            interim: String::new(),
            item: String::new(),
            emitted: String::new(),
            awaiting: None,
            expired_items: Vec::new(),
            expired_unknown: 0,
        }
    }

    fn vllm(&self) -> bool {
        self.p.dialect == Dialect::Vllm
    }

    fn reset_hypothesis(&mut self) {
        self.interim.clear();
        self.item.clear();
        self.emitted.clear();
    }

    /// A delta of the item `item` (empty: unknown).
    fn on_delta(&mut self, item: &str, delta: &str, act: &mut Actions) {
        act.healthy = true;
        if !self.p.dialect.streams_interims() {
            return;
        }
        if !item.is_empty() && self.item != item {
            // A new item: the hypothesis of the previous one is over.
            self.interim.clear();
            self.emitted.clear();
            self.item.clear();
            self.item.push_str(item);
        }
        if self.p.replaces {
            self.interim.clear();
        }
        self.interim.push_str(delta);
        let shown = self.interim.trim();
        if !shown.is_empty() && shown != self.emitted {
            act.interim(shown);
            self.emitted.clear();
            self.emitted.push_str(shown);
        }
    }

    /// Whether a transcript of `item` is the late answer of a commit that timed out (and so is
    /// dropped).
    fn is_late(&mut self, item: &str) -> bool {
        if let Some(at) = self.expired_items.iter().position(|i| i == item) {
            self.expired_items.swap_remove(at);
            return true;
        }
        if self.expired_unknown > 0 && self.awaiting.is_none() {
            self.expired_unknown -= 1;
            return true;
        }
        false
    }

    /// A finished transcript (`completed` / `done`) of `item`.
    fn on_completed(&mut self, item: &str, transcript: &str, act: &mut Actions) {
        act.healthy = true;
        if self.is_late(item) {
            tracing::debug!(
                provider = PROVIDER,
                "a late transcript of an expired commit was dropped"
            );
            return;
        }
        let text = transcript.trim();
        let answers = self.awaiting.as_ref().is_some_and(|a| {
            item.is_empty() || a.item.as_deref().is_none_or(|committed| committed == item)
        });
        self.reset_hypothesis();
        if answers {
            self.awaiting = None;
            self.appended = 0;
            act.answer(text);
            act.trim = Trim::ToMark;
        } else {
            // The server ended a turn by itself (its VAD or endpointing): what it consumed is
            // settled, so a reconnect replays only a short overlap.
            if !text.is_empty() {
                act.endpoint_final(text);
            }
            act.trim = Trim::KeepLast(SERVER_TURN_TAIL);
        }
    }

    /// vLLM: restart the generation that the last `done` ended.
    fn restart_generation(&mut self, act: &mut Actions) {
        self.appended = 0;
        act.send.push(Message::text(COMMIT));
    }
}

impl Wire for RealtimeWire {
    fn opened(&mut self, act: &mut Actions) {
        // Whatever the old connection owed is gone: a pending commit is answered by the core's
        // re-asking after the replay, a late transcript cannot come from a new connection.
        self.appended = 0;
        self.reset_hypothesis();
        self.awaiting = None;
        self.expired_items.clear();
        self.expired_unknown = 0;
        for message in session_messages(&self.p) {
            act.send.push(Message::text(message));
        }
    }

    fn audio(&mut self, pcm: &[u8]) -> Result<Message> {
        let bytes = append_audio(pcm, &mut self.scratch)?;
        self.appended = self.appended.saturating_add(pcm.len());
        let text = Utf8Bytes::try_from(bytes)
            .map_err(|_| protocol("an audio frame is not valid UTF-8"))?;
        Ok(Message::Text(text))
    }

    fn finalize(&mut self, act: &mut Actions) {
        if self.awaiting.is_some() {
            return;
        }
        if self.appended == 0 || self.appended < self.min_commit {
            // Nothing (or too little for the server to accept) is buffered: nothing to flush.
            // Too little audio stays in the server's buffer and leads the next commit.
            act.answer("");
            act.trim = Trim::ToMark;
            return;
        }
        self.awaiting = Some(Awaiting::default());
        if self.vllm() {
            // The generation ends with the final commit; audio that arrives meanwhile is held
            // for the next one (the server clears its queue when this one is done).
            act.gate = Some(false);
            act.send.push(Message::text(COMMIT_FINAL));
        } else {
            self.appended = 0;
            act.send.push(Message::text(COMMIT));
        }
    }

    fn text(&mut self, text: &str, act: &mut Actions) -> Result<()> {
        let incoming: Incoming<'_> =
            serde_json::from_str(text).map_err(|_| protocol("a server event is not valid JSON"))?;
        match incoming.kind.as_ref() {
            "conversation.item.input_audio_transcription.delta" | "transcription.delta" => {
                self.on_delta(&incoming.item_id, &incoming.delta, act);
            }
            "conversation.item.input_audio_transcription.completed" => {
                self.on_completed(&incoming.item_id, &incoming.transcript, act);
            }
            "transcription.done" => {
                let had_request = self.awaiting.is_some();
                self.on_completed("", &incoming.text, act);
                if had_request {
                    act.gate = Some(true);
                }
                self.restart_generation(act);
            }
            "input_audio_buffer.committed" => {
                if let Some(awaiting) = self.awaiting.as_mut()
                    && awaiting.item.is_none()
                    && !incoming.item_id.is_empty()
                {
                    awaiting.item = Some(incoming.item_id.to_string());
                }
            }
            "conversation.item.input_audio_transcription.failed" => {
                tracing::warn!(
                    provider = PROVIDER,
                    "the server could not transcribe a commit"
                );
                if self.awaiting.take().is_some() {
                    self.reset_hypothesis();
                    act.answer("");
                    act.trim = Trim::ToMark;
                }
            }
            "error" => {
                let (code, message) = error_parts(&incoming);
                if let Some(status) = fatal_status(&code) {
                    return Err(Error::Rejected {
                        provider: PROVIDER,
                        status,
                    });
                }
                tracing::warn!(provider = PROVIDER, code = %code, "the server reported an error");
                tracing::debug!(provider = PROVIDER, message = %message, "server error message");
                if self.awaiting.take().is_some() {
                    // E.g. a commit of an empty or too short buffer: there is nothing to wait
                    // for; the turn ends empty and the session carries on.
                    self.reset_hypothesis();
                    act.answer("");
                    act.trim = Trim::ToMark;
                    if self.vllm() {
                        act.gate = Some(true);
                        self.restart_generation(act);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn closed(&mut self, code: Option<CloseCode>, _act: &mut Actions) -> Closed {
        default_closed(PROVIDER, code)
    }

    fn finalize_timed_out(&mut self, act: &mut Actions) {
        let Some(awaiting) = self.awaiting.take() else {
            act.answer("");
            return;
        };
        match awaiting.item {
            Some(item) => {
                if self.expired_items.len() < MAX_EXPIRED {
                    self.expired_items.push(item);
                }
            }
            None => self.expired_unknown = (self.expired_unknown + 1).min(MAX_EXPIRED),
        }
        let last = std::mem::take(&mut self.interim);
        self.reset_hypothesis();
        act.answer(last.trim());
        if self.vllm() {
            // The generation may still be running on the server: start over on a new
            // connection rather than guess its state.
            act.gate = Some(true);
            act.rotate = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SttEvent;

    fn params(dialect: Dialect) -> SessionParams {
        SessionParams {
            dialect,
            model: dialect.default_model().map(str::to_owned),
            language: None,
            prompt: None,
            punctuation: true,
            replaces: false,
            min_commit: dialect.min_commit(),
            rate: dialect.default_rate(),
        }
    }

    fn wire(dialect: Dialect) -> (RealtimeWire, Actions) {
        let mut p = params(dialect);
        if p.model.is_none() {
            p.model = Some("m".to_owned());
        }
        let mut w = RealtimeWire::new(p);
        let mut act = Actions::default();
        w.opened(&mut act);
        act.reset();
        (w, act)
    }

    fn feed_audio(w: &mut RealtimeWire, ms: usize) {
        let bytes = w.p.rate.hz() as usize * 2 * ms / 1000;
        assert!(w.audio(&vec![0_u8; bytes]).is_ok());
    }

    fn texts(act: &Actions) -> Vec<(String, bool, bool, bool)> {
        act.events
            .iter()
            .filter_map(|e| match e {
                SttEvent::Transcript {
                    text,
                    is_final,
                    speech_final,
                    from_finalize,
                } => Some((text.to_string(), *is_final, *speech_final, *from_finalize)),
                SttEvent::Error(_) => None,
            })
            .collect()
    }

    fn sent_text(act: &Actions, at: usize) -> String {
        match act.send.get(at) {
            Some(Message::Text(t)) => t.as_str().to_owned(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn the_openai_session_is_a_transcription_session_without_server_vad() {
        let mut p = params(Dialect::OpenAi);
        p.language = Some("tr".to_owned());
        let messages = session_messages(&p);
        assert_eq!(messages.len(), 1);
        let v: Value = serde_json::from_str(&messages[0]).unwrap_or_default();
        assert_eq!(v["type"], "session.update");
        assert_eq!(v["session"]["type"], "transcription");
        let input = &v["session"]["audio"]["input"];
        assert_eq!(input["format"], json!({"type": "audio/pcm", "rate": 24000}));
        assert_eq!(input["transcription"]["model"], "gpt-live-transcribe");
        assert_eq!(input["transcription"]["languages"], json!(["tr"]));
        assert!(input["transcription"].get("language").is_none());
        assert!(input["turn_detection"].is_null());
        // Other models take the singular field.
        p.model = Some("gpt-transcribe".to_owned());
        let v: Value = serde_json::from_str(&session_messages(&p)[0]).unwrap_or_default();
        let t = &v["session"]["audio"]["input"]["transcription"];
        assert_eq!(t["language"], "tr");
        assert!(t.get("languages").is_none());
    }

    #[test]
    fn the_nim_session_sets_pcm16_rate_language_and_punctuation() {
        let mut p = params(Dialect::Nim);
        p.language = Some("tr-TR".to_owned());
        let v: Value = serde_json::from_str(&session_messages(&p)[0]).unwrap_or_default();
        assert_eq!(v["type"], "transcription_session.update");
        let s = &v["session"];
        assert_eq!(s["modalities"], json!(["text"]));
        assert_eq!(s["input_audio_format"], "pcm16");
        assert_eq!(
            s["input_audio_params"],
            json!({"sample_rate_hz": 16000, "num_channels": 1})
        );
        assert_eq!(s["input_audio_transcription"], json!({"language": "tr-TR"}));
        assert_eq!(
            s["recognition_config"]["enable_automatic_punctuation"],
            true
        );
    }

    #[test]
    fn speaches_needs_no_session_message_and_vllm_names_its_model_then_starts() {
        assert!(session_messages(&params(Dialect::Speaches)).is_empty());
        let mut p = params(Dialect::Vllm);
        p.model = Some("mistralai/Voxtral-Mini-4B-Realtime-2602".to_owned());
        let messages = session_messages(&p);
        assert_eq!(
            serde_json::from_str::<Value>(&messages[0]).unwrap_or_default(),
            json!({"type": "session.update", "model": "mistralai/Voxtral-Mini-4B-Realtime-2602"})
        );
        assert_eq!(messages[1], COMMIT);
    }

    #[test]
    fn audio_is_base64_pcm_in_an_append_event() {
        let mut out = BytesMut::new();
        let bytes = append_audio(&[1, 2, 3, 4], &mut out).unwrap_or_default();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or_default();
        assert_eq!(v["type"], "input_audio_buffer.append");
        assert_eq!(v["audio"], "AQIDBA==");
        // The buffer is reused: a second frame starts clean.
        let bytes = append_audio(&[], &mut out).unwrap_or_default();
        assert_eq!(
            &bytes[..],
            br#"{"type":"input_audio_buffer.append","audio":""}"#
        );
    }

    #[test]
    fn deltas_are_interims_and_completed_answers_a_commit() {
        let (mut w, mut act) = wire(Dialect::OpenAi);
        feed_audio(&mut w, 400);
        w.text(
            r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"i1","content_index":0,"delta":"Hello"}"#,
            &mut act,
        )
        .ok();
        w.text(
            r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"i1","delta":" there"}"#,
            &mut act,
        )
        .ok();
        assert_eq!(
            texts(&act),
            [
                ("Hello".into(), false, false, false),
                ("Hello there".into(), false, false, false)
            ]
        );
        act.reset();
        w.finalize(&mut act);
        assert_eq!(sent_text(&act, 0), COMMIT);
        assert!(!act.answered);
        act.reset();
        w.text(
            r#"{"type":"input_audio_buffer.committed","item_id":"i1","previous_item_id":null}"#,
            &mut act,
        )
        .ok();
        w.text(
            r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"i1","content_index":0,"transcript":"Hello there."}"#,
            &mut act,
        )
        .ok();
        assert_eq!(texts(&act), [("Hello there.".into(), true, true, true)]);
        assert!(act.answered && act.trim == Trim::ToMark);
    }

    #[test]
    fn a_completed_of_another_item_is_a_server_turn_not_the_answer() {
        let (mut w, mut act) = wire(Dialect::Speaches);
        feed_audio(&mut w, 500);
        w.finalize(&mut act);
        act.reset();
        w.text(
            r#"{"type":"input_audio_buffer.committed","item_id":"ours"}"#,
            &mut act,
        )
        .ok();
        // The server's VAD finished an earlier turn meanwhile.
        w.text(
            r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"vad","transcript":"earlier words"}"#,
            &mut act,
        )
        .ok();
        assert_eq!(texts(&act), [("earlier words".into(), true, true, false)]);
        assert!(!act.answered);
        assert_eq!(act.trim, Trim::KeepLast(SERVER_TURN_TAIL));
        act.reset();
        w.text(
            r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"ours","transcript":"the answer"}"#,
            &mut act,
        )
        .ok();
        assert_eq!(texts(&act), [("the answer".into(), true, true, true)]);
        assert!(act.answered);
    }

    #[test]
    fn nothing_or_too_little_audio_is_answered_empty_without_a_commit() {
        let (mut w, mut act) = wire(Dialect::OpenAi);
        w.finalize(&mut act);
        assert!(act.answered && act.send.is_empty());
        assert_eq!(texts(&act), [(String::new(), true, true, true)]);
        // 50 ms is below the 100 ms the servers accept.
        let (mut w, mut act) = wire(Dialect::Speaches);
        feed_audio(&mut w, 50);
        w.finalize(&mut act);
        assert!(act.answered && act.send.is_empty());
        // NIM has no minimum.
        let (mut w, mut act) = wire(Dialect::Nim);
        feed_audio(&mut w, 20);
        w.finalize(&mut act);
        assert!(!act.answered && act.send.len() == 1);
    }

    #[test]
    fn a_server_error_while_a_commit_is_open_ends_the_turn_empty() {
        let (mut w, mut act) = wire(Dialect::OpenAi);
        feed_audio(&mut w, 300);
        w.finalize(&mut act);
        act.reset();
        w.text(
            r#"{"type":"error","error":{"type":"invalid_request_error","code":"input_audio_buffer_commit_empty","message":"buffer too small"}}"#,
            &mut act,
        )
        .ok();
        assert_eq!(texts(&act), [(String::new(), true, true, true)]);
        // An error with nothing open is only logged.
        act.reset();
        w.text(
            r#"{"type":"error","error":{"code":"whatever","message":"x"}}"#,
            &mut act,
        )
        .ok();
        assert!(act.events.is_empty() && !act.answered);
        // Credentials and missing models are final.
        let err = w.text(
            r#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_api_key","message":"x"}}"#,
            &mut act,
        );
        assert!(matches!(err, Err(Error::Rejected { status: 401, .. })));
    }

    #[test]
    fn a_timed_out_commit_answers_with_the_hypothesis_and_its_late_transcript_is_dropped() {
        let (mut w, mut act) = wire(Dialect::OpenAi);
        feed_audio(&mut w, 300);
        w.text(
            r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"i1","delta":"half"}"#,
            &mut act,
        )
        .ok();
        w.finalize(&mut act);
        w.text(
            r#"{"type":"input_audio_buffer.committed","item_id":"i1"}"#,
            &mut act,
        )
        .ok();
        act.reset();
        w.finalize_timed_out(&mut act);
        assert_eq!(texts(&act), [("half".into(), true, true, true)]);
        act.reset();
        // The real answer shows up later: dropped, not a second final.
        w.text(
            r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"i1","transcript":"half of it"}"#,
            &mut act,
        )
        .ok();
        assert!(act.events.is_empty() && !act.answered);
        // A later turn of the same connection works as usual.
        feed_audio(&mut w, 300);
        w.finalize(&mut act);
        act.reset();
        w.text(
            r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"i2","transcript":"next"}"#,
            &mut act,
        )
        .ok();
        assert_eq!(texts(&act), [("next".into(), true, true, true)]);
    }

    #[test]
    fn speaches_has_no_interims() {
        let (mut w, mut act) = wire(Dialect::Speaches);
        w.text(
            r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"i","delta":"x"}"#,
            &mut act,
        )
        .ok();
        assert!(act.events.is_empty());
    }

    #[test]
    fn replacing_deltas_show_the_latest_hypothesis() {
        let mut p = params(Dialect::Nim);
        p.replaces = true;
        let mut w = RealtimeWire::new(p);
        let mut act = Actions::default();
        w.text(r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"a","delta":"hel"}"#, &mut act).ok();
        w.text(r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"a","delta":"hello wor"}"#, &mut act).ok();
        assert_eq!(
            texts(&act),
            [
                ("hel".into(), false, false, false),
                ("hello wor".into(), false, false, false)
            ]
        );
    }

    #[test]
    fn vllm_finalizes_with_a_final_commit_holds_audio_and_restarts_after_done() {
        let (mut w, mut act) = wire(Dialect::Vllm);
        feed_audio(&mut w, 500);
        w.text(
            r#"{"type":"transcription.delta","delta":"merhaba"}"#,
            &mut act,
        )
        .ok();
        assert_eq!(texts(&act), [("merhaba".into(), false, false, false)]);
        act.reset();
        w.finalize(&mut act);
        assert_eq!(sent_text(&act, 0), COMMIT_FINAL);
        assert_eq!(act.gate, Some(false));
        act.reset();
        w.text(
            r#"{"type":"transcription.done","text":"merhaba dunya","usage":{"total_tokens":3}}"#,
            &mut act,
        )
        .ok();
        assert_eq!(texts(&act), [("merhaba dunya".into(), true, true, true)]);
        assert_eq!(act.gate, Some(true));
        assert_eq!(sent_text(&act, 0), COMMIT);
        assert!(act.answered);
        // A generation that ends by itself is a server-side final and is restarted.
        act.reset();
        w.text(
            r#"{"type":"transcription.done","text":"unasked"}"#,
            &mut act,
        )
        .ok();
        assert_eq!(texts(&act), [("unasked".into(), true, true, false)]);
        assert!(!act.answered);
        assert_eq!(sent_text(&act, 0), COMMIT);
        // vLLM errors are a string with a sibling code.
        act.reset();
        feed_audio(&mut w, 300);
        w.finalize(&mut act);
        act.reset();
        w.text(
            r#"{"type":"error","error":"Invalid audio data","code":"invalid_audio"}"#,
            &mut act,
        )
        .ok();
        assert!(act.answered && act.gate == Some(true));
        assert_eq!(sent_text(&act, 0), COMMIT);
    }

    #[test]
    fn vllm_timeout_replaces_the_connection() {
        let (mut w, mut act) = wire(Dialect::Vllm);
        feed_audio(&mut w, 300);
        w.finalize(&mut act);
        act.reset();
        w.finalize_timed_out(&mut act);
        assert!(act.answered && act.rotate && act.gate == Some(true));
    }

    #[test]
    fn malformed_json_is_a_protocol_error_and_unknown_events_are_ignored() {
        let (mut w, mut act) = wire(Dialect::OpenAi);
        assert!(matches!(
            w.text("<<not json>>", &mut act),
            Err(Error::Protocol { .. })
        ));
        assert!(
            w.text(
                r#"{"type":"session.created","session":{"id":"s"}}"#,
                &mut act
            )
            .is_ok()
        );
        assert!(
            w.text(r#"{"type":"rate_limits.updated"}"#, &mut act)
                .is_ok()
        );
        assert!(act.events.is_empty());
    }

    #[test]
    fn min_commit_is_in_wire_bytes() {
        let w = RealtimeWire::new(params(Dialect::OpenAi));
        // 100 ms of 24 kHz PCM16.
        assert_eq!(w.min_commit, 4800);
    }
}
