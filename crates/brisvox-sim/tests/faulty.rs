//! The suite must be able to fail: every other test feeds it the reference
//! chain or a transparent wrapper of it, which only proves it passes. These feed it
//! deliberately faulty pipelines and pin which profile accepts or rejects each fault, so a
//! suite body that ignores its `profile` argument, a Budget range that was widened, a deleted
//! `check_no_stale_audio...` call or a weakened `interrupts == 1` check is caught here.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)] // test helpers outside #[test] fns

use std::sync::Arc;
use std::time::Duration;

use brisvox_agent::Message;
use brisvox_core::frame::{AudioFrame, DataFrame, Frame, SystemFrame};
use brisvox_core::{Direction, Pipeline, Processor, ProcessorContext, TurnEpoch};
use brisvox_sim::factory::{BuiltSession, ReferenceFactory, SessionFactory, SessionParts};
use brisvox_sim::input::InputSink;
use brisvox_sim::latency::LatencyProfile;
use brisvox_sim::output::{OutputLog, WireKind};
use brisvox_sim::reference::{
    LlmStage, OutputPacer, PlayLog, SessionProbes, SttStage, TtsStage, TurnManager, WordLog,
};
use brisvox_sim::scenario::SimTransportOut;
use brisvox_sim::suite::{Profile, basic, played_text, scenarios};
use brisvox_sim::vad::EnergyVad;

const MS: Duration = Duration::from_millis(1);

/// What is wrong with the pipeline under test.
#[derive(Debug, Clone, Copy)]
enum Fault {
    /// The VAD needs this much longer silence to end a turn (a slower end-of-turn decision).
    SlowerStop(Duration),
    /// The VAD needs this much longer speech to confirm a start (a slower barge-in).
    SlowerStart(Duration),
    /// The output transport takes this long per frame (a slow consumer).
    SlowTransport(Duration),
    /// The `Interrupt` frame never reaches the output tap.
    DropsInterrupts,
    /// Audio of the interrupted turn reaches the output after the interrupt.
    ReplaysStaleAudio,
    /// The client receives one frame of the interrupted turn right after the clear, in the same
    /// instant (a transport that released a buffered frame behind the clear).
    StaleWireAudioAfterClear,
    /// The LLM takes this long to its first token (barge-ins land before any audio played).
    SlowLlm(Duration),
    /// Like `SlowLlm`, and a barge-in before any audio leaves the first word of the answer in
    /// the context (the response is not dropped whole).
    KeepsAnUnheardWord(Duration),
}

/// The reference chain with one [`Fault`].
#[derive(Debug, Clone, Copy)]
struct Faulty(Fault);

/// Swallows every downstream `Interrupt` (the transport behind it never learns of it).
struct DropInterrupts;

impl Processor for DropInterrupts {
    fn name(&self) -> &'static str {
        "drop_interrupts"
    }

    async fn process(
        &mut self,
        frame: Frame,
        dir: Direction,
        ctx: &mut ProcessorContext,
    ) -> brisvox_core::Result<()> {
        if matches!(frame, Frame::System(SystemFrame::Interrupt { .. }))
            && dir == Direction::Downstream
        {
            return Ok(());
        }
        ctx.forward(frame, dir).await
    }
}

/// The reference chain of `ReferenceFactory`, with `DropInterrupts` in front of the transport.
fn chain_without_interrupts(parts: SessionParts) -> BuiltSession {
    let SessionParts {
        clock,
        stt,
        llm: mock_llm,
        tts: mock_tts,
        vad,
        transport_delay,
        timeline,
        origin,
        input_rate,
        output_rate,
    } = parts;
    let output = OutputLog::new(origin, Some(timeline.clone()));
    let transport = SimTransportOut::from_log(clock.clone(), output.clone())
        .with_consume_delay(transport_delay);
    let words = WordLog::default();
    let played = PlayLog::default();
    let llm = LlmStage::new(
        Arc::new(mock_llm),
        clock.clone(),
        timeline.clone(),
        Arc::clone(&words),
        Arc::clone(&played),
    );
    let tts = TtsStage::new(
        Arc::new(mock_tts),
        output_rate,
        clock.clone(),
        timeline.clone(),
        Arc::clone(&words),
    )
    .with_play_log(Arc::clone(&played));
    let probes = SessionProbes::new(llm.history(), words, Arc::clone(&played), llm.truncations());
    let pipeline = Pipeline::new("faulty-session")
        .then(EnergyVad::new(vad))
        .then(SttStage::new(Arc::new(stt), input_rate, clock.clone()))
        .then(TurnManager::new(clock.clone(), timeline))
        .then(llm)
        .then(tts)
        .then(OutputPacer::new(clock, played))
        .then(DropInterrupts)
        .then(transport);
    BuiltSession::new(pipeline, probes, InputSink::pipeline(), output)
}

impl SessionFactory for Faulty {
    async fn build(&self, mut parts: SessionParts) -> brisvox_sim::Result<BuiltSession> {
        match self.0 {
            Fault::SlowerStop(extra) => parts.vad.stop_after += extra,
            Fault::SlowerStart(extra) => parts.vad.start_after += extra,
            Fault::SlowTransport(delay) => parts.transport_delay = delay,
            Fault::DropsInterrupts => return Ok(chain_without_interrupts(parts)),
            Fault::ReplaysStaleAudio => {
                let (clock, rate) = (parts.clock.clone(), parts.output_rate);
                let built = ReferenceFactory.build(parts).await?;
                let output = built.output.clone();
                // After the pipeline stopped, one frame of turn 0 shows up at the output behind
                // the interrupt to turn 1 (a transport that kept a buffered frame).
                return Ok(built.with_drain(move || async move {
                    let stale = AudioFrame::new(
                        bytes::Bytes::from(vec![0u8; 640]),
                        rate,
                        clock.now(),
                        0,
                        TurnEpoch::ZERO,
                    )
                    .unwrap();
                    output.record_frame(&Frame::Data(DataFrame::Audio(stale)), clock.now());
                }));
            }
            Fault::StaleWireAudioAfterClear => {
                let built = ReferenceFactory.build(parts).await?;
                let output = built.output.clone();
                // The reference has no wire: this drain adds what a client would have received
                // around the first interrupt, at its instant T: turn-0 audio, `Clear{1}`, then a
                // stale turn-0 frame, also at T. The output log files that frame before the
                // interrupt (same instant), so only the wire order shows it.
                return Ok(built.with_drain(move || async move {
                    let records = output.records();
                    let Some(cut) = records.iter().find(|r| r.kind == "interrupt") else {
                        return;
                    };
                    let (at, epoch) = (output.origin() + cut.at, cut.epoch.unwrap());
                    let (new, old) = (TurnEpoch::new(epoch), TurnEpoch::new(epoch - 1));
                    output.record_wire(WireKind::Audio, at, Some(old), Some(20 * MS));
                    output.record_wire(WireKind::Clear, at, Some(new), None);
                    output.record_wire(WireKind::Audio, at, Some(old), Some(20 * MS));
                    output.record_audio(old, at, 20 * MS);
                }));
            }
            Fault::SlowLlm(first_token) => slow_llm(&parts, first_token),
            Fault::KeepsAnUnheardWord(first_token) => {
                slow_llm(&parts, first_token);
                let built = ReferenceFactory.build(parts).await?;
                let context = Arc::clone(&built.probes.context);
                return Ok(built.with_drain(move || async move {
                    let mut context = context.lock().unwrap();
                    let messages = context.messages_mut();
                    // The first user message got no assistant message (its response was dropped
                    // before any audio): keep "Here", the first word of the answer, after it.
                    if let Some(first) = messages
                        .iter()
                        .position(|m| matches!(m, Message::User { .. }))
                        && !matches!(messages.get(first + 1), Some(Message::Assistant { .. }))
                    {
                        messages.insert(first + 1, Message::assistant("Here"));
                    }
                }));
            }
        }
        ReferenceFactory.build(parts).await
    }
}

/// Sets the first-token latency of the session's LLM (the mocks share their state).
fn slow_llm(parts: &SessionParts, first_token: Duration) {
    let _ =
        parts
            .llm
            .clone()
            .with_latency(LatencyProfile::Fixed(first_token), LatencyProfile::ZERO, 0);
}

/// The message of the assertion that failed `result`; panics if it passed.
#[track_caller]
fn failure(what: &str, result: brisvox_sim::Result<()>) -> String {
    match result {
        Err(brisvox_sim::Error::Assertion(message)) => message,
        Err(other) => panic!("{what}: failed with {other:?}, not an assertion"),
        Ok(()) => panic!("{what}: passed but should have failed"),
    }
}

#[track_caller]
fn names(what: &str, message: &str, needle: &str) {
    assert!(
        message.contains(needle),
        "{what}: {message:?} does not name {needle:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_faultless_wrapper_passes_both_profiles() {
    // Control: without a fault the wrapper is the reference, so every failure below is caused by
    // its fault.
    for profile in [Profile::Exact, Profile::Budget] {
        let factory = Faulty(Fault::SlowerStop(Duration::ZERO));
        basic::one_full_turn(&factory, profile).await.unwrap();
        scenarios::s02_barge_in(&factory, profile).await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_slower_end_of_turn_fails_exact_and_passes_budget() {
    // 100 ms more silence before the turn ends: first audio at 1580 ms instead of 1480 ms and an
    // end-of-turn delay of 380 ms instead of 280 ms.
    let factory = Faulty(Fault::SlowerStop(100 * MS));
    let message = failure(
        "exact one_full_turn",
        basic::one_full_turn(&factory, Profile::Exact).await,
    );
    names("exact", &message, "first bot audio");
    basic::one_full_turn(&factory, Profile::Budget)
        .await
        .unwrap();
    scenarios::s01_happy_path_with_realistic_latency(&factory, Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_slower_barge_in_fails_exact_and_passes_budget() {
    // The VAD confirms the barge-in 20 ms late: the interrupt reaches the output at 2440 ms, the
    // edge of the Budget range (set from measurements: 2400 to 2440 ms).
    let factory = Faulty(Fault::SlowerStart(20 * MS));
    let message = failure(
        "exact barge-in",
        basic::barge_in_silences_the_bot_and_starts_a_new_turn(&factory, Profile::Exact).await,
    );
    names("exact", &message, "interrupt at the output");
    basic::barge_in_silences_the_bot_and_starts_a_new_turn(&factory, Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_40_ms_slower_barge_in_fails_both_profiles() {
    // Pins the upper edge of the Budget range: 40 ms late puts the interrupt at 2460 ms.
    let factory = Faulty(Fault::SlowerStart(40 * MS));
    for profile in [Profile::Exact, Profile::Budget] {
        let message = failure(
            "barge-in 40 ms late",
            basic::barge_in_silences_the_bot_and_starts_a_new_turn(&factory, profile).await,
        );
        names("barge-in 40 ms late", &message, "interrupt at the output");
    }
}

#[tokio::test(start_paused = true)]
async fn stale_audio_right_behind_the_clear_on_the_wire_fails_both_profiles() {
    let factory = Faulty(Fault::StaleWireAudioAfterClear);
    for profile in [Profile::Exact, Profile::Budget] {
        let message = failure("s02", scenarios::s02_barge_in(&factory, profile).await);
        names("s02", &message, "after the clear");
        let message = failure(
            "barge_in",
            basic::barge_in_silences_the_bot_and_starts_a_new_turn(&factory, profile).await,
        );
        names("barge_in", &message, "after the clear");
    }
}

#[tokio::test(start_paused = true)]
async fn a_slow_transport_is_felt_by_both_profiles() {
    // 150 ms per frame at the output: the first bot audio is 300 ms late (past the Budget range
    // of one turn, 1480 to 1700 ms), and the interrupt waits behind the backed-up
    // frames, so the silence after a barge-in takes 260 ms (outside the 50 ms Budget bound).
    let factory = Faulty(Fault::SlowTransport(150 * MS));
    for profile in [Profile::Exact, Profile::Budget] {
        let message = failure(
            "one_full_turn",
            basic::one_full_turn(&factory, profile).await,
        );
        names("one_full_turn", &message, "first bot audio");
    }
    for profile in [Profile::Exact, Profile::Budget] {
        let message = failure("s02", scenarios::s02_barge_in(&factory, profile).await);
        names("s02", &message, "time to silence");
    }
}

#[tokio::test(start_paused = true)]
async fn a_slightly_slow_transport_is_inside_the_silence_budget_only() {
    let factory = Faulty(Fault::SlowTransport(30 * MS));
    let message = failure(
        "exact s02",
        scenarios::s02_barge_in(&factory, Profile::Exact).await,
    );
    names("exact s02", &message, "time to silence");
    scenarios::s02_barge_in(&factory, Profile::Budget)
        .await
        .unwrap();
    // Its first audio is late by less than the Budget margin of one turn, too.
    let message = failure(
        "exact one_full_turn",
        basic::one_full_turn(&factory, Profile::Exact).await,
    );
    names("exact one_full_turn", &message, "first bot audio");
    basic::one_full_turn(&factory, Profile::Budget)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_pipeline_a_second_too_slow_fails_both_profiles() {
    // The end of the turn is decided 1 s late: the first audio would come at 2480 ms, past the
    // Budget range (1480 to 1700 ms) and past the 1.5 s bound of the happy path (in the short
    // one-turn call the caller's audio ends before the late decision, so no audio comes at all).
    let factory = Faulty(Fault::SlowerStop(1_000 * MS));
    for profile in [Profile::Exact, Profile::Budget] {
        let message = failure(
            "one_full_turn",
            basic::one_full_turn(&factory, profile).await,
        );
        names("one_full_turn", &message, "bot audio");
        let message = failure(
            "s01",
            scenarios::s01_happy_path_with_realistic_latency(&factory, profile).await,
        );
        names("s01", &message, "bot audio");
    }
}

#[tokio::test(start_paused = true)]
async fn an_interrupt_that_never_reaches_the_output_fails_both_profiles() {
    let factory = Faulty(Fault::DropsInterrupts);
    for profile in [Profile::Exact, Profile::Budget] {
        let message = failure("s02", scenarios::s02_barge_in(&factory, profile).await);
        names("s02", &message, "interrupt");
        let message = failure(
            "barge_in",
            basic::barge_in_silences_the_bot_and_starts_a_new_turn(&factory, profile).await,
        );
        names("barge_in", &message, "interrupt");
    }
}

#[tokio::test(start_paused = true)]
async fn audio_of_an_old_turn_after_the_interrupt_fails_both_profiles() {
    let factory = Faulty(Fault::ReplaysStaleAudio);
    for profile in [Profile::Exact, Profile::Budget] {
        let message = failure("s02", scenarios::s02_barge_in(&factory, profile).await);
        names("s02", &message, "after the interrupt");
        let message = failure(
            "barge_in",
            basic::barge_in_silences_the_bot_and_starts_a_new_turn(&factory, profile).await,
        );
        names("barge_in", &message, "after the interrupt");
    }
}

#[tokio::test(start_paused = true)]
async fn a_barge_in_before_any_audio_keeps_nothing_in_either_profile() {
    // Control: a slow LLM alone is fine. Some of the 16 barge-ins of the streaming check now land
    // before the first audio, and the pipeline drops those responses whole.
    // (Exact is the reference's closed form: its dropped responses are not the point here.)
    let slow = Faulty(Fault::SlowLlm(700 * MS));
    played_text::a_barge_in_while_the_llm_is_still_streaming_keeps_only_the_heard_part_of_what_was_sent(&slow, Profile::Budget)
        .await
        .unwrap();
    // The same pipeline keeping one unheard word: the Budget word slack must not excuse it.
    let leaky = Faulty(Fault::KeepsAnUnheardWord(700 * MS));
    for profile in [Profile::Exact, Profile::Budget] {
        let message = failure(
            "streaming barge-in",
            played_text::a_barge_in_while_the_llm_is_still_streaming_keeps_only_the_heard_part_of_what_was_sent(&leaky, profile)
                .await,
        );
        names("streaming barge-in", &message, "nothing heard");
    }
}
