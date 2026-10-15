//! Conversation events of a call: what a UI shows while the call runs (who speaks,
//! the transcripts, the bot's text, barge-ins and the turn latencies), as [`EngineEvent`]s.
//!
//! # How the events are observed
//! [`SessionBuilder::with_events`](crate::SessionBuilder::with_events) installs an observer on
//! the inputs of five stages. It is not a stage: the stage's own task calls it with each frame
//! the stage is about to handle, before the stage sees it, so it adds no queue, no task and no
//! hop to the pipeline. The turn driver, the call's epoch authority, also publishes what it
//! decides itself ([`TurnDriver::with_events`](crate::TurnDriver::with_events)), at the moment
//! it decides it: a user turn can end without any frame for it (an empty turn, a held end a new
//! turn supersedes, the hang-up), and only the driver knows that. What each site reports:
//!
//! | site | frame or decision | event |
//! |---|---|---|
//! | turn driver (input) | `Transcript` from STT (interim and final, not empty) | [`EngineEvent::UserTranscript`] |
//! | turn driver (decision) | `Interrupt` | [`EngineEvent::BotInterrupted`] |
//! | turn driver (decision) | a spoken turn's `UserTurnStarted` | [`EngineEvent::UserStartedSpeaking`] |
//! | turn driver (decision) | the turn's final `Transcript` (the text the LLM answers) | [`EngineEvent::UserText`] |
//! | turn driver (decision) | the spoken turn's end: `UserTurnEnded`, an empty end, a superseded end, the hang-up | [`EngineEvent::UserStoppedSpeaking`] (once per start) |
//! | LLM runner (input, upstream) | `TtsStarted` / `TtsEnded` | [`EngineEvent::BotStartedSpeaking`] (+ [`EngineEvent::TurnMetrics`]) / [`EngineEvent::BotStoppedSpeaking`] |
//! | segmenter | `LlmResponseStarted`, `LlmText`, `LlmResponseEnded` | [`EngineEvent::BotLlmStarted`], [`EngineEvent::BotLlmText`], [`EngineEvent::BotLlmStopped`] |
//! | TTS stage | `LlmText` (the chunks spoken) | [`EngineEvent::BotTtsText`] |
//! | transport stage | `TtsEnded` (downstream: the TTS produced the last audio) | [`EngineEvent::BotTtsStopped`] |
//! | transport stage | `Error` | [`EngineEvent::Error`] |
//!
//! "User speaking" is the user's *turn* as the turn driver decides it (Pipecat's
//! `user-started-speaking` is its turn start too), not the raw VAD: a pause inside a turn is
//! not a stop, and a backchannel the interruption policy rejects starts nothing. A typed turn
//! (`crate::text_input`) is not speech: it raises no speaking events, only its `UserText`. "Bot speaking"
//! is the transport's playout: it starts when the first audio of the response is handed to the
//! transport and stops when the transport reports the end mark played.
//!
//! The runner, segmenter and TTS stage drop frames of ended epochs before the observer sees
//! them; the turn driver and the transport stage see everything, so an event from them may
//! belong to an epoch that a barge-in already ended (every bot event carries its epoch; the
//! RTVI bridge drops stale ones).
//!
//! # Delivery: bounded, drop-oldest, never blocking
//! Events go into a [`tokio::sync::broadcast`] ring ([`EngineEvents::new`], capacity rounded
//! up to a power of two). Publishing never waits and never allocates (the texts are the
//! frames' `Arc<str>`s): a subscriber that falls behind loses the OLDEST events, counted by
//! [`EventStream::lost`], so a slow UI can never stall the pipeline. Events from one site
//! keep their order; events from different sites are ordered as the stages happened to handle
//! them, which follows the causal order of the pipeline (a stage reports a frame before it
//! produces the frames that follow from it).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use brisvox_core::frame::{ControlFrame, ControlKind, DataFrame, Frame, SystemFrame};
use brisvox_core::{
    Direction, EpochPolicy, Processor, ProcessorContext, Result as CoreResult, TimelineRecorder,
    TurnEpoch, TurnMetrics,
};
use brisvox_providers::Usage;
use tokio::sync::broadcast;

use crate::llm::LlmStats;

/// Default capacity of an [`EngineEvents`] ring: about a minute of a talkative call's text
/// deltas.
pub const DEFAULT_EVENT_CAPACITY: usize = 1024;

/// Largest capacity of an [`EngineEvents`] ring; a larger request is clamped to it. The ring
/// allocates all its slots up front, so this bounds the memory of a call's events (65 536
/// slots, a few MB at most).
pub const MAX_EVENT_CAPACITY: usize = 1 << 16;

/// One conversation event (see the [module docs](self) for where each one comes from).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum EngineEvent {
    /// A user turn started (a confirmed barge-in starts one too, right after
    /// [`Self::BotInterrupted`]).
    UserStartedSpeaking {
        /// The turn's epoch.
        epoch: TurnEpoch,
    },
    /// The user turn ended: the engine answers it now.
    UserStoppedSpeaking {
        /// The turn's epoch.
        epoch: TurnEpoch,
    },
    /// A transcript from STT (interim or final segment), as the provider sent it.
    UserTranscript {
        /// The text.
        text: Arc<str>,
        /// `false` for an interim hypothesis.
        is_final: bool,
    },
    /// The whole text of the user turn the LLM answers (spoken finals and typed text joined).
    UserText {
        /// The turn's epoch.
        epoch: TurnEpoch,
        /// The text.
        text: Arc<str>,
    },
    /// The LLM started a response (one per round: a tool loop has several).
    BotLlmStarted {
        /// The response's epoch.
        epoch: TurnEpoch,
    },
    /// A streamed LLM text delta.
    BotLlmText {
        /// The response's epoch.
        epoch: TurnEpoch,
        /// The delta.
        text: Arc<str>,
    },
    /// The LLM finished the response of the epoch (the end of its last round: a tool loop has
    /// several rounds, one [`Self::BotLlmStarted`] each).
    BotLlmStopped {
        /// The response's epoch.
        epoch: TurnEpoch,
        /// What the provider reported for the provider responses of the turn, summed (for a
        /// turn of one round that is the round's usage). `None` when none of them reported
        /// usage, and for a speech-to-speech call.
        usage: Option<Usage>,
    },
    /// A chunk of text sent to the TTS (verbatim, before speech normalization).
    BotTtsText {
        /// The response's epoch.
        epoch: TurnEpoch,
        /// The chunk.
        text: Arc<str>,
    },
    /// The TTS produced the response's last audio.
    BotTtsStopped {
        /// The response's epoch.
        epoch: TurnEpoch,
    },
    /// The first audio of the response was handed to the transport.
    BotStartedSpeaking {
        /// The response's epoch.
        epoch: TurnEpoch,
    },
    /// The transport played the response's end.
    BotStoppedSpeaking {
        /// The response's epoch.
        epoch: TurnEpoch,
    },
    /// A barge-in: the call moved to `epoch` and everything older was dropped.
    BotInterrupted {
        /// The new epoch.
        epoch: TurnEpoch,
    },
    /// The latencies of the turn, from the call's timeline, at the moment its first audio went
    /// out (right after [`Self::BotStartedSpeaking`] of the same epoch).
    TurnMetrics {
        /// The turn's epoch.
        epoch: TurnEpoch,
        /// The stage latencies known then (`time_to_silence` is not known yet).
        metrics: TurnMetrics,
    },
    /// A stage reported an error.
    Error {
        /// The stage, e.g. `"stt"`.
        stage: &'static str,
        /// The rendered error (never contains credentials).
        message: String,
        /// Whether the call cannot continue.
        fatal: bool,
    },
}

/// The publishing end of a call's events: give it to
/// [`SessionBuilder::with_events`](crate::SessionBuilder::with_events) and
/// [`subscribe`](Self::subscribe) to read them. Cheap to clone.
///
/// The streams end ([`EventStream::recv`] returns `None`) once every `EngineEvents` clone is
/// gone, which happens when the call's pipeline is dropped if the application keeps no clone.
#[derive(Debug, Clone)]
pub struct EngineEvents {
    tx: broadcast::Sender<EngineEvent>,
    published: Arc<AtomicU64>,
}

impl EngineEvents {
    /// A ring of `capacity` events (clamped to `1..=`[`MAX_EVENT_CAPACITY`]; rounded up to a
    /// power of two).
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.clamp(1, MAX_EVENT_CAPACITY));
        Self {
            tx,
            published: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A stream of the events published from now on.
    pub fn subscribe(&self) -> EventStream {
        EventStream {
            rx: self.tx.subscribe(),
            lost: 0,
        }
    }

    /// Publishes `event` to every stream; never waits. Without a subscriber the event is
    /// dropped.
    pub fn publish(&self, event: EngineEvent) {
        self.published.fetch_add(1, Ordering::Relaxed);
        // Err only when nobody subscribed: nothing to deliver.
        let _ = self.tx.send(event);
    }

    /// Events published so far.
    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }
}

impl Default for EngineEvents {
    fn default() -> Self {
        Self::new(DEFAULT_EVENT_CAPACITY)
    }
}

/// One subscriber's view of a call's events.
#[derive(Debug)]
pub struct EventStream {
    rx: broadcast::Receiver<EngineEvent>,
    lost: u64,
}

impl EventStream {
    /// The next event; `None` once every [`EngineEvents`] is gone and the ring is drained.
    /// Events this stream fell too far behind for are skipped and counted ([`Self::lost`]).
    /// Cancel-safe.
    pub async fn recv(&mut self) -> Option<EngineEvent> {
        loop {
            match self.rx.recv().await {
                Ok(event) => return Some(event),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    self.lost = self.lost.saturating_add(skipped);
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// Events overwritten before this stream read them (the oldest go first).
    pub const fn lost(&self) -> u64 {
        self.lost
    }
}

/// Where an observer sits: the input of which stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Site {
    Driver,
    Runner,
    Segmenter,
    Tts,
    Out,
}

/// The event a frame entering the stage at `site` stands for, if any (the table of the module
/// docs). Pure.
pub(crate) fn event_of(site: Site, frame: &Frame, direction: Direction) -> Option<EngineEvent> {
    use Direction::{Downstream, Upstream};
    let control = |frame: &Frame| match frame {
        Frame::Control(ControlFrame { epoch, kind }) => Some((*epoch, *kind)),
        _ => None,
    };
    match (site, direction) {
        (Site::Driver, Downstream) => match frame {
            Frame::Data(DataFrame::Transcript(t)) if !t.text.trim().is_empty() => {
                Some(EngineEvent::UserTranscript {
                    text: Arc::clone(&t.text),
                    is_final: t.is_final,
                })
            }
            _ => None,
        },
        (Site::Runner, Upstream) => match control(frame)? {
            (epoch, ControlKind::TtsStarted) => Some(EngineEvent::BotStartedSpeaking { epoch }),
            (epoch, ControlKind::TtsEnded) => Some(EngineEvent::BotStoppedSpeaking { epoch }),
            _ => None,
        },
        (Site::Segmenter, Downstream) => match frame {
            Frame::Data(DataFrame::LlmText(t)) if !t.text.is_empty() => {
                Some(EngineEvent::BotLlmText {
                    epoch: t.epoch,
                    text: Arc::clone(&t.text),
                })
            }
            _ => match control(frame)? {
                (epoch, ControlKind::LlmResponseStarted) => {
                    Some(EngineEvent::BotLlmStarted { epoch })
                }
                (epoch, ControlKind::LlmResponseEnded) => {
                    Some(EngineEvent::BotLlmStopped { epoch, usage: None })
                }
                _ => None,
            },
        },
        (Site::Tts, Downstream) => match frame {
            Frame::Data(DataFrame::LlmText(t)) if !t.text.is_empty() => {
                Some(EngineEvent::BotTtsText {
                    epoch: t.epoch,
                    text: Arc::clone(&t.text),
                })
            }
            _ => None,
        },
        (Site::Out, Downstream) => match frame {
            Frame::System(SystemFrame::Error(error)) => Some(EngineEvent::Error {
                stage: error.stage,
                message: error.message.clone(),
                fatal: error.fatal,
            }),
            _ => match control(frame)? {
                (epoch, ControlKind::TtsEnded) => Some(EngineEvent::BotTtsStopped { epoch }),
                _ => None,
            },
        },
        _ => None,
    }
}

/// Publishes the events of the frames the stages handle.
#[derive(Debug, Clone)]
pub(crate) struct Observer {
    events: EngineEvents,
    timeline: TimelineRecorder,
    /// The LLM runner's counters: where the usage of a turn's responses is read at its end.
    llm: Option<LlmStats>,
}

impl Observer {
    pub(crate) const fn new(events: EngineEvents, timeline: TimelineRecorder) -> Self {
        Self {
            events,
            timeline,
            llm: None,
        }
    }

    /// Reads the usage of `BotLlmStopped` from the runner `llm`.
    pub(crate) fn with_llm(mut self, llm: LlmStats) -> Self {
        self.llm = Some(llm);
        self
    }

    fn observe(&self, site: Site, frame: &Frame, direction: Direction) {
        let Some(mut event) = event_of(site, frame, direction) else {
            return;
        };
        // The runner sums the usage of the turn's responses before it sends `LlmResponseEnded`.
        if let EngineEvent::BotLlmStopped { epoch, usage } = &mut event
            && let Some(llm) = &self.llm
        {
            *usage = llm.take_turn_usage(*epoch);
        }
        let started = match event {
            EngineEvent::BotStartedSpeaking { epoch } => Some(epoch),
            _ => None,
        };
        self.events.publish(event);
        // The transport stage marked `FirstAudioOut` before it sent `TtsStarted`: the turn's
        // record has its latencies now (one short lock per turn).
        if let Some(epoch) = started
            && let Some(record) = self.timeline.current()
            && record.epoch() == epoch
        {
            self.events.publish(EngineEvent::TurnMetrics {
                epoch,
                metrics: record.metrics(),
            });
        }
    }
}

/// A stage with an optional observer on its input; with none it is the stage itself.
pub(crate) struct Observed<P> {
    inner: P,
    observer: Option<(Site, Observer)>,
}

impl<P> Observed<P> {
    pub(crate) const fn new(inner: P, observer: Option<(Site, Observer)>) -> Self {
        Self { inner, observer }
    }
}

impl<P: Processor> Processor for Observed<P> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn epoch_policy(&self) -> EpochPolicy {
        self.inner.epoch_policy()
    }

    async fn process(
        &mut self,
        frame: Frame,
        direction: Direction,
        ctx: &mut ProcessorContext,
    ) -> CoreResult<()> {
        if let Some((site, observer)) = &self.observer {
            observer.observe(*site, &frame, direction);
        }
        self.inner.process(frame, direction, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::frame::{ErrorFrame, TextFrame, TranscriptFrame};

    use super::*;

    fn now() -> tokio::time::Instant {
        #[allow(clippy::disallowed_methods)] // tests need some Instant value
        tokio::time::Instant::now()
    }

    fn ep(n: u32) -> TurnEpoch {
        let mut epoch = TurnEpoch::ZERO;
        for _ in 0..n {
            epoch = epoch.next();
        }
        epoch
    }

    fn control(epoch: TurnEpoch, kind: ControlKind) -> Frame {
        ControlFrame { epoch, kind }.into()
    }

    fn transcript(text: &str, is_final: bool) -> Frame {
        DataFrame::Transcript(TranscriptFrame {
            epoch: ep(1),
            text: Arc::from(text),
            is_final,
            received_at: now(),
        })
        .into()
    }

    fn llm_text(text: &str) -> Frame {
        DataFrame::LlmText(TextFrame {
            epoch: ep(1),
            text: Arc::from(text),
        })
        .into()
    }

    #[test]
    fn every_site_reports_its_frames_and_nothing_else() {
        use Direction::{Downstream, Upstream};
        let e = ep(1);
        let cases: Vec<(Site, Frame, Direction, Option<EngineEvent>)> = vec![
            (
                Site::Driver,
                transcript("hel", false),
                Downstream,
                Some(EngineEvent::UserTranscript {
                    text: Arc::from("hel"),
                    is_final: false,
                }),
            ),
            (Site::Driver, transcript("  ", true), Downstream, None),
            (
                Site::Driver,
                control(e, ControlKind::TtsStarted),
                Upstream,
                None,
            ),
            // The runner's input reports none of the user's frames: the turn driver publishes
            // its own decisions (an empty or superseded turn has no frame at all).
            (
                Site::Runner,
                SystemFrame::Interrupt { new_epoch: e }.into(),
                Downstream,
                None,
            ),
            (
                Site::Runner,
                control(e, ControlKind::UserTurnStarted),
                Downstream,
                None,
            ),
            (
                Site::Runner,
                transcript("hello there", true),
                Downstream,
                None,
            ),
            (Site::Runner, transcript("hello", false), Downstream, None),
            (
                Site::Runner,
                control(e, ControlKind::UserTurnEnded),
                Downstream,
                None,
            ),
            (
                Site::Runner,
                control(e, ControlKind::TtsStarted),
                Upstream,
                Some(EngineEvent::BotStartedSpeaking { epoch: e }),
            ),
            (
                Site::Runner,
                control(e, ControlKind::TtsEnded),
                Upstream,
                Some(EngineEvent::BotStoppedSpeaking { epoch: e }),
            ),
            (
                Site::Runner,
                control(e, ControlKind::TtsEnded),
                Downstream,
                None,
            ),
            (
                Site::Segmenter,
                control(e, ControlKind::LlmResponseStarted),
                Downstream,
                Some(EngineEvent::BotLlmStarted { epoch: e }),
            ),
            (
                Site::Segmenter,
                llm_text("Hi"),
                Downstream,
                Some(EngineEvent::BotLlmText {
                    epoch: e,
                    text: Arc::from("Hi"),
                }),
            ),
            (Site::Segmenter, llm_text(""), Downstream, None),
            (
                Site::Segmenter,
                control(e, ControlKind::LlmResponseEnded),
                Downstream,
                Some(EngineEvent::BotLlmStopped {
                    epoch: e,
                    usage: None,
                }),
            ),
            (
                Site::Tts,
                llm_text("Hi there."),
                Downstream,
                Some(EngineEvent::BotTtsText {
                    epoch: e,
                    text: Arc::from("Hi there."),
                }),
            ),
            (Site::Tts, control(e, ControlKind::TtsEnded), Upstream, None),
            (
                Site::Out,
                control(e, ControlKind::TtsEnded),
                Downstream,
                Some(EngineEvent::BotTtsStopped { epoch: e }),
            ),
            (
                Site::Out,
                SystemFrame::Error(Box::new(ErrorFrame {
                    stage: "stt",
                    message: "lost".to_owned(),
                    fatal: false,
                }))
                .into(),
                Downstream,
                Some(EngineEvent::Error {
                    stage: "stt",
                    message: "lost".to_owned(),
                    fatal: false,
                }),
            ),
            (
                Site::Out,
                control(e, ControlKind::TtsStarted),
                Upstream,
                None,
            ),
        ];
        for (site, frame, direction, expected) in cases {
            assert_eq!(
                event_of(site, &frame, direction),
                expected,
                "{site:?} {direction:?} {frame:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_slow_subscriber_loses_the_oldest_events_and_publishing_never_waits() {
        let events = EngineEvents::new(4);
        let mut stream = events.subscribe();
        for n in 0..10_u32 {
            events.publish(EngineEvent::BotInterrupted { epoch: ep(n) });
        }
        // The ring kept the newest four.
        assert_eq!(
            stream.recv().await,
            Some(EngineEvent::BotInterrupted { epoch: ep(6) })
        );
        assert_eq!(stream.lost(), 6);
        assert_eq!(events.published(), 10);
        drop(events);
        for n in 7..10 {
            assert_eq!(
                stream.recv().await,
                Some(EngineEvent::BotInterrupted { epoch: ep(n) })
            );
        }
        assert_eq!(stream.recv().await, None);
    }

    #[test]
    fn a_huge_capacity_is_clamped_instead_of_allocated_or_panicking() {
        // `broadcast::channel` panics above `usize::MAX >> 1` and allocates every slot up front.
        for capacity in [usize::MAX, usize::MAX >> 1, 50_000_000] {
            let events = EngineEvents::new(capacity);
            let mut stream = events.subscribe();
            events.publish(EngineEvent::BotInterrupted { epoch: ep(1) });
            assert_eq!(
                futures::FutureExt::now_or_never(stream.recv()),
                Some(Some(EngineEvent::BotInterrupted { epoch: ep(1) }))
            );
        }
        let events = EngineEvents::new(0);
        let _stream = events.subscribe();
        events.publish(EngineEvent::BotInterrupted { epoch: ep(1) });
    }
}
