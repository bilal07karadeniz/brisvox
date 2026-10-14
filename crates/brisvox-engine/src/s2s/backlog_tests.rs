//! The session task with the audio queue and the command queue driven directly:
//! local turn mode, where the commands of the VAD and the caller audio travel on two queues.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::disallowed_methods
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use brisvox_agent::Context;
use brisvox_core::{Direction, Inbox, QueueConfig, SimClock};
use brisvox_providers::s2s::{S2sConfig, S2sControl, S2sEvent, S2sProvider, S2sSession};
use bytes::Bytes;
use tokio::sync::mpsc;

use super::{Cmd, S2sOptions, S2sStageConfig, S2sStats, S2sTurnMode, TaskParts, task};
use crate::config::EngineConfig;
use crate::offload::Offload;
use crate::playout::PlayoutBook;
use crate::rates::CallRatesCell;

/// What the provider session saw, in the order its two queues were served.
#[derive(Default)]
struct Seen {
    /// `'a'` per audio chunk, `'c'` per commit.
    log: Mutex<String>,
    opened: AtomicUsize,
}

struct Provider {
    seen: Arc<Seen>,
    /// Keeps the event sender alive: a closed event stream would end the session.
    keep: Mutex<Vec<mpsc::Sender<S2sEvent>>>,
}

impl S2sProvider for Provider {
    fn name(&self) -> &'static str {
        "backlog-mock"
    }

    async fn connect(&self, _config: S2sConfig) -> brisvox_providers::Result<S2sSession> {
        let (audio_tx, mut audio_rx) = mpsc::channel::<Bytes>(64);
        let (control_tx, mut control_rx) = mpsc::channel::<S2sControl>(32);
        let (event_tx, event_rx) = mpsc::channel::<S2sEvent>(8);
        self.keep.lock().unwrap().push(event_tx);
        let log = Arc::clone(&self.seen);
        tokio::spawn(async move {
            while audio_rx.recv().await.is_some() {
                log.log.lock().unwrap().push('a');
            }
        });
        let log = Arc::clone(&self.seen);
        tokio::spawn(async move {
            while let Some(control) = control_rx.recv().await {
                if matches!(control, S2sControl::Commit) {
                    log.log.lock().unwrap().push('c');
                }
            }
        });
        self.seen.opened.fetch_add(1, Ordering::SeqCst);
        Ok(S2sSession {
            audio: audio_tx,
            control: control_tx,
            events: event_rx,
        })
    }
}

/// 20 ms of 16 kHz PCM16 (a tone, never silence).
fn chunk() -> Bytes {
    let mut data = Vec::with_capacity(640);
    for i in 0..320_usize {
        let sample: i16 = if (i / 8) % 2 == 0 { 3000 } else { -3000 };
        data.extend_from_slice(&sample.to_le_bytes());
    }
    Bytes::from(data)
}

#[tokio::test(start_paused = true)]
async fn local_commands_see_the_audio_queued_before_them() {
    let clock: brisvox_core::SharedClock = Arc::new(SimClock::new());
    let engine = EngineConfig::DEFAULT;
    let config = S2sStageConfig::from_engine(
        &engine,
        Arc::from("test"),
        S2sOptions {
            turn: S2sTurnMode::Local,
            ..S2sOptions::default()
        },
    );
    let seen = Arc::new(Seen::default());
    let provider = Arc::new(Provider {
        seen: Arc::clone(&seen),
        keep: Mutex::default(),
    });
    let (audio_tx, audio_rx) = mpsc::channel::<Bytes>(64);
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(16);
    // Frames the task emits go nowhere in particular; the inbox only has to outlive the task.
    let inbox = Inbox::new(QueueConfig::DEFAULT);
    let downstream = inbox.sender(Direction::Downstream);
    let parts = TaskParts {
        config,
        provider,
        clock: Arc::clone(&clock),
        timeline: brisvox_core::TimelineRecorder::new(),
        book: Arc::new(PlayoutBook::new(64)),
        rates: CallRatesCell::new(engine.rates.fallback_call),
        offload: Offload::Inline(Arc::clone(&clock)),
        hook: Arc::new(|_| Box::pin(async { Ok(()) })),
        tools: None,
        events: None,
        context: Arc::new(Mutex::new(Context::new())),
        stats: S2sStats::default(),
        audio_rx,
        cmd_rx,
    };
    let handle = tokio::spawn(task::run(parts, downstream));
    while seen.opened.load(Ordering::SeqCst) == 0 {
        clock.sleep(Duration::from_millis(5)).await;
    }

    // The VAD hears speech and its end while ten frames of it are still queued: no yield
    // between the sends, so the task sees two commands and ten chunks at once.
    let now = clock.now();
    cmd_tx.try_send(Cmd::UserStarted(now, 0)).unwrap();
    for _ in 0..10 {
        audio_tx.try_send(chunk()).unwrap();
    }
    cmd_tx.try_send(Cmd::UserStopped(now, 10)).unwrap();
    clock.sleep(Duration::from_millis(200)).await;

    let log = seen.log.lock().unwrap().clone();
    let commit = log.find('c').expect("the utterance was committed");
    let before = log[..commit].matches('a').count();
    let after = log[commit..].matches('a').count();
    // The 16 kHz to 24 kHz resampler may hold back a frame or two; the point is that the
    // utterance is in the committed buffer, not in the next turn's pre-roll.
    assert!(before >= 8, "{before} frames before the commit: {log}");
    assert_eq!(after, 0, "nothing leaks behind the commit: {log}");
    drop(audio_tx);
    drop(cmd_tx);
    let _ = handle.await;
    drop(inbox);
}
