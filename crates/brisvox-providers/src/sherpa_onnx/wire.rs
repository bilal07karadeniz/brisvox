//! The sherpa-onnx streaming WebSocket protocol (`docs/providers/sherpa-onnx.md`): audio out as
//! little-endian `float32`, results in as JSON, and the `Done` exchange that stands in for a
//! mid-stream finalize.

use std::time::Duration;

use bytes::{BufMut, BytesMut};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::PROVIDER;
use crate::error::{Error, Result};
use crate::wsstt::{Actions, Closed, Trim, Wire, default_closed};

/// The text message that ends the audio of a connection.
pub(super) const DONE: &str = "Done";

/// The text the C++ server sends after the last result of a `Done` exchange.
pub(super) const DONE_ACK: &str = "Done!";

/// Sent audio kept for replay after a server-side endpoint settled what came before (the
/// recognizer was reset there, so only a short overlap is worth replaying).
pub(super) const ENDPOINT_TAIL: Duration = Duration::from_secs(1);

/// What a result message holds. The Python example server sends `text` and `segment`; the C++
/// server sends many more fields (tokens, timestamps, ...), of which `is_final` and `is_eof`
/// matter here. Unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct ResultMsg<'a> {
    #[serde(borrow, default)]
    text: std::borrow::Cow<'a, str>,
    #[serde(default)]
    segment: Option<u64>,
    #[serde(default)]
    is_final: Option<bool>,
    #[serde(default)]
    is_eof: Option<bool>,
}

fn protocol(detail: impl Into<String>) -> Error {
    Error::Protocol {
        provider: PROVIDER,
        detail: detail.into(),
    }
}

/// Converts PCM16 LE samples to the `float32` LE samples (range -1..1) the server expects,
/// appending to `out`.
pub(super) fn pcm16_to_f32le(pcm: &[u8], out: &mut BytesMut) {
    out.reserve(pcm.len().saturating_mul(2));
    for pair in pcm.as_chunks::<2>().0 {
        out.put_f32_le(f32::from(i16::from_le_bytes(*pair)) / 32_768.0);
    }
}

/// Per-session protocol state.
#[derive(Debug, Default)]
pub(super) struct SherpaWire {
    /// Encoding buffer: frames are cut off its front, so its allocation is reclaimed as the
    /// socket drops them and the steady state does not allocate per frame.
    scratch: BytesMut,
    /// `segment` of the latest result (Python server).
    segment: Option<u64>,
    /// The hypothesis of the open segment, as of the latest result.
    text: String,
    /// The last interim sent to the caller (identical results are not repeated).
    emitted: String,
    /// Audio was written on this connection.
    has_audio: bool,
    /// `Done` was sent and the exchange is not over.
    draining: bool,
    /// Silence (`float32` zeros) written before `Done`; empty: none.
    padding: bytes::Bytes,
}

impl SherpaWire {
    /// A protocol state that writes `padding` (already `float32` zeros) before every `Done`.
    pub(super) fn new(padding: bytes::Bytes) -> Self {
        Self {
            padding,
            ..Self::default()
        }
    }

    /// Ends the `Done` exchange: the answer to the `Finalize`, the connection is spent.
    fn complete(&mut self, text: &str, act: &mut Actions) {
        act.answer(text);
        act.trim = Trim::ToMark;
        act.gate = Some(true);
        act.rotate = true;
        self.draining = false;
        self.text.clear();
        self.emitted.clear();
    }

    fn set_text(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
    }
}

impl Wire for SherpaWire {
    fn opened(&mut self, _act: &mut Actions) {
        // No setup message: the protocol has no parameters.
        self.segment = None;
        self.text.clear();
        self.emitted.clear();
        self.has_audio = false;
        self.draining = false;
    }

    fn audio(&mut self, pcm: &[u8]) -> Result<Message> {
        pcm16_to_f32le(pcm, &mut self.scratch);
        self.has_audio = true;
        Ok(Message::Binary(self.scratch.split().freeze()))
    }

    fn finalize(&mut self, act: &mut Actions) {
        if self.draining {
            return;
        }
        if !self.has_audio {
            // Nothing was heard on this connection: nothing to flush, no round trip.
            act.answer("");
            act.trim = Trim::ToMark;
            return;
        }
        // No mid-stream finalize exists: `Done` makes the server pad the tail, decode and send
        // the last result; the connection is spent afterwards. Audio that arrives meanwhile is
        // held for the connection that replaces it.
        self.draining = true;
        act.gate = Some(false);
        if !self.padding.is_empty() {
            // The server pads only 0.3 s itself, which is too little for the decoder's right
            // context: the last word was cut and a short utterance came back empty.
            act.send.push(Message::Binary(self.padding.clone()));
        }
        act.send.push(Message::text(DONE));
    }

    fn text(&mut self, text: &str, act: &mut Actions) -> Result<()> {
        if text == DONE_ACK {
            // C++ server: the end marker, after the eof result (normally already handled).
            if self.draining {
                let last = std::mem::take(&mut self.text);
                self.complete(&last, act);
            }
            return Ok(());
        }
        let result: ResultMsg<'_> = serde_json::from_str(text)
            .map_err(|_| protocol("a result message is not valid JSON"))?;
        act.healthy = true;
        let hypothesis = result.text.trim();
        // Python server: the endpoint carries no flag; a higher `segment` means the previous
        // segment ended (its last hypothesis is final).
        if let (Some(segment), None) = (result.segment, result.is_final) {
            if self.segment.is_some_and(|previous| segment > previous) {
                if !self.text.trim().is_empty() {
                    let ended = std::mem::take(&mut self.text);
                    act.endpoint_final(ended.trim());
                    act.trim = Trim::KeepLast(ENDPOINT_TAIL);
                }
                self.text.clear();
                self.emitted.clear();
            }
            self.segment = Some(segment);
        }
        if result.is_final == Some(true) {
            // C++ server: the recognizer's own endpoint (or the end of a `Done`).
            let eof = result.is_eof == Some(true);
            if self.draining && eof {
                self.complete(hypothesis, act);
                return Ok(());
            }
            if !hypothesis.is_empty() {
                act.endpoint_final(hypothesis);
                act.trim = Trim::KeepLast(ENDPOINT_TAIL);
            }
            self.text.clear();
            self.emitted.clear();
            return Ok(());
        }
        self.set_text(hypothesis);
        // While the `Done` exchange is open the answer is on its way; it carries this text.
        if !self.draining && !hypothesis.is_empty() && hypothesis != self.emitted {
            act.interim(hypothesis);
            self.emitted.clear();
            self.emitted.push_str(hypothesis);
        }
        if self.draining && result.is_eof == Some(true) {
            let last = std::mem::take(&mut self.text);
            self.complete(&last, act);
        }
        Ok(())
    }

    fn closed(&mut self, code: Option<CloseCode>, act: &mut Actions) -> Closed {
        if self.draining {
            // Python server: the last result came, then the server closed. The text of the
            // latest result is the answer.
            let last = std::mem::take(&mut self.text);
            self.complete(&last, act);
        }
        default_closed(PROVIDER, code)
    }

    fn finalize_timed_out(&mut self, act: &mut Actions) {
        // The server did not finish in time: the latest hypothesis is the best there is, and the
        // connection (which may still be decoding) is replaced.
        let last = std::mem::take(&mut self.text);
        self.complete(&last, act);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SttEvent;

    fn wire() -> (SherpaWire, Actions) {
        let mut w = SherpaWire::default();
        let mut act = Actions::default();
        w.opened(&mut act);
        (w, act)
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

    #[test]
    fn finalize_writes_the_padding_before_done_only_after_audio() {
        let mut w = SherpaWire::new(bytes::Bytes::from(vec![0_u8; 64]));
        let mut act = Actions::default();
        w.opened(&mut act);
        // Nothing heard: the empty shortcut, no padding, no Done.
        act.reset();
        w.finalize(&mut act);
        assert!(act.send.is_empty() && act.answered);
        // After audio: padding, then Done.
        let (_, mut act) = (0, Actions::default());
        w.opened(&mut act);
        w.audio(&[0, 0]).unwrap();
        act.reset();
        w.finalize(&mut act);
        assert_eq!(act.send.len(), 2);
        assert!(matches!(&act.send[0], Message::Binary(b) if b.len() == 64));
        assert!(matches!(&act.send[1], Message::Text(t) if t.as_str() == DONE));
        // No padding configured: Done alone.
        let mut w = SherpaWire::default();
        let mut act = Actions::default();
        w.opened(&mut act);
        w.audio(&[0, 0]).unwrap();
        act.reset();
        w.finalize(&mut act);
        assert_eq!(act.send.len(), 1);
    }

    #[test]
    fn pcm16_becomes_little_endian_float32() {
        let mut out = BytesMut::new();
        let pcm: Vec<u8> = [0_i16, 16_384, -32_768, 32_767]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        pcm16_to_f32le(&pcm, &mut out);
        assert_eq!(out.len(), 16);
        let values: Vec<f32> = out
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        assert_eq!(values.len(), 4);
        assert!((values[0] - 0.0).abs() < 1e-9);
        assert!((values[1] - 0.5).abs() < 1e-6);
        assert!((values[2] + 1.0).abs() < 1e-6);
        assert!((values[3] - 0.999_97).abs() < 1e-4);
        // An odd trailing byte is ignored, not a panic.
        let mut out = BytesMut::new();
        pcm16_to_f32le(&[1, 2, 3], &mut out);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn python_results_become_interims_and_a_segment_bump_is_an_endpoint_final() {
        let (mut w, mut act) = wire();
        w.text(r#"{"text":"merhaba","segment":0}"#, &mut act).ok();
        w.text(r#"{"text":"merhaba","segment":0}"#, &mut act).ok();
        w.text(r#"{"text":"merhaba dunya","segment":0}"#, &mut act)
            .ok();
        // Identical hypotheses are not repeated.
        assert_eq!(
            texts(&act),
            [
                ("merhaba".into(), false, false, false),
                ("merhaba dunya".into(), false, false, false)
            ]
        );
        act.reset();
        w.text(r#"{"text":"","segment":1}"#, &mut act).ok();
        assert_eq!(texts(&act), [("merhaba dunya".into(), true, true, false)]);
        assert_eq!(act.trim, Trim::KeepLast(ENDPOINT_TAIL));
        assert!(!act.answered);
        assert!(act.healthy);
    }

    #[test]
    fn cpp_results_use_is_final() {
        let (mut w, mut act) = wire();
        w.text(
            r#"{ "text": "hello", "tokens": ["he","llo"], "timestamps": [0.1, 0.2], "segment": 0, "is_final": false, "is_eof": false}"#,
            &mut act,
        )
        .ok();
        w.text(
            r#"{ "text": "hello there", "segment": 0, "is_final": true, "is_eof": false}"#,
            &mut act,
        )
        .ok();
        assert_eq!(
            texts(&act),
            [
                ("hello".into(), false, false, false),
                ("hello there".into(), true, true, false)
            ]
        );
        // The segment counter of a C++ result does not trigger the Python-style bump.
        act.reset();
        w.text(
            r#"{ "text": "", "segment": 1, "is_final": false}"#,
            &mut act,
        )
        .ok();
        assert!(texts(&act).is_empty());
    }

    #[test]
    fn finalize_without_audio_is_answered_at_once_and_sends_nothing() {
        let (mut w, mut act) = wire();
        w.finalize(&mut act);
        assert!(act.answered && act.send.is_empty() && !act.rotate);
        assert_eq!(texts(&act), [(String::new(), true, true, true)]);
    }

    #[test]
    fn finalize_sends_done_shuts_the_gate_and_the_close_answers_with_the_last_text() {
        let (mut w, mut act) = wire();
        assert!(w.audio(&[0, 0, 0, 0]).is_ok());
        w.text(r#"{"text":"selam nasilsin","segment":0}"#, &mut act)
            .ok();
        act.reset();
        w.finalize(&mut act);
        assert_eq!(act.send.len(), 1);
        assert!(matches!(&act.send[0], Message::Text(t) if t.as_str() == DONE));
        assert_eq!(act.gate, Some(false));
        assert!(!act.answered);
        // A second request while draining adds nothing.
        act.reset();
        w.finalize(&mut act);
        assert!(act.send.is_empty() && !act.answered);
        // The last result, then the close.
        w.text(
            r#"{"text":"selam nasilsin iyi misin","segment":0}"#,
            &mut act,
        )
        .ok();
        assert!(!act.answered);
        act.reset();
        let verdict = w.closed(Some(CloseCode::Normal), &mut act);
        assert!(matches!(verdict, Closed::Lost));
        assert!(act.answered && act.rotate && act.gate == Some(true));
        assert_eq!(
            texts(&act),
            [("selam nasilsin iyi misin".into(), true, true, true)]
        );
    }

    #[test]
    fn cpp_eof_result_or_done_ack_completes_the_exchange() {
        for end in ["eof_result", "done_ack"] {
            let (mut w, mut act) = wire();
            assert!(w.audio(&[0, 0]).is_ok());
            w.finalize(&mut act);
            act.reset();
            w.text(
                r#"{"text":"one two","segment":0,"is_final":false,"is_eof":false}"#,
                &mut act,
            )
            .ok();
            if end == "eof_result" {
                w.text(
                    r#"{"text":"one two three","segment":0,"is_final":true,"is_eof":true}"#,
                    &mut act,
                )
                .ok();
                assert_eq!(
                    texts(&act).last(),
                    Some(&("one two three".to_owned(), true, true, true))
                );
            } else {
                w.text(DONE_ACK, &mut act).ok();
                assert_eq!(
                    texts(&act).last(),
                    Some(&("one two".to_owned(), true, true, true))
                );
            }
            assert!(act.answered && act.rotate, "{end}");
        }
    }

    #[test]
    fn a_timeout_answers_with_the_latest_hypothesis_and_replaces_the_connection() {
        let (mut w, mut act) = wire();
        assert!(w.audio(&[0, 0]).is_ok());
        w.text(r#"{"text":"yarim","segment":0}"#, &mut act).ok();
        w.finalize(&mut act);
        act.reset();
        w.finalize_timed_out(&mut act);
        assert_eq!(texts(&act), [("yarim".into(), true, true, true)]);
        assert!(act.rotate && act.answered);
    }

    #[test]
    fn garbage_is_a_protocol_error_and_a_close_without_done_is_a_drop() {
        let (mut w, mut act) = wire();
        let err = w.text("<<not json>>", &mut act);
        assert!(matches!(err, Err(Error::Protocol { .. })));
        let verdict = w.closed(Some(CloseCode::Policy), &mut act);
        assert!(matches!(
            verdict,
            Closed::Fatal(Error::Rejected { status: 1008, .. })
        ));
        let verdict = w.closed(None, &mut act);
        assert!(matches!(verdict, Closed::Lost));
        assert!(!act.answered);
    }
}
