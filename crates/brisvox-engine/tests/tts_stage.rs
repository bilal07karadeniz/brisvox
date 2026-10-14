//! The TTS stage over the simulator's `MockTts` on a paused clock: contexts and
//! utterances, exact frames at the call's output rate, words, the end of the response, barge-in,
//! provider faults (disconnect, stall, connect failures), normalization and the book.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::cast_possible_truncation
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::Language;
use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, Frame, SampleRate, SystemFrame, TextFrame,
    WordTimestamp,
};
use brisvox_core::{
    Links, Pipeline, RunningPipeline, SharedClock, SimClock, TimelineRecorder, TurnEpoch, TurnEvent,
};
use brisvox_engine::{
    CallRatesCell, EngineConfig, Offload, PlayoutBook, RoundKey, TtsStage, TtsStageConfig, TtsStats,
};
use brisvox_sim::mock_common::StreamFault;
use brisvox_sim::mock_tts::MockTts;
use brisvox_transport::CallRates;
use common::{Recorded, Recorder};
use tokio_util::sync::CancellationToken;

const MS: Duration = Duration::from_millis(1);

fn text(epoch: u32, text: &str) -> Frame {
    Frame::Data(DataFrame::LlmText(TextFrame {
        epoch: TurnEpoch::new(epoch),
        text: Arc::from(text),
    }))
}

fn control(epoch: u32, kind: ControlKind) -> Frame {
    Frame::Control(ControlFrame {
        epoch: TurnEpoch::new(epoch),
        kind,
    })
}

fn interrupt(epoch: u32) -> Frame {
    Frame::System(SystemFrame::Interrupt {
        new_epoch: TurnEpoch::new(epoch),
    })
}

fn key(epoch: u32, round: u32) -> RoundKey {
    RoundKey::new(TurnEpoch::new(epoch), round)
}

struct Rig {
    running: RunningPipeline,
    rec: Recorded,
    clock: SharedClock,
    tts: MockTts,
    stats: TtsStats,
    book: Arc<PlayoutBook>,
    timeline: TimelineRecorder,
}

struct Options {
    output: SampleRate,
    language: Language,
    tweak: Box<dyn FnOnce(&mut TtsStageConfig)>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            output: SampleRate::Hz16000,
            language: Language::En,
            tweak: Box::new(|_| {}),
        }
    }
}

impl Rig {
    async fn new() -> Self {
        Self::with(Options::default(), MockTts::new).await
    }

    async fn with(options: Options, tts: impl FnOnce(SharedClock) -> MockTts) -> Self {
        let clock = SimClock::shared();
        let tts = tts(clock.clone());
        let mut config =
            TtsStageConfig::from_engine(&EngineConfig::DEFAULT).with_language(options.language);
        (options.tweak)(&mut config);
        let timeline = TimelineRecorder::new();
        let book = PlayoutBook::shared(16);
        let stage = TtsStage::new(
            Arc::new(tts.clone()),
            config,
            clock.clone(),
            timeline.clone(),
        )
        .with_book(Arc::clone(&book))
        .with_rates(CallRatesCell::new(CallRates::symmetric(options.output)))
        .with_offload(Offload::Inline(clock.clone()));
        let stats = stage.stats();
        let rec = Recorded::default();
        let running = Pipeline::new("tts")
            .then(stage)
            .then(Recorder(rec.clone()))
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
        // Let the Start frame reach the stage (it connects the session).
        for _ in 0..30 {
            tokio::task::yield_now().await;
        }
        Self {
            running,
            rec,
            clock,
            tts,
            stats,
            book,
            timeline,
        }
    }

    async fn feed(&self, frame: Frame) {
        self.running.input().send(frame).await.unwrap();
    }

    /// Lets `d` of simulated time pass (everything that is due runs).
    async fn run_for(&self, d: Duration) {
        self.clock.sleep(d).await;
    }

    /// One whole response: started, the chunks, ended.
    async fn respond(&self, epoch: u32, chunks: &[&str]) {
        self.feed(control(epoch, ControlKind::LlmResponseStarted))
            .await;
        for chunk in chunks {
            self.feed(text(epoch, chunk)).await;
        }
        self.feed(control(epoch, ControlKind::LlmResponseEnded))
            .await;
    }

    fn frames(&self) -> Vec<Frame> {
        self.rec.all().into_iter().map(|(f, _)| f).collect()
    }

    /// The frames in order as short names: `started:<utterance>`, `audio`, `word:<w>`,
    /// `tts_ended:<epoch>` and the other control kinds by name.
    fn names(&self) -> Vec<String> {
        self.frames()
            .into_iter()
            .filter_map(|frame| match frame {
                Frame::Control(ControlFrame {
                    kind: ControlKind::UtteranceStarted { utterance },
                    ..
                }) => Some(format!("started:{utterance}")),
                Frame::Control(ControlFrame {
                    epoch,
                    kind: ControlKind::TtsEnded,
                }) => Some(format!("tts_ended:{}", epoch.get())),
                Frame::Control(c) => Some(c.kind.name().to_owned()),
                Frame::Data(DataFrame::Audio(_)) => Some("audio".to_owned()),
                Frame::Data(DataFrame::Word(w)) => Some(format!("word:{}", w.word)),
                _ => None,
            })
            .collect()
    }

    /// The `Word` frames in order.
    fn words(&self) -> Vec<WordTimestamp> {
        self.frames()
            .into_iter()
            .filter_map(|frame| match frame {
                Frame::Data(DataFrame::Word(w)) => Some(*w),
                _ => None,
            })
            .collect()
    }

    fn audio(&self) -> Vec<AudioFrame> {
        self.frames()
            .into_iter()
            .filter_map(|frame| match frame {
                Frame::Data(DataFrame::Audio(a)) => Some(a),
                _ => None,
            })
            .collect()
    }

    fn audio_of(&self, epoch: u32) -> Duration {
        self.audio()
            .iter()
            .filter(|a| a.epoch() == TurnEpoch::new(epoch))
            .map(AudioFrame::duration)
            .sum()
    }

    fn count(&self, name: &str) -> usize {
        self.names().iter().filter(|n| n.starts_with(name)).count()
    }
}

/// How far `got` is from `want`, absolute.
fn off(got: Duration, want: Duration) -> Duration {
    got.abs_diff(want)
}

#[tokio::test(start_paused = true)]
async fn a_response_is_exact_frames_at_the_call_rate_with_words_and_one_end() {
    let rig = Rig::new().await;
    assert_eq!(rig.tts.connects(), 1, "connected when the pipeline started");
    rig.respond(1, &["Hello there"]).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    let names = rig.names();
    // UtteranceStarted precedes the first audio and every word; one end, after all audio.
    let started = names.iter().position(|n| n == "started:1").unwrap();
    let first_audio = names.iter().position(|n| n == "audio").unwrap();
    let last_audio = names.iter().rposition(|n| n == "audio").unwrap();
    let ended = names.iter().position(|n| n == "tts_ended:1").unwrap();
    assert!(started < first_audio, "{names:?}");
    assert!(last_audio < ended, "{names:?}");
    assert_eq!(rig.count("tts_ended"), 1);
    assert_eq!(rig.count("started"), 1);
    assert_eq!(names.last().unwrap(), "tts_ended:1", "{names:?}");
    // 11 characters at 60 ms each.
    assert!(
        off(rig.audio_of(1), 660 * MS) <= 2 * MS,
        "{:?}",
        rig.audio_of(1)
    );
    // Every frame is 20 ms at 16 kHz except possibly the last of the utterance.
    let frames = rig.audio();
    assert!(
        frames
            .iter()
            .all(|f| f.sample_rate() == SampleRate::Hz16000)
    );
    let (last, whole) = frames.split_last().unwrap();
    assert!(
        whole.iter().all(|f| f.duration() == 20 * MS),
        "exact 20 ms frames"
    );
    assert!(last.duration() <= 20 * MS);
    // Sequence numbers count up.
    assert!(frames.windows(2).all(|w| w[1].seq() == w[0].seq() + 1));
    // The words, in order.
    let words: Vec<_> = names.iter().filter(|n| n.starts_with("word:")).collect();
    assert_eq!(words, ["word:Hello", "word:there"]);
    // The book knows the round, the verbatim text and the words are the transport stage's.
    let snapshot = rig.book.snapshot(key(1, 0)).unwrap();
    assert_eq!(snapshot.utterance, Some(1));
    assert_eq!(snapshot.played.sent(), "Hello there");
    assert!(!snapshot.lost);
}

#[tokio::test(start_paused = true)]
async fn each_text_bearing_round_is_one_utterance_and_the_end_comes_once_at_the_end() {
    let rig = Rig::new().await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Let me check")).await;
    rig.feed(control(1, ControlKind::Flush)).await;
    // Round 2 is a filler-less pause (a tool call): no text, no context.
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(control(1, ControlKind::Flush)).await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "It is nine")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(3 * Duration::from_secs(1)).await;
    let names = rig.names();
    assert_eq!(rig.count("started"), 2, "{names:?}");
    assert_eq!(rig.count("tts_ended"), 1, "{names:?}");
    assert_eq!(names.last().unwrap(), "tts_ended:1");
    // Round ordinals are the LLM runner's: the textless middle round still counts.
    assert_eq!(rig.book.snapshot(key(1, 0)).unwrap().utterance, Some(1));
    assert_eq!(rig.book.snapshot(key(1, 2)).unwrap().utterance, Some(2));
    assert_eq!(
        rig.book.snapshot(key(1, 2)).unwrap().played.sent(),
        "It is nine"
    );
    assert!(
        rig.book
            .snapshot(key(1, 1))
            .is_none_or(|s| s.utterance.is_none())
    );
    // Both utterances were spoken completely; the first one's end did not end the response.
    assert!(off(rig.audio_of(1), (12 + 10) * 60 * MS) <= 4 * MS);
    let first_end = names
        .iter()
        .position(|n| n.starts_with("tts_ended"))
        .unwrap();
    let second_start = names.iter().position(|n| n == "started:2").unwrap();
    assert!(second_start < first_end, "{names:?}");
}

#[tokio::test(start_paused = true)]
async fn an_empty_response_ends_at_once_and_only_once() {
    let rig = Rig::new().await;
    rig.respond(1, &[]).await;
    rig.run_for(50 * MS).await;
    assert_eq!(
        rig.names().iter().filter(|n| *n == "tts_ended:1").count(),
        1
    );
    assert_eq!(rig.count("started"), 0);
    // The same response cannot end twice.
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(50 * MS).await;
    assert_eq!(rig.count("tts_ended"), 1);
    assert_eq!(rig.stats.snapshot().duplicate_ends, 1);
}

#[tokio::test(start_paused = true)]
async fn context_ids_are_monotonic_across_turns_and_each_turn_ends_once() {
    let rig = Rig::new().await;
    rig.respond(1, &["First answer."]).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    rig.respond(2, &["Second answer."]).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    rig.respond(3, &[]).await;
    rig.run_for(100 * MS).await;
    let names = rig.names();
    assert_eq!(
        names
            .iter()
            .filter(|n| n.starts_with("started") || n.starts_with("tts_ended"))
            .collect::<Vec<_>>(),
        [
            "started:1",
            "tts_ended:1",
            "started:2",
            "tts_ended:2",
            "tts_ended:3"
        ]
    );
    assert_eq!(rig.book.snapshot(key(2, 0)).unwrap().utterance, Some(2));
    assert_eq!(rig.tts.connects(), 1, "one session serves the whole call");
}

#[tokio::test(start_paused = true)]
async fn timeline_marks_the_first_sentence_the_request_and_the_first_byte() {
    // The provider's first byte comes 300 ms after the text: TtsFirstByte is marked when the
    // first audio arrives, not when the text is sent.
    let rig = Rig::with(Options::default(), |clock| {
        MockTts::new(clock).with_latency(
            brisvox_sim::latency::LatencyProfile::ZERO,
            brisvox_sim::latency::LatencyProfile::Fixed(300 * MS),
            brisvox_sim::latency::LatencyProfile::ZERO,
            1,
        )
    })
    .await;
    // Some time passes before the text so that the marks are not at the clock's origin.
    rig.run_for(Duration::from_secs(1)).await;
    let before = rig.clock.now();
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Hello there")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(Duration::from_secs(2)).await;
    rig.timeline.finish();
    let record = rig
        .timeline
        .history()
        .into_iter()
        .find(|r| r.at(TurnEvent::TtsRequest).is_some())
        .unwrap();
    let sentence = record.at(TurnEvent::LlmFirstSentence).unwrap();
    let request = record.at(TurnEvent::TtsRequest).unwrap();
    let first_byte = record.at(TurnEvent::TtsFirstByte).unwrap();
    // The first text of the response, marked once with the two above (no mark per chunk).
    let first_text = record.at(TurnEvent::TtsFirstText).unwrap();
    assert!(first_text >= before && first_text <= sentence);
    assert_eq!(rig.timeline.stats().repeated, 0, "once per response");
    assert!(sentence >= before, "marked when the text arrived");
    assert!(sentence <= request, "sentence then request");
    assert!(request < first_byte, "the first byte follows the request");
    assert!(
        request.duration_since(before) <= 2 * MS,
        "the request is marked when the text arrives: {:?}",
        request.duration_since(before)
    );
    let latency = first_byte.duration_since(request);
    assert!(
        off(latency, 300 * MS) <= 20 * MS,
        "first byte {latency:?} after the request, the mock's latency is 300 ms"
    );
}

#[tokio::test(start_paused = true)]
async fn resampling_to_the_call_rate_keeps_the_duration_for_8_16_and_24_khz() {
    for output in [
        SampleRate::Hz8000,
        SampleRate::Hz16000,
        SampleRate::Hz24000,
        SampleRate::Hz48000,
    ] {
        let rig = Rig::with(
            Options {
                output,
                ..Options::default()
            },
            MockTts::new,
        )
        .await;
        rig.respond(1, &["Hello there"]).await;
        rig.run_for(2 * Duration::from_secs(1)).await;
        let frames = rig.audio();
        assert!(
            frames.iter().all(|f| f.sample_rate() == output),
            "{output:?}"
        );
        let (last, whole) = frames.split_last().unwrap();
        assert!(
            whole.iter().all(|f| f.duration() == 20 * MS),
            "{output:?}: exact frames"
        );
        assert!(last.duration() <= 20 * MS);
        assert!(
            off(rig.audio_of(1), 660 * MS) <= 2 * MS,
            "{output:?}: {:?}",
            rig.audio_of(1)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn an_interrupt_cancels_the_context_and_late_audio_never_comes_out() {
    // First byte after 300 ms, one chunk every 40 ms.
    let rig = Rig::with(Options::default(), |clock| {
        MockTts::new(clock).with_latency(
            brisvox_sim::latency::LatencyProfile::ZERO,
            brisvox_sim::latency::LatencyProfile::Fixed(300 * MS),
            brisvox_sim::latency::LatencyProfile::Fixed(40 * MS),
            1,
        )
    })
    .await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "This is a rather long sentence to speak aloud"))
        .await;
    rig.run_for(500 * MS).await;
    let before = rig.audio().len();
    assert!(before > 0, "audio is flowing");
    rig.feed(interrupt(2)).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    let epoch1_after: usize = rig
        .frames()
        .iter()
        .skip_while(|f| !matches!(f, Frame::System(SystemFrame::Interrupt { .. })))
        .filter(|f| matches!(f, Frame::Data(DataFrame::Audio(a)) if a.epoch() == TurnEpoch::new(1)))
        .count();
    assert_eq!(
        epoch1_after, 0,
        "no audio of the cut epoch after the interrupt"
    );
    assert!(
        rig.audio().len() < before + 4,
        "the provider's output stopped"
    );
    assert_eq!(rig.stats.snapshot().cancels, 1);
    // No TtsEnded for the cut response, and the next response is clean.
    assert_eq!(rig.count("tts_ended"), 0);
    rig.respond(2, &["Okay then"]).await;
    rig.run_for(3 * Duration::from_secs(1)).await;
    assert_eq!(rig.count("tts_ended:2"), 1);
    assert!(
        off(rig.audio_of(2), 540 * MS) <= 2 * MS,
        "{:?}",
        rig.audio_of(2)
    );
}

#[tokio::test(start_paused = true)]
async fn chunks_are_spoken_normalized_but_recorded_verbatim() {
    let rig = Rig::with(
        Options {
            language: Language::Tr,
            ..Options::default()
        },
        MockTts::new,
    )
    .await;
    let raw = " Toplam 12 TL";
    rig.respond(1, &["Merhaba", raw]).await;
    rig.run_for(3 * Duration::from_secs(1)).await;
    let spoken: Vec<String> = rig.tts.texts().into_iter().map(|(_, t)| t).collect();
    assert_eq!(spoken[0], "Merhaba");
    assert_ne!(spoken[1], raw, "the digits are spelled out: {spoken:?}");
    assert!(
        spoken[1].starts_with(" Toplam "),
        "leading space kept: {spoken:?}"
    );
    assert!(!spoken[1].contains('1'), "{spoken:?}");
    // What the book holds is what the segmenter sent.
    assert_eq!(
        rig.book.snapshot(key(1, 0)).unwrap().played.sent(),
        format!("Merhaba{raw}")
    );
    assert_eq!(rig.stats.snapshot().normalized, 1);
}

#[tokio::test(start_paused = true)]
async fn a_long_chunk_is_normalized_on_the_offload_and_gives_the_same_text() {
    let rig = Rig::with(
        Options {
            language: Language::Tr,
            ..Options::default()
        },
        MockTts::new,
    )
    .await;
    let long = format!("Fiyat 25 TL. {}", "kelime ".repeat(60));
    assert!(long.chars().count() > 256);
    rig.respond(1, &[&long]).await;
    rig.run_for(30 * Duration::from_secs(1)).await;
    let spoken = rig.tts.texts().remove(0).1;
    let expected = brisvox_agent::normalize(&long, Language::Tr).into_owned();
    assert_eq!(spoken, expected);
    assert_ne!(spoken, long);
    let stats = rig.stats.snapshot();
    assert_eq!(stats.normalize_offloaded, 1);
    assert_eq!(stats.normalize_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn a_provider_without_word_timestamps_speaks_without_words() {
    let rig = Rig::with(Options::default(), |clock| {
        MockTts::new(clock).without_word_timings()
    })
    .await;
    rig.respond(1, &["Hello there"]).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    assert_eq!(rig.count("word:"), 0);
    assert_eq!(rig.count("tts_ended"), 1);
    assert!(off(rig.audio_of(1), 660 * MS) <= 2 * MS);
    assert_eq!(rig.stats.snapshot().words, 0);
}

#[tokio::test(start_paused = true)]
async fn a_disconnect_loses_the_utterance_ends_the_response_and_the_next_turn_reconnects() {
    // The session dies after its third event (a word, the other word, the first audio).
    let rig = Rig::with(Options::default(), |clock| {
        let tts = MockTts::new(clock);
        tts.push_session_faults(vec![StreamFault::Disconnect { after_events: 3 }]);
        tts
    })
    .await;
    rig.respond(1, &["A fairly long sentence that dies half way through"])
        .await;
    rig.run_for(3 * Duration::from_secs(1)).await;
    assert_eq!(rig.count("tts_ended:1"), 1, "{:?}", rig.names());
    let snapshot = rig.book.snapshot(key(1, 0)).unwrap();
    assert!(snapshot.lost, "the utterance is marked lost");
    assert!(
        rig.audio_of(1) < Duration::from_secs(1),
        "only what was received before the drop is spoken"
    );
    let stats = rig.stats.snapshot();
    assert_eq!(stats.session_deaths, 1);
    assert_eq!(stats.contexts_lost, 1);
    // The next response is spoken on a new session.
    rig.respond(2, &["Back again"]).await;
    rig.run_for(3 * Duration::from_secs(1)).await;
    assert_eq!(rig.tts.connects(), 2);
    assert_eq!(rig.count("tts_ended:2"), 1);
    assert!(
        off(rig.audio_of(2), 600 * MS) <= 2 * MS,
        "{:?}",
        rig.audio_of(2)
    );
    assert!(!rig.book.snapshot(key(2, 0)).unwrap().lost);
}

#[tokio::test(start_paused = true)]
async fn a_disconnect_after_the_response_ended_still_gives_exactly_one_end() {
    // The first byte comes 500 ms late; the response is complete (Flush sent, end requested)
    // long before, and the session dies on its first event.
    let rig = Rig::with(Options::default(), |clock| {
        let tts = MockTts::new(clock).with_latency(
            brisvox_sim::latency::LatencyProfile::ZERO,
            brisvox_sim::latency::LatencyProfile::Fixed(500 * MS),
            brisvox_sim::latency::LatencyProfile::ZERO,
            1,
        );
        tts.push_session_faults(vec![StreamFault::Disconnect { after_events: 1 }]);
        tts
    })
    .await;
    rig.respond(1, &["Short one"]).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    assert_eq!(rig.count("tts_ended:1"), 1, "{:?}", rig.names());
    assert!(rig.book.snapshot(key(1, 0)).unwrap().lost);
}

#[tokio::test(start_paused = true)]
async fn a_stall_delays_the_audio_but_the_response_completes() {
    let rig = Rig::with(Options::default(), |clock| {
        let tts = MockTts::new(clock);
        tts.push_session_faults(vec![StreamFault::Stall {
            before_event: 2,
            duration: Duration::from_secs(2),
        }]);
        tts
    })
    .await;
    rig.respond(1, &["Hello there"]).await;
    rig.run_for(Duration::from_secs(1)).await;
    assert_eq!(rig.count("tts_ended"), 0, "still stalled");
    rig.run_for(4 * Duration::from_secs(1)).await;
    assert_eq!(rig.count("tts_ended:1"), 1);
    assert!(off(rig.audio_of(1), 660 * MS) <= 2 * MS);
    assert_eq!(rig.stats.snapshot().stalls, 0);
}

#[tokio::test(start_paused = true)]
async fn a_provider_that_goes_silent_is_given_up_on_and_the_next_turn_reconnects() {
    let rig = Rig::with(
        Options {
            tweak: Box::new(|config| config.idle_timeout = Duration::from_secs(3)),
            ..Options::default()
        },
        |clock| {
            let tts = MockTts::new(clock);
            tts.push_session_faults(vec![StreamFault::Stall {
                before_event: 1,
                duration: Duration::from_secs(3600),
            }]);
            tts
        },
    )
    .await;
    rig.respond(1, &["Hello there"]).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    assert_eq!(
        rig.count("tts_ended"),
        0,
        "not yet: the provider owes events for 3 s"
    );
    rig.run_for(3 * Duration::from_secs(1)).await;
    assert_eq!(rig.count("tts_ended:1"), 1, "{:?}", rig.names());
    let stats = rig.stats.snapshot();
    assert_eq!(stats.stalls, 1);
    assert_eq!(stats.contexts_lost, 1);
    assert!(rig.book.snapshot(key(1, 0)).unwrap().lost);
    rig.respond(2, &["Fine now"]).await;
    rig.run_for(3 * Duration::from_secs(1)).await;
    assert_eq!(rig.tts.connects(), 2);
    assert_eq!(rig.count("tts_ended:2"), 1);
    assert!(off(rig.audio_of(2), 480 * MS) <= 2 * MS);
}

#[tokio::test(start_paused = true)]
async fn a_failed_connect_drops_the_speech_ends_the_response_and_is_retried_after_the_backoff() {
    let rig = Rig::with(Options::default(), |clock| {
        let tts = MockTts::new(clock);
        tts.push_session_faults(vec![StreamFault::ConnectError { status: 503 }]);
        tts
    })
    .await;
    assert_eq!(rig.stats.snapshot().connect_failures, 1);
    // Inside the 1 s backoff the text cannot be spoken, but the response still ends.
    rig.respond(1, &["Nobody hears this"]).await;
    rig.run_for(100 * MS).await;
    assert_eq!(rig.count("started"), 0);
    assert_eq!(rig.count("tts_ended:1"), 1);
    assert!(rig.stats.snapshot().text_dropped >= 1);
    // After the backoff the next response connects and is spoken.
    rig.run_for(2 * Duration::from_secs(1)).await;
    rig.respond(2, &["Hello again"]).await;
    rig.run_for(3 * Duration::from_secs(1)).await;
    assert_eq!(rig.tts.connects(), 2);
    assert_eq!(rig.count("tts_ended:2"), 1);
    assert!(off(rig.audio_of(2), 660 * MS) <= 2 * MS);
}

#[tokio::test(start_paused = true)]
async fn a_connect_that_never_finishes_times_out_and_does_not_block_the_stage_for_ever() {
    let rig = Rig::with(Options::default(), |clock| {
        let tts = MockTts::new(clock);
        tts.push_session_faults(vec![StreamFault::ConnectStall {
            duration: Duration::from_secs(3600),
        }]);
        tts
    })
    .await;
    // The prewarm connect gave up after the 5 s connect timeout.
    rig.run_for(6 * Duration::from_secs(1)).await;
    assert_eq!(rig.stats.snapshot().connect_failures, 1);
    rig.respond(1, &[]).await;
    rig.run_for(100 * MS).await;
    assert_eq!(rig.count("tts_ended:1"), 1);
}

#[tokio::test(start_paused = true)]
async fn repeated_connect_failures_degrade_the_stage_until_the_next_turn() {
    let rig = Rig::with(Options::default(), |clock| {
        let tts = MockTts::new(clock);
        for _ in 0..8 {
            tts.push_session_faults(vec![StreamFault::ConnectError { status: 503 }]);
        }
        tts
    })
    .await;
    // The prewarm failed (1). Every response retries after the backoff within one turn: the
    // third retry exhausts the budget (max_reconnects 3), after which the stage stops trying
    // for the rest of the turn.
    for round in 0..6 {
        rig.run_for(1100 * MS).await;
        rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
        rig.feed(text(1, &format!("round {round}"))).await;
        rig.feed(control(1, ControlKind::Flush)).await;
    }
    rig.run_for(Duration::from_secs(1)).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.degraded, 1, "{stats:?}");
    let connects_in_turn = rig.tts.connects();
    assert!(connects_in_turn <= 5, "{connects_in_turn}");
    // The next turn grants a fresh budget.
    rig.respond(2, &["again"]).await;
    rig.run_for(100 * MS).await;
    assert!(
        rig.tts.connects() > connects_in_turn,
        "a new turn tries again"
    );
}

#[tokio::test(start_paused = true)]
async fn upstream_frames_go_on_upstream_without_blocking() {
    let rig = Rig::new().await;
    // Nothing is upstream of the TTS stage here: the frame is dropped at the edge.
    rig.running
        .upstream_input()
        .send(control(1, ControlKind::TtsStarted))
        .await
        .unwrap();
    rig.respond(1, &["Still fine"]).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    assert_eq!(rig.count("tts_ended:1"), 1);
}

#[tokio::test(start_paused = true)]
async fn a_new_epoch_without_an_interrupt_cancels_what_the_dropped_turn_left_open() {
    let rig = Rig::with(Options::default(), |clock| {
        MockTts::new(clock).with_latency(
            brisvox_sim::latency::LatencyProfile::ZERO,
            brisvox_sim::latency::LatencyProfile::Fixed(200 * MS),
            brisvox_sim::latency::LatencyProfile::ZERO,
            1,
        )
    })
    .await;
    // Turn 1 sends text and never ends (the runner dropped it); turn 2 starts right away.
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Abandoned words here")).await;
    rig.run_for(50 * MS).await;
    rig.respond(2, &["Fresh"]).await;
    rig.run_for(3 * Duration::from_secs(1)).await;
    assert_eq!(rig.stats.snapshot().cancels, 1);
    assert_eq!(
        rig.audio_of(1),
        Duration::ZERO,
        "the abandoned speech never plays"
    );
    assert_eq!(rig.count("tts_ended:2"), 1);
}

/// Real time, a real inference pool: a long chunk is normalized and the (sinc) resampler for
/// 8 kHz output is built on the pool, not on the runtime's workers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_real_pool_normalization_and_the_resampler_run_off_the_workers() {
    use brisvox_core::SystemClock;
    use brisvox_inference::{InferencePool, PoolConfig};
    let clock = SystemClock::shared();
    let pool = InferencePool::new(
        &PoolConfig {
            threads: 1,
            queue_capacity: 8,
            name: "tts-pool-test".to_owned(),
        },
        clock.clone(),
    )
    .unwrap();
    let tts = MockTts::new(clock.clone()).with_audio(1, 20 * MS);
    let config = TtsStageConfig::from_engine(&EngineConfig::DEFAULT).with_language(Language::Tr);
    let stage = TtsStage::new(
        Arc::new(tts.clone()),
        config,
        clock.clone(),
        TimelineRecorder::new(),
    )
    .with_rates(CallRatesCell::new(CallRates::symmetric(SampleRate::Hz8000)))
    .with_offload(Offload::Pool(pool.handle()));
    let stats = stage.stats();
    let rec = Recorded::default();
    let running = Pipeline::new("tts-rt")
        .then(stage)
        .then(Recorder(rec.clone()))
        .start(Links::default(), &CancellationToken::new())
        .await
        .unwrap();
    let long = format!("Fiyat 25 TL. {}", "kelime ".repeat(60));
    for frame in [
        control(1, ControlKind::LlmResponseStarted),
        text(1, &long),
        control(1, ControlKind::LlmResponseEnded),
    ] {
        running.input().send(frame).await.unwrap();
    }
    let ended = |rec: &Recorded| {
        rec.all()
            .iter()
            .any(|(f, _)| matches!(f, Frame::Control(c) if c.kind == ControlKind::TtsEnded))
    };
    for _ in 0..500 {
        if ended(&rec) {
            break;
        }
        tokio::time::sleep(10 * MS).await;
    }
    assert!(ended(&rec), "the response ended: {:?}", stats.snapshot());
    let frames: Vec<AudioFrame> = rec
        .all()
        .into_iter()
        .filter_map(|(f, _)| match f {
            Frame::Data(DataFrame::Audio(a)) => Some(a),
            _ => None,
        })
        .collect();
    assert!(!frames.is_empty());
    assert!(frames.iter().all(|f| f.sample_rate() == SampleRate::Hz8000));
    let snapshot = stats.snapshot();
    assert_eq!(snapshot.normalize_offloaded, 1, "{snapshot:?}");
    assert_eq!(
        (snapshot.resampler_builds, snapshot.resampler_failures),
        (1, 0)
    );
    assert!(
        pool.metrics().completed >= 2,
        "normalization and the resampler ran on the pool: {:?}",
        pool.metrics()
    );
    assert_eq!(
        tts.texts()[0].1,
        brisvox_agent::normalize(&long, Language::Tr)
    );
    drop(running);
    tokio::task::spawn_blocking(move || pool.shutdown())
        .await
        .unwrap();
}

// -- review: idle detection counts only from unanswered input -------------------------

fn short_idle(config: &mut TtsStageConfig) {
    config.idle_timeout = Duration::from_secs(3);
}

#[tokio::test(start_paused = true)]
async fn a_round_that_streams_slowly_is_not_a_stalled_provider() {
    // idle_timeout 3 s. "Hi" is answered in full; then the LLM takes 10 s (a long tool-call
    // argument) before the next sentence and the flush. The provider owes nothing meanwhile.
    let rig = Rig::with(
        Options {
            tweak: Box::new(short_idle),
            ..Options::default()
        },
        MockTts::new,
    )
    .await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Hi")).await;
    rig.run_for(10 * Duration::from_secs(1)).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.stalls, 0, "{stats:?}");
    assert_eq!(stats.session_deaths, 0, "{stats:?}");
    assert_eq!(stats.contexts_lost, 0);
    rig.feed(text(1, " there")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    assert_eq!(rig.tts.connects(), 1, "the same session finished the round");
    assert_eq!(rig.count("started"), 1, "{:?}", rig.names());
    assert_eq!(rig.count("tts_ended:1"), 1, "{:?}", rig.names());
    assert!(off(rig.audio_of(1), 8 * 60 * MS) <= 2 * MS);
    let stats = rig.stats.snapshot();
    assert_eq!(stats.stalls, 0);
    assert!(!rig.book.snapshot(key(1, 0)).unwrap().lost);
}

#[tokio::test(start_paused = true)]
async fn silence_after_late_text_is_a_stall_counted_from_that_text() {
    // "Hi" is answered by 4 events (a word and three audio chunks); the provider then goes
    // silent for good on the next text, which arrives long after the first.
    let rig = Rig::with(
        Options {
            tweak: Box::new(short_idle),
            ..Options::default()
        },
        |clock| {
            let tts = MockTts::new(clock);
            tts.push_session_faults(vec![StreamFault::Stall {
                before_event: 4,
                duration: Duration::from_secs(3600),
            }]);
            tts
        },
    )
    .await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Hi")).await;
    rig.run_for(10 * Duration::from_secs(1)).await;
    assert_eq!(rig.stats.snapshot().stalls, 0, "the gap alone is no stall");
    rig.feed(text(1, " there")).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    assert_eq!(
        rig.stats.snapshot().stalls,
        0,
        "not yet: the clock started with the new text, not with the old audio"
    );
    rig.run_for(2 * Duration::from_secs(1)).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.stalls, 1, "{stats:?}");
    assert_eq!(stats.contexts_lost, 1);
    assert!(rig.book.snapshot(key(1, 0)).unwrap().lost);
}

#[tokio::test(start_paused = true)]
async fn an_unconfirmed_flush_is_a_stall_even_after_all_text_was_answered() {
    let rig = Rig::with(
        Options {
            tweak: Box::new(short_idle),
            ..Options::default()
        },
        |clock| {
            let tts = MockTts::new(clock);
            // "Hi": a word and three audio chunks, then the Flushed event never comes.
            tts.push_session_faults(vec![StreamFault::Stall {
                before_event: 4,
                duration: Duration::from_secs(3600),
            }]);
            tts
        },
    )
    .await;
    rig.respond(1, &["Hi"]).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    assert_eq!(rig.stats.snapshot().stalls, 0);
    rig.run_for(3 * Duration::from_secs(1)).await;
    assert_eq!(rig.stats.snapshot().stalls, 1);
    assert_eq!(rig.count("tts_ended:1"), 1, "{:?}", rig.names());
}

// -- review: ends, session death mid-round, words ---------------------------------------

#[tokio::test(start_paused = true)]
async fn a_flushed_round_does_not_end_the_response_while_the_llm_prepares_the_next_one() {
    let rig = Rig::new().await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Let me check")).await;
    rig.feed(control(1, ControlKind::Flush)).await;
    // Round 1 is spoken and its Flushed arrives while the tool runs and the model thinks.
    rig.run_for(2 * Duration::from_secs(1)).await;
    assert_eq!(rig.count("started"), 1);
    assert_eq!(
        rig.count("tts_ended"),
        0,
        "no end: the response was not asked to end yet: {:?}",
        rig.names()
    );
    assert!(off(rig.audio_of(1), 12 * 60 * MS) <= 2 * MS);
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "It is nine")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    let names = rig.names();
    assert_eq!(rig.count("started"), 2, "{names:?}");
    assert_eq!(rig.count("tts_ended"), 1, "{names:?}");
    assert_eq!(names.last().unwrap(), "tts_ended:1");
    assert!(off(rig.audio_of(1), 22 * 60 * MS) <= 4 * MS);
}

#[tokio::test(start_paused = true)]
async fn the_rest_of_a_round_whose_session_died_is_dropped_not_replayed_on_a_new_session() {
    // The session dies after its seventh event (six words, the first audio), in the first chunk.
    let rig = Rig::with(Options::default(), |clock| {
        let tts = MockTts::new(clock);
        tts.push_session_faults(vec![StreamFault::Disconnect { after_events: 7 }]);
        tts
    })
    .await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "A fairly long sentence that dies")).await;
    rig.run_for(Duration::from_secs(1)).await;
    assert_eq!(rig.stats.snapshot().session_deaths, 1);
    // The rest of the same round arrives after the death.
    rig.feed(text(1, " half way through")).await;
    rig.feed(text(1, " and goes on.")).await;
    rig.feed(control(1, ControlKind::Flush)).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(3 * Duration::from_secs(1)).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.contexts, 1, "no second context: {stats:?}");
    assert_eq!(stats.text_sent, 1, "{stats:?}");
    assert!(stats.text_dropped >= 2, "{stats:?}");
    assert_eq!(
        rig.tts.connects(),
        1,
        "no reconnect for the rest of the round"
    );
    assert_eq!(rig.count("started"), 1, "{:?}", rig.names());
    assert!(!rig.names().contains(&"started:2".to_owned()));
    assert_eq!(rig.tts.texts().len(), 1, "only the first chunk was sent");
    assert!(rig.book.snapshot(key(1, 0)).unwrap().lost);
    assert_eq!(rig.count("tts_ended:1"), 1, "{:?}", rig.names());
    // The next turn speaks on a new session.
    rig.respond(2, &["Back again"]).await;
    rig.run_for(3 * Duration::from_secs(1)).await;
    assert_eq!(rig.tts.connects(), 2);
    assert_eq!(rig.count("tts_ended:2"), 1);
}

#[tokio::test(start_paused = true)]
async fn word_frames_carry_the_utterance_the_epoch_and_the_providers_offsets() {
    for output in [SampleRate::Hz8000, SampleRate::Hz16000, SampleRate::Hz24000] {
        let rig = Rig::with(
            Options {
                output,
                ..Options::default()
            },
            MockTts::new,
        )
        .await;
        // Two chunks of one utterance, then a second utterance (round 1) of the same epoch.
        rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
        rig.feed(text(1, "Hello there")).await;
        rig.feed(text(1, " dear friend")).await;
        rig.feed(control(1, ControlKind::Flush)).await;
        rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
        rig.feed(text(1, "Next one")).await;
        rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
        rig.run_for(4 * Duration::from_secs(1)).await;
        let words = rig.words();
        let summary: Vec<_> = words
            .iter()
            .map(|w| (w.word.to_string(), w.utterance, w.epoch.get(), w.start))
            .collect();
        let want = [
            ("Hello", 1, 1, Duration::ZERO),
            ("there", 1, 1, 6 * 60 * MS),
            ("dear", 1, 1, 12 * 60 * MS),
            ("friend", 1, 1, 17 * 60 * MS),
            ("Next", 2, 1, Duration::ZERO),
            ("one", 2, 1, 5 * 60 * MS),
        ];
        assert_eq!(summary.len(), want.len(), "{output:?}: {summary:?}");
        for (got, want) in summary.iter().zip(want) {
            assert_eq!(got.0, want.0, "{output:?}: {summary:?}");
            assert_eq!((got.1, got.2), (want.1, want.2), "{output:?}: {summary:?}");
            assert!(
                off(got.3, want.3) <= MS,
                "{output:?}: {} starts at {:?}, wanted {:?}",
                got.0,
                got.3,
                want.3
            );
        }
        // The words are in the provider's units, not scaled to the output rate: the audio of the
        // utterance is as long as the last word's end.
        assert!(off(rig.audio_of(1), 31 * 60 * MS) <= 4 * MS);
    }
}

// -- review: whitespace-only chunks (the segmenter's overflow cut) ----------------------

#[tokio::test(start_paused = true)]
async fn a_whitespace_chunk_is_recorded_as_sent_text_but_never_spoken() {
    let rig = Rig::new().await;
    let gap = " ".repeat(5000);
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "Hello there my friend")).await;
    rig.feed(text(1, &gap)).await;
    rig.feed(text(1, "and more words.")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(5 * Duration::from_secs(1)).await;
    let message = format!("Hello there my friend{gap}and more words.");
    let snapshot = rig.book.snapshot(key(1, 0)).unwrap();
    assert_eq!(
        snapshot.played.sent(),
        message,
        "sent text is a prefix-exact record"
    );
    let spoken: Vec<String> = rig.tts.texts().into_iter().map(|(_, t)| t).collect();
    assert_eq!(spoken, ["Hello there my friend", "and more words."]);
    let stats = rig.stats.snapshot();
    assert_eq!(stats.whitespace_chunks, 1);
    assert_eq!(stats.whitespace_dropped, 0);
    assert_eq!(stats.contexts, 1, "one utterance");
    assert_eq!(rig.count("tts_ended:1"), 1);
    assert!(!snapshot.lost);
}

#[tokio::test(start_paused = true)]
async fn whitespace_before_the_first_spoken_chunk_is_kept_and_recorded_with_it() {
    let rig = Rig::new().await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "\n\n   ")).await;
    rig.feed(text(1, "  ")).await;
    rig.feed(text(1, "Hello there")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    assert_eq!(
        rig.book.snapshot(key(1, 0)).unwrap().played.sent(),
        "\n\n     Hello there"
    );
    let spoken: Vec<String> = rig.tts.texts().into_iter().map(|(_, t)| t).collect();
    assert_eq!(spoken, ["Hello there"], "the lead is not spoken");
    assert_eq!(rig.stats.snapshot().whitespace_chunks, 2);
    assert_eq!(rig.count("tts_ended:1"), 1);
    // Whitespace alone opens no context and sends nothing.
    rig.feed(control(2, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(2, "   ")).await;
    rig.feed(control(2, ControlKind::LlmResponseEnded)).await;
    rig.run_for(Duration::from_secs(1)).await;
    assert_eq!(rig.stats.snapshot().contexts, 1);
    assert_eq!(rig.count("tts_ended:2"), 1);
}

#[tokio::test(start_paused = true)]
async fn the_kept_whitespace_lead_is_bounded() {
    let rig = Rig::new().await;
    let big = " ".repeat(brisvox_engine::WHITESPACE_LEAD_CAP / 2 + 1);
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, &big)).await;
    rig.feed(text(1, &big)).await;
    rig.feed(text(1, "Hi")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(2 * Duration::from_secs(1)).await;
    let stats = rig.stats.snapshot();
    assert_eq!(stats.whitespace_chunks, 2);
    assert_eq!(
        stats.whitespace_dropped, 1,
        "the second chunk would pass the cap"
    );
    assert_eq!(
        rig.book.snapshot(key(1, 0)).unwrap().played.sent(),
        format!("{big}Hi")
    );
}
