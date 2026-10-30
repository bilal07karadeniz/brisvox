//! Turns AssemblyAI `Turn` messages into [`SttEvent`]s.
//!
//! Mapping (see `docs/providers/assemblyai.md`, "Turn semantics"):
//! - A `Turn` with `end_of_turn:false` is an interim. It re-transcribes the whole turn, so it
//!   replaces the previous interim (the consumer replaces, the adapter never concatenates).
//!   Blank and repeated interims are not forwarded.
//! - A `Turn` with `end_of_turn:true` is the final of the turn: one final transcript with
//!   `speech_final` (an AssemblyAI turn is the utterance). With `format_turns` on a Universal
//!   Streaming model the unformatted final is held back (forwarded as an interim) and the
//!   formatted twin is the final.
//! - A `ForceEndpoint` is answered exactly once: by the first final turn after it (the docs say
//!   nothing on the wire tells a forced final from a natural one, so that final gets
//!   `from_finalize`), by an empty final turn, or synthetically by
//!   [`Assembler::finalize_timed_out`] (also used for an idle `Finalize` with a zero wait).
//! - A real final that arrives after a synthetic one is a duplicate when its text is the same
//!   (ignoring case and punctuation); otherwise it is real speech the synthetic final missed and
//!   is delivered as another utterance end. This only holds for a short grace period after the
//!   synthetic final: later finals are ordinary utterance ends (no `from_finalize`).
//! - A repeated final for a `turn_order` that already ended, and interims of an ended turn, are
//!   dropped. An empty final with nothing waiting for it (the closing turn after `Terminate`)
//!   ends nothing.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::protocol::Turn;
use crate::stt::{SpokenLanguage, SttEvent};

/// How long after a synthetic final a real final still counts as the late answer to the
/// `Finalize` (and gets `from_finalize`): well past the service's typical turn-completion time,
/// short enough that the next utterance is never taken for it.
const LATE_ANSWER_GRACE: Duration = Duration::from_secs(3);

/// What a synthetic final said, to recognise the server's real final of the same speech.
#[derive(Debug)]
struct Synthetic {
    /// When the synthetic final was made.
    at: Instant,
    /// Text of the synthetic final (possibly empty).
    text: String,
    /// `turn_order` of the interim it was made from, if there was one.
    turn: Option<u64>,
}

/// State of the utterance being transcribed.
#[derive(Debug)]
pub(crate) struct Assembler {
    /// Wait for the formatted twin of an unformatted final (`format_turns` on Universal
    /// Streaming).
    wait_formatted: bool,
    /// Latest non-blank interim text of the open turn.
    interim: String,
    /// `turn_order` of the open turn.
    open_turn: Option<u64>,
    /// `turn_order` of the last turn that ended on this connection.
    last_final_turn: Option<u64>,
    /// A `Finalize` was requested and not yet answered.
    finalize_pending: bool,
    /// The last final that was made by the adapter, until the server's own final or a new turn.
    synthetic: Option<Synthetic>,
}

impl Assembler {
    /// A fresh assembler. `wait_formatted`: hold back an unformatted final turn until its
    /// formatted twin arrives (`AssemblyAiConfig::waits_for_formatted`).
    pub(crate) fn new(wait_formatted: bool) -> Self {
        Self {
            wait_formatted,
            interim: String::new(),
            open_turn: None,
            last_final_turn: None,
            finalize_pending: false,
            synthetic: None,
        }
    }

    /// Whether a `Finalize` is waiting for its answer.
    pub(crate) fn finalize_pending(&self) -> bool {
        self.finalize_pending
    }

    /// Nothing is in progress: no interim text and no `Finalize`.
    pub(crate) fn is_idle(&self) -> bool {
        self.interim.is_empty() && !self.finalize_pending
    }

    /// A partial transcript is open (the model reported speech that has not ended yet).
    pub(crate) fn has_interim(&self) -> bool {
        !self.interim.is_empty()
    }

    /// The caller asked to finalize the utterance now.
    pub(crate) fn finalize_requested(&mut self) {
        self.finalize_pending = true;
        self.synthetic = None;
    }

    /// The connection was replaced: the in-flight hypothesis is gone and the new connection
    /// numbers its turns afresh (presumed, UNVERIFIED). A synthetic final stays, so the replayed
    /// audio's real final is still recognised as its duplicate.
    pub(crate) fn connection_replaced(&mut self) {
        self.interim.clear();
        self.open_turn = None;
        self.last_final_turn = None;
    }

    /// Handles one `Turn`. Returns whether a final transcript was produced (the turn ended).
    pub(crate) fn on_turn(&mut self, turn: &Turn, now: Instant, out: &mut Vec<SttEvent>) -> bool {
        // A synthetic final is only waiting for its late answer for a short while.
        if self
            .synthetic
            .as_ref()
            .is_some_and(|s| now.saturating_duration_since(s.at) > LATE_ANSWER_GRACE)
        {
            self.synthetic = None;
        }
        let held = self.wait_formatted
            && turn.end_of_turn
            && !turn.formatted
            && !turn.transcript.is_empty();
        if !turn.end_of_turn || held {
            self.on_interim(turn.order, &turn.transcript, out);
            return false;
        }
        self.on_final(
            turn.order,
            &turn.transcript,
            turn.language.as_ref(),
            now,
            out,
        )
    }

    fn on_interim(&mut self, order: u64, text: &str, out: &mut Vec<SttEvent>) {
        if text.is_empty() || self.last_final_turn == Some(order) {
            return;
        }
        match &self.synthetic {
            // More of a turn the adapter already ended: the consumer has moved on.
            Some(s) if s.turn == Some(order) => return,
            Some(_) => self.synthetic = None,
            None => {}
        }
        if self.open_turn == Some(order) && self.interim == text {
            return;
        }
        self.open_turn = Some(order);
        self.interim.clear();
        self.interim.push_str(text);
        out.push(transcript(text, false, false, false));
    }

    fn on_final(
        &mut self,
        order: u64,
        text: &str,
        language: Option<&SpokenLanguage>,
        now: Instant,
        out: &mut Vec<SttEvent>,
    ) -> bool {
        if text.is_empty() {
            // An empty final matters only to a Finalize that is waiting for its answer.
            if self.finalize_pending {
                self.finalize_pending = false;
                self.end_with("", None, true, now, out);
                return true;
            }
            return false;
        }
        if self.last_final_turn == Some(order) {
            return false;
        }
        let late = self.synthetic.take();
        if late.as_ref().is_some_and(|s| same_text(text, &s.text)) {
            self.last_final_turn = Some(order);
            self.interim.clear();
            self.open_turn = None;
            return false;
        }
        let answers_late = late
            .as_ref()
            .is_some_and(|s| s.turn.is_none() || s.turn == Some(order));
        let from_finalize = self.finalize_pending || answers_late;
        let mut event = transcript(text, true, true, from_finalize);
        if let SttEvent::Transcript {
            language: reported, ..
        } = &mut event
        {
            reported.clone_from(&language.cloned());
        }
        out.push(event);
        self.last_final_turn = Some(order);
        self.interim.clear();
        self.open_turn = None;
        self.finalize_pending = false;
        true
    }

    /// The `Finalize` was not answered in time: ends the utterance with the last interim (or
    /// empty text) so the turn machine never waits for a response that will not come. A real
    /// answer that still arrives later is handled as described in the module docs.
    pub(crate) fn finalize_timed_out(&mut self, now: Instant, out: &mut Vec<SttEvent>) {
        self.finalize_pending = false;
        let text = std::mem::take(&mut self.interim);
        let turn = self.open_turn.take();
        self.end_with(&text, turn, true, now, out);
    }

    /// Ends the utterance with a final carrying `text` (possibly empty) that the adapter made.
    fn end_with(
        &mut self,
        text: &str,
        turn: Option<u64>,
        from_finalize: bool,
        now: Instant,
        out: &mut Vec<SttEvent>,
    ) {
        out.push(transcript(text, true, true, from_finalize));
        self.interim.clear();
        self.open_turn = None;
        self.synthetic = Some(Synthetic {
            at: now,
            text: text.to_owned(),
            turn,
        });
    }
}

/// Text equality that ignores case, punctuation and spacing (an interim and the final of the
/// same words differ in formatting). Blank text is never "the same".
///
/// Turkish casing is handled on purpose: the locale-free `char::to_lowercase` turns `I` into
/// `i` (not `ı`) and `İ` into `i` plus a combining dot (U+0307), so `Işık` and `ışık` would
/// differ. Lowercasing happens first, the combining mark is then dropped by the alphanumeric
/// filter, and the dotless `ı` is folded to `i`, so both spellings of a word compare equal.
fn same_text(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.chars()
            .flat_map(char::to_lowercase)
            .map(|c| if c == '\u{131}' { 'i' } else { c })
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
    };
    let b = norm(b);
    !b.is_empty() && norm(a) == b
}

fn transcript(text: &str, is_final: bool, speech_final: bool, from_finalize: bool) -> SttEvent {
    SttEvent::Transcript {
        text: Arc::from(text),
        is_final,
        speech_final,
        from_finalize,
        language: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed point in time (the assembler never reads a clock).
    #[allow(clippy::disallowed_methods)]
    fn t0() -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now)
    }

    fn turn(order: u64, text: &str, end: bool, formatted: bool) -> Turn {
        Turn {
            order,
            end_of_turn: end,
            formatted,
            transcript: text.to_owned(),
            last_word_end_ms: None,
            language: None,
        }
    }

    #[test]
    fn the_detected_language_goes_to_the_final_of_the_turn_only() {
        let mut a = Assembler::new(false);
        let mut out = Vec::new();
        let mut interim = turn(0, "merhaba", false, false);
        interim.language = Some(SpokenLanguage::new("tr"));
        a.on_turn(&interim, t0(), &mut out);
        let mut done = turn(0, "merhaba dünya", true, true);
        done.language = Some(SpokenLanguage::new("tr").with_confidence(0.93));
        a.on_turn(&done, t0(), &mut out);
        let found: Vec<Option<(String, Option<f32>)>> = out
            .iter()
            .map(|e| match e {
                SttEvent::Transcript { language, .. } => language
                    .as_ref()
                    .map(|l| (l.code.to_string(), l.confidence)),
                SttEvent::Error(_) => None,
            })
            .collect();
        assert_eq!(found, [None, Some(("tr".to_owned(), Some(0.93)))]);
    }

    /// (text, is_final, speech_final, from_finalize)
    type Flat = (String, bool, bool, bool);

    fn flat(events: &[SttEvent]) -> Vec<Flat> {
        events
            .iter()
            .map(|e| match e {
                SttEvent::Transcript {
                    text,
                    is_final,
                    speech_final,
                    from_finalize,
                    ..
                } => (text.to_string(), *is_final, *speech_final, *from_finalize),
                SttEvent::Error(e) => panic!("unexpected error event {e}"),
            })
            .collect()
    }

    fn t(text: &str, f: bool, s: bool, ff: bool) -> Flat {
        (text.to_owned(), f, s, ff)
    }

    fn run(a: &mut Assembler, turns: &[Turn]) -> (Vec<Flat>, Vec<bool>) {
        let mut out = Vec::new();
        let ended: Vec<bool> = turns.iter().map(|x| a.on_turn(x, t0(), &mut out)).collect();
        (flat(&out), ended)
    }

    #[test]
    fn partials_replace_each_other_and_the_final_ends_the_utterance() {
        let mut a = Assembler::new(false);
        let (events, ended) = run(
            &mut a,
            &[
                turn(0, "merhaba", false, false),
                turn(0, "merhaba dünya", false, false),
                turn(0, "Merhaba dünya.", true, true),
            ],
        );
        assert_eq!(
            events,
            [
                t("merhaba", false, false, false),
                t("merhaba dünya", false, false, false),
                t("Merhaba dünya.", true, true, false),
            ]
        );
        assert_eq!(ended, [false, false, true]);
        assert!(a.is_idle());
    }

    #[test]
    fn blank_repeated_and_stale_partials_are_not_forwarded() {
        let mut a = Assembler::new(false);
        let (events, _) = run(
            &mut a,
            &[
                turn(0, "", false, false),
                turn(0, "evet", false, false),
                turn(0, "evet", false, false),
                turn(0, "Evet.", true, true),
                turn(0, "evet", false, false), // late partial of the ended turn
                turn(1, "hayır", false, false),
            ],
        );
        assert_eq!(
            events,
            [
                t("evet", false, false, false),
                t("Evet.", true, true, false),
                t("hayır", false, false, false),
            ]
        );
    }

    #[test]
    fn a_repeated_final_of_an_ended_turn_is_dropped() {
        let mut a = Assembler::new(false);
        let (events, ended) = run(
            &mut a,
            &[turn(4, "Tamam.", true, true), turn(4, "Tamam.", true, true)],
        );
        assert_eq!(events, [t("Tamam.", true, true, false)]);
        assert_eq!(ended, [true, false]);
    }

    #[test]
    fn with_format_turns_the_unformatted_final_waits_for_its_formatted_twin() {
        let mut a = Assembler::new(true);
        let (events, ended) = run(
            &mut a,
            &[
                turn(0, "hello world", true, false),
                turn(0, "Hello, world.", true, true),
            ],
        );
        assert_eq!(
            events,
            [
                t("hello world", false, false, false),
                t("Hello, world.", true, true, false),
            ]
        );
        assert_eq!(ended, [false, true]);
        // Without the option the first end_of_turn is the final and the twin a duplicate.
        let mut b = Assembler::new(false);
        let (events, _) = run(
            &mut b,
            &[
                turn(0, "hello world", true, false),
                turn(0, "Hello, world.", true, true),
            ],
        );
        assert_eq!(events, [t("hello world", true, true, false)]);
    }

    #[test]
    fn the_final_after_a_finalize_answers_it_with_from_finalize() {
        let mut a = Assembler::new(false);
        let mut out = Vec::new();
        a.on_turn(&turn(0, "evet", false, false), t0(), &mut out);
        a.finalize_requested();
        assert!(a.finalize_pending() && !a.is_idle());
        assert!(a.on_turn(&turn(0, "Evet.", true, true), t0(), &mut out));
        assert!(!a.finalize_pending());
        assert_eq!(
            flat(&out),
            [t("evet", false, false, false), t("Evet.", true, true, true)]
        );
    }

    #[test]
    fn an_empty_final_answers_a_waiting_finalize_and_otherwise_ends_nothing() {
        let mut a = Assembler::new(false);
        let mut out = Vec::new();
        assert!(
            !a.on_turn(&turn(0, "", true, true), t0(), &mut out),
            "closing turn"
        );
        assert!(out.is_empty());
        a.finalize_requested();
        assert!(a.on_turn(&turn(0, "", true, true), t0(), &mut out));
        assert_eq!(flat(&out), [t("", true, true, true)]);
    }

    #[test]
    fn a_timed_out_finalize_ends_with_the_last_interim_and_the_late_real_final_is_deduplicated() {
        let mut a = Assembler::new(false);
        let mut out = Vec::new();
        a.on_turn(&turn(2, "bir iki", false, false), t0(), &mut out);
        a.finalize_requested();
        a.finalize_timed_out(t0(), &mut out);
        assert!(!a.finalize_pending());
        // Further partials of the turn the adapter ended are stale; the real final, with the
        // same words in the final formatting, is a duplicate.
        a.on_turn(&turn(2, "bir iki üç", false, false), t0(), &mut out);
        assert!(!a.on_turn(&turn(2, "Bir iki.", true, true), t0(), &mut out));
        assert_eq!(
            flat(&out),
            [
                t("bir iki", false, false, false),
                t("bir iki", true, true, true)
            ]
        );
    }

    #[test]
    fn a_late_final_with_other_text_is_delivered_as_the_answer() {
        let mut a = Assembler::new(false);
        let mut out = Vec::new();
        a.on_turn(&turn(2, "bir", false, false), t0(), &mut out);
        a.finalize_requested();
        a.finalize_timed_out(t0(), &mut out);
        assert!(a.on_turn(&turn(2, "Bir iki üç.", true, true), t0(), &mut out));
        assert_eq!(
            flat(&out).last().cloned(),
            Some(t("Bir iki üç.", true, true, true))
        );
        // A final of a later turn is not an answer to the Finalize.
        let mut b = Assembler::new(false);
        let mut out = Vec::new();
        b.on_turn(&turn(2, "bir", false, false), t0(), &mut out);
        b.finalize_requested();
        b.finalize_timed_out(t0(), &mut out);
        b.on_turn(&turn(3, "yeni", false, false), t0(), &mut out);
        b.on_turn(&turn(3, "Yeni.", true, true), t0(), &mut out);
        assert_eq!(
            flat(&out).last().cloned(),
            Some(t("Yeni.", true, true, false))
        );
    }

    #[test]
    fn a_finalize_with_no_interim_ends_empty_and_a_later_real_final_is_still_delivered() {
        let mut a = Assembler::new(false);
        let mut out = Vec::new();
        a.finalize_requested();
        a.finalize_timed_out(t0(), &mut out);
        assert!(a.on_turn(&turn(0, "Geç kalan.", true, true), t0(), &mut out));
        assert_eq!(
            flat(&out),
            [t("", true, true, true), t("Geç kalan.", true, true, true)]
        );
    }

    #[test]
    fn the_synthetic_marker_expires_so_a_later_natural_final_is_not_from_finalize() {
        let late = |after: Duration| {
            let mut a = Assembler::new(false);
            let mut out = Vec::new();
            a.finalize_requested();
            a.finalize_timed_out(t0(), &mut out);
            out.clear();
            // A very short utterance: a final turn with no partial before it.
            assert!(a.on_turn(&turn(0, "Evet.", true, true), t0() + after, &mut out));
            flat(&out)
        };
        assert_eq!(
            late(Duration::from_millis(500)),
            [t("Evet.", true, true, true)],
            "inside the grace period it is the late answer"
        );
        assert_eq!(
            late(LATE_ANSWER_GRACE + Duration::from_millis(1)),
            [t("Evet.", true, true, false)],
            "after it, nothing was outstanding"
        );
    }

    #[test]
    fn a_replaced_connection_forgets_the_hypothesis_and_renumbers_turns() {
        let mut a = Assembler::new(false);
        let mut out = Vec::new();
        a.on_turn(&turn(0, "ilk", false, false), t0(), &mut out);
        a.on_turn(&turn(0, "İlk.", true, true), t0(), &mut out);
        a.on_turn(&turn(1, "ikinci", false, false), t0(), &mut out);
        a.connection_replaced();
        assert!(a.is_idle());
        // The new connection starts at turn 0 again: not a duplicate of the old turn 0.
        assert!(a.on_turn(&turn(0, "Üçüncü.", true, true), t0(), &mut out));
        assert_eq!(
            flat(&out).last().cloned(),
            Some(t("Üçüncü.", true, true, false))
        );
    }

    #[test]
    fn same_text_handles_turkish_casing_and_formatting() {
        assert!(same_text("Işık yanıyor.", "ışık yanıyor"));
        assert!(same_text("İstanbul'a gidiyorum", "istanbula gidiyorum."));
        assert!(!same_text("", ""));
        assert!(!same_text("a", "b"));
    }
}
