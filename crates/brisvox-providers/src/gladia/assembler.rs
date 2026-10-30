//! Turns Gladia `transcript` and speech messages into [`SttEvent`]s and answers `Finalize`.
//!
//! Mapping (see `docs/providers/gladia.md`, "Result semantics" and "Finalize"):
//! - `is_final=false` is a partial. Partials replace each other (a newer partial replaces the
//!   provisional text of the utterance), so each is forwarded as an interim. Blank partials are
//!   not forwarded.
//! - `is_final=true` is the fixed text of one utterance, closed by Gladia's endpointing. It is
//!   forwarded as a final with `speech_final` set: one Gladia final is one utterance, and the
//!   engine's own turn logic decides whether a pause ended the turn.
//! - Gladia has no flush command, so a [`Finalize`](crate::stt::SttInput::Finalize) is answered
//!   by the adapter: with Gladia's own final of the open utterance if that arrives within the
//!   finalize timeout (`from_finalize`, `speech_final`), else by a synthetic final carrying the
//!   last partial (as for Deepgram). When no utterance is open and the
//!   session has already said something (so what the caller said before has been dealt with) the
//!   answer is an empty final at once. The request waits like an open utterance instead when
//!   speech is under way (`speech_start` without a final since), or when audio the server has not
//!   answered yet may hold speech: on a session that has said nothing yet, and on any session
//!   while the caller's audio is being replayed after a reconnect ([`InFlight`]). Partials and
//!   speech events lag the audio, so "nothing seen" does not yet mean "nothing said" there. On a
//!   live session that has spoken before, a short utterance whose first message has not arrived
//!   when the request comes is not waited for: the engine sends a `Finalize` per VAD stop and the
//!   provider's own final normally arrives before it, so waiting on every request would put
//!   `finalize_timeout` on every turn; a final that does arrive late is delivered as an ordinary
//!   (endpointing) final. The real final of an utterance that was ended synthetically is
//!   dropped (the turn has moved on; delivering corrected text would duplicate words).
//! - Every `Finalize` is answered exactly once (the engine's ledger holds one entry per request
//!   and pops one per `from_finalize` final). A request that arrives while another waits is
//!   counted, not dropped: whatever answers the waiting one (the real final, the timeout) is
//!   followed by one empty `from_finalize` final per extra request. At most
//!   [`MAX_PENDING_FINALIZES`] requests are counted; the engine never has more outstanding
//!   (its ledger is bounded at 32), a request beyond that is ignored.
//! - Utterance ids are scoped to a session: a reconnect forgets them.

use std::sync::Arc;

use super::protocol::Transcript;
use crate::stt::{SpokenLanguage, SttEvent};

/// Most `Finalize` requests counted while one waits. Equals the engine's default
/// `finalize_capacity` (32), so a caller that respects its ledger bound never exceeds it; it
/// keeps the burst of empty answers bounded.
pub(crate) const MAX_PENDING_FINALIZES: u32 = 32;

/// The utterance whose partials have been seen and whose final has not.
#[derive(Debug)]
struct Open {
    /// Utterance id; `None` after a reconnect (ids belong to the old session), when the first
    /// final of the new session is taken to be its final.
    id: Option<String>,
    /// Latest partial text (non-blank).
    text: String,
}

/// What the session knows about audio that the server has not answered yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InFlight {
    /// The replay ring holds audio that has not become a final transcript yet.
    pub(crate) audio_unfinalized: bool,
    /// The session is being replaced: the unfinalized audio has not even reached a server yet.
    pub(crate) reconnecting: bool,
}

/// How a `Finalize` request was handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalizeStart {
    /// An answer is awaited: the open utterance's final, or the timeout.
    Waiting,
    /// Nothing was open: the empty answer was produced at once.
    Answered,
    /// A request was already open; this one was counted and is answered together with it.
    AlreadyPending,
}

/// State of the utterance being transcribed.
#[derive(Debug, Default)]
pub(crate) struct Assembler {
    open: Option<Open>,
    /// `speech_start` seen and no final since: speech whose partial or final may be on its way.
    /// A final clears it (the order of `speech_end` and the final is not documented), and so
    /// does giving up on a `Finalize`.
    speaking: bool,
    /// The session has sent a transcript or a speech event (or a `Finalize` was given up on):
    /// from then on "nothing open" really means nothing is in flight.
    heard: bool,
    /// Number of `Finalize` requests waiting for an answer (0 when none); the first is the one
    /// the deadline and the open utterance belong to, the rest are answered with empty finals
    /// right after it.
    pending: u32,
    /// Id of the utterance that was ended synthetically: its remaining messages are dropped.
    closed: Option<String>,
}

impl Assembler {
    /// Whether a `Finalize` is waiting for its answer.
    pub(crate) fn finalize_pending(&self) -> bool {
        self.pending > 0
    }

    /// Takes the waiting requests; returns how many extra ones (beyond the first) each need an
    /// empty answer after the first one's.
    fn take_extra_pending(&mut self) -> u32 {
        std::mem::take(&mut self.pending).saturating_sub(1)
    }

    /// `speech_start`.
    pub(crate) fn speech_start(&mut self) {
        self.speaking = true;
        self.heard = true;
    }

    /// `speech_end`.
    pub(crate) fn speech_end(&mut self) {
        self.speaking = false;
        self.heard = true;
    }

    /// The connection was replaced by a new session: utterance ids and speech state are gone, the
    /// last partial text stays (a pending `Finalize` may still need it).
    pub(crate) fn connection_replaced(&mut self) {
        if let Some(open) = &mut self.open {
            open.id = None;
        }
        self.closed = None;
        self.speaking = false;
        self.heard = false;
    }

    /// The caller asked to finalize the utterance now.
    pub(crate) fn finalize_requested(
        &mut self,
        in_flight: InFlight,
        out: &mut Vec<SttEvent>,
    ) -> FinalizeStart {
        if self.pending > 0 {
            if self.pending < MAX_PENDING_FINALIZES {
                self.pending += 1;
            }
            return FinalizeStart::AlreadyPending;
        }
        let unconfirmed = in_flight.audio_unfinalized && (in_flight.reconnecting || !self.heard);
        if self.open.is_some() || self.speaking || unconfirmed {
            self.pending = 1;
            return FinalizeStart::Waiting;
        }
        out.push(transcript("", true, true, true));
        FinalizeStart::Answered
    }

    /// Handles one `transcript` message.
    pub(crate) fn on_transcript(&mut self, t: &Transcript, out: &mut Vec<SttEvent>) {
        self.heard = true;
        if t.is_final {
            self.speaking = false;
        }
        if self.closed.as_deref() == Some(t.id.as_str()) {
            // The rest of an utterance that already ended synthetically.
            if t.is_final {
                self.closed = None;
            }
            return;
        }
        self.closed = None;
        if !t.is_final {
            if !t.text.is_empty() {
                self.open = Some(Open {
                    id: Some(t.id.clone()),
                    text: t.text.clone(),
                });
                out.push(transcript(&t.text, false, false, false));
            }
            return;
        }
        let belongs_to_open = self
            .open
            .as_ref()
            .is_some_and(|o| o.id.as_deref().is_none_or(|id| id == t.id));
        if belongs_to_open {
            self.open = None;
        }
        // A final answers a waiting Finalize when it is the final of the open utterance, or when
        // no utterance is open (speech whose partial never arrived, or a reconnect).
        let answers = self.pending > 0 && (belongs_to_open || self.open.is_none());
        let extra = if answers {
            self.take_extra_pending()
        } else {
            0
        };
        if t.text.is_empty() {
            // Blank finals matter only to a waiting Finalize.
            if answers {
                out.push(transcript("", true, true, true));
                push_extra_answers(extra, out);
            }
            return;
        }
        let mut event = transcript(&t.text, true, true, answers);
        if let SttEvent::Transcript { language, .. } = &mut event {
            *language = t.language.as_deref().map(SpokenLanguage::new);
        }
        out.push(event);
        push_extra_answers(extra, out);
    }

    /// The `Finalize` got no answer in time: ends the utterance with the last partial (or empty
    /// text) so the turn machine never waits for a final that may not come.
    pub(crate) fn finalize_timed_out(&mut self, out: &mut Vec<SttEvent>) {
        if self.pending == 0 {
            return;
        }
        let extra = self.take_extra_pending();
        self.heard = true;
        self.speaking = false;
        let (id, text) = match self.open.take() {
            Some(open) => (open.id, open.text),
            None => (None, String::new()),
        };
        self.closed = id;
        out.push(transcript(&text, true, true, true));
        push_extra_answers(extra, out);
    }
}

/// One empty `from_finalize` final for each extra `Finalize` that waited with the first.
fn push_extra_answers(extra: u32, out: &mut Vec<SttEvent>) {
    for _ in 0..extra {
        out.push(transcript("", true, true, true));
    }
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

    fn msg(id: &str, text: &str, is_final: bool) -> Transcript {
        Transcript {
            id: id.to_owned(),
            is_final,
            text: text.to_owned(),
            end: 1.0,
            language: None,
        }
    }

    #[test]
    fn the_utterance_language_goes_to_the_final_only() {
        let mut a = Assembler::default();
        let mut out = Vec::new();
        let mut partial = msg("u1", "merhaba", false);
        partial.language = Some("tr".to_owned());
        a.on_transcript(&partial, &mut out);
        let mut done = msg("u1", "merhaba dünya", true);
        done.language = Some("tr".to_owned());
        a.on_transcript(&done, &mut out);
        let found: Vec<Option<String>> = out
            .iter()
            .map(|e| match e {
                SttEvent::Transcript { language, .. } => {
                    language.as_ref().map(|l| l.code.to_string())
                }
                SttEvent::Error(_) => None,
            })
            .collect();
        assert_eq!(found, [None, Some("tr".to_owned())]);
    }

    fn run(a: &mut Assembler, m: Transcript) -> Vec<Flat> {
        let mut out = Vec::new();
        a.on_transcript(&m, &mut out);
        flat(&out)
    }

    /// A `Finalize` with unfinalized audio in the ring on a connected session.
    fn finalize(a: &mut Assembler) -> (FinalizeStart, Vec<Flat>) {
        finalize_with(
            a,
            InFlight {
                audio_unfinalized: true,
                reconnecting: false,
            },
        )
    }

    fn finalize_with(a: &mut Assembler, in_flight: InFlight) -> (FinalizeStart, Vec<Flat>) {
        let mut out = Vec::new();
        let start = a.finalize_requested(in_flight, &mut out);
        (start, flat(&out))
    }

    const IDLE: InFlight = InFlight {
        audio_unfinalized: false,
        reconnecting: false,
    };

    fn timeout(a: &mut Assembler) -> Vec<Flat> {
        let mut out = Vec::new();
        a.finalize_timed_out(&mut out);
        flat(&out)
    }

    #[test]
    fn partials_replace_and_the_final_is_an_utterance_end() {
        let mut a = Assembler::default();
        assert_eq!(
            run(&mut a, msg("1", "merhaba", false)),
            [t("merhaba", false, false, false)]
        );
        assert_eq!(
            run(&mut a, msg("1", "merhaba nasılsın", false)),
            [t("merhaba nasılsın", false, false, false)]
        );
        assert_eq!(
            run(&mut a, msg("1", "Merhaba, nasılsın?", true)),
            [t("Merhaba, nasılsın?", true, true, false)]
        );
        // A final needs no partial before it.
        assert_eq!(
            run(&mut a, msg("2", "iyiyim", true)),
            [t("iyiyim", true, true, false)]
        );
    }

    #[test]
    fn blank_partials_and_blank_finals_without_a_waiter_are_dropped() {
        let mut a = Assembler::default();
        assert!(run(&mut a, msg("1", "", false)).is_empty());
        assert!(run(&mut a, msg("1", "", true)).is_empty());
    }

    #[test]
    fn a_finalize_is_answered_by_the_final_of_the_open_utterance() {
        let mut a = Assembler::default();
        run(&mut a, msg("1", "merhaba", false));
        assert_eq!(finalize(&mut a), (FinalizeStart::Waiting, vec![]));
        assert_eq!(finalize(&mut a).0, FinalizeStart::AlreadyPending);
        // The second request is answered too, right after the first.
        assert_eq!(
            run(&mut a, msg("1", "Merhaba.", true)),
            [t("Merhaba.", true, true, true), t("", true, true, true)]
        );
        assert!(!a.finalize_pending());
        // The timeout later finds nothing to do.
        assert!(timeout(&mut a).is_empty());
    }

    #[test]
    fn every_finalize_is_answered_exactly_once_by_the_final_the_timeout_or_a_blank_final() {
        // By the open utterance's final.
        let mut a = Assembler::default();
        run(&mut a, msg("1", "bir", false));
        for _ in 0..3 {
            finalize(&mut a);
        }
        let out = run(&mut a, msg("1", "Bir.", true));
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], t("Bir.", true, true, true));
        assert!(out[1..].iter().all(|e| *e == t("", true, true, true)));
        // By the timeout.
        let mut b = Assembler::default();
        run(&mut b, msg("1", "iki", false));
        finalize(&mut b);
        finalize(&mut b);
        assert_eq!(
            timeout(&mut b),
            [t("iki", true, true, true), t("", true, true, true)]
        );
        assert!(!b.finalize_pending());
        // By a blank final.
        let mut c = Assembler::default();
        c.speech_start();
        finalize(&mut c);
        finalize(&mut c);
        assert_eq!(
            run(&mut c, msg("7", "", true)),
            [t("", true, true, true), t("", true, true, true)]
        );
        // Across a reconnect the count survives.
        let mut d = Assembler::default();
        run(&mut d, msg("1", "uc", false));
        finalize(&mut d);
        finalize(&mut d);
        d.connection_replaced();
        assert_eq!(
            run(&mut d, msg("0", "Üç.", true)),
            [t("Üç.", true, true, true), t("", true, true, true)]
        );
        // A later request starts a fresh count.
        run(&mut d, msg("1", "dort", false));
        assert_eq!(finalize(&mut d).0, FinalizeStart::Waiting);
        assert_eq!(timeout(&mut d), [t("dort", true, true, true)]);
    }

    #[test]
    fn the_count_of_waiting_finalizes_is_bounded() {
        let mut a = Assembler::default();
        run(&mut a, msg("1", "bir", false));
        for _ in 0..(MAX_PENDING_FINALIZES * 4) {
            finalize(&mut a);
        }
        let out = timeout(&mut a);
        assert_eq!(out.len(), MAX_PENDING_FINALIZES as usize);
        assert!(out.iter().all(|e| e.3 && e.2 && e.1));
    }

    #[test]
    fn a_stale_final_does_not_answer_a_finalize_for_a_newer_utterance() {
        let mut a = Assembler::default();
        run(&mut a, msg("1", "bir", false));
        run(&mut a, msg("2", "iki", false));
        finalize(&mut a);
        assert_eq!(
            run(&mut a, msg("1", "Bir.", true)),
            [t("Bir.", true, true, false)]
        );
        assert!(a.finalize_pending());
        assert_eq!(
            run(&mut a, msg("2", "İki.", true)),
            [t("İki.", true, true, true)]
        );
    }

    #[test]
    fn a_finalize_with_nothing_open_is_answered_at_once_with_an_empty_final() {
        let mut a = Assembler::default();
        // A session that has said nothing yet may still have speech on its way: wait.
        assert_eq!(finalize(&mut a), (FinalizeStart::Waiting, vec![]));
        assert_eq!(timeout(&mut a), [t("", true, true, true)]);
        // From then on nothing open means nothing in flight.
        assert_eq!(
            finalize(&mut a),
            (FinalizeStart::Answered, vec![t("", true, true, true)])
        );
        assert!(!a.finalize_pending());
        // After a final the utterance is closed, so the same holds.
        run(&mut a, msg("1", "merhaba", false));
        run(&mut a, msg("1", "Merhaba.", true));
        assert_eq!(finalize(&mut a).0, FinalizeStart::Answered);
        // A new session starts unsure again.
        a.connection_replaced();
        assert_eq!(finalize(&mut a).0, FinalizeStart::Waiting);
    }

    #[test]
    fn a_fresh_session_waits_for_the_first_speech_and_a_final_answers_it() {
        let mut a = Assembler::default();
        assert_eq!(finalize(&mut a).0, FinalizeStart::Waiting);
        assert_eq!(
            run(&mut a, msg("0", "Merhaba.", true)),
            [t("Merhaba.", true, true, true)]
        );
    }

    #[test]
    fn speech_without_a_partial_yet_makes_a_finalize_wait() {
        let mut a = Assembler::default();
        a.speech_start();
        assert_eq!(finalize(&mut a), (FinalizeStart::Waiting, vec![]));
        // Its final answers it even though no partial was seen.
        assert_eq!(
            run(&mut a, msg("7", "Selam.", true)),
            [t("Selam.", true, true, true)]
        );
        a.speech_end();
        assert_eq!(finalize(&mut a).0, FinalizeStart::Answered);
    }

    #[test]
    fn a_blank_final_answers_a_waiting_finalize_with_an_empty_final() {
        let mut a = Assembler::default();
        a.speech_start();
        finalize(&mut a);
        assert_eq!(run(&mut a, msg("7", "", true)), [t("", true, true, true)]);
    }

    #[test]
    fn the_timeout_ends_the_utterance_with_the_last_partial_and_drops_the_real_final() {
        let mut a = Assembler::default();
        run(&mut a, msg("1", "merhaba nasıl", false));
        finalize(&mut a);
        assert_eq!(timeout(&mut a), [t("merhaba nasıl", true, true, true)]);
        assert!(!a.finalize_pending());
        // Late messages of the same utterance are dropped, the final closes the drop.
        assert!(run(&mut a, msg("1", "merhaba nasılsın", false)).is_empty());
        assert!(run(&mut a, msg("1", "Merhaba nasılsın?", true)).is_empty());
        // The next utterance is delivered, and so is a re-used id once the drop is closed.
        assert_eq!(
            run(&mut a, msg("2", "iyiyim", true)),
            [t("iyiyim", true, true, false)]
        );
        assert_eq!(
            run(&mut a, msg("1", "yine", true)),
            [t("yine", true, true, false)]
        );
    }

    #[test]
    fn a_timeout_without_any_text_ends_with_an_empty_final() {
        let mut a = Assembler::default();
        a.speech_start();
        finalize(&mut a);
        assert_eq!(timeout(&mut a), [t("", true, true, true)]);
    }

    #[test]
    fn a_different_id_clears_the_drop_state() {
        let mut a = Assembler::default();
        run(&mut a, msg("1", "bir", false));
        finalize(&mut a);
        timeout(&mut a);
        assert_eq!(
            run(&mut a, msg("2", "iki", false)),
            [t("iki", false, false, false)]
        );
        // Id 1 is no longer dropped: it would be a new utterance now.
        assert_eq!(
            run(&mut a, msg("1", "Bir.", true)),
            [t("Bir.", true, true, false)]
        );
    }

    #[test]
    fn a_reconnect_forgets_ids_but_keeps_the_last_partial_for_a_pending_finalize() {
        let mut a = Assembler::default();
        run(&mut a, msg("1", "merhaba", false));
        finalize(&mut a);
        a.connection_replaced();
        // The new session numbers its utterances from the start: a final with any id answers.
        assert_eq!(
            run(&mut a, msg("00", "Merhaba.", true)),
            [t("Merhaba.", true, true, true)]
        );
        // Or the timeout falls back on the text that was heard before the drop.
        let mut b = Assembler::default();
        run(&mut b, msg("1", "merhaba", false));
        finalize(&mut b);
        b.connection_replaced();
        assert_eq!(timeout(&mut b), [t("merhaba", true, true, true)]);
        // Nothing is dropped as "closed" after a reconnect.
        assert_eq!(
            run(&mut b, msg("1", "yeni", true)),
            [t("yeni", true, true, false)]
        );
    }

    #[test]
    fn a_final_ends_the_speech_so_a_finalize_before_speech_end_is_answered_at_once() {
        let mut a = Assembler::default();
        a.speech_start();
        assert_eq!(
            run(&mut a, msg("1", "Merhaba.", true)),
            [t("Merhaba.", true, true, false)]
        );
        // The final came before speech_end: the speech is over, nothing is in flight.
        assert_eq!(
            finalize_with(&mut a, IDLE),
            (FinalizeStart::Answered, vec![t("", true, true, true)])
        );
        a.speech_end();
        assert_eq!(finalize_with(&mut a, IDLE).0, FinalizeStart::Answered);
    }

    #[test]
    fn a_missed_speech_end_does_not_make_every_later_finalize_wait() {
        let mut a = Assembler::default();
        a.speech_start();
        assert_eq!(finalize(&mut a).0, FinalizeStart::Waiting);
        assert_eq!(timeout(&mut a), [t("", true, true, true)]);
        // The first request gave up on the speech: the second finds the line idle.
        assert_eq!(finalize_with(&mut a, IDLE).0, FinalizeStart::Answered);
    }

    #[test]
    fn a_reconnect_with_nothing_unfinalized_does_not_make_a_finalize_wait() {
        let mut a = Assembler::default();
        run(&mut a, msg("1", "bir", true));
        a.connection_replaced();
        assert_eq!(finalize_with(&mut a, IDLE).0, FinalizeStart::Answered);
    }

    #[test]
    fn unfinalized_audio_that_no_server_has_answered_yet_makes_a_finalize_wait() {
        // While reconnecting: even though the old session had spoken before.
        let mut a = Assembler::default();
        run(&mut a, msg("1", "bir", true));
        let reconnecting = InFlight {
            audio_unfinalized: true,
            reconnecting: true,
        };
        assert_eq!(
            finalize_with(&mut a, reconnecting),
            (FinalizeStart::Waiting, vec![])
        );
        // The replayed audio's final answers it.
        a.connection_replaced();
        assert_eq!(
            run(&mut a, msg("0", "iki", true)),
            [t("iki", true, true, true)]
        );
        // On the new session, until it has said something, replayed audio is still unanswered.
        let mut b = Assembler::default();
        run(&mut b, msg("1", "bir", true));
        b.connection_replaced();
        assert_eq!(finalize(&mut b).0, FinalizeStart::Waiting);
        // ... and once it has, an idle line is idle.
        let mut c = Assembler::default();
        c.connection_replaced();
        run(&mut c, msg("0", "iki", true));
        assert_eq!(finalize(&mut c).0, FinalizeStart::Answered);
    }

    #[test]
    fn nothing_was_ever_sent_means_nothing_is_in_flight() {
        let mut a = Assembler::default();
        assert_eq!(finalize_with(&mut a, IDLE).0, FinalizeStart::Answered);
    }
}
