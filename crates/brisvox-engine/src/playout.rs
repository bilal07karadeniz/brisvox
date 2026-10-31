//! [`PlayoutBook`]: what the bot said versus what the caller heard (`docs/PIPELINE.md` section 5).
//!
//! When the caller barges in, the LLM context must keep only the words that were actually
//! played. Four tasks each know a piece of that answer, and none of them may block on another:
//!
//! | Writer | Writes |
//! |---|---|
//! | LLM runner | the round exists; the context index of its assistant message |
//! | TTS stage | the utterance of a round; the text slices sent to TTS (verbatim) |
//! | transport-out stage | word timestamps, total audio handed to the transport, mark ids |
//! | transport pump | playback positions, mark echoes, the clear notice |
//!
//! The book is a side channel on purpose (D9): a `std::sync::Mutex` over plain data that is never
//! held across an await, written at word rate and at the transport's position rate (~10 Hz),
//! plus a `watch` that wakes a reader waiting for the transport's final position.
//!
//! # Keys
//! Rounds are keyed explicitly by [`RoundKey`] `(epoch, round)`; an utterance belongs to exactly
//! the round that registered it. Rounds are never zipped to utterances by order, because a
//! round without text has no utterance of its own, and a round's context message can come later
//! (a filler waits until the tool results are in) or never.
//!
//! # What "heard" means
//! The transport's `PlaybackPosition` is an **upper bound**: audio *released* to the wire, ahead
//! of the caller by the sink queue plus the wire queue. So
//! `heard = min(released, total_audio) - lead`, never below zero ([`RoundSnapshot::heard_position`]).
//! `lead` is the audio the client still holds: the transport's own report when it sends one
//! ([`PlayoutBook::set_client_lead`], Twilio; the report in force when a `Clear` cut the
//! utterance), else the configured `lead_margin`. The audio of later utterances in the client's
//! buffer is taken off the lead first ([`RoundSnapshot::later_released`]): an earlier utterance
//! is only as unplayed as what is left over.
//! The configured `lead_margin` assumes audio was still going to the wire when the caller barged
//! in. When it was not (a tool or filler wait followed the utterance), the client's buffer has
//! drained in real time since the last release, so the margin shrinks by that time
//! ([`RoundSnapshot::margin_at`]; the release times are stamped on the clock of
//! [`PlayoutBook::attach_clock`] when a position report moves). A stamp is taken when the report
//! arrives, at or after the real release, so the fade never takes more than what drained. The
//! drain stops at the `Clear` that cut the utterance (stamped by [`PlayoutBook::note_clear`]):
//! the fade runs up to the earlier of the clear and the runner's interruption stamp. A
//! reported client lead is a measurement and is not faded.
//! An utterance whose end mark was echoed (`marked_played`), or that a later utterance's echoed
//! mark followed, and that was not `lost`, was heard completely and is kept whole, which also
//! avoids cutting text the TTS rewrote (numbers). An echo that arrives after a `Clear` covered
//! the utterance is not trusted as "played" (a stale mark of the old epoch that reached the
//! wire after the clear is echoed at once): it counts as a cut mark and the position decides.
//! Positions only move forward: a late, older report is ignored.
//!
//! # Settling
//! After a `Clear` the transport flushes one final position per utterance (only if the position
//! moved since its last report), then reports every cut mark as `MarkCleared`; on its event
//! queue the final positions therefore precede the `MarkCleared` events. An utterance is
//! *settled* when nothing more can change its position: it never had audio, its mark was echoed
//! or cut, it was lost, all of its audio was released, or (utterances without an unresolved
//! mark only) a position arrived after the clear. [`PlayoutBook::wait_settled_through`] waits
//! for that, bounded by the injected clock, and counts a timeout. It ignores utterances of
//! rounds already cut to what was heard ([`PlayoutBook::mark_applied`]) and cut utterances of
//! rounds without a context message (which nothing is ever cut to and `mark_applied` never
//! reaches): their position can no longer matter, so a never-flushed
//! old utterance cannot add a full settle wait to every later turn (nor keep the runner's
//! speculation off, which asks the same question). Not waiting for such an utterance's final
//! position only makes the chain of an earlier utterance more cautious: a stale, lower
//! `released` behind it takes less off its lead.
//!
//! ## Ordering contract of [`PlayoutBook::note_clear`]
//! The TurnDriver calls `note_clear(new_epoch)` when it issues the `Clear` command, which is
//! *before* the transport has applied it. The pump may still hold periodic (~10 Hz) position
//! reports sent before the flush. A position after `note_clear` is therefore not provably the
//! final one. The book removes the ambiguity where the protocol allows it: while an utterance's
//! end mark is registered and unresolved, only `MarkCleared` or `MarkPlayed` (which follow the
//! final position on the same ordered queue) settle it, never a position. That is the common
//! barge-in case (the text was fully sent, the mark queued).
//!
//! **Known limitation:** a barge-in in the middle of an utterance has no mark yet, and the
//! transport emits no clear acknowledgement, so there the first position after `note_clear` is
//! taken as final. If that was a stale periodic report, the cut uses a position up to one report
//! interval (about 100 ms, roughly a word) too low and the truncation may drop a word the caller
//! heard. A clear acknowledgement from the transport would close this; the test
//! `known_limitation_a_stale_report_settles_a_markless_utterance` records the behaviour.
//!
//! The opposite also happens: the transport flushes a final position only if audio was released
//! since its last report, so a clear that follows a report immediately has none to send (with a
//! report per frame, every barge-in between two frames; with a 100 ms interval, about one in
//! five). The wait then runs out and the last report is used, which is the final position. The
//! reader measures the wait from the barge-in, so by the time the turn ends it costs no time
//! ([`PlayoutTruncation`](crate::PlayoutTruncation)); the count in
//! [`BookStats::settle_timeouts`] is therefore not a failure signal by itself.
//!
//! # Bounds
//! At most `capacity` rounds are kept; registering a new one evicts the oldest (with its
//! utterance and marks), and waiters are woken so none sleeps on a removed utterance. A round
//! owns at most one utterance and an utterance id belongs to exactly one round: a second
//! registration that would break either is ignored and counted
//! ([`BookStats::conflicting_registrations`]), so nothing can be orphaned. Updates for evicted
//! or unknown utterances are ignored and counted.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use brisvox_agent::tools::ClipKey;
use brisvox_agent::{PlayedConfig, PlayedText, Truncation};
use brisvox_core::{Clock, SharedClock, TurnEpoch};
use brisvox_transport::{CommandSender, TransportCommand};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::turn_driver::ClearHook;

/// Rounds kept by [`PlayoutBook::default`]: a few dozen turns of a long call are plenty, since
/// only rounds not yet cut to what was heard matter and those are the most recent ones.
pub const DEFAULT_BOOK_CAPACITY: usize = 64;

/// Identifies one LLM round of one turn: `epoch` is the turn's epoch, `round` the ordinal of the
/// round within it (a tool loop has several). Orders by epoch, then round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoundKey {
    /// Epoch of the turn.
    pub epoch: TurnEpoch,
    /// Round ordinal within the turn.
    pub round: u32,
}

impl RoundKey {
    /// A key from its parts.
    pub const fn new(epoch: TurnEpoch, round: u32) -> Self {
        Self { epoch, round }
    }
}

/// Counters of the book, for metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BookStats {
    /// Rounds evicted to stay within the capacity.
    pub evicted_rounds: u64,
    /// Of those, rounds that still had a context message and were not yet cut to the heard text.
    pub evicted_unapplied: u64,
    /// Updates for an utterance or mark the book does not know (never registered, evicted,
    /// already resolved) or that belong to a different epoch than registered.
    pub unknown_updates: u64,
    /// Waits for the final position that ended by timeout.
    pub settle_timeouts: u64,
    /// `register_utterance` calls ignored because the round already had a different utterance
    /// or the utterance id already belonged to another round (the first mapping is kept).
    pub conflicting_registrations: u64,
    /// Calls of [`PlayoutBook::note_clear`]. Zero while barge-ins happen means the turn driver
    /// was not given [`clear_hook`] (see [`PlayoutTruncation::unnoted_clear_cuts`](crate::PlayoutTruncation::unnoted_clear_cuts)).
    pub clears_noted: u64,
}

/// A transport mark matched to the utterance it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedMark {
    /// The utterance the mark ends.
    pub utterance: u64,
    /// The epoch that utterance was registered in.
    pub epoch: TurnEpoch,
}

#[derive(Debug)]
struct Round {
    msg_index: Option<usize>,
    utterance: Option<u64>,
    applied: bool,
    /// The pre-rendered clip the round is to be played with (a filler).
    clip: Option<ClipKey>,
}

#[derive(Debug)]
struct Utterance {
    key: RoundKey,
    played: PlayedText,
    released: Duration,
    /// When `released` last moved forward, on the book's clock (`None` while no clock is
    /// attached, see [`PlayoutBook::attach_clock`]).
    released_at: Option<Instant>,
    total_audio: Duration,
    marked_played: bool,
    mark_cleared: bool,
    lost: bool,
    cleared: bool,
    /// Which clear first covered this utterance, and the client's playback lead at that moment
    /// (`None` when the transport reports no lead).
    clear: Option<ClearNote>,
    final_position: bool,
    /// Registered end marks not yet echoed or cut.
    marks_pending: u32,
}

/// One clear as seen by the book: its number, the client's lead when it was issued and when
/// (on the book's clock; `None` without one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClearNote {
    seq: u64,
    lead: Option<Duration>,
    at: Option<Instant>,
}

impl Utterance {
    fn new(key: RoundKey) -> Self {
        Self {
            key,
            played: PlayedText::new(),
            released: Duration::ZERO,
            released_at: None,
            total_audio: Duration::ZERO,
            marked_played: false,
            mark_cleared: false,
            lost: false,
            cleared: false,
            clear: None,
            final_position: false,
            marks_pending: 0,
        }
    }

    fn settled(&self) -> bool {
        self.total_audio.is_zero()
            || self.marked_played
            || self.mark_cleared
            || self.lost
            || self.final_position
            || self.released >= self.total_audio
    }
}

#[derive(Debug, Default)]
struct State {
    rounds: BTreeMap<RoundKey, Round>,
    utterances: HashMap<u64, Utterance>,
    /// Mark id to utterance, until the mark is echoed or cut.
    marks: HashMap<String, u64>,
    /// The client's latest report of audio it holds unplayed (`TransportEvent::PlaybackLead`),
    /// for transports that send one (Twilio). Reset by a clear.
    client_lead: Option<Duration>,
    /// Number of clears that covered a new utterance so far.
    clears: u64,
    stats: BookStats,
}

impl State {
    /// Inserts the round if needed and evicts the oldest rounds beyond `capacity`. `None` when
    /// the new round is itself the oldest and was evicted at once (a round older than every
    /// kept one registering late).
    fn ensure_round(&mut self, key: RoundKey, capacity: usize) -> Option<&mut Round> {
        if let Entry::Vacant(vacant) = self.rounds.entry(key) {
            vacant.insert(Round {
                msg_index: None,
                utterance: None,
                applied: false,
                clip: None,
            });
            while self.rounds.len() > capacity {
                let Some((old_key, old)) = self.rounds.pop_first() else {
                    break;
                };
                self.stats.evicted_rounds += 1;
                if old.msg_index.is_some() && !old.applied {
                    self.stats.evicted_unapplied += 1;
                }
                if let Some(utterance) = old.utterance {
                    self.utterances.remove(&utterance);
                    self.marks.retain(|_, owner| *owner != utterance);
                }
                tracing::debug!(
                    target: "brisvox::engine",
                    epoch = old_key.epoch.get(),
                    round = old_key.round,
                    "playout book evicted a round"
                );
            }
        }
        self.rounds.get_mut(&key)
    }

    fn utterance_mut(&mut self, utterance: u64) -> Option<&mut Utterance> {
        let found = self.utterances.get_mut(&utterance);
        if found.is_none() {
            self.stats.unknown_updates += 1;
        }
        found
    }

    /// What sits behind `utterance` in the client's buffer, see [`RoundSnapshot::later_released`]:
    /// the released audio of the utterances after it that were not yet played to their end and
    /// belong to the same clear (or to none).
    fn later_released(&self, utterance: &Utterance) -> Duration {
        self.behind(utterance)
            .map(|u| u.released.min(u.total_audio))
            .fold(Duration::ZERO, Duration::saturating_add)
    }

    /// The utterances sitting behind `utterance` in the client's buffer (see
    /// [`Self::later_released`]).
    fn behind<'a>(&'a self, utterance: &Utterance) -> impl Iterator<Item = &'a Utterance> + 'a {
        let group = utterance.clear.map(|c| c.seq);
        let key = utterance.key;
        self.utterances
            .values()
            .filter(move |u| u.key > key && !u.marked_played)
            .filter(move |u| u.clear.map(|c| c.seq) == group)
    }

    /// When audio of `utterance` or of an utterance behind it last went to the wire: the client's
    /// buffer was not refilled after that.
    fn last_release_at(&self, utterance: &Utterance) -> Option<Instant> {
        self.behind(utterance)
            .filter_map(|u| u.released_at)
            .chain(utterance.released_at)
            .max()
    }

    /// Whether an utterance after `utterance` in the same epoch was played to its end: the
    /// client had played this one completely by then. Only the same epoch counts: a clear
    /// separates epochs, so an utterance cut mid-way by a clear must not be reported whole
    /// because the answer of a later turn was played and acked.
    fn followed_by_played(&self, utterance: &Utterance) -> bool {
        self.utterances.values().any(|u| {
            u.key > utterance.key
                && u.key.epoch == utterance.key.epoch
                && u.marked_played
                && !u.lost
        })
    }

    /// Whether every utterance of epochs up to `epoch` that can still matter is settled. The
    /// utterances that cannot are skipped: those of rounds already cut to what was heard, and
    /// cut ones of rounds without a context message. Nothing is
    /// ever cut to their position and `mark_applied` never reaches them, so a final position
    /// that was never flushed would otherwise hold up every later wait for the rest of the call.
    fn settled_through(&self, epoch: TurnEpoch) -> bool {
        self.utterances
            .values()
            .filter(|u| u.key.epoch <= epoch)
            .filter(|u| {
                self.rounds
                    .get(&u.key)
                    .is_none_or(|round| !round.applied && !(u.cleared && round.msg_index.is_none()))
            })
            .all(Utterance::settled)
    }
}

/// How an utterance's text is cut, see [`RoundSnapshot::heard`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    /// The end mark was echoed and nothing was lost: the whole message was heard, keep it as
    /// sent.
    Whole,
    /// Cut to this heard prefix (apply it with `apply_truncation`).
    Cut(Truncation),
}

/// A copy of one round's record, taken under the lock and used without it.
#[derive(Debug, Clone)]
pub struct RoundSnapshot {
    /// The round.
    pub key: RoundKey,
    /// Index of the round's assistant message in the LLM context, once the runner pushed it.
    pub msg_index: Option<usize>,
    /// The round's TTS utterance, if it had text to speak.
    pub utterance: Option<u64>,
    /// The text sent to TTS and the words the TTS timed.
    pub played: PlayedText,
    /// Audio released to the wire so far (the transport's position, an upper bound of playback).
    pub released: Duration,
    /// Audio handed to the transport so far for this utterance.
    pub total_audio: Duration,
    /// The client echoed the utterance's end mark.
    pub marked_played: bool,
    /// The end mark was cut by a clear before the client echoed it.
    pub mark_cleared: bool,
    /// The TTS failed mid-utterance: audio and words after the failure are missing.
    pub lost: bool,
    /// A `Clear` covered this utterance.
    pub cleared: bool,
    /// Nothing more can change this utterance's position (see the module docs).
    pub settled: bool,
    /// The round was already cut to what was heard.
    pub applied: bool,
    /// The client's playback lead (audio it holds unplayed) when the transport reports one:
    /// the latest report, or, for an utterance a `Clear` covered, the one at that clear.
    pub client_lead: Option<Duration>,
    /// Released audio of the later utterances that sit behind this one in the client's buffer
    /// (not played to their end, same clear). The lead covers them first, so only the rest of
    /// it reaches back into this utterance.
    pub later_released: Duration,
    /// A later utterance of the same epoch was played to its end (its mark was echoed), so the
    /// client played this one completely.
    pub followed_by_played: bool,
    /// When audio of this utterance, or of one behind it in the client's buffer, last went to the
    /// wire (on the clock of [`PlayoutBook::attach_clock`]); `None` while the book has no clock
    /// or nothing was released.
    pub last_release_at: Option<Instant>,
    /// When the `Clear` that covered this utterance was issued ([`PlayoutBook::note_clear`], on
    /// the clock of [`PlayoutBook::attach_clock`]); `None` while not cleared or without a clock.
    /// The client's buffer is flushed then, so the margin fades only up to it
    /// ([`Self::margin_at`]).
    pub cleared_at: Option<Instant>,
}

impl RoundSnapshot {
    /// The audio of this utterance taken as still unplayed at the transport's position: the
    /// client's reported lead when there is one, else `lead_margin`, less what the later
    /// utterances occupy of it ([`Self::later_released`]).
    pub fn unplayed_estimate(&self, lead_margin: Duration) -> Duration {
        self.client_lead
            .unwrap_or(lead_margin)
            .saturating_sub(self.later_released)
    }

    /// The fixed `lead_margin` as it applies to a barge-in at `interrupted_at`. Audio stops going
    /// to the wire when the utterance ends (a tool call or a filler wait follows) and the
    /// client's buffer then drains in real time, so the margin shrinks by the time passed since
    /// the last release ([`Self::last_release_at`]) and is gone once that exceeds it. The drain
    /// ends at the `Clear` that cut the utterance ([`Self::cleared_at`]), which flushes the
    /// client's buffer: the fade runs up to the earlier of `interrupted_at` (the runner's
    /// stamp, which can lag the clear) and the clear. Without a release time, or with a
    /// transport-reported lead (measured, not assumed), the margin is returned as is.
    pub fn margin_at(&self, lead_margin: Duration, interrupted_at: Instant) -> Duration {
        let until = self
            .cleared_at
            .map_or(interrupted_at, |cleared| cleared.min(interrupted_at));
        match self.last_release_at {
            Some(at) if self.client_lead.is_none() => {
                lead_margin.saturating_sub(until.saturating_duration_since(at))
            }
            _ => lead_margin,
        }
    }

    /// The playback position taken as heard: `min(released, total_audio)` less
    /// [`Self::unplayed_estimate`] (the transport's reported lead, else `lead_margin`),
    /// saturating at zero.
    pub fn heard_position(&self, lead_margin: Duration) -> Duration {
        self.released
            .min(self.total_audio)
            .saturating_sub(self.unplayed_estimate(lead_margin))
    }

    /// Whether the whole message counts as heard: its end mark was echoed, or a later
    /// utterance's was, and the TTS did not fail.
    pub const fn kept_whole(&self) -> bool {
        (self.marked_played || self.followed_by_played) && !self.lost
    }

    /// What the caller heard of this round's text.
    pub fn heard(&self, lead_margin: Duration, config: &PlayedConfig) -> Heard {
        if self.kept_whole() {
            return Heard::Whole;
        }
        Heard::Cut(self.played.heard(self.heard_position(lead_margin), config))
    }
}

/// The turn driver's [`ClearHook`] that also tells `book` about the clear: calls
/// [`PlayoutBook::note_clear`] with the new epoch, then sends `TransportCommand::Clear` through
/// `commands` (the fast path). Use it instead of `TurnDriver::with_commands` wherever a
/// [`PlayoutTruncation`](crate::PlayoutTruncation) reads the book: without the note an utterance
/// cut in the middle (no end mark yet) never counts as settled and every barge-in waits out the
/// settle timeout, and the client's playback lead is not tied to the clear it belongs to.
pub fn clear_hook(commands: CommandSender, book: Arc<PlayoutBook>) -> ClearHook {
    note_then_send(
        book,
        Arc::new(move |epoch| {
            let commands = commands.clone();
            Box::pin(async move {
                commands
                    .send(TransportCommand::Clear { epoch })
                    .await
                    .map_err(|error| error.to_string())
            })
        }),
    )
}

/// The ordering of [`clear_hook`], over any way of sending the command: the book is told BEFORE
/// `send` runs, because the transport flushes its final position right after the clear and only
/// a position that comes after the note counts as the final one.
fn note_then_send(book: Arc<PlayoutBook>, send: ClearHook) -> ClearHook {
    Arc::new(move |epoch| {
        // Before the command, and before the returned future is first polled.
        book.note_clear(epoch);
        send(epoch)
    })
}

/// The shared record of sent and played speech. See the [module docs](self).
#[derive(Debug)]
pub struct PlayoutBook {
    state: Mutex<State>,
    capacity: usize,
    changed: watch::Sender<u64>,
    /// Stamps the release times; set once by [`PlayoutBook::attach_clock`].
    clock: OnceLock<SharedClock>,
}

impl Default for PlayoutBook {
    fn default() -> Self {
        Self::new(DEFAULT_BOOK_CAPACITY)
    }
}

impl PlayoutBook {
    /// A book keeping at most `capacity` rounds (at least 1 is used).
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            capacity: capacity.max(1),
            changed: watch::channel(0).0,
            clock: OnceLock::new(),
        }
    }

    /// Gives the book the call's clock, to stamp when audio last went to the wire (the first
    /// clock attached wins, later calls are ignored).
    /// [`PlayoutTruncation::new`](crate::PlayoutTruncation::new) does it, so the assembly needs
    /// no extra wiring. Without a clock the book has no release times and the fixed lead margin
    /// applies in full ([`RoundSnapshot::margin_at`]).
    pub fn attach_clock(&self, clock: SharedClock) {
        let _ = self.clock.set(clock);
    }

    /// An empty book in an `Arc`, the form the stages share.
    pub fn shared(capacity: usize) -> Arc<Self> {
        Arc::new(Self::new(capacity))
    }

    /// Maximum number of rounds kept.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Critical sections never panic, so a poisoned lock still guards consistent data.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wakes waiters in [`Self::wait_settled_through`]; called after the lock is released.
    fn notify(&self) {
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    // -- LLM runner ---------------------------------------------------------------------------

    /// Registers a round (idempotent). The runner calls it when the round starts, before any
    /// text of it is sent to TTS.
    pub fn register_round(&self, key: RoundKey) {
        let mut state = self.lock();
        let evicted = state.stats.evicted_rounds;
        let _ = state.ensure_round(key, self.capacity);
        let changed = state.stats.evicted_rounds != evicted;
        drop(state);
        if changed {
            self.notify();
        }
    }

    /// Records the index of the round's assistant message in the LLM context. Returns whether
    /// the round is known.
    pub fn set_msg_index(&self, key: RoundKey, index: usize) -> bool {
        let mut state = self.lock();
        match state.rounds.get_mut(&key) {
            Some(round) => {
                round.msg_index = Some(index);
                true
            }
            None => {
                state.stats.unknown_updates += 1;
                false
            }
        }
    }

    /// Cues round `key` (registered first if needed) to be played with the pre-rendered clip
    /// `clip` instead of its text being rendered by the TTS (a filler). The runner cues
    /// it before the round's `LlmResponseStarted` goes downstream; the TTS stage reads it when the
    /// round's first text arrives ([`Self::clip_cue`]).
    pub fn cue_clip(&self, key: RoundKey, clip: ClipKey) {
        let mut state = self.lock();
        let evicted = state.stats.evicted_rounds;
        if let Some(round) = state.ensure_round(key, self.capacity) {
            round.clip = Some(clip);
        }
        let changed = state.stats.evicted_rounds != evicted;
        drop(state);
        if changed {
            self.notify();
        }
    }

    /// The clip round `key` was cued with ([`Self::cue_clip`]).
    pub fn clip_cue(&self, key: RoundKey) -> Option<ClipKey> {
        self.lock()
            .rounds
            .get(&key)
            .and_then(|round| round.clip.clone())
    }

    /// Marks the round as cut to what was heard, so it is not cut again.
    pub fn mark_applied(&self, key: RoundKey) {
        let known = self
            .lock()
            .rounds
            .get_mut(&key)
            .map(|round| round.applied = true)
            .is_some();
        if known {
            // An applied round no longer holds up a settle wait.
            self.notify();
        }
    }

    // -- TTS stage ----------------------------------------------------------------------------

    /// Registers the TTS `utterance` of round `key` (creating the round if the runner's
    /// registration has not been seen yet). Registering the same pair again changes nothing.
    ///
    /// The first mapping always wins: a round that already has a different utterance (a TTS
    /// reconnect inside one round), or an utterance id that already belongs to another round,
    /// leaves the book untouched and is counted in [`BookStats::conflicting_registrations`].
    /// Later updates for the ignored id count as unknown. This keeps every utterance reachable
    /// from exactly one round, so eviction can always remove it.
    pub fn register_utterance(&self, key: RoundKey, utterance: u64) {
        let capacity = self.capacity;
        let mut state = self.lock();
        let evicted = state.stats.evicted_rounds;
        if state
            .utterances
            .get(&utterance)
            .is_some_and(|u| u.key != key)
        {
            state.stats.conflicting_registrations += 1;
            return;
        }
        match state
            .ensure_round(key, capacity)
            .map(|round| round.utterance)
        {
            None => state.stats.unknown_updates += 1,
            Some(Some(existing)) if existing != utterance => {
                state.stats.conflicting_registrations += 1;
            }
            Some(Some(_)) => {}
            Some(None) => {
                if let Some(round) = state.rounds.get_mut(&key) {
                    round.utterance = Some(utterance);
                }
                state
                    .utterances
                    .entry(utterance)
                    .or_insert_with(|| Utterance::new(key));
            }
        }
        let changed = state.stats.evicted_rounds != evicted;
        drop(state);
        if changed {
            self.notify();
        }
    }

    /// Appends a verbatim slice of the text sent to TTS for `utterance`. Returns whether the
    /// utterance is known.
    pub fn push_sent(&self, utterance: u64, text: &str) -> bool {
        let mut state = self.lock();
        state.utterance_mut(utterance).is_some_and(|u| {
            u.played.push_sent(text);
            true
        })
    }

    /// The text most recently sent to TTS, at most `max_chars` characters from its end: what the
    /// bot has just said or is about to say. Used to recognise the bot's own voice coming back
    /// in the caller's transcript.
    pub fn recent_sent_text(&self, max_chars: usize) -> String {
        let state = self.lock();
        let mut ids: Vec<u64> = state.utterances.keys().copied().collect();
        ids.sort_unstable();
        let mut text = String::new();
        for id in ids.iter().rev().take(2).rev() {
            if let Some(u) = state.utterances.get(id) {
                text.push_str(u.played.sent());
                text.push(' ');
            }
        }
        let text = text.trim_end();
        let skip = text.chars().count().saturating_sub(max_chars);
        text.chars().skip(skip).collect()
    }

    /// The TTS failed or was cut off without a clean end: the audio and words after this point
    /// are missing, so the whole message can no longer count as heard.
    pub fn mark_lost(&self, utterance: u64) {
        let known = self
            .lock()
            .utterance_mut(utterance)
            .map(|u| u.lost = true)
            .is_some();
        if known {
            self.notify();
        }
    }

    // -- transport-out stage ------------------------------------------------------------------

    /// Records a word the TTS timed, `start` being the offset from the utterance's first audio
    /// sample. Returns whether the utterance is known.
    pub fn push_word(&self, utterance: u64, word: impl Into<Arc<str>>, start: Duration) -> bool {
        let word = word.into();
        let mut state = self.lock();
        state.utterance_mut(utterance).is_some_and(|u| {
            u.played.push_word(word, start);
            true
        })
    }

    /// Adds `audio` to the audio handed to the transport for `utterance`. Returns whether the
    /// utterance is known.
    pub fn add_audio(&self, utterance: u64, audio: Duration) -> bool {
        let mut state = self.lock();
        state.utterance_mut(utterance).is_some_and(|u| {
            u.total_audio = u.total_audio.saturating_add(audio);
            true
        })
    }

    /// Remembers that the transport mark `id` ends `utterance`, so the pump can resolve the
    /// echo. Returns whether the utterance is known.
    pub fn register_mark(&self, id: impl Into<String>, utterance: u64) -> bool {
        let id = id.into();
        let mut state = self.lock();
        let Some(owner) = state.utterance_mut(utterance) else {
            return false;
        };
        owner.marks_pending += 1;
        if let Some(replaced) = state.marks.insert(id, utterance)
            && let Some(previous) = state.utterances.get_mut(&replaced)
        {
            // The same id registered again: the old registration is gone.
            previous.marks_pending = previous.marks_pending.saturating_sub(1);
        }
        true
    }

    // -- transport pump -----------------------------------------------------------------------

    /// The transport released `played` of `utterance` (cumulative). Positions only move
    /// forward: an older report changes nothing. A report for another epoch than the utterance
    /// was registered in is ignored (and counted). Returns whether it was accepted.
    pub fn position(&self, epoch: TurnEpoch, utterance: u64, played: Duration) -> bool {
        let now = self.clock.get().map(|clock| clock.now());
        let mut state = self.lock();
        let registered = state.utterance_mut(utterance).map(|u| u.key.epoch);
        if registered != Some(epoch) {
            if registered.is_some() {
                state.stats.unknown_updates += 1;
            }
            return false;
        }
        if let Some(u) = state.utterances.get_mut(&utterance) {
            if played > u.released {
                u.released = played;
                u.released_at = now.or(u.released_at);
            }
            // With an end mark outstanding, only its echo or cut ends the wait (the transport
            // sends the final position before `MarkCleared`); see the module docs.
            if u.cleared && u.marks_pending == 0 {
                u.final_position = true;
            }
        }
        drop(state);
        self.notify();
        true
    }

    /// The client echoed mark `id`: its utterance was played to the end. Returns the utterance,
    /// or `None` for an unknown or already resolved mark.
    pub fn mark_played(&self, id: &str) -> Option<u64> {
        self.mark_played_resolved(id).map(|mark| mark.utterance)
    }

    /// Like [`Self::mark_played`], also returning the epoch the utterance was registered in
    /// (the pump needs it to tag the upstream `TtsEnded`).
    ///
    /// An echo that arrives after a `Clear` covered the utterance ([`Self::note_clear`]) does not
    /// count as played: a mark of an old epoch that reached the wire after the clear (the end of
    /// a response whose `TtsEnded` was handled while the barge-in was being applied) is echoed
    /// at once by the client, whatever it had heard. It is recorded like a cut mark, so the
    /// position decides what was heard. Trade-off: a genuine echo of a fully played response
    /// that is still in flight when the caller barges in is treated the same way, so up to the
    /// lead of text at its end can be dropped (the real fix, not sending a stale mark to the
    /// wire, belongs to the transport stage; the book cannot tell the two apart).
    pub fn mark_played_resolved(&self, id: &str) -> Option<ResolvedMark> {
        let resolved = self.resolve_mark(id, |u| {
            if u.cleared {
                u.mark_cleared = true;
            } else {
                u.marked_played = true;
            }
        });
        if resolved.is_some() {
            self.notify();
        }
        resolved
    }

    /// Mark `id` was cut by a clear before the client echoed it. Returns the utterance, or
    /// `None` for an unknown or already resolved mark.
    pub fn mark_cleared(&self, id: &str) -> Option<u64> {
        self.mark_cleared_resolved(id).map(|mark| mark.utterance)
    }

    /// Like [`Self::mark_cleared`], also returning the utterance's epoch.
    pub fn mark_cleared_resolved(&self, id: &str) -> Option<ResolvedMark> {
        let resolved = self.resolve_mark(id, |u| u.mark_cleared = true);
        if resolved.is_some() {
            self.notify();
        }
        resolved
    }

    fn resolve_mark(&self, id: &str, update: impl FnOnce(&mut Utterance)) -> Option<ResolvedMark> {
        let mut state = self.lock();
        let Some(utterance) = state.marks.remove(id) else {
            state.stats.unknown_updates += 1;
            return None;
        };
        // The utterance may have been evicted after the mark was registered (evicting removes
        // its marks too, so this is defensive).
        let owner = state.utterances.get_mut(&utterance)?;
        owner.marks_pending = owner.marks_pending.saturating_sub(1);
        update(owner);
        Some(ResolvedMark {
            utterance,
            epoch: owner.key.epoch,
        })
    }

    /// A `Clear` to `floor` was issued: every utterance of an older epoch is now being cut, and
    /// the next position the transport reports for it is taken as its final one, unless an end
    /// mark is outstanding (then `MarkCleared` ends the wait). Call it when the `Clear` command
    /// is issued, before the transport applies it; the module docs state the resulting limit.
    pub fn note_clear(&self, floor: TurnEpoch) {
        let at = self.clock.get().map(|clock| clock.now());
        let mut state = self.lock();
        state.stats.clears_noted += 1;
        // The client's buffer is flushed by the clear: the lead it reported until now belongs to
        // the utterances this clear cuts.
        let lead = state.client_lead.take();
        let seq = state.clears.saturating_add(1);
        let mut covered = false;
        for u in state.utterances.values_mut() {
            if u.key.epoch < floor && !u.cleared {
                u.cleared = true;
                u.clear = Some(ClearNote { seq, lead, at });
                covered = true;
            }
        }
        if covered {
            state.clears = seq;
        }
        drop(state);
        self.notify();
    }

    /// Records the client's latest playback lead (audio sent to it and not yet played). A level:
    /// the newest report replaces the previous one. [`RoundSnapshot::heard_position`]
    /// subtracts it from released audio to estimate what the caller heard mid-utterance, in
    /// place of the fixed `lead_margin`. A `Clear` ([`Self::note_clear`]) takes the report along
    /// to the utterances it cuts; a report that arrives after it belongs to the next audio.
    pub fn set_client_lead(&self, lead: Duration) {
        self.lock().client_lead = Some(lead);
    }

    /// The client's latest playback lead, if the transport reports one.
    #[must_use]
    pub fn client_lead(&self) -> Option<Duration> {
        self.lock().client_lead
    }

    // -- reading ------------------------------------------------------------------------------

    /// A copy of the round's record.
    pub fn snapshot(&self, key: RoundKey) -> Option<RoundSnapshot> {
        let state = self.lock();
        let round = state.rounds.get(&key)?;
        let utterance = round.utterance.and_then(|id| state.utterances.get(&id));
        Some(RoundSnapshot {
            key,
            msg_index: round.msg_index,
            utterance: round.utterance,
            played: utterance.map(|u| u.played.clone()).unwrap_or_default(),
            released: utterance.map_or(Duration::ZERO, |u| u.released),
            total_audio: utterance.map_or(Duration::ZERO, |u| u.total_audio),
            marked_played: utterance.is_some_and(|u| u.marked_played),
            mark_cleared: utterance.is_some_and(|u| u.mark_cleared),
            lost: utterance.is_some_and(|u| u.lost),
            cleared: utterance.is_some_and(|u| u.cleared),
            settled: utterance.is_none_or(Utterance::settled),
            applied: round.applied,
            client_lead: utterance.and_then(|u| match u.clear {
                Some(clear) => clear.lead,
                None => state.client_lead,
            }),
            later_released: utterance.map_or(Duration::ZERO, |u| state.later_released(u)),
            followed_by_played: utterance.is_some_and(|u| state.followed_by_played(u)),
            last_release_at: utterance.and_then(|u| state.last_release_at(u)),
            cleared_at: utterance.and_then(|u| u.clear.and_then(|c| c.at)),
        })
    }

    /// The rounds that have a context message and were not yet cut to what was heard, with an
    /// epoch up to `through`, oldest first. The runner walks them last to first.
    pub fn pending_truncation(&self, through: TurnEpoch) -> Vec<RoundKey> {
        self.lock()
            .rounds
            .iter()
            .filter(|(key, round)| {
                key.epoch <= through && round.msg_index.is_some() && !round.applied
            })
            .map(|(key, _)| *key)
            .collect()
    }

    /// Rounds kept now.
    pub fn len(&self) -> usize {
        self.lock().rounds.len()
    }

    /// Whether no round is kept.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Counters since the book was created.
    pub fn stats(&self) -> BookStats {
        self.lock().stats
    }

    /// Whether every utterance of epochs up to `epoch` is settled.
    pub fn is_settled_through(&self, epoch: TurnEpoch) -> bool {
        self.lock().settled_through(epoch)
    }

    /// Waits until every utterance of epochs up to `epoch` is settled, at most `timeout` on
    /// `clock`. Returns whether they settled; a timeout is counted in
    /// [`BookStats::settle_timeouts`] and the caller then uses the last known positions.
    ///
    /// Cancel-safe; the lock is never held while waiting.
    pub async fn wait_settled_through(
        &self,
        epoch: TurnEpoch,
        clock: &dyn Clock,
        timeout: Duration,
    ) -> bool {
        // Subscribe before the first check so a change in between is not missed.
        let mut changes = self.changed.subscribe();
        let deadline = brisvox_core::clock::deadline_after(clock.now(), timeout);
        let timer = clock.sleep_until(deadline);
        tokio::pin!(timer);
        loop {
            if self.is_settled_through(epoch) {
                return true;
            }
            if clock.now() >= deadline {
                break;
            }
            tokio::select! {
                biased;
                changed = changes.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                () = &mut timer => break,
            }
        }
        let settled = self.is_settled_through(epoch);
        if !settled {
            self.lock().stats.settle_timeouts += 1;
            tracing::debug!(
                target: "brisvox::engine",
                epoch = epoch.get(),
                ?timeout,
                "playout settle wait timed out"
            );
        }
        settled
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::SimClock;

    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn key(epoch: u32, round: u32) -> RoundKey {
        RoundKey::new(TurnEpoch::new(epoch), round)
    }

    /// One round with an utterance saying "Merhaba dünya nasılsın" (words at 0/300/600 ms).
    fn speaking(book: &PlayoutBook, k: RoundKey, utterance: u64) {
        book.register_round(k);
        book.set_msg_index(k, 1);
        book.register_utterance(k, utterance);
        assert!(book.push_sent(utterance, "Merhaba dünya"));
        assert!(book.push_sent(utterance, " nasılsın"));
        for (word, start) in [("Merhaba", 0), ("dünya", 300), ("nasılsın", 600)] {
            assert!(book.push_word(utterance, word, ms(start)));
        }
        assert!(book.add_audio(utterance, ms(2_000)));
    }

    #[test]
    fn the_recent_sent_text_is_the_tail_of_the_latest_utterances() {
        let book = PlayoutBook::new(8);
        assert_eq!(book.recent_sent_text(100), "");
        speaking(&book, key(0, 0), 1);
        assert_eq!(book.recent_sent_text(100).trim(), "Merhaba dünya nasılsın");
        // Only the tail when it is longer than asked.
        assert_eq!(book.recent_sent_text(8).trim(), "nasılsın");
        speaking(&book, key(1, 0), 2);
        speaking(&book, key(2, 0), 3);
        // The two newest utterances only.
        assert_eq!(
            book.recent_sent_text(1_000).matches("Merhaba").count(),
            2,
            "{}",
            book.recent_sent_text(1_000)
        );
    }

    #[test]
    fn rounds_are_keyed_by_epoch_and_round_not_by_arrival_order() {
        let book = PlayoutBook::default();
        for k in [key(2, 1), key(1, 0), key(2, 0), key(1, 1)] {
            book.register_round(k);
            book.set_msg_index(k, 0);
        }
        // Registering twice changes nothing.
        book.register_round(key(1, 0));
        assert_eq!(book.len(), 4);
        assert_eq!(
            book.pending_truncation(TurnEpoch::new(2)),
            [key(1, 0), key(1, 1), key(2, 0), key(2, 1)]
        );
        assert_eq!(
            book.pending_truncation(TurnEpoch::new(1)),
            [key(1, 0), key(1, 1)]
        );
        book.mark_applied(key(1, 1));
        assert_eq!(
            book.pending_truncation(TurnEpoch::new(1)),
            [key(1, 0)],
            "applied rounds are not pending again"
        );
        assert!(book.snapshot(key(1, 1)).unwrap().applied);
        assert!(book.snapshot(key(9, 9)).is_none());
    }

    #[test]
    fn a_resolved_mark_names_the_utterance_and_its_epoch() {
        let book = PlayoutBook::default();
        book.register_utterance(key(3, 0), 9);
        book.register_utterance(key(4, 0), 10);
        assert!(book.register_mark("end-9", 9));
        assert!(book.register_mark("end-10", 10));
        assert_eq!(
            book.mark_played_resolved("end-9"),
            Some(ResolvedMark {
                utterance: 9,
                epoch: TurnEpoch::new(3)
            })
        );
        assert_eq!(
            book.mark_cleared_resolved("end-10"),
            Some(ResolvedMark {
                utterance: 10,
                epoch: TurnEpoch::new(4)
            })
        );
        // Resolved once: a repeat or a stranger is unknown, and counted.
        assert_eq!(book.mark_played_resolved("end-9"), None);
        assert_eq!(book.mark_played("never-registered"), None);
        assert_eq!(book.stats().unknown_updates, 2);
        assert!(book.snapshot(key(3, 0)).unwrap().marked_played);
        assert!(book.snapshot(key(4, 0)).unwrap().mark_cleared);
    }

    #[test]
    fn rounds_without_a_context_message_are_never_pending() {
        // A filler round has no message index and may have an utterance of its own.
        let book = PlayoutBook::default();
        book.register_round(key(1, 0));
        book.register_utterance(key(1, 0), 5);
        assert!(book.pending_truncation(TurnEpoch::new(1)).is_empty());
        let snap = book.snapshot(key(1, 0)).unwrap();
        assert_eq!(snap.msg_index, None);
        assert_eq!(snap.utterance, Some(5));
    }

    #[test]
    fn an_utterance_registered_before_its_round_creates_the_round() {
        let book = PlayoutBook::default();
        book.register_utterance(key(3, 0), 9);
        assert_eq!(book.len(), 1);
        assert!(book.set_msg_index(key(3, 0), 4));
        assert_eq!(book.snapshot(key(3, 0)).unwrap().msg_index, Some(4));
        assert!(!book.set_msg_index(key(3, 1), 4));
        assert_eq!(book.stats().unknown_updates, 1);
    }

    #[test]
    fn positions_are_monotone_and_heard_is_min_of_released_and_total_minus_lead() {
        let book = PlayoutBook::default();
        speaking(&book, key(1, 0), 1);
        assert!(book.position(TurnEpoch::new(1), 1, ms(500)));
        assert!(
            book.position(TurnEpoch::new(1), 1, ms(400)),
            "accepted but ignored"
        );
        let snap = book.snapshot(key(1, 0)).unwrap();
        assert_eq!(snap.released, ms(500));
        assert_eq!(snap.heard_position(Duration::ZERO), ms(500));
        assert_eq!(snap.heard_position(ms(200)), ms(300));
        assert_eq!(snap.heard_position(ms(900)), Duration::ZERO);

        // Released beyond the audio handed over (an upper bound gone wild): capped by total.
        book.position(TurnEpoch::new(1), 1, ms(9_000));
        let snap = book.snapshot(key(1, 0)).unwrap();
        assert_eq!(snap.heard_position(Duration::ZERO), ms(2_000));
    }

    #[test]
    fn heard_text_follows_the_position_and_the_lead_margin() {
        let book = PlayoutBook::default();
        speaking(&book, key(1, 0), 1);
        book.position(TurnEpoch::new(1), 1, ms(500));
        let snap = book.snapshot(key(1, 0)).unwrap();
        let config = PlayedConfig::default();
        let Heard::Cut(cut) = snap.heard(Duration::ZERO, &config) else {
            panic!("not whole");
        };
        assert_eq!(&*cut.text, "Merhaba dünya");
        let Heard::Cut(cut) = snap.heard(ms(250), &config) else {
            panic!("not whole");
        };
        assert_eq!(
            &*cut.text, "Merhaba",
            "the lead margin takes the last word back"
        );
    }

    #[test]
    fn a_marked_played_utterance_is_kept_whole_unless_it_was_lost() {
        let book = PlayoutBook::default();
        speaking(&book, key(1, 0), 1);
        speaking(&book, key(1, 1), 2);
        assert!(book.register_mark("m1", 1));
        assert!(book.register_mark("m2", 2));
        assert_eq!(book.mark_played("m1"), Some(1));
        assert_eq!(book.mark_played("m1"), None, "a mark resolves once");
        assert_eq!(book.mark_played("m2"), Some(2));
        book.mark_lost(2);

        let whole = book.snapshot(key(1, 0)).unwrap();
        assert!(whole.kept_whole());
        assert_eq!(whole.heard(ms(160), &PlayedConfig::default()), Heard::Whole);
        let lost = book.snapshot(key(1, 1)).unwrap();
        assert!(!lost.kept_whole());
        assert!(matches!(
            lost.heard(Duration::ZERO, &PlayedConfig::default()),
            Heard::Cut(_)
        ));
    }

    #[test]
    fn unknown_utterances_marks_and_epochs_are_ignored_and_counted() {
        let book = PlayoutBook::default();
        speaking(&book, key(1, 0), 1);
        assert!(!book.position(TurnEpoch::new(1), 77, ms(10)));
        assert!(!book.push_sent(77, "x"));
        assert!(!book.push_word(77, "x", ms(0)));
        assert!(!book.add_audio(77, ms(20)));
        assert!(!book.register_mark("m", 77));
        assert_eq!(book.mark_cleared("nope"), None);
        // Right utterance, wrong epoch.
        assert!(!book.position(TurnEpoch::new(2), 1, ms(10)));
        assert_eq!(book.snapshot(key(1, 0)).unwrap().released, Duration::ZERO);
        assert_eq!(book.stats().unknown_updates, 7);
    }

    #[test]
    fn settled_rules() {
        let book = PlayoutBook::default();
        let e1 = TurnEpoch::new(1);
        // No utterances: trivially settled.
        assert!(book.is_settled_through(e1));
        // Registered but no audio yet: nothing to wait for.
        book.register_utterance(key(1, 0), 1);
        book.set_msg_index(key(1, 0), 0);
        assert!(book.is_settled_through(e1));
        // Audio handed over, nothing reported: waiting for the transport.
        book.add_audio(1, ms(1_000));
        assert!(!book.is_settled_through(e1));
        // A position before any clear does not settle it...
        book.position(e1, 1, ms(400));
        assert!(!book.is_settled_through(e1));
        // ...a position after the clear does.
        book.note_clear(TurnEpoch::new(2));
        assert!(!book.is_settled_through(e1));
        book.position(e1, 1, ms(450));
        assert!(book.is_settled_through(e1));
        assert!(book.snapshot(key(1, 0)).unwrap().cleared);

        // Other ways to settle: echoed mark, cut mark, lost, all audio released.
        for (utterance, how) in [
            (2u64, "played"),
            (3, "cleared"),
            (4, "lost"),
            (5, "released"),
        ] {
            let k = key(1, u32::try_from(utterance).unwrap());
            book.register_utterance(k, utterance);
            book.set_msg_index(k, 0);
            book.add_audio(utterance, ms(1_000));
            book.register_mark(format!("m{utterance}"), utterance);
            assert!(!book.is_settled_through(e1), "{how}");
            match how {
                "played" => drop(book.mark_played("m2")),
                "cleared" => drop(book.mark_cleared("m3")),
                "lost" => book.mark_lost(4),
                _ => drop(book.position(e1, 5, ms(1_000))),
            }
            assert!(book.is_settled_through(e1), "{how}");
        }

        // An older epoch's wait ignores newer epochs' utterances.
        book.register_utterance(key(2, 0), 10);
        book.set_msg_index(key(2, 0), 0);
        book.add_audio(10, ms(100));
        assert!(book.is_settled_through(e1));
        assert!(!book.is_settled_through(TurnEpoch::new(2)));
    }

    #[test]
    fn a_cut_utterance_of_a_round_without_a_message_is_not_waited_for() {
        let book = PlayoutBook::default();
        let e1 = TurnEpoch::new(1);
        // A filler: its round has no context message, so nothing is ever cut to its position
        // and `mark_applied` never reaches it.
        book.register_round(key(1, 0));
        book.register_utterance(key(1, 0), 1);
        book.add_audio(1, ms(1_000));
        book.position(e1, 1, ms(400));
        // Playing, not cut: still waited for.
        assert!(!book.is_settled_through(e1));
        // Cut between two position reports: no final position, no end mark, not all released.
        book.note_clear(TurnEpoch::new(2));
        assert!(!book.snapshot(key(1, 0)).unwrap().settled);
        assert!(
            book.is_settled_through(e1),
            "it can never settle and must not block the wait"
        );
        // A round that does have a message (the same situation) is waited for.
        book.register_utterance(key(1, 1), 2);
        book.set_msg_index(key(1, 1), 3);
        book.add_audio(2, ms(1_000));
        book.position(e1, 2, ms(400));
        book.note_clear(TurnEpoch::new(3));
        assert!(!book.is_settled_through(e1));
        book.mark_applied(key(1, 1));
        assert!(book.is_settled_through(e1));
    }

    #[tokio::test(start_paused = true)]
    async fn the_last_release_is_stamped_on_the_attached_clock_and_covers_the_utterances_behind() {
        let clock = SimClock::new();
        let book = PlayoutBook::default();
        let e1 = TurnEpoch::new(1);
        for (k, utterance) in [(key(1, 0), 1u64), (key(1, 1), 2)] {
            book.register_utterance(k, utterance);
            book.add_audio(utterance, ms(1_000));
        }
        // No clock yet: a release has no time, and the margin applies in full.
        book.position(e1, 1, ms(100));
        let first = book.snapshot(key(1, 0)).unwrap();
        assert_eq!(first.last_release_at, None);
        let now = clock.now();
        assert_eq!(first.margin_at(ms(160), now + ms(5_000)), ms(160));

        book.attach_clock(Arc::new(clock));
        book.attach_clock(Arc::new(SimClock::new())); // the first clock stays
        let t0 = clock.now();
        book.position(e1, 1, ms(500));
        assert_eq!(book.snapshot(key(1, 0)).unwrap().last_release_at, Some(t0));
        // A report that moves nothing is not a release.
        book.position(e1, 1, ms(500));
        book.position(e1, 1, ms(300));
        assert_eq!(book.snapshot(key(1, 0)).unwrap().last_release_at, Some(t0));
        // The later utterance's release is later: the client's buffer was refilled by it.
        clock.sleep(ms(40)).await;
        book.position(e1, 2, ms(200));
        let second_at = book.snapshot(key(1, 1)).unwrap().last_release_at;
        assert_eq!(second_at, Some(t0 + ms(40)));
        assert_eq!(book.snapshot(key(1, 0)).unwrap().last_release_at, second_at);
    }

    #[tokio::test(start_paused = true)]
    async fn the_margin_fades_with_the_time_since_the_last_release() {
        let clock = SimClock::new();
        let book = PlayoutBook::default();
        book.attach_clock(Arc::new(clock));
        let e1 = TurnEpoch::new(1);
        book.register_utterance(key(1, 0), 1);
        book.add_audio(1, ms(1_000));
        book.position(e1, 1, ms(1_000));
        let at = clock.now();
        let snapshot = book.snapshot(key(1, 0)).unwrap();
        for (later, expected) in [(0, 160), (60, 100), (160, 0), (9_000, 0)] {
            assert_eq!(snapshot.margin_at(ms(160), at + ms(later)), ms(expected));
        }
        // A barge-in stamped before the release (the release came right after the clear).
        assert_eq!(snapshot.margin_at(ms(160), at - ms(50)), ms(160));
        // A reported lead is measured, not faded.
        book.set_client_lead(ms(300));
        let reported = book.snapshot(key(1, 0)).unwrap();
        assert_eq!(reported.margin_at(ms(160), at + ms(9_000)), ms(160));
    }

    #[tokio::test(start_paused = true)]
    async fn the_margin_fades_only_up_to_the_clear_not_to_the_runners_later_stamp() {
        // The runner stamps `interrupted_at` after the clear; the client's buffer
        // stopped draining at the clear, so the time between the two must not fade the margin.
        let clock = SimClock::new();
        let book = PlayoutBook::default();
        book.attach_clock(Arc::new(clock));
        let e1 = TurnEpoch::new(1);
        book.register_utterance(key(1, 0), 1);
        book.add_audio(1, ms(1_000));
        book.position(e1, 1, ms(1_000));
        let released = clock.now();
        clock.sleep(ms(40)).await;
        book.note_clear(TurnEpoch::new(2));
        let cleared = clock.now();
        clock.sleep(ms(200)).await;
        let runner = clock.now();
        let snapshot = book.snapshot(key(1, 0)).unwrap();
        assert_eq!(snapshot.cleared_at, Some(cleared));
        assert_eq!(snapshot.last_release_at, Some(released));
        assert_eq!(
            snapshot.margin_at(ms(160), runner),
            ms(120),
            "faded 40 ms, not 240"
        );
        // A runner stamp before the clear still bounds the fade.
        assert_eq!(snapshot.margin_at(ms(160), released + ms(10)), ms(150));
        // A final position stamped after the clear: nothing drained between the two.
        book.position(e1, 1, ms(1_000));
        let late = book.snapshot(key(1, 0)).unwrap();
        assert_eq!(late.margin_at(ms(160), runner), ms(120));
    }

    #[tokio::test(start_paused = true)]
    async fn a_context_round_followed_by_a_cut_filler_keeps_a_nonzero_margin_up_to_the_clear() {
        // One epoch: round 0 spoke its whole answer (a context message), then a filler (round 1,
        // no message) started and was cut by the barge-in's clear. The filler's audio sits
        // behind round 0 in the client's buffer and the margin fades only from the filler's
        // release to the clear.
        let clock = SimClock::new();
        let book = PlayoutBook::default();
        book.attach_clock(Arc::new(clock));
        let e1 = TurnEpoch::new(1);
        book.register_round(key(1, 0));
        book.set_msg_index(key(1, 0), 1);
        book.register_utterance(key(1, 0), 1);
        book.add_audio(1, ms(1_000));
        book.position(e1, 1, ms(1_000));
        clock.sleep(ms(10)).await;
        book.register_round(key(1, 1));
        book.register_utterance(key(1, 1), 2);
        book.add_audio(2, ms(400));
        book.position(e1, 2, ms(60));
        clock.sleep(ms(40)).await;
        book.note_clear(TurnEpoch::new(2));
        clock.sleep(ms(250)).await;
        let interrupted_at = clock.now();
        let round = book.snapshot(key(1, 0)).unwrap();
        assert!(round.cleared);
        assert_eq!(round.later_released, ms(60));
        let margin = round.margin_at(ms(160), interrupted_at);
        assert_eq!(
            margin,
            ms(120),
            "160 ms less the 40 ms from the filler's release to the clear"
        );
        // 120 ms of margin, 60 of it covered by the filler behind: 60 ms of round 0 unheard.
        assert_eq!(round.heard_position(margin), ms(940));
    }

    #[tokio::test]
    async fn the_clear_hook_tells_the_book_before_it_sends_the_command() {
        let book = PlayoutBook::shared(8);
        book.register_utterance(key(1, 0), 1);
        book.add_audio(1, ms(1_000));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let send: ClearHook = Arc::new({
            let (book, seen) = (Arc::clone(&book), Arc::clone(&seen));
            move |epoch| {
                // What the transport would see when the command arrives.
                seen.lock().unwrap().push((
                    epoch,
                    book.stats().clears_noted,
                    book.snapshot(key(1, 0)).unwrap().cleared,
                ));
                Box::pin(async { Ok(()) })
            }
        });
        let hook = note_then_send(Arc::clone(&book), send);
        hook(TurnEpoch::new(2)).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), [(TurnEpoch::new(2), 1, true)]);
        // A failing send is reported and the note was still made.
        let failing: ClearHook = Arc::new(|_| Box::pin(async { Err("closed".to_owned()) }));
        let hook = note_then_send(Arc::clone(&book), failing);
        assert_eq!(hook(TurnEpoch::new(3)).await, Err("closed".to_owned()));
        assert_eq!(book.stats().clears_noted, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_settled_returns_when_the_final_position_arrives() {
        let clock = SimClock::new();
        let book = PlayoutBook::shared(8);
        let e1 = TurnEpoch::new(1);
        book.register_utterance(key(1, 0), 1);
        book.set_msg_index(key(1, 0), 0);
        book.add_audio(1, ms(1_000));
        book.note_clear(TurnEpoch::new(2));

        let reporter = tokio::spawn({
            let (book, clock) = (Arc::clone(&book), clock);
            async move {
                clock.sleep(ms(80)).await;
                book.position(e1, 1, ms(640));
            }
        });
        let settled = book.wait_settled_through(e1, &clock, ms(250)).await;
        assert!(settled);
        assert_eq!(
            clock.elapsed(),
            ms(80),
            "woken by the position, not by the timeout"
        );
        assert_eq!(book.stats().settle_timeouts, 0);
        reporter.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn wait_settled_gives_up_at_the_deadline_and_counts_it() {
        let clock = SimClock::new();
        let book = PlayoutBook::default();
        let e1 = TurnEpoch::new(1);
        book.register_utterance(key(1, 0), 1);
        book.add_audio(1, ms(1_000));
        book.position(e1, 1, ms(300));

        let settled = book.wait_settled_through(e1, &clock, ms(250)).await;
        assert!(!settled);
        assert_eq!(clock.elapsed(), ms(250));
        assert_eq!(book.stats().settle_timeouts, 1);
        // The last known position is still there for the fallback.
        assert_eq!(book.snapshot(key(1, 0)).unwrap().released, ms(300));

        // Already settled: returns at once without touching the timer.
        book.mark_lost(1);
        let before = clock.elapsed();
        assert!(book.wait_settled_through(e1, &clock, ms(250)).await);
        assert_eq!(clock.elapsed(), before);
        // A zero timeout checks once.
        book.register_utterance(key(2, 0), 2);
        book.add_audio(2, ms(10));
        assert!(
            !book
                .wait_settled_through(TurnEpoch::new(2), &clock, Duration::ZERO)
                .await
        );
        assert_eq!(book.stats().settle_timeouts, 2);
    }

    #[test]
    fn the_book_is_bounded_and_eviction_takes_the_oldest_round_with_its_state() {
        let book = PlayoutBook::new(3);
        assert_eq!(book.capacity(), 3);
        for i in 0..10u32 {
            let k = key(i, 0);
            book.register_round(k);
            book.set_msg_index(k, 0);
            book.register_utterance(k, u64::from(i));
            book.register_mark(format!("m{i}"), u64::from(i));
            book.add_audio(u64::from(i), ms(20));
        }
        assert_eq!(book.len(), 3, "never above the capacity");
        assert!(book.snapshot(key(6, 0)).is_none());
        assert!(book.snapshot(key(7, 0)).is_some());
        assert!(book.snapshot(key(9, 0)).is_some());
        let stats = book.stats();
        assert_eq!(stats.evicted_rounds, 7);
        assert_eq!(stats.evicted_unapplied, 7);

        // The evicted utterance and its mark are gone: updates are ignored and counted.
        let before = book.stats().unknown_updates;
        assert!(!book.add_audio(0, ms(20)));
        assert_eq!(book.mark_played("m0"), None);
        assert_eq!(book.stats().unknown_updates, before + 2);
        // The kept ones still work.
        assert_eq!(book.mark_played("m9"), Some(9));
        assert_eq!(
            book.pending_truncation(TurnEpoch::new(100)),
            [key(7, 0), key(8, 0), key(9, 0)]
        );

        // Applied rounds that get evicted are not counted as lost work.
        let book = PlayoutBook::new(1);
        book.register_round(key(1, 0));
        book.set_msg_index(key(1, 0), 0);
        book.mark_applied(key(1, 0));
        book.register_round(key(2, 0));
        assert_eq!(book.stats().evicted_rounds, 1);
        assert_eq!(book.stats().evicted_unapplied, 0);
    }

    fn counts(book: &PlayoutBook) -> (usize, usize) {
        let state = book.lock();
        (state.utterances.len(), state.marks.len())
    }

    #[test]
    fn a_second_utterance_for_one_round_is_ignored_and_nothing_leaks() {
        let book = PlayoutBook::new(2);
        let k = key(1, 0);
        book.register_round(k);
        book.register_utterance(k, 10);
        book.register_utterance(k, 10);
        assert_eq!(
            book.stats().conflicting_registrations,
            0,
            "same pair is idempotent"
        );
        // A TTS reconnect inside the round registers another id: the first mapping stays.
        book.register_utterance(k, 11);
        assert_eq!(book.stats().conflicting_registrations, 1);
        assert_eq!(book.snapshot(k).unwrap().utterance, Some(10));
        assert!(!book.push_sent(11, "ignored"), "the ignored id is unknown");
        assert!(book.register_mark("m10", 10));
        assert_eq!(counts(&book), (1, 1));

        // Evicting the round removes everything that was ever registered for it.
        book.register_round(key(2, 0));
        book.register_round(key(3, 0));
        assert!(book.snapshot(k).is_none());
        assert_eq!(counts(&book), (0, 0), "no orphaned utterance or mark");
    }

    #[test]
    fn one_utterance_id_cannot_belong_to_two_rounds() {
        let book = PlayoutBook::new(2);
        book.register_utterance(key(1, 0), 5);
        book.register_utterance(key(2, 0), 5);
        assert_eq!(book.stats().conflicting_registrations, 1);
        assert_eq!(
            book.len(),
            1,
            "the conflicting registration created no round"
        );
        assert_eq!(book.snapshot(key(1, 0)).unwrap().utterance, Some(5));
        assert_eq!(book.stats().unknown_updates, 0);
        // The newer round is not touched by the older one's eviction.
        book.register_round(key(2, 0));
        book.register_round(key(3, 0));
        assert!(book.snapshot(key(1, 0)).is_none());
        assert_eq!(counts(&book), (0, 0));
    }

    #[test]
    fn utterances_of_applied_rounds_do_not_hold_up_a_settle_wait() {
        // A client that never echoes marks: the last utterance of a turn is played in full but
        // the last report stayed below the total, so it never settles by itself.
        let book = PlayoutBook::default();
        let e1 = TurnEpoch::new(1);
        let e2 = TurnEpoch::new(2);
        book.register_round(key(1, 0));
        book.set_msg_index(key(1, 0), 1);
        book.register_utterance(key(1, 0), 1);
        book.add_audio(1, ms(1_000));
        book.position(e1, 1, ms(900));
        assert!(!book.is_settled_through(e1));

        // Once that round is cut to what was heard it stops mattering for later waits.
        book.mark_applied(key(1, 0));
        assert!(book.is_settled_through(e1));
        assert!(book.is_settled_through(e2));

        // A newer unsettled utterance still counts.
        book.register_utterance(key(2, 0), 2);
        book.add_audio(2, ms(500));
        assert!(!book.is_settled_through(e2));
        assert!(book.is_settled_through(e1), "epoch 1 only looks at epoch 1");
    }

    #[tokio::test(start_paused = true)]
    async fn an_old_applied_utterance_does_not_cost_a_settle_timeout() {
        let clock = SimClock::new();
        let book = PlayoutBook::default();
        book.register_round(key(1, 0));
        book.register_utterance(key(1, 0), 1);
        book.add_audio(1, ms(1_000));
        book.position(TurnEpoch::new(1), 1, ms(900));
        book.mark_applied(key(1, 0));

        book.register_round(key(2, 0));
        book.register_utterance(key(2, 0), 2);
        book.add_audio(2, ms(1_000));
        book.mark_lost(2);

        let before = clock.elapsed();
        assert!(
            book.wait_settled_through(TurnEpoch::new(2), &clock, ms(250))
                .await
        );
        assert_eq!(clock.elapsed(), before, "no wait at all");
        assert_eq!(book.stats().settle_timeouts, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn applying_a_round_or_evicting_it_wakes_a_waiter() {
        let clock = SimClock::new();
        let book = PlayoutBook::shared(1);
        let e1 = TurnEpoch::new(1);
        book.register_utterance(key(1, 0), 1);
        book.add_audio(1, ms(1_000));

        let waiter = tokio::spawn({
            let book = Arc::clone(&book);
            async move { book.wait_settled_through(e1, &clock, ms(250)).await }
        });
        tokio::task::yield_now().await;
        clock.sleep(ms(40)).await;
        book.mark_applied(key(1, 0));
        assert!(waiter.await.unwrap());
        assert_eq!(
            clock.elapsed(),
            ms(40),
            "woken by the apply, not the timeout"
        );

        // Eviction of an unsettled utterance wakes a waiter as well.
        let book = PlayoutBook::shared(1);
        book.register_utterance(key(1, 0), 1);
        book.add_audio(1, ms(1_000));
        let clock = SimClock::new();
        let waiter = tokio::spawn({
            let book = Arc::clone(&book);
            async move { book.wait_settled_through(e1, &clock, ms(250)).await }
        });
        tokio::task::yield_now().await;
        clock.sleep(ms(30)).await;
        book.register_round(key(2, 0));
        assert!(waiter.await.unwrap());
        assert_eq!(clock.elapsed(), ms(30));
    }

    #[test]
    fn a_stale_position_does_not_settle_an_utterance_whose_mark_is_outstanding() {
        let book = PlayoutBook::default();
        let e1 = TurnEpoch::new(1);
        speaking(&book, key(1, 0), 1);
        assert!(book.register_mark("end", 1));
        book.position(e1, 1, ms(500));
        // The driver issues the Clear; a periodic report sent before it is still in the queue.
        book.note_clear(TurnEpoch::new(2));
        book.position(e1, 1, ms(600));
        assert!(!book.is_settled_through(e1), "that report may be stale");
        // The transport's final position, then MarkCleared (in that order on the queue).
        book.position(e1, 1, ms(640));
        assert!(!book.is_settled_through(e1));
        assert_eq!(book.mark_cleared("end"), Some(1));
        assert!(book.is_settled_through(e1));
        assert_eq!(book.snapshot(key(1, 0)).unwrap().released, ms(640));
    }

    #[test]
    fn known_limitation_a_stale_report_settles_a_markless_utterance() {
        // Mid-utterance barge-in: no end mark yet and no clear acknowledgement from the
        // transport, so the first position after note_clear is taken as the final one. If it
        // was a stale periodic report the cut position is too low (module docs, "Known
        // limitation"). This test pins the behaviour so a change is a conscious one.
        let book = PlayoutBook::default();
        let e1 = TurnEpoch::new(1);
        speaking(&book, key(1, 0), 1);
        book.position(e1, 1, ms(500));
        book.note_clear(TurnEpoch::new(2));
        book.position(e1, 1, ms(600)); // stale: sent before the flush
        assert!(book.is_settled_through(e1));
        book.position(e1, 1, ms(640)); // the real final position, too late for a waiter
        assert_eq!(book.snapshot(key(1, 0)).unwrap().released, ms(640));
    }

    #[test]
    fn note_clear_cuts_only_utterances_older_than_the_floor() {
        let book = PlayoutBook::default();
        let (e1, e2) = (TurnEpoch::new(1), TurnEpoch::new(2));
        speaking(&book, key(1, 0), 1);
        // The new turn's own utterance exists when its Clear is issued (floor == its epoch).
        speaking(&book, key(2, 0), 2);
        book.note_clear(e2);
        assert!(book.snapshot(key(1, 0)).unwrap().cleared);
        assert!(
            !book.snapshot(key(2, 0)).unwrap().cleared,
            "an utterance of the floor epoch is not cut"
        );
        // Its periodic report must not be taken as a final position.
        book.position(e2, 2, ms(100));
        assert!(!book.is_settled_through(e2));
        assert!(!book.snapshot(key(2, 0)).unwrap().settled);
        // The older utterance still settles on its first position after the clear.
        book.position(e1, 1, ms(600));
        assert!(book.is_settled_through(e1));
    }

    #[test]
    fn a_poisoned_lock_does_not_break_the_book() {
        let book = Arc::new(PlayoutBook::default());
        let other = Arc::clone(&book);
        let _ = std::thread::spawn(move || {
            let _guard = other.state.lock().unwrap();
            panic!("poison");
        })
        .join();
        book.register_round(key(1, 0));
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn the_client_lead_is_a_level_and_a_clear_resets_it() {
        let book = PlayoutBook::new(4);
        assert_eq!(book.client_lead(), None);
        book.set_client_lead(Duration::from_millis(240));
        book.set_client_lead(Duration::from_millis(120));
        assert_eq!(book.client_lead(), Some(Duration::from_millis(120)));
        book.note_clear(TurnEpoch::ZERO.next());
        assert_eq!(book.client_lead(), None);
    }

    #[test]
    fn a_mark_echoed_after_a_clear_covered_the_utterance_does_not_count_as_played() {
        let book = PlayoutBook::default();
        let e1 = TurnEpoch::new(1);
        speaking(&book, key(1, 0), 1);
        speaking(&book, key(1, 1), 2);
        assert!(book.register_mark("late", 1));
        assert!(book.register_mark("honest", 2));
        // Round 1's echo comes before any clear: played to its end.
        assert_eq!(book.mark_played("honest"), Some(2));
        // The clear covers round 0, then a mark of the old epoch that reached the wire after it
        // is echoed at once.
        book.note_clear(TurnEpoch::new(2));
        assert_eq!(book.mark_played("late"), Some(1));
        let late = book.snapshot(key(1, 0)).unwrap();
        assert!(!late.marked_played && late.mark_cleared);
        assert!(book.snapshot(key(1, 1)).unwrap().marked_played);
        assert!(book.is_settled_through(e1));
    }

    #[test]
    fn the_lead_at_the_clear_is_kept_per_utterance_and_chained_over_later_audio() {
        let book = PlayoutBook::default();
        let e1 = TurnEpoch::new(1);
        speaking(&book, key(1, 0), 1);
        speaking(&book, key(1, 1), 2);
        book.position(e1, 1, ms(2_000));
        book.position(e1, 2, ms(700));
        // Without a report both use the margin the reader passes in, less what sits behind.
        let first = book.snapshot(key(1, 0)).unwrap();
        assert_eq!(first.client_lead, None);
        assert_eq!(first.later_released, ms(700));
        assert_eq!(first.unplayed_estimate(ms(160)), Duration::ZERO);
        assert_eq!(first.heard_position(ms(160)), ms(2_000));
        let second = book.snapshot(key(1, 1)).unwrap();
        assert_eq!(second.heard_position(ms(160)), ms(540));

        // A report of 1 s, then the clear takes it along.
        book.set_client_lead(ms(1_000));
        assert_eq!(
            book.snapshot(key(1, 0)).unwrap().client_lead,
            Some(ms(1_000))
        );
        book.note_clear(TurnEpoch::new(2));
        book.set_client_lead(ms(5));
        let first = book.snapshot(key(1, 0)).unwrap();
        let second = book.snapshot(key(1, 1)).unwrap();
        assert_eq!(first.client_lead, Some(ms(1_000)));
        assert_eq!(
            first.unplayed_estimate(ms(160)),
            ms(300),
            "1000 - 700 behind it"
        );
        assert_eq!(first.heard_position(ms(160)), ms(1_700));
        assert_eq!(second.unplayed_estimate(ms(160)), ms(1_000));
        assert_eq!(second.heard_position(ms(160)), Duration::ZERO);
        // The utterance of a later turn is not behind the cleared ones.
        book.register_utterance(key(3, 0), 9);
        book.add_audio(9, ms(500));
        book.position(TurnEpoch::new(3), 9, ms(500));
        assert_eq!(
            book.snapshot(key(1, 1)).unwrap().later_released,
            Duration::ZERO
        );
        assert_eq!(book.snapshot(key(3, 0)).unwrap().client_lead, Some(ms(5)));
    }

    #[test]
    fn a_genuine_echo_that_races_a_barge_in_is_cut_by_position_not_kept_whole() {
        // Known trade-off (see `mark_played_resolved`): a response fully played whose end-mark
        // echo is still in flight when the caller barges in cannot be told from a stale mark
        // that reached the wire after the clear. The echo counts as a cut mark and the position
        // decides: the loss is bounded by the lead (here 160 ms), never more.
        let book = PlayoutBook::default();
        let e1 = TurnEpoch::new(1);
        speaking(&book, key(1, 0), 1);
        book.position(e1, 1, ms(1_000));
        assert!(book.register_mark("end", 1));
        book.note_clear(TurnEpoch::new(2));
        assert_eq!(book.mark_played("end"), Some(1));
        let snap = book.snapshot(key(1, 0)).unwrap();
        assert!(snap.mark_cleared && !snap.marked_played && !snap.kept_whole());
        assert_eq!(snap.heard_position(ms(160)), ms(840));
    }

    #[test]
    fn followed_by_played_does_not_cross_epochs() {
        let book = PlayoutBook::default();
        speaking(&book, key(1, 0), 1);
        speaking(&book, key(2, 0), 2);
        assert!(book.register_mark("end", 2));
        assert_eq!(book.mark_played("end"), Some(2));
        // The next turn's answer was played and acked; the first one was cut by a clear.
        let first = book.snapshot(key(1, 0)).unwrap();
        assert!(!first.followed_by_played && !first.kept_whole());
        // Within one epoch the later round's echo still keeps the earlier round whole.
        speaking(&book, key(3, 0), 3);
        speaking(&book, key(3, 1), 4);
        assert!(book.register_mark("end2", 4));
        assert_eq!(book.mark_played("end2"), Some(4));
        assert!(book.snapshot(key(3, 0)).unwrap().kept_whole());
    }

    #[test]
    fn an_utterance_followed_by_a_played_one_is_kept_whole_unless_it_was_lost() {
        let book = PlayoutBook::default();
        speaking(&book, key(1, 0), 1);
        speaking(&book, key(1, 1), 2);
        assert!(!book.snapshot(key(1, 0)).unwrap().kept_whole());
        assert!(book.register_mark("end", 2));
        assert_eq!(book.mark_played("end"), Some(2));
        let first = book.snapshot(key(1, 0)).unwrap();
        assert!(first.followed_by_played && first.kept_whole());
        assert_eq!(first.heard(ms(160), &PlayedConfig::default()), Heard::Whole);
        book.mark_lost(1);
        assert!(!book.snapshot(key(1, 0)).unwrap().kept_whole());
        // The last utterance is followed by nothing.
        assert!(!book.snapshot(key(1, 1)).unwrap().followed_by_played);
    }
}
