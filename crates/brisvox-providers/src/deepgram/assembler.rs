//! Turns Deepgram `Results` / `UtteranceEnd` messages into [`SttEvent`]s.
//!
//! Mapping (see `docs/providers/deepgram.md`, "Result semantics"):
//! - `is_final=false` results are interims. They replace the in-progress text of the current
//!   segment (the text is the segment's, not the utterance's). Blank interims are not forwarded.
//! - Every `is_final=true` result with text is forwarded as a final segment. Deepgram's rule is
//!   to append all `is_final` segments and treat `speech_final` as the end of the utterance;
//!   the consumer does the appending (the trait's finals are segments), this module only decides
//!   which final carries `speech_final`.
//! - `speech_final` of a final result is forwarded. A result that answers a `Finalize`
//!   (`from_finalize`) always ends the utterance: the documentation does not say whether
//!   Deepgram sets `speech_final` on it (UNVERIFIED), so the adapter sets it itself.
//! - A `Finalize` is answered exactly once: by its `from_finalize` result, by a final that
//!   already covers the audio up to the Finalize position (the caller of
//!   [`Assembler::on_results`] judges that), or synthetically by
//!   [`Assembler::finalize_timed_out`] / [`Assembler::finalize_settled`]. An ordinary endpointed
//!   final that arrives while a Finalize is open does **not** answer it: it is forwarded as what
//!   it is (`from_finalize=false`) and the Finalize still gets its own answer.
//! - A `from_finalize` result that arrives after the Finalize was answered synthetically is
//!   late. Blank ones are dropped; a non-blank one whose text equals the synthetic final's is a
//!   duplicate and dropped; anything else is real text the synthetic final missed and is
//!   delivered as another utterance end.
//! - `is_final=false` together with `speech_final=true` appears in Deepgram's own endpointing
//!   example and is unexplained (UNVERIFIED). Both readings are handled: if finals of the
//!   utterance were already forwarded, the utterance is closed with an empty `speech_final`
//!   transcript; otherwise the flag is remembered for the next [`INHERIT_WINDOW`] results and
//!   given to the first final among them.
//! - `UtteranceEnd` closes an open utterance the same way (the last interim is promoted to a
//!   final if no final text is pending); `last_word_end == -1` is ignored.

use std::sync::Arc;

use super::protocol::Results;
use crate::stt::{SpokenLanguage, SttEvent};

/// How many results after a non-final `speech_final=true` may still inherit that flag. The final
/// of the same segment follows immediately in practice; the bound stops a flag that no final ever
/// claims from ending an unrelated utterance later.
const INHERIT_WINDOW: u8 = 2;

/// State of the utterance being transcribed.
#[derive(Debug, Default)]
pub(crate) struct Assembler {
    /// Final segments were forwarded since the last `speech_final`.
    open_finals: bool,
    /// Latest non-blank interim text since the last final.
    last_interim: String,
    /// Results left that may inherit a `speech_final` seen on a non-final result.
    inherit_left: u8,
    /// A `Finalize` was requested and not yet answered.
    finalize_pending: bool,
    /// Text of the last final that was made from an interim (not by the server): a real final
    /// with the same text that follows is a duplicate.
    synthetic_text: String,
}

impl Assembler {
    /// Whether a `Finalize` is waiting for its answer.
    pub(crate) fn finalize_pending(&self) -> bool {
        self.finalize_pending
    }

    /// Nothing is in progress: no open finals, no interim text, no flag owed, no `Finalize`.
    pub(crate) fn is_idle(&self) -> bool {
        !self.open_finals
            && self.last_interim.is_empty()
            && self.inherit_left == 0
            && !self.finalize_pending
    }

    /// The caller asked to finalize the utterance now.
    pub(crate) fn finalize_requested(&mut self) {
        self.finalize_pending = true;
        self.synthetic_text.clear();
    }

    /// The connection was replaced: the in-flight hypothesis is gone (finals already forwarded
    /// stay forwarded).
    pub(crate) fn connection_replaced(&mut self) {
        self.last_interim.clear();
    }

    /// Handles one `Results` message. `covers_finalize` says that this (final) result already
    /// reaches the audio position at which the pending `Finalize` was or will be sent.
    pub(crate) fn on_results(
        &mut self,
        r: &Results,
        covers_finalize: bool,
        out: &mut Vec<SttEvent>,
    ) {
        let inherited = self.inherit_left > 0;
        self.inherit_left = self.inherit_left.saturating_sub(1);
        if !r.is_final {
            if !r.transcript.is_empty() {
                self.last_interim.clear();
                self.last_interim.push_str(&r.transcript);
                self.synthetic_text.clear();
                out.push(transcript(&r.transcript, false, false, false));
            }
            if r.speech_final {
                if self.open_finals {
                    self.close_utterance(out);
                } else {
                    self.inherit_left = INHERIT_WINDOW;
                }
            }
            return;
        }
        let late = r.from_finalize && !self.finalize_pending;
        if late && r.transcript.is_empty() {
            return;
        }
        if !self.finalize_pending && same_text(&r.transcript, &self.synthetic_text) {
            // The server's version of what a synthetic final already delivered.
            self.synthetic_text.clear();
            self.last_interim.clear();
            return;
        }
        let answers = self.finalize_pending && (r.from_finalize || covers_finalize);
        let ends = r.speech_final || r.from_finalize || inherited || answers;
        self.last_interim.clear();
        if r.transcript.is_empty() {
            // Blank finals carry no text; they matter only as an utterance end that someone is
            // waiting for (an open utterance or a Finalize).
            if answers {
                self.finalize_pending = false;
                self.end_with("", true, out);
            } else if ends && self.open_finals {
                self.close_utterance(out);
            } else if ends {
                self.inherit_left = 0;
            }
            return;
        }
        self.synthetic_text.clear();
        let mut event = transcript(&r.transcript, true, ends, r.from_finalize || answers);
        if let SttEvent::Transcript { language, .. } = &mut event {
            *language = r.language.as_deref().map(SpokenLanguage::new);
        }
        out.push(event);
        self.open_finals = !ends;
        if ends {
            self.inherit_left = 0;
        }
        if answers {
            self.finalize_pending = false;
        }
    }

    /// Handles `UtteranceEnd` (`last_word_end` in seconds, `-1` = already finalized).
    pub(crate) fn on_utterance_end(&mut self, last_word_end: f64, out: &mut Vec<SttEvent>) {
        if last_word_end < 0.0 {
            return;
        }
        // Whatever happens below, a flag owed to a final that never came is stale now.
        self.inherit_left = 0;
        if !self.last_interim.is_empty() {
            let text = std::mem::take(&mut self.last_interim);
            self.end_with(&text, false, out);
        } else if self.open_finals {
            self.close_utterance(out);
        }
    }

    /// The `Finalize` was not answered in time: ends the utterance with the last interim (or
    /// empty text) so the turn machine never waits for a response that will not come. A real
    /// answer that still arrives later is handled as described in the module docs.
    pub(crate) fn finalize_timed_out(&mut self, out: &mut Vec<SttEvent>) {
        let text = std::mem::take(&mut self.last_interim);
        self.finalize_pending = false;
        self.end_with(&text, true, out);
    }

    /// The server is known to have nothing left for a `Finalize` requested now (see
    /// `Session::settled`): answers it at once with an empty `speech_final` transcript.
    pub(crate) fn finalize_settled(&mut self, out: &mut Vec<SttEvent>) {
        self.end_with("", true, out);
    }

    /// Ends the utterance with a final carrying `text` (possibly empty).
    fn end_with(&mut self, text: &str, from_finalize: bool, out: &mut Vec<SttEvent>) {
        out.push(transcript(text, true, true, from_finalize));
        self.open_finals = false;
        self.inherit_left = 0;
        self.last_interim.clear();
        self.synthetic_text.clear();
        self.synthetic_text.push_str(text);
    }

    /// Ends the utterance with an empty `speech_final` transcript.
    fn close_utterance(&mut self, out: &mut Vec<SttEvent>) {
        self.end_with("", false, out);
    }
}

/// Text equality that ignores case, punctuation and spacing (the interim and the final of the
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

    fn res(text: &str, is_final: bool, speech_final: bool, from_finalize: bool) -> Results {
        Results {
            transcript: text.to_owned(),
            is_final,
            speech_final,
            from_finalize,
            start: 0.0,
            duration: 1.0,
            language: None,
        }
    }

    #[test]
    fn the_reported_language_goes_to_final_segments_only() {
        let mut a = Assembler::default();
        let mut out = Vec::new();
        let mut interim = res("merhaba", false, false, false);
        interim.language = Some(Arc::from("tr"));
        a.on_results(&interim, false, &mut out);
        let mut done = res("merhaba dünya", true, true, false);
        done.language = Some(Arc::from("tr"));
        a.on_results(&done, false, &mut out);
        let languages: Vec<Option<String>> = out
            .iter()
            .map(|e| match e {
                SttEvent::Transcript { language, .. } => {
                    language.as_ref().map(|l| l.code.to_string())
                }
                SttEvent::Error(_) => None,
            })
            .collect();
        assert_eq!(languages, [None, Some("tr".to_owned())]);
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

    fn run(a: &mut Assembler, r: Results) -> Vec<Flat> {
        run_covering(a, r, false)
    }

    fn run_covering(a: &mut Assembler, r: Results, covers: bool) -> Vec<Flat> {
        let mut out = Vec::new();
        a.on_results(&r, covers, &mut out);
        flat(&out)
    }

    #[test]
    fn interims_replace_and_finals_accumulate_until_speech_final() {
        let mut a = Assembler::default();
        assert_eq!(
            run(&mut a, res("merhaba", false, false, false)),
            [t("merhaba", false, false, false)]
        );
        assert_eq!(
            run(&mut a, res("merhaba nasılsın", true, false, false)),
            [t("merhaba nasılsın", true, false, false)]
        );
        assert_eq!(
            run(&mut a, res("iyiyim", false, false, false)),
            [t("iyiyim", false, false, false)]
        );
        assert_eq!(
            run(&mut a, res("iyiyim", true, true, false)),
            [t("iyiyim", true, true, false)]
        );
    }

    #[test]
    fn blank_interims_and_blank_finals_without_a_waiter_are_dropped() {
        let mut a = Assembler::default();
        assert!(run(&mut a, res("", false, false, false)).is_empty());
        assert!(run(&mut a, res("", true, false, false)).is_empty());
        assert!(run(&mut a, res("", true, true, false)).is_empty());
    }

    #[test]
    fn a_blank_speech_final_closes_an_open_utterance() {
        let mut a = Assembler::default();
        run(&mut a, res("one", true, false, false));
        assert_eq!(
            run(&mut a, res("", true, true, false)),
            [t("", true, true, false)]
        );
        // And only once.
        assert!(run(&mut a, res("", true, true, false)).is_empty());
    }

    #[test]
    fn from_finalize_results_always_end_the_utterance() {
        let mut a = Assembler::default();
        a.finalize_requested();
        assert_eq!(
            run(&mut a, res("hello there", true, false, true)),
            [t("hello there", true, true, true)]
        );
        assert!(!a.finalize_pending());
        // An empty from_finalize answer is still an answer.
        a.finalize_requested();
        assert_eq!(
            run(&mut a, res("", true, false, true)),
            [t("", true, true, true)]
        );
        assert!(!a.finalize_pending());
    }

    #[test]
    fn an_endpointed_final_during_a_finalize_does_not_answer_it() {
        // "hello" is endpointed at the moment the Finalize goes out; the Finalize's own answer
        // ("world", the audio after the endpoint) must still be recognised as the answer.
        let mut a = Assembler::default();
        a.finalize_requested();
        assert_eq!(
            run(&mut a, res("hello", true, true, false)),
            [t("hello", true, true, false)]
        );
        assert!(a.finalize_pending(), "still owed its own answer");
        assert_eq!(
            run(&mut a, res("world", true, false, true)),
            [t("world", true, true, true)]
        );
        assert!(!a.finalize_pending());
    }

    #[test]
    fn an_endpointed_final_that_covers_the_finalize_position_answers_it() {
        let mut a = Assembler::default();
        a.finalize_requested();
        assert_eq!(
            run_covering(&mut a, res("done", true, true, false), true),
            [t("done", true, true, true)]
        );
        assert!(!a.finalize_pending());
        // A segment final (no speech_final) that covers it also ends the utterance.
        let mut b = Assembler::default();
        b.finalize_requested();
        assert_eq!(
            run_covering(&mut b, res("seg", true, false, false), true),
            [t("seg", true, true, true)]
        );
        // Blank cover: the empty answer.
        let mut c = Assembler::default();
        c.finalize_requested();
        assert_eq!(
            run_covering(&mut c, res("", true, false, false), true),
            [t("", true, true, true)]
        );
    }

    #[test]
    fn a_blank_endpointed_final_during_a_finalize_is_not_its_answer() {
        let mut a = Assembler::default();
        a.finalize_requested();
        assert!(run(&mut a, res("", true, true, false)).is_empty());
        assert!(a.finalize_pending());
    }

    #[test]
    fn a_late_from_finalize_answer_after_the_synthetic_final_is_deduplicated() {
        let mut a = Assembler::default();
        run(&mut a, res("merhaba", false, false, false));
        a.finalize_requested();
        let mut out = Vec::new();
        a.finalize_timed_out(&mut out);
        assert_eq!(flat(&out), [t("merhaba", true, true, true)]);
        // Same words (formatting differs): the server's copy of the synthetic final.
        assert!(run(&mut a, res("Merhaba.", true, false, true)).is_empty());
        // The record is spent: a later identical answer is real text again.
        assert_eq!(
            run(&mut a, res("merhaba", true, false, true)),
            [t("merhaba", true, true, true)]
        );
    }

    #[test]
    fn turkish_casing_does_not_defeat_the_duplicate_check() {
        // The interim is lowercase with a dotless i; smart_format capitalises the final with a
        // plain `I`, or a dotted `İ`.
        for (synthetic, late) in [
            ("ışık geldi", "Işık geldi."),
            ("iyiyim", "İyiyim."),
            ("İYİYİM", "iyiyim"),
            ("IŞIK GELDİ", "ışık geldi."),
        ] {
            let mut a = Assembler::default();
            run(&mut a, res(synthetic, false, false, false));
            a.finalize_requested();
            let mut out = Vec::new();
            a.finalize_timed_out(&mut out);
            assert_eq!(flat(&out), [t(synthetic, true, true, true)]);
            assert!(
                run(&mut a, res(late, true, true, true)).is_empty(),
                "{synthetic} vs {late}"
            );
        }
        // Different words stay different.
        assert!(!same_text("ışık", "ısık geldi"));
        assert!(!same_text("kırk", "kirk bir"));
        assert!(!same_text("", ""));
    }

    #[test]
    fn a_late_answer_with_new_text_is_delivered_and_a_late_blank_one_is_not() {
        let mut a = Assembler::default();
        a.finalize_requested();
        let mut out = Vec::new();
        a.finalize_timed_out(&mut out);
        assert_eq!(flat(&out), [t("", true, true, true)]);
        assert!(run(&mut a, res("", true, false, true)).is_empty());
        assert_eq!(
            run(&mut a, res("hello world", true, false, true)),
            [t("hello world", true, true, true)]
        );
        // A real final that repeats the synthetic text is a duplicate too (the Finalize was
        // ignored and endpointing fired later).
        let mut b = Assembler::default();
        run(&mut b, res("evet", false, false, false));
        b.finalize_requested();
        let mut out = Vec::new();
        b.finalize_timed_out(&mut out);
        assert!(run(&mut b, res("evet", true, true, false)).is_empty());
        // New speech (an interim) ends the dedupe window.
        let mut c = Assembler::default();
        run(&mut c, res("evet", false, false, false));
        c.finalize_requested();
        let mut out = Vec::new();
        c.finalize_timed_out(&mut out);
        run(&mut c, res("e", false, false, false));
        assert_eq!(
            run(&mut c, res("evet", true, true, false)),
            [t("evet", true, true, false)]
        );
    }

    #[test]
    fn a_non_final_speech_final_closes_open_finals_or_is_remembered() {
        // Reading 1: finals came earlier; the flag closes the utterance.
        let mut a = Assembler::default();
        run(&mut a, res("part one", true, false, false));
        assert_eq!(
            run(&mut a, res("", false, true, false)),
            [t("", true, true, false)]
        );
        // Reading 2: the final follows; it inherits the flag.
        let mut b = Assembler::default();
        assert!(run(&mut b, res("", false, true, false)).is_empty());
        assert_eq!(
            run(&mut b, res("the text", true, false, false)),
            [t("the text", true, true, false)]
        );
        // The flag is spent.
        assert_eq!(
            run(&mut b, res("next", true, false, false)),
            [t("next", true, false, false)]
        );
    }

    #[test]
    fn a_remembered_speech_final_expires_and_is_cleared_by_utterance_end() {
        // The flag is owed to the next INHERIT_WINDOW results only.
        let mut a = Assembler::default();
        assert!(run(&mut a, res("", false, true, false)).is_empty());
        run(&mut a, res("still talking", false, false, false));
        run(&mut a, res("still talking more", false, false, false));
        assert_eq!(
            run(&mut a, res("a later segment", true, false, false)),
            [t("a later segment", true, false, false)],
            "a stale flag ended an unrelated utterance"
        );
        // UtteranceEnd promotes the interim and clears the owed flag with it.
        let mut b = Assembler::default();
        run(&mut b, res("words", false, true, false));
        let mut out = Vec::new();
        b.on_utterance_end(1.0, &mut out);
        assert_eq!(flat(&out), [t("words", true, true, false)]);
        assert_eq!(
            run(&mut b, res("next utterance", true, false, false)),
            [t("next utterance", true, false, false)]
        );
        // Also when nothing was open.
        let mut c = Assembler::default();
        run(&mut c, res("", false, true, false));
        let mut out = Vec::new();
        c.on_utterance_end(1.0, &mut out);
        assert!(out.is_empty());
        assert!(c.is_idle());
    }

    #[test]
    fn a_non_final_result_with_text_and_speech_final_never_becomes_final_text() {
        let mut a = Assembler::default();
        let out = run(&mut a, res("looks final", false, true, false));
        assert_eq!(out, [t("looks final", false, false, false)]);
    }

    #[test]
    fn utterance_end_promotes_the_last_interim_or_closes_open_finals() {
        let mut a = Assembler::default();
        run(&mut a, res("still talking", false, false, false));
        let mut out = Vec::new();
        a.on_utterance_end(-1.0, &mut out);
        assert!(out.is_empty(), "-1 means already finalized");
        a.on_utterance_end(2.5, &mut out);
        assert_eq!(flat(&out), [t("still talking", true, true, false)]);

        let mut b = Assembler::default();
        run(&mut b, res("seg", true, false, false));
        let mut out = Vec::new();
        b.on_utterance_end(1.0, &mut out);
        assert_eq!(flat(&out), [t("", true, true, false)]);

        let mut c = Assembler::default();
        let mut out = Vec::new();
        c.on_utterance_end(1.0, &mut out);
        assert!(out.is_empty(), "nothing open, nothing to close");
    }

    #[test]
    fn an_unanswered_finalize_ends_with_the_last_interim_or_empty_text() {
        let mut a = Assembler::default();
        run(&mut a, res("almost", false, false, false));
        a.finalize_requested();
        let mut out = Vec::new();
        a.finalize_timed_out(&mut out);
        assert_eq!(flat(&out), [t("almost", true, true, true)]);
        assert!(!a.finalize_pending());

        let mut b = Assembler::default();
        b.finalize_requested();
        let mut out = Vec::new();
        b.finalize_timed_out(&mut out);
        assert_eq!(flat(&out), [t("", true, true, true)]);
    }

    #[test]
    fn idleness_and_the_settled_answer() {
        let mut a = Assembler::default();
        assert!(a.is_idle());
        run(&mut a, res("talking", false, false, false));
        assert!(!a.is_idle());
        run(&mut a, res("talking", true, true, false));
        assert!(a.is_idle());
        let mut out = Vec::new();
        a.finalize_settled(&mut out);
        assert_eq!(flat(&out), [t("", true, true, true)]);
        assert!(a.is_idle());
    }

    #[test]
    fn a_replaced_connection_forgets_the_interim_but_not_the_finals() {
        let mut a = Assembler::default();
        run(&mut a, res("seg", true, false, false));
        run(&mut a, res("lost hypothesis", false, false, false));
        a.connection_replaced();
        let mut out = Vec::new();
        a.on_utterance_end(1.0, &mut out);
        assert_eq!(flat(&out), [t("", true, true, false)]);
    }
}
