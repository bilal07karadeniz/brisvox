//! The recording sinks of the calls: a transcript writer and an audio writer per call.
//!
//! # Where the data comes from
//! - **Transcript**: the call's [`EngineEvents`] ring ([`EngineEvent::UserText`] for what the
//!   caller said, [`EngineEvent::BotTtsText`] for what the bot sent to the speech synthesis),
//!   read by one task per call, plus the engine's own answer to "what did the caller hear",
//!   taken from the truncation source ([`SessionBuilder::with_source_wrap`], see below).
//!   Nothing is added to the pipeline.
//!
//! # What the bot line holds: only what was heard
//! `BotTtsText` is what was sent to the speech synthesis, not what was played: a barge-in
//! leaves sentences the caller never heard. So the transcript never writes `BotTtsText` as the
//! bot's words on its own authority. A bot line is written
//! - **in full** when the transport reported the response played to its end
//!   ([`EngineEvent::BotStoppedSpeaking`]);
//! - **as heard** from the engine's played-text truncation (the same `PlayoutBook` answer that
//!   cuts the LLM context after a barge-in), when the response was interrupted or its end was
//!   not confirmed: the heard prefix, one line per LLM round, written when the engine settles
//!   the context (at the caller's next turn or at the end of the call; the line's `barge_in_ms`
//!   says when the barge-in happened);
//! - **as unknown** (`"heard": "unknown"`, empty text) when the heard extent is not known: the
//!   truncation source had no answer, an event or the answer was lost, or the call ended first.
//!   The unheard text is never written as a guess.
//! - **Caller audio**: a [`CallerAudioTap`] on the first stage, [`SessionBuilder::with_caller_audio_tap`].
//! - **Bot audio**: a [`FrameTap`] right before `TransportOut`, [`SessionBuilder::with_tap`].
//!
//! # Never in the way of the call
//! The taps run on the pipeline's tasks. Each does one `try_send` of a `Bytes` handle (no copy,
//! no allocation per frame) into a bounded queue ([`AUDIO_QUEUE`] frames) and returns; a full
//! queue drops the NEWEST frame and counts it ([`RecordingStats::frames_dropped`]). A dropped
//! caller frame is filled with silence from the sequence hole it leaves; a dropped bot frame
//! shortens the bot track. The transcript task reads a bounded broadcast ring that drops its
//! OLDEST events for a slow reader. All disk writes happen in the recording tasks, each with a
//! timeout ([`IO_TIMEOUT`]), on tokio's file API (the blocking pool, never a worker). A failing
//! disk disables the failing file, logs once, and the call goes on.
//!
//! # Lifetime
//! The tasks end when the call's pipeline is dropped (the queues close) or when
//! [`CallRecording::finish`] / the drop of the [`CallRecording`] cancels them; either way they
//! drain what is queued, finish the files (WAV lengths written into the header) and exit.
//! [`CallRecording::finish`] waits for that for at most [`FINISH_TIMEOUT`].
//!
//! Log lines never contain transcript text, only counts and file names.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use brisvox_core::frame::{AudioFrame, DataFrame, Frame, SampleRate, SystemFrame};
use brisvox_core::{SharedClock, TurnEpoch};
use brisvox_engine::session::SessionBuilder;
use brisvox_engine::{BoxFuture, Cut, CutRequest, RoundKey, TruncationSource};
use brisvox_engine::{
    CallerAudioTap, DEFAULT_EVENT_CAPACITY, EngineEvent, EngineEvents, EventStream, FrameTap,
};
use brisvox_providers::{LlmProvider, SttProvider, TtsProvider};
use bytes::Bytes;
use futures::FutureExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::files::{Kind, WavWriter, file_name_for, within};
use super::redact::{PiiKinds, redact_pii_with};
use super::retention::SweepStats;
use super::settings::RecordingSettings;
use super::sink::{AgeRecipients, RecordingSink};
use super::track::{BotTrack, CallerTrack};

/// Frames the audio queue of a call holds (20 ms each: about 20 s of both tracks together).
pub const AUDIO_QUEUE: usize = 1_024;

/// Slots of the audio queue only a barge-in marker may take: an audio frame is dropped when
/// fewer than this many are free, so a full queue (a stalled disk) never costs the bot track its
/// barge-in cut.
pub const INTERRUPT_RESERVE: usize = 16;

/// Answers of the truncation source the transcript task can have waiting (a few per turn;
/// when full the newest is dropped and counted, and the line is written as unknown).
pub const HEARD_QUEUE: usize = 64;

/// Epochs the transcript remembers the extent of (confirmed, or waiting for the engine's answer).
const MAX_TRACKED_EPOCHS: usize = 64;

/// Timeout of each disk operation.
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long [`CallRecording::finish`] waits for the recording tasks.
pub const FINISH_TIMEOUT: Duration = Duration::from_secs(15);

/// Most bytes of bot text kept for one response (the rest is cut, with a marker in the line).
pub const MAX_BOT_TURN_BYTES: usize = 32 * 1024;

/// Counters over all calls, readable while the server runs.
#[derive(Debug, Default)]
pub struct RecordingStats {
    calls: AtomicU64,
    transcript_lines: AtomicU64,
    audio_files: AtomicU64,
    frames_dropped: AtomicU64,
    write_errors: AtomicU64,
    events_lost: AtomicU64,
    heard_lost: AtomicU64,
    interrupts_lost: AtomicU64,
}

impl RecordingStats {
    /// Calls that started recording.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// Transcript lines written.
    pub fn transcript_lines(&self) -> u64 {
        self.transcript_lines.load(Ordering::Relaxed)
    }

    /// Audio files finished.
    pub fn audio_files(&self) -> u64 {
        self.audio_files.load(Ordering::Relaxed)
    }

    /// Audio frames the taps could not queue (full queue): the overflow policy's drops.
    pub fn frames_dropped(&self) -> u64 {
        self.frames_dropped.load(Ordering::Relaxed)
    }

    /// Disk operations that failed or timed out.
    pub fn write_errors(&self) -> u64 {
        self.write_errors.load(Ordering::Relaxed)
    }

    /// Conversation events the transcript tasks fell too far behind for (the ring's drops).
    pub fn events_lost(&self) -> u64 {
        self.events_lost.load(Ordering::Relaxed)
    }

    /// Played-text answers of the engine that could not be queued for the transcript (their
    /// bot lines are written as unknown).
    pub fn heard_lost(&self) -> u64 {
        self.heard_lost.load(Ordering::Relaxed)
    }

    /// Barge-in markers that could not be queued even in the reserved slots (the bot track then
    /// keeps audio the caller did not hear until the next marker).
    pub fn interrupts_lost(&self) -> u64 {
        self.interrupts_lost.load(Ordering::Relaxed)
    }
}

/// Who a recorded call is, for the file names and the transcript header. No caller identity
/// (number, name) is ever part of it.
#[derive(Debug, Clone, Copy)]
pub struct CallInfo<'a> {
    /// The call's session number.
    pub session: u64,
    /// The agent's name.
    pub agent: &'a str,
}

/// The recording of the server's calls; see `docs/PRIVACY.md`.
#[derive(Debug)]
pub struct Recording {
    settings: RecordingSettings,
    clock: SharedClock,
    stats: Arc<RecordingStats>,
}

impl Recording {
    /// A recording with `settings` (checked with [`RecordingSettings::validate`]) on `clock`.
    ///
    /// # Errors
    /// [`crate::Error::Config`] when the settings are inconsistent.
    pub fn new(settings: RecordingSettings, clock: SharedClock) -> crate::Result<Arc<Self>> {
        settings.validate()?;
        Ok(Arc::new(Self {
            settings,
            clock,
            stats: Arc::new(RecordingStats::default()),
        }))
    }

    /// The settings.
    pub const fn settings(&self) -> &RecordingSettings {
        &self.settings
    }

    /// The counters.
    pub fn stats(&self) -> Arc<RecordingStats> {
        Arc::clone(&self.stats)
    }

    /// Creates the recording directory if it does not exist (call once at startup).
    ///
    /// # Errors
    /// [`crate::Error::Config`] when the directory cannot be created or is not a directory.
    pub async fn prepare(&self) -> crate::Result<()> {
        let dir = &self.settings.dir;
        within(&*self.clock, IO_TIMEOUT, tokio::fs::create_dir_all(dir))
            .await
            .map_err(|e| {
                crate::Error::Config(format!("recording directory {}: {e}", dir.display()))
            })?;
        let meta = within(&*self.clock, IO_TIMEOUT, tokio::fs::metadata(dir))
            .await
            .map_err(|e| {
                crate::Error::Config(format!("recording directory {}: {e}", dir.display()))
            })?;
        if !meta.is_dir() {
            return Err(crate::Error::Config(format!(
                "recording directory {} is not a directory",
                dir.display()
            )));
        }
        Ok(())
    }

    /// Starts the retention sweeper, if the settings have a retention. It stops when `cancel`
    /// fires. The returned [`Sweeper`] must be kept: it carries the counters and is awaited at
    /// shutdown ([`Sweeper::join`]); a panic of the sweep is logged and counted
    /// ([`SweepStats::panics`]) even if it is dropped.
    pub fn spawn_sweeper(&self, cancel: &CancellationToken) -> Option<Sweeper> {
        let retention = self.settings.retention?;
        let stats = Arc::new(SweepStats::default());
        let sweep = tokio::spawn(super::retention::run_sweeper(
            self.settings.dir.clone(),
            retention,
            self.settings.sweep_interval,
            IO_TIMEOUT,
            self.clock.clone(),
            Arc::clone(&stats),
            cancel.child_token(),
        ));
        let watched = Arc::clone(&stats);
        let watcher = tokio::spawn(async move {
            if let Err(error) = sweep.await
                && error.is_panic()
            {
                watched.note_panic();
                tracing::error!(target: "brisvox::privacy", "the retention sweeper panicked; expired recordings are no longer deleted");
            }
        });
        Some(Sweeper { watcher, stats })
    }

    /// Attaches the recording of one call to `builder` and starts its tasks. The returned
    /// [`CallRecording`] must be kept for the life of the call and finished at its end.
    ///
    /// The transcript uses the builder's event ring when it has one (the RTVI bridge's) and
    /// installs its own otherwise, so call this before anything that creates the ring.
    pub fn attach<S, L, T>(
        &self,
        builder: SessionBuilder<S, L, T>,
        call: CallInfo<'_>,
    ) -> (SessionBuilder<S, L, T>, CallRecording)
    where
        S: SttProvider,
        L: LlmProvider,
        T: TtsProvider,
    {
        let start = self.clock.now();
        let started_unix = self
            .clock
            .wall()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let stop = CancellationToken::new();
        let mut tasks = Vec::new();
        let mut builder = builder;
        self.stats.calls.fetch_add(1, Ordering::Relaxed);
        let path = |kind| {
            self.settings.dir.join(file_name_for(
                started_unix,
                call.session,
                kind,
                self.settings.age_recipients.is_some(),
            ))
        };

        if self.settings.transcripts {
            let ring = builder
                .events()
                .cloned()
                .unwrap_or_else(|| EngineEvents::new(DEFAULT_EVENT_CAPACITY));
            let stream = ring.subscribe();
            builder = builder.with_events(ring);
            // The engine's answer to "what did the caller hear", for the bot lines.
            let (heard_tx, heard_rx) = mpsc::channel(HEARD_QUEUE);
            let observer_stats = Arc::clone(&self.stats);
            builder = builder.with_source_wrap(Arc::new(move |inner| {
                Arc::new(HeardObserver {
                    inner,
                    tx: heard_tx.clone(),
                    stats: Arc::clone(&observer_stats),
                })
            }));
            tasks.push(tokio::spawn(transcript_task(TranscriptJob {
                path: path(Kind::Transcript),
                header: Header {
                    session: call.session,
                    agent: call.agent.to_owned(),
                    started_unix,
                    redacted: self.settings.redact_pii,
                },
                redact: self
                    .settings
                    .redact_pii
                    .then_some(self.settings.redact_kinds),
                recipients: self.settings.age_recipients.clone(),
                events: stream,
                heard: heard_rx,
                clock: self.clock.clone(),
                start,
                stop: stop.clone(),
                stats: Arc::clone(&self.stats),
            })));
        }

        if self.settings.audio {
            let (tx, rx) = mpsc::channel(AUDIO_QUEUE);
            builder = builder
                .with_caller_audio_tap(CallerTap {
                    tx: tx.clone(),
                    stats: Arc::clone(&self.stats),
                })
                .with_tap(BotTap {
                    tx,
                    clock: self.clock.clone(),
                    epoch: TurnEpoch::ZERO,
                    stats: Arc::clone(&self.stats),
                });
            tasks.push(tokio::spawn(audio_task(AudioJob {
                rx,
                caller_path: path(Kind::Caller),
                bot_path: path(Kind::Bot),
                clock: self.clock.clone(),
                start,
                max_audio: self.settings.max_audio,
                recipients: self.settings.age_recipients.clone(),
                stop: stop.clone(),
                stats: Arc::clone(&self.stats),
            })));
        }
        (builder, CallRecording { stop, tasks })
    }
}

/// The running retention sweeper; see [`Recording::spawn_sweeper`].
#[derive(Debug)]
pub struct Sweeper {
    watcher: JoinHandle<()>,
    stats: Arc<SweepStats>,
}

impl Sweeper {
    /// The sweeper's counters.
    pub fn stats(&self) -> Arc<SweepStats> {
        Arc::clone(&self.stats)
    }

    /// Waits for the sweeper to end (after its cancellation token fired), for at most
    /// `timeout` on `clock`; a sweep that does not stop in time is abandoned and logged.
    pub async fn join(self, clock: &dyn brisvox_core::Clock, timeout: Duration) {
        let deadline = brisvox_core::clock::deadline_after(clock.now(), timeout);
        tokio::select! {
            biased;
            _ = self.watcher => {}
            () = clock.sleep_until(deadline) => {
                tracing::warn!(target: "brisvox::privacy", "the retention sweeper did not stop in time");
            }
        }
    }
}

/// The recording tasks of one call.
#[derive(Debug)]
pub struct CallRecording {
    stop: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl CallRecording {
    /// Stops the recording tasks after they drained what is queued and waits for them to finish
    /// their files, for at most [`FINISH_TIMEOUT`]. A task that does not finish is aborted
    /// (its file keeps the streaming WAV header, which players read to the end).
    pub async fn finish(mut self) {
        self.stop.cancel();
        let mut tasks = std::mem::take(&mut self.tasks);
        let all_done = async {
            for task in &mut tasks {
                // A task that panicked was already logged by the runtime's hook; its files stay
                // as they are.
                let _ = task.await;
            }
        };
        if tokio::time::timeout(FINISH_TIMEOUT, all_done)
            .await
            .is_err()
        {
            for task in &tasks {
                task.abort();
            }
            tracing::warn!(target: "brisvox::privacy", "a recording task did not finish in time and was aborted");
        }
    }
}

impl Drop for CallRecording {
    /// A call that ends without [`Self::finish`] (an early error) still gets its files
    /// finished: the tasks are told to stop and complete on their own.
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

// ---------------------------------------------------------------------------------------------
// Audio

/// One piece of audio traffic for the audio task.
#[derive(Debug)]
enum AudioMsg {
    Caller {
        rate: SampleRate,
        seq: u64,
        pcm: Bytes,
    },
    Bot {
        at: Instant,
        rate: SampleRate,
        pcm: Bytes,
    },
    BotInterrupt {
        at: Instant,
    },
}

/// Queues an audio frame, counting the drop when the queue has fewer than [`INTERRUPT_RESERVE`]
/// free slots (the rest is kept for barge-in markers) or is closed.
fn queue(tx: &mpsc::Sender<AudioMsg>, msg: AudioMsg, stats: &RecordingStats) {
    if tx.capacity() <= INTERRUPT_RESERVE || tx.try_send(msg).is_err() {
        stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Queues a barge-in marker: it may use the slots audio frames leave free.
fn queue_interrupt(tx: &mpsc::Sender<AudioMsg>, msg: AudioMsg, stats: &RecordingStats) {
    if tx.try_send(msg).is_err() {
        stats.interrupts_lost.fetch_add(1, Ordering::Relaxed);
    }
}

/// The caller-audio tap of a recorded call.
struct CallerTap {
    tx: mpsc::Sender<AudioMsg>,
    stats: Arc<RecordingStats>,
}

impl CallerAudioTap for CallerTap {
    fn observe(&mut self, audio: &AudioFrame) {
        queue(
            &self.tx,
            AudioMsg::Caller {
                rate: audio.sample_rate(),
                seq: audio.seq(),
                pcm: audio.data().clone(),
            },
            &self.stats,
        );
    }
}

/// The bot-audio tap of a recorded call. It drops audio of epochs a barge-in ended (the tap
/// sees the frames `TransportOut` is about to discard) and tells the track about the barge-in.
struct BotTap {
    tx: mpsc::Sender<AudioMsg>,
    clock: SharedClock,
    epoch: TurnEpoch,
    stats: Arc<RecordingStats>,
}

impl FrameTap for BotTap {
    fn observe(&mut self, frame: &Frame) {
        match frame {
            Frame::System(SystemFrame::Interrupt { new_epoch }) => {
                self.epoch = self.epoch.max(*new_epoch);
                queue_interrupt(
                    &self.tx,
                    AudioMsg::BotInterrupt {
                        at: self.clock.now(),
                    },
                    &self.stats,
                );
            }
            Frame::Data(DataFrame::Audio(audio)) if audio.epoch() >= self.epoch => {
                queue(
                    &self.tx,
                    AudioMsg::Bot {
                        at: self.clock.now(),
                        rate: audio.sample_rate(),
                        pcm: audio.data().clone(),
                    },
                    &self.stats,
                );
            }
            _ => {}
        }
    }
}

struct AudioJob {
    rx: mpsc::Receiver<AudioMsg>,
    caller_path: PathBuf,
    bot_path: PathBuf,
    clock: SharedClock,
    start: Instant,
    max_audio: Duration,
    recipients: Option<AgeRecipients>,
    stop: CancellationToken,
    stats: Arc<RecordingStats>,
}

/// One track's file: created at the first chunk, abandoned at the first error.
struct TrackFile {
    path: PathBuf,
    clock: SharedClock,
    writer: Option<WavWriter>,
    failed: bool,
    recipients: Option<AgeRecipients>,
}

impl TrackFile {
    const fn new(path: PathBuf, clock: SharedClock, recipients: Option<AgeRecipients>) -> Self {
        Self {
            path,
            clock,
            recipients,
            writer: None,
            failed: false,
        }
    }

    /// Appends `chunks` (creating the file at the first one, at `rate`).
    async fn write(
        &mut self,
        rate: Option<SampleRate>,
        chunks: &mut Vec<Bytes>,
        stats: &RecordingStats,
    ) {
        if self.failed {
            chunks.clear();
            return;
        }
        for chunk in chunks.drain(..) {
            if self.writer.is_none() {
                let Some(rate) = rate else { continue };
                match WavWriter::create_with(
                    self.clock.clone(),
                    &self.path,
                    rate.hz(),
                    IO_TIMEOUT,
                    self.recipients.as_ref(),
                )
                .await
                {
                    Ok(writer) => self.writer = Some(writer),
                    Err(e) => return self.fail("create", &e, stats),
                }
            }
            if let Some(writer) = self.writer.as_mut()
                && let Err(e) = writer.append(&chunk).await
            {
                return self.fail("write", &e, stats);
            }
        }
    }

    fn fail(&mut self, what: &str, error: &std::io::Error, stats: &RecordingStats) {
        self.failed = true;
        self.writer = None;
        stats.write_errors.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(target: "brisvox::privacy", file = %file_label(&self.path), what, %error, "audio recording disabled for this file");
    }

    async fn finish(self, stats: &RecordingStats) {
        let Some(writer) = self.writer else { return };
        match writer.finalize().await {
            Ok(_) => {
                stats.audio_files.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                stats.write_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(target: "brisvox::privacy", file = %file_label(&self.path), error = %e, "cannot finish a WAV file");
            }
        }
    }
}

/// The file name of `path`, for logs.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// The state of the audio task.
struct AudioState {
    caller: CallerTrack,
    caller_file: TrackFile,
    bot: BotTrack,
    bot_file: TrackFile,
    out: Vec<Bytes>,
}

impl AudioState {
    async fn handle(&mut self, msg: AudioMsg, stats: &RecordingStats) {
        match msg {
            AudioMsg::Caller { rate, seq, pcm } => {
                self.caller.push(rate, seq, &pcm, &mut self.out);
                self.caller_file
                    .write(self.caller.rate(), &mut self.out, stats)
                    .await;
            }
            AudioMsg::Bot { at, rate, pcm } => {
                self.bot.push(at, rate, &pcm, &mut self.out);
                self.bot_file
                    .write(self.bot.rate(), &mut self.out, stats)
                    .await;
            }
            AudioMsg::BotInterrupt { at } => {
                self.bot.interrupt(at, &mut self.out);
                self.bot_file
                    .write(self.bot.rate(), &mut self.out, stats)
                    .await;
            }
        }
    }
}

async fn audio_task(job: AudioJob) {
    let AudioJob {
        mut rx,
        caller_path,
        bot_path,
        clock,
        start,
        max_audio,
        recipients,
        stop,
        stats,
    } = job;
    let mut state = AudioState {
        caller: CallerTrack::new(max_audio),
        caller_file: TrackFile::new(caller_path, clock.clone(), recipients.clone()),
        bot: BotTrack::new(start, max_audio),
        bot_file: TrackFile::new(bot_path, clock.clone(), recipients),
        out: Vec::with_capacity(4),
    };
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            msg = rx.recv() => match msg {
                Some(msg) => state.handle(msg, &stats).await,
                None => break,
            },
        }
    }
    // Whatever is still queued (bounded by the queue's capacity), then the end of the call.
    while let Ok(msg) = rx.try_recv() {
        state.handle(msg, &stats).await;
    }
    state.bot.finish(clock.now(), &mut state.out);
    state
        .bot_file
        .write(state.bot.rate(), &mut state.out, &stats)
        .await;
    let AudioState {
        caller,
        caller_file,
        bot,
        bot_file,
        ..
    } = state;
    tracing::info!(
        target: "brisvox::privacy",
        caller = ?caller.counters(),
        bot = ?bot.counters(),
        "audio recording finished"
    );
    caller_file.finish(&stats).await;
    bot_file.finish(&stats).await;
}

// ---------------------------------------------------------------------------------------------
// Transcript

/// First line of a transcript file.
struct Header {
    session: u64,
    agent: String,
    started_unix: u64,
    redacted: bool,
}

struct TranscriptJob {
    path: PathBuf,
    header: Header,
    /// The kinds to redact; `None` records the text as it is.
    redact: Option<PiiKinds>,
    events: EventStream,
    heard: mpsc::Receiver<HeardNote>,
    clock: SharedClock,
    start: Instant,
    stop: CancellationToken,
    stats: Arc<RecordingStats>,
    /// Encrypt the file to these recipients.
    recipients: Option<AgeRecipients>,
}

/// What the engine settled about the caller's hearing of one LLM round.
#[derive(Debug)]
enum Heard {
    /// A prefix of the round's text (empty: nothing was heard).
    Partial { text: Arc<str>, estimated: bool },
    /// The whole round's text.
    Whole(Arc<str>),
    /// The engine had no answer (`Cut::Keep`).
    Unknown,
}

/// The answer of the truncation source about one round, on its way to the transcript task.
#[derive(Debug)]
struct HeardNote {
    key: RoundKey,
    /// When the barge-in happened; `None` for a round that ended by itself.
    barge_in: Option<Instant>,
    heard: Heard,
}

/// The call's truncation source with a view on its answers (the engine's `PlayoutBook` verdict
/// on what was played, which also cuts the LLM context): every answer is queued, bounded, for
/// the transcript task. The answer itself passes unchanged.
struct HeardObserver {
    inner: Arc<dyn TruncationSource>,
    tx: mpsc::Sender<HeardNote>,
    stats: Arc<RecordingStats>,
}

impl TruncationSource for HeardObserver {
    fn cut<'a>(&'a self, request: &'a CutRequest) -> BoxFuture<'a, Cut> {
        Box::pin(async move {
            let cut = self.inner.cut(request).await;
            let heard = match &cut {
                Cut::Keep => Heard::Unknown,
                Cut::Heard(t) => Heard::Partial {
                    text: Arc::clone(&t.text),
                    estimated: t.estimated,
                },
                Cut::HeardInFull(t) => Heard::Whole(Arc::clone(&t.text)),
            };
            let note = HeardNote {
                key: request.key,
                barge_in: request.interrupted_at,
                heard,
            };
            if self.tx.try_send(note).is_err() {
                self.stats.heard_lost.fetch_add(1, Ordering::Relaxed);
            }
            cut
        })
    }
}

/// Text of at most [`MAX_BOT_TURN_BYTES`] bytes, cut at a character boundary; whether it was cut.
fn clip(text: &str) -> (&str, bool) {
    if text.len() <= MAX_BOT_TURN_BYTES {
        return (text, false);
    }
    let mut end = MAX_BOT_TURN_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text.get(..end).unwrap_or_default(), true)
}

/// The bot's words of the response being spoken: sent to the speech synthesis, not yet known to
/// have been heard. Written only when the transport confirms the response played to its end.
#[derive(Default)]
struct BotLine {
    epoch: Option<TurnEpoch>,
    text: String,
    cut: bool,
}

impl BotLine {
    fn push(&mut self, text: &str) {
        let needs_space = !self.text.is_empty()
            && !self.text.ends_with(char::is_whitespace)
            && !text.starts_with(char::is_whitespace);
        let extra = text.len() + usize::from(needs_space);
        if self.text.len() + extra > MAX_BOT_TURN_BYTES {
            self.cut = true;
            return;
        }
        if needs_space {
            self.text.push(' ');
        }
        self.text.push_str(text);
    }
}

/// An epoch whose spoken extent the transport did not confirm (interrupted, superseded, or the
/// call ended): the engine's answer decides what is written for it.
struct Unconfirmed {
    epoch: TurnEpoch,
    /// An answer for it was written.
    noted: bool,
}

struct TranscriptWriter {
    file: RecordingSink,
    path: PathBuf,
    redact: Option<PiiKinds>,
    clock: SharedClock,
    start: Instant,
    stats: Arc<RecordingStats>,
    failed: bool,
    bot: BotLine,
    /// Epochs whose text was written in full (the transport confirmed the end).
    confirmed: VecDeque<TurnEpoch>,
    unconfirmed: VecDeque<Unconfirmed>,
}

impl TranscriptWriter {
    fn t_ms(&self) -> u64 {
        self.ms_of(self.clock.now())
    }

    fn ms_of(&self, at: Instant) -> u64 {
        u64::try_from(at.saturating_duration_since(self.start).as_millis()).unwrap_or(u64::MAX)
    }

    fn clean<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        match self.redact {
            Some(kinds) => redact_pii_with(text, kinds),
            None => std::borrow::Cow::Borrowed(text),
        }
    }

    async fn line(&mut self, value: serde_json::Value) {
        if self.failed {
            return;
        }
        let Ok(mut line) = serde_json::to_string(&value) else {
            return;
        };
        line.push('\n');
        match self
            .file
            .write_all(&*self.clock, IO_TIMEOUT, line.as_bytes())
            .await
        {
            Ok(()) => {
                self.stats.transcript_lines.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                self.failed = true;
                self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(target: "brisvox::privacy", file = %file_label(&self.path), error = %e, "transcript recording disabled for this call");
            }
        }
    }

    fn remember(list: &mut VecDeque<TurnEpoch>, epoch: TurnEpoch) {
        if list.len() >= MAX_TRACKED_EPOCHS {
            list.pop_front();
        }
        list.push_back(epoch);
    }

    /// The response being spoken ended and the transport confirmed it played out: its text is
    /// the bot's line, in full.
    async fn confirm_bot(&mut self) {
        let line = std::mem::take(&mut self.bot);
        let Some(epoch) = line.epoch else { return };
        Self::remember(&mut self.confirmed, epoch);
        if line.text.is_empty() {
            return;
        }
        let text = self.clean(&line.text).into_owned();
        let t_ms = self.t_ms();
        self.line(serde_json::json!({
            "type": "bot",
            "t_ms": t_ms,
            "text": text,
            "interrupted": false,
            "heard": "full",
            "cut": line.cut,
        }))
        .await;
    }

    /// The response being spoken ended without the transport confirming it (a barge-in, a
    /// newer response, the end of the call, or a lost event): its text is dropped, never
    /// written; the engine's answer says what was heard.
    async fn unconfirm_bot(&mut self) {
        let line = std::mem::take(&mut self.bot);
        let Some(epoch) = line.epoch else { return };
        self.unconfirm(epoch).await;
    }

    async fn unconfirm(&mut self, epoch: TurnEpoch) {
        if self.confirmed.contains(&epoch) || self.unconfirmed.iter().any(|u| u.epoch == epoch) {
            return;
        }
        if self.unconfirmed.len() >= MAX_TRACKED_EPOCHS
            && let Some(oldest) = self.unconfirmed.pop_front()
            && !oldest.noted
        {
            self.unknown_line(None, None).await;
        }
        self.unconfirmed.push_back(Unconfirmed {
            epoch,
            noted: false,
        });
    }

    async fn unknown_line(&mut self, round: Option<u32>, barge_in: Option<u64>) {
        let t_ms = self.t_ms();
        self.line(serde_json::json!({
            "type": "bot",
            "t_ms": t_ms,
            "text": "",
            "interrupted": barge_in.is_some().then_some(true),
            "heard": "unknown",
            "round": round,
            "barge_in_ms": barge_in,
        }))
        .await;
    }

    /// The engine's answer about one round.
    async fn note(&mut self, note: HeardNote) {
        let epoch = note.key.epoch;
        let interrupted = note.barge_in.is_some();
        // A round that ended by itself is only of interest when the transport did not confirm
        // its end (and no line was written for the epoch).
        let waiting =
            self.unconfirmed.iter().any(|u| u.epoch == epoch) || self.bot.epoch == Some(epoch);
        let relevant = interrupted || (waiting && !self.confirmed.contains(&epoch));
        if !relevant {
            return;
        }
        // The answer replaces whatever the events say about the epoch.
        if self.bot.epoch == Some(epoch) {
            self.bot = BotLine::default();
        }
        match self.unconfirmed.iter_mut().find(|u| u.epoch == epoch) {
            Some(entry) => entry.noted = true,
            None if !self.confirmed.contains(&epoch) => {
                if self.unconfirmed.len() >= MAX_TRACKED_EPOCHS {
                    self.unconfirmed.pop_front();
                }
                self.unconfirmed
                    .push_back(Unconfirmed { epoch, noted: true });
            }
            None => {}
        }
        let barge_in = note.barge_in.map(|at| self.ms_of(at));
        let round = note.key.round;
        let (text, estimated, whole) = match &note.heard {
            Heard::Partial { text, estimated } => (text, *estimated, false),
            Heard::Whole(text) => (text, false, true),
            Heard::Unknown => return self.unknown_line(Some(round), barge_in).await,
        };
        let (text, cut) = clip(text.trim());
        if text.is_empty() {
            return; // nothing was heard
        }
        let text = self.clean(text).into_owned();
        let t_ms = self.t_ms();
        self.line(serde_json::json!({
            "type": "bot",
            "t_ms": t_ms,
            "text": text,
            "interrupted": interrupted,
            "heard": if whole { "full" } else { "partial" },
            "estimated": estimated,
            "round": round,
            "barge_in_ms": barge_in,
            "cut": cut,
        }))
        .await;
    }

    async fn event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::UserText { text, .. } => {
                let text = self.clean(&text).into_owned();
                let t_ms = self.t_ms();
                self.line(serde_json::json!({"type": "user", "t_ms": t_ms, "text": text}))
                    .await;
            }
            EngineEvent::BotTtsText { epoch, text } => {
                if self.bot.epoch.is_some_and(|e| e != epoch) {
                    // A newer response started without the older one's end being confirmed.
                    self.unconfirm_bot().await;
                }
                self.bot.epoch = Some(epoch);
                self.bot.push(&text);
            }
            EngineEvent::BotStoppedSpeaking { epoch } => {
                if self.bot.epoch == Some(epoch) {
                    self.confirm_bot().await;
                }
            }
            EngineEvent::BotInterrupted { .. } => self.unconfirm_bot().await,
            _ => {}
        }
    }

    /// The end of the call: what is still open is unconfirmed; every epoch the engine did not
    /// answer for is written as unknown.
    async fn close(&mut self) {
        self.unconfirm_bot().await;
    }

    async fn write_unknowns(&mut self) {
        let open: Vec<TurnEpoch> = self
            .unconfirmed
            .iter()
            .filter(|u| !u.noted)
            .map(|u| u.epoch)
            .collect();
        for _ in open {
            self.unknown_line(None, None).await;
        }
        self.unconfirmed.clear();
    }
}

async fn transcript_task(job: TranscriptJob) {
    let TranscriptJob {
        path,
        header,
        redact,
        mut events,
        mut heard,
        clock,
        start,
        stop,
        stats,
        recipients,
    } = job;
    let file = match RecordingSink::create(&*clock, &path, IO_TIMEOUT, recipients.as_ref()).await {
        Ok(file) => file,
        Err(e) => {
            stats.write_errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(target: "brisvox::privacy", file = %file_label(&path), error = %e, "cannot create the transcript file; this call's transcript is not recorded");
            return;
        }
    };
    let mut writer = TranscriptWriter {
        file,
        path,
        redact,
        clock,
        start,
        stats: Arc::clone(&stats),
        failed: false,
        bot: BotLine::default(),
        confirmed: VecDeque::new(),
        unconfirmed: VecDeque::new(),
    };
    writer
        .line(serde_json::json!({
            "type": "call",
            "version": 2,
            "session": header.session,
            "agent": header.agent,
            "started_unix": header.started_unix,
            "pii_redacted": header.redacted,
        }))
        .await;
    let mut heard_open = true;
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            event = events.recv() => match event {
                Some(event) => writer.event(event).await,
                None => break,
            },
            note = heard.recv(), if heard_open => match note {
                Some(note) => writer.note(note).await,
                None => heard_open = false,
            },
        }
    }
    // The events already in the ring when the call ended (the bot's last words), then the
    // engine's answers that are in already (it settles the context when the call stops).
    while let Some(Some(event)) = events.recv().now_or_never() {
        writer.event(event).await;
    }
    writer.close().await;
    while let Ok(note) = heard.try_recv() {
        writer.note(note).await;
    }
    writer.write_unknowns().await;
    let lost = events.lost();
    stats.events_lost.fetch_add(lost, Ordering::Relaxed);
    let t_ms = writer.t_ms();
    writer
        .line(serde_json::json!({"type": "end", "t_ms": t_ms, "events_lost": lost}))
        .await;
    if !writer.failed {
        let finished = writer.file.finish(&*writer.clock, IO_TIMEOUT).await;
        if let Err(e) = finished {
            stats.write_errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(target: "brisvox::privacy", file = %file_label(&writer.path), error = %e, "cannot finish the transcript");
        }
    }
}

#[cfg(test)]
mod tests;
