//! Pre-rendered filler clips through the TTS stage over the simulator's `MockTts` on a
//! paused clock: a round the runner cued with a clip plays the clip (its own utterance, audio at
//! the call rate, words, book entries, the end of the response) after the audio of the rounds
//! before it, the provider never hears its text, a clip the stage cannot play falls back to the
//! provider, and a barge-in drops a clip like provider audio.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    clippy::cast_possible_truncation
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::tools::ClipKey;
use brisvox_core::TurnEpoch;
use brisvox_core::frame::{
    AudioFrame, ControlFrame, ControlKind, DataFrame, Frame, SampleRate, SystemFrame, TextFrame,
    WordTimestamp,
};
use brisvox_core::{Links, Pipeline, RunningPipeline, SharedClock, SimClock, TimelineRecorder};
use brisvox_engine::{
    CallRatesCell, ClipWord, EngineConfig, FillerClip, FillerClipStore, Offload, PlayoutBook,
    RoundKey, TtsStage, TtsStageConfig, TtsStats,
};
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::mock_tts::MockTts;
use brisvox_transport::CallRates;
use bytes::Bytes;
use common::{MS, Recorded, Recorder};
use tokio_util::sync::CancellationToken;

const SECOND: Duration = Duration::from_secs(1);
const FILLER: &str = "One moment, please.";

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

fn key(epoch: u32, round: u32) -> RoundKey {
    RoundKey::new(TurnEpoch::new(epoch), round)
}

/// `ms` of a square wave at 24 kHz (not silence).
fn pcm(ms: usize) -> Bytes {
    let samples = 24 * ms;
    let mut out = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let v: i16 = if (i / 8) % 2 == 0 { 3000 } else { -3000 };
        out.extend_from_slice(&v.to_le_bytes());
    }
    Bytes::from(out)
}

/// The filler clip: 900 ms at 24 kHz, three timed words.
fn clip(ms: usize) -> FillerClip {
    FillerClip::new(
        SampleRate::Hz24000,
        pcm(ms),
        vec![
            ClipWord::new("One", Duration::ZERO),
            ClipWord::new("moment,", 250 * MS),
            ClipWord::new("please.", 600 * MS),
        ],
    )
    .unwrap()
}

fn store(ms: usize) -> Arc<FillerClipStore> {
    Arc::new(
        FillerClipStore::new()
            .with_clip(ClipKey::new("en/one-moment"), clip(ms))
            // A clip at another rate than the provider's: never played.
            .with_clip(
                ClipKey::new("en/wrong-rate"),
                FillerClip::with_spread_words(
                    SampleRate::Hz16000,
                    Bytes::from(vec![0u8; 3200]),
                    FILLER,
                )
                .unwrap(),
            ),
    )
}

struct Rig {
    running: RunningPipeline,
    rec: Recorded,
    clock: SharedClock,
    tts: MockTts,
    stats: TtsStats,
    book: Arc<PlayoutBook>,
}

impl Rig {
    /// A stage at a 16 kHz call rate with the clips of `store`; the mock's first byte comes
    /// after 200 ms and a chunk every 40 ms (so a round is still streaming when the next one
    /// starts).
    async fn new(store: Option<Arc<FillerClipStore>>) -> Self {
        let clock = SimClock::shared();
        let tts = MockTts::new(clock.clone()).with_latency(
            LatencyProfile::ZERO,
            LatencyProfile::Fixed(200 * MS),
            LatencyProfile::Fixed(40 * MS),
            1,
        );
        let config = TtsStageConfig::from_engine(&EngineConfig::DEFAULT);
        let book = PlayoutBook::shared(16);
        let mut stage = TtsStage::new(
            Arc::new(tts.clone()),
            config,
            clock.clone(),
            TimelineRecorder::new(),
        )
        .with_book(Arc::clone(&book))
        .with_rates(CallRatesCell::new(CallRates::symmetric(
            SampleRate::Hz16000,
        )))
        .with_offload(Offload::Inline(clock.clone()));
        if let Some(store) = store {
            stage = stage.with_clips(store);
        }
        let stats = stage.stats();
        let rec = Recorded::default();
        let running = Pipeline::new("tts")
            .then(stage)
            .then(Recorder(rec.clone()))
            .start(Links::default(), &CancellationToken::new())
            .await
            .unwrap();
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
        }
    }

    async fn feed(&self, frame: Frame) {
        self.running.input().send(frame).await.unwrap();
    }

    async fn run_for(&self, d: Duration) {
        self.clock.sleep(d).await;
    }

    /// A round as the runner sends it: started, the text, `Flush`.
    async fn round(&self, epoch: u32, chunks: &[&str]) {
        self.feed(control(epoch, ControlKind::LlmResponseStarted))
            .await;
        for chunk in chunks {
            self.feed(text(epoch, chunk)).await;
        }
        self.feed(control(epoch, ControlKind::Flush)).await;
    }

    /// The filler round `round` of `epoch`, cued with `clip` first (as the runner does).
    async fn filler(&self, epoch: u32, round: u32, clip: &str) {
        self.book.cue_clip(key(epoch, round), ClipKey::new(clip));
        // The segmenter may cut the phrase: the stage records every piece.
        self.round(epoch, &["One moment,", " please."]).await;
    }

    fn frames(&self) -> Vec<Frame> {
        self.rec.all().into_iter().map(|(f, _)| f).collect()
    }

    /// `started:<utterance>`, `audio:<utterance>` (the utterance announced last), `word:<w>`,
    /// `tts_ended:<epoch>`.
    fn names(&self) -> Vec<String> {
        let mut current = 0;
        self.frames()
            .into_iter()
            .filter_map(|frame| match frame {
                Frame::Control(ControlFrame {
                    kind: ControlKind::UtteranceStarted { utterance },
                    ..
                }) => {
                    current = utterance;
                    Some(format!("started:{utterance}"))
                }
                Frame::Control(ControlFrame {
                    epoch,
                    kind: ControlKind::TtsEnded,
                }) => Some(format!("tts_ended:{}", epoch.get())),
                Frame::Data(DataFrame::Audio(_)) => Some(format!("audio:{current}")),
                Frame::Data(DataFrame::Word(w)) => Some(format!("word:{}", w.word)),
                Frame::System(SystemFrame::Interrupt { .. }) => Some("interrupt".to_owned()),
                _ => None,
            })
            .collect()
    }

    fn words_of(&self, utterance: u64) -> Vec<WordTimestamp> {
        self.frames()
            .into_iter()
            .filter_map(|frame| match frame {
                Frame::Data(DataFrame::Word(w)) if w.utterance == utterance => Some(*w),
                _ => None,
            })
            .collect()
    }

    /// Audio of `utterance` (by the `UtteranceStarted` before it).
    fn audio_of(&self, utterance: u64) -> Duration {
        let mut current = 0;
        let mut total = Duration::ZERO;
        for frame in self.frames() {
            match frame {
                Frame::Control(ControlFrame {
                    kind: ControlKind::UtteranceStarted { utterance: u },
                    ..
                }) => current = u,
                Frame::Data(DataFrame::Audio(a)) if current == utterance => {
                    total += AudioFrame::duration(&a);
                }
                _ => {}
            }
        }
        total
    }

    fn provider_texts(&self) -> String {
        self.tts
            .texts()
            .into_iter()
            .map(|(_, t)| t)
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[tokio::test(start_paused = true)]
async fn a_cued_clip_plays_as_its_own_utterance_after_the_audio_before_it() {
    let rig = Rig::new(Some(store(900))).await;
    rig.round(1, &["Let me check that."]).await;
    // The filler starts while the first round is still streaming (18 chars, about 1.1 s).
    rig.run_for(300 * MS).await;
    rig.filler(1, 1, "en/one-moment").await;
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "It is nine.")).await;
    rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
    rig.run_for(5 * SECOND).await;

    let names = rig.names();
    // Three utterances, each announced once, in round order: the clip waited for the audio of
    // the first round and the third round's audio waited for the clip.
    let starts: Vec<&String> = names.iter().filter(|n| n.starts_with("started")).collect();
    assert_eq!(starts, ["started:1", "started:2", "started:3"], "{names:?}");
    let last_audio_1 = names.iter().rposition(|n| n == "audio:1").unwrap();
    let first_audio_2 = names.iter().position(|n| n == "audio:2").unwrap();
    let last_audio_2 = names.iter().rposition(|n| n == "audio:2").unwrap();
    let first_audio_3 = names.iter().position(|n| n == "audio:3").unwrap();
    assert!(last_audio_1 < first_audio_2, "{names:?}");
    assert!(last_audio_2 < first_audio_3, "{names:?}");
    // One end, after everything.
    assert_eq!(
        names.iter().filter(|n| n.starts_with("tts_ended")).count(),
        1
    );
    assert_eq!(names.last().unwrap(), "tts_ended:1");

    // The clip's audio, whole, at the call rate; the provider never heard the filler.
    let clip_audio = rig.audio_of(2);
    assert!(clip_audio.abs_diff(900 * MS) <= 2 * MS, "{clip_audio:?}");
    let sent = rig.provider_texts();
    assert!(!sent.contains("moment"), "{sent}");
    assert!(
        sent.contains("Let me check") && sent.contains("nine"),
        "{sent}"
    );

    // The clip's words, on its utterance, with the clip's offsets.
    let words: Vec<(String, Duration)> = rig
        .words_of(2)
        .into_iter()
        .map(|w| (w.word.to_string(), w.start))
        .collect();
    assert_eq!(
        words,
        vec![
            ("One".to_owned(), Duration::ZERO),
            ("moment,".to_owned(), 250 * MS),
            ("please.".to_owned(), 600 * MS),
        ]
    );
    // The book knows the round's utterance and its text verbatim (the played-text cut works on
    // it like on provider speech).
    let snapshot = rig.book.snapshot(key(1, 1)).unwrap();
    assert_eq!(snapshot.utterance, Some(2));
    assert_eq!(snapshot.played.sent(), FILLER);

    let stats = rig.stats.snapshot();
    assert_eq!((stats.clips, stats.clips_played), (1, 1), "{stats:?}");
    assert_eq!((stats.clip_fallbacks, stats.clips_dropped), (0, 0));
    assert_eq!(stats.contexts, 3);
    // The third round's context was held back until the clip was out.
    assert_eq!((stats.clip_waits, stats.clip_wait_timeouts), (1, 0));
    // The provider got a flush for its two contexts only.
    assert_eq!(stats.flushes, 2);
}

#[tokio::test(start_paused = true)]
async fn a_clip_the_stage_cannot_play_falls_back_to_the_provider() {
    for (store, clip) in [
        (Some(store(900)), "en/missing"),
        (Some(store(900)), "en/wrong-rate"),
        (None, "en/one-moment"),
    ] {
        let rig = Rig::new(store).await;
        rig.filler(1, 0, clip).await;
        rig.feed(control(1, ControlKind::LlmResponseEnded)).await;
        rig.run_for(5 * SECOND).await;
        let stats = rig.stats.snapshot();
        assert_eq!(
            (stats.clips, stats.clip_fallbacks),
            (0, 1),
            "{clip}: {stats:?}"
        );
        assert_eq!(rig.provider_texts(), "One moment,| please.", "{clip}");
        // The provider's rendering of the 19 characters, at 60 ms each.
        assert!(rig.audio_of(1).abs_diff(19 * 60 * MS) <= 2 * MS, "{clip}");
        assert_eq!(rig.book.snapshot(key(1, 0)).unwrap().played.sent(), FILLER);
        assert_eq!(rig.names().last().unwrap(), "tts_ended:1");
    }
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_while_a_clip_waits_for_the_audio_before_it_drops_the_clip() {
    // (A barge-in in the middle of a clip, against a paced transport, is in `filler_sim.rs`.)
    let rig = Rig::new(Some(store(2000))).await;
    rig.round(1, &["Let me check that."]).await;
    rig.run_for(300 * MS).await;
    rig.filler(1, 1, "en/one-moment").await;
    // The clip waits for the first round (still streaming); the caller barges in.
    rig.run_for(200 * MS).await;
    rig.feed(Frame::System(SystemFrame::Interrupt {
        new_epoch: TurnEpoch::new(2),
    }))
    .await;
    rig.run_for(3 * SECOND).await;
    let names = rig.names();
    assert!(!names.iter().any(|n| n == "started:2"), "{names:?}");
    assert_eq!(rig.audio_of(2), Duration::ZERO);
    let after: Vec<&String> = names
        .iter()
        .skip_while(|n| *n != "interrupt")
        .filter(|n| n.starts_with("audio"))
        .collect();
    assert!(after.is_empty(), "{names:?}");
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.clips, stats.clips_played, stats.clips_dropped),
        (1, 0, 1)
    );
    // The provider was told to cancel its own context only.
    assert_eq!(stats.cancels, 1, "{stats:?}");

    // The next response is clean and can use the clip again.
    rig.filler(2, 0, "en/one-moment").await;
    rig.feed(control(2, ControlKind::LlmResponseEnded)).await;
    rig.run_for(5 * SECOND).await;
    assert!(rig.audio_of(3).abs_diff(2000 * MS) <= 2 * MS);
    assert_eq!(rig.names().last().unwrap(), "tts_ended:2");
    assert_eq!(rig.stats.snapshot().clips_played, 1);
}

#[tokio::test(start_paused = true)]
async fn a_round_held_back_for_a_clip_is_released_by_a_barge_in() {
    let rig = Rig::new(Some(store(900))).await;
    rig.round(1, &["Let me check that for you right now."])
        .await;
    rig.run_for(300 * MS).await;
    rig.filler(1, 1, "en/one-moment").await;
    // The answer's text arrives while the clip still waits for the first round's audio: the
    // stage holds it back.
    rig.feed(control(1, ControlKind::LlmResponseStarted)).await;
    rig.feed(text(1, "It is nine.")).await;
    rig.run_for(100 * MS).await;
    assert_eq!(rig.stats.snapshot().clip_waits, 1);
    assert_eq!(rig.stats.snapshot().contexts, 2);
    // The barge-in ends the wait at once: the held-back text is dropped, not spoken late.
    rig.feed(Frame::System(SystemFrame::Interrupt {
        new_epoch: TurnEpoch::new(2),
    }))
    .await;
    rig.run_for(100 * MS).await;
    let stats = rig.stats.snapshot();
    assert_eq!(
        (stats.contexts, stats.clip_wait_timeouts),
        (2, 0),
        "{stats:?}"
    );
    assert!(!rig.provider_texts().contains("nine"));

    rig.round(2, &["Okay."]).await;
    rig.feed(control(2, ControlKind::LlmResponseEnded)).await;
    rig.run_for(3 * SECOND).await;
    assert_eq!(rig.names().last().unwrap(), "tts_ended:2");
    assert_eq!(rig.stats.snapshot().clips_played, 0);
}
