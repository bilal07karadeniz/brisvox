//! The recording tasks and taps on a real file system and real time (disk IO is real).
#![allow(clippy::disallowed_methods)]

use std::fs;
use std::path::PathBuf;

use brisvox_core::SystemClock;
use serde_json::Value;

use super::*;

const WAIT: Duration = Duration::from_secs(10);

/// A fresh empty directory for one test.
fn dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("brisvox-recording-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The sample bytes of a WAV file, after checking its header describes exactly them.
fn wav_data(path: &Path, rate: u32) -> Vec<u8> {
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), rate);
    let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
    assert_eq!(
        data_len,
        bytes.len() - 44,
        "the header carries the true length"
    );
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
        data_len + 36
    );
    bytes[44..].to_vec()
}

struct Transcript {
    ring: EngineEvents,
    notes: mpsc::Sender<HeardNote>,
    stop: CancellationToken,
    stats: Arc<RecordingStats>,
    task: JoinHandle<()>,
    path: PathBuf,
}

fn start_transcript(dir: &Path, redact: bool, capacity: usize) -> Transcript {
    start_transcript_with(dir, redact, capacity, None)
}

fn start_transcript_with(
    dir: &Path,
    redact: bool,
    capacity: usize,
    recipients: Option<AgeRecipients>,
) -> Transcript {
    let clock = SystemClock::shared();
    let ring = EngineEvents::new(capacity);
    let stop = CancellationToken::new();
    let stats = Arc::new(RecordingStats::default());
    let path = dir.join(file_name_for(
        1_760_000_000,
        7,
        Kind::Transcript,
        recipients.is_some(),
    ));
    let (notes, heard) = mpsc::channel(HEARD_QUEUE);
    let task = tokio::spawn(transcript_task(TranscriptJob {
        path: path.clone(),
        header: Header {
            session: 7,
            agent: "test-agent".to_owned(),
            started_unix: 1_760_000_000,
            redacted: redact,
        },
        redact: redact.then_some(PiiKinds::ALL),
        events: ring.subscribe(),
        heard,
        start: clock.now(),
        clock,
        stop: stop.clone(),
        stats: Arc::clone(&stats),
        recipients,
    }));
    Transcript {
        ring,
        notes,
        stop,
        stats,
        task,
        path,
    }
}

fn epoch(n: u32) -> TurnEpoch {
    TurnEpoch::new(n)
}

fn user(ring: &EngineEvents, text: &str) {
    ring.publish(EngineEvent::UserText {
        epoch: epoch(1),
        text: Arc::from(text),
    });
}

fn bot(ring: &EngineEvents, n: u32, text: &str) {
    ring.publish(EngineEvent::BotTtsText {
        epoch: epoch(n),
        text: Arc::from(text),
    });
}

async fn finished(task: JoinHandle<()>) {
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
}

#[tokio::test]
async fn a_transcript_has_a_header_the_turns_in_order_and_an_end_line() {
    let dir = dir("transcript");
    let t = start_transcript(&dir, false, 64);
    user(&t.ring, "hello there");
    bot(&t.ring, 1, "Hi, how can I help?");
    t.ring
        .publish(EngineEvent::BotStoppedSpeaking { epoch: epoch(1) });
    user(&t.ring, "book me a table");
    bot(&t.ring, 2, "Sure.");
    bot(&t.ring, 2, "For how many?");
    t.ring
        .publish(EngineEvent::BotStoppedSpeaking { epoch: epoch(2) });
    drop(t.ring);
    finished(t.task).await;

    let l = lines(&t.path);
    let kinds: Vec<&str> = l.iter().map(|v| v["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["call", "user", "bot", "user", "bot", "end"]);
    assert_eq!(l[0]["session"], 7);
    assert_eq!(l[0]["agent"], "test-agent");
    assert_eq!(l[0]["started_unix"], 1_760_000_000_u64);
    assert_eq!(l[0]["pii_redacted"], false);
    assert_eq!(l[1]["text"], "hello there");
    assert_eq!(l[2]["text"], "Hi, how can I help?");
    assert_eq!(l[2]["interrupted"], false);
    assert_eq!(
        l[4]["text"], "Sure. For how many?",
        "chunks of one response join"
    );
    assert_eq!(l[5]["events_lost"], 0);
    assert_eq!(t.stats.transcript_lines(), 6);
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn pii_is_redacted_before_it_reaches_the_disk_when_asked_to() {
    let dir = dir("redact");
    let t = start_transcript(&dir, true, 64);
    user(
        &t.ring,
        "my email is jane@example.com and my number 0532 123 45 67",
    );
    bot(&t.ring, 1, "Thanks, I noted jane@example.com.");
    t.ring
        .publish(EngineEvent::BotStoppedSpeaking { epoch: epoch(1) });
    drop(t.ring);
    finished(t.task).await;

    let raw = fs::read_to_string(&t.path).unwrap();
    assert!(!raw.contains("jane@example.com"), "{raw}");
    assert!(!raw.contains("0532"), "{raw}");
    let l = lines(&t.path);
    assert_eq!(l[0]["pii_redacted"], true);
    assert_eq!(l[1]["text"], "my email is [EMAIL] and my number [PHONE]");
    assert_eq!(l[2]["text"], "Thanks, I noted [EMAIL].");
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn redaction_off_records_the_text_as_it_is() {
    let dir = dir("noredact");
    let t = start_transcript(&dir, false, 64);
    user(&t.ring, "write to jane@example.com");
    drop(t.ring);
    finished(t.task).await;
    assert_eq!(lines(&t.path)[1]["text"], "write to jane@example.com");
    fs::remove_dir_all(&dir).unwrap();
}

/// The engine's answer for round `round` of `n`: `heard` of it, after a barge-in (or not).
fn answer(n: u32, round: u32, barge_in: bool, heard: Heard) -> HeardNote {
    HeardNote {
        key: RoundKey::new(epoch(n), round),
        barge_in: barge_in.then(|| SystemClock::shared().now()),
        heard,
    }
}

fn partial(text: &str) -> Heard {
    Heard::Partial {
        text: Arc::from(text),
        estimated: false,
    }
}

/// Lets the transcript task run until it has read what was published (the task starts with a
/// file creation on the blocking pool, so this is real time).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

#[tokio::test]
async fn a_mid_response_barge_in_writes_only_the_heard_part_of_the_bot_line() {
    let dir = dir("heard");
    let t = start_transcript(&dir, false, 64);
    user(&t.ring, "what are your opening hours");
    // Three sentences went to the speech synthesis; the caller cut in during the first.
    bot(&t.ring, 1, "We open at nine.");
    bot(&t.ring, 1, "On Saturdays we open at ten.");
    bot(&t.ring, 1, "On Sundays we are closed all day.");
    t.ring
        .publish(EngineEvent::BotInterrupted { epoch: epoch(2) });
    user(&t.ring, "stop, I only need the weekday hours");
    settle().await;
    // The engine settles the context at the caller's next turn: only "We open at nine." was heard.
    t.notes
        .send(answer(1, 0, true, partial("We open at nine.")))
        .await
        .unwrap();
    bot(&t.ring, 2, "Weekdays we open at nine.");
    t.ring
        .publish(EngineEvent::BotStoppedSpeaking { epoch: epoch(2) });
    drop(t.ring);
    drop(t.notes);
    finished(t.task).await;

    let raw = fs::read_to_string(&t.path).unwrap();
    for unheard in ["Saturdays", "Sundays", "closed all day"] {
        assert!(!raw.contains(unheard), "unheard text in the file: {raw}");
    }
    let l = lines(&t.path);
    let heard: Vec<&Value> = l
        .iter()
        .filter(|v| v["type"] == "bot" && v["interrupted"] == true)
        .collect();
    assert_eq!(heard.len(), 1, "{raw}");
    assert_eq!(heard[0]["text"], "We open at nine.");
    assert_eq!(heard[0]["heard"], "partial");
    assert_eq!(heard[0]["round"], 0);
    assert!(heard[0]["barge_in_ms"].is_u64());
    let full: Vec<&Value> = l.iter().filter(|v| v["heard"] == "full").collect();
    assert_eq!(full.len(), 1, "{raw}");
    assert_eq!(full[0]["text"], "Weekdays we open at nine.");
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_barge_in_nobody_answered_for_is_written_as_unknown_without_the_text() {
    let dir = dir("interrupt");
    let t = start_transcript(&dir, false, 64);
    bot(&t.ring, 1, "I can offer you a table at");
    t.ring
        .publish(EngineEvent::BotInterrupted { epoch: epoch(2) });
    user(&t.ring, "actually, no");
    drop(t.ring);
    finished(t.task).await;
    let raw = fs::read_to_string(&t.path).unwrap();
    assert!(!raw.contains("table at"), "{raw}");
    let l = lines(&t.path);
    let bot_lines: Vec<&Value> = l.iter().filter(|v| v["type"] == "bot").collect();
    assert_eq!(bot_lines.len(), 1);
    assert_eq!(bot_lines[0]["heard"], "unknown");
    assert_eq!(bot_lines[0]["text"], "");
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_lost_barge_in_event_does_not_turn_unheard_text_into_a_complete_line() {
    let dir = dir("lostint");
    // A ring of 2 and the task reading one event at a time: publishing the rest in one go
    // overwrites the BotInterrupted before the task sees it.
    let t = start_transcript(&dir, false, 2);
    bot(&t.ring, 1, "Your appointment is on Monday.");
    bot(
        &t.ring,
        1,
        "Then there is the second thing, which nobody heard.",
    );
    settle().await;
    t.ring
        .publish(EngineEvent::BotInterrupted { epoch: epoch(2) });
    user(&t.ring, "one");
    user(&t.ring, "two");
    settle().await;
    // The next response arrives; the first was never confirmed as played.
    bot(&t.ring, 2, "Sorry, go on.");
    t.ring
        .publish(EngineEvent::BotStoppedSpeaking { epoch: epoch(2) });
    drop(t.ring);
    drop(t.notes);
    finished(t.task).await;

    let raw = fs::read_to_string(&t.path).unwrap();
    assert!(!raw.contains("nobody heard"), "{raw}");
    assert!(!raw.contains("Monday"), "{raw}");
    let l = lines(&t.path);
    assert!(
        l.iter()
            .any(|v| v["type"] == "bot" && v["heard"] == "unknown"),
        "{raw}"
    );
    let end = l.last().unwrap();
    assert!(end["events_lost"].as_u64().unwrap() >= 1, "{end}");
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn the_engines_answer_fills_in_a_response_whose_end_was_never_confirmed() {
    let dir = dir("lateanswer");
    let t = start_transcript(&dir, false, 64);
    bot(&t.ring, 1, "Your table is booked. Anything else?");
    // No BotStoppedSpeaking and no BotInterrupted (both lost); a later response begins.
    bot(&t.ring, 2, "Goodbye.");
    t.ring
        .publish(EngineEvent::BotStoppedSpeaking { epoch: epoch(2) });
    settle().await;
    t.notes
        .send(answer(
            1,
            0,
            false,
            Heard::Whole(Arc::from("Your table is booked. Anything else?")),
        ))
        .await
        .unwrap();
    drop(t.ring);
    drop(t.notes);
    finished(t.task).await;
    let l = lines(&t.path);
    let texts: Vec<&str> = l
        .iter()
        .filter(|v| v["type"] == "bot")
        .map(|v| v["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["Goodbye.", "Your table is booked. Anything else?"]);
    assert!(
        l.iter().all(|v| v["heard"] != "unknown"),
        "no unknown line once the answer arrived"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn an_answer_for_a_response_already_written_in_full_adds_nothing() {
    let dir = dir("noduplicate");
    let t = start_transcript(&dir, false, 64);
    bot(&t.ring, 1, "All done.");
    t.ring
        .publish(EngineEvent::BotStoppedSpeaking { epoch: epoch(1) });
    settle().await;
    t.notes
        .send(answer(1, 0, false, Heard::Whole(Arc::from("All done."))))
        .await
        .unwrap();
    drop(t.ring);
    drop(t.notes);
    finished(t.task).await;
    let bots = lines(&t.path)
        .into_iter()
        .filter(|v| v["type"] == "bot")
        .count();
    assert_eq!(bots, 1);
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn heard_text_is_redacted_like_the_rest() {
    let dir = dir("heardredact");
    let t = start_transcript(&dir, true, 64);
    bot(&t.ring, 1, "Write to jane@example.com or wait.");
    t.ring
        .publish(EngineEvent::BotInterrupted { epoch: epoch(2) });
    settle().await;
    t.notes
        .send(answer(1, 0, true, partial("Write to jane@example.com")))
        .await
        .unwrap();
    drop(t.ring);
    drop(t.notes);
    finished(t.task).await;
    let raw = fs::read_to_string(&t.path).unwrap();
    assert!(!raw.contains("jane@example.com"), "{raw}");
    assert!(raw.contains("Write to [EMAIL]"), "{raw}");
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn the_truncation_observer_passes_the_answer_on_and_queues_it() {
    use brisvox_agent::Truncation;
    use std::time::Duration as D;

    struct Fixed(Cut);
    impl TruncationSource for Fixed {
        fn cut<'a>(&'a self, _request: &'a CutRequest) -> BoxFuture<'a, Cut> {
            let cut = self.0.clone();
            Box::pin(async move { cut })
        }
    }
    let heard = Truncation {
        text: Arc::from("We open"),
        estimated: true,
        words_heard: 2,
        unmatched_words: 0,
        gaps: 0,
        played: D::from_millis(500),
    };
    let (tx, mut rx) = mpsc::channel(2);
    let stats = Arc::new(RecordingStats::default());
    let observer = HeardObserver {
        inner: Arc::new(Fixed(Cut::Heard(heard.clone()))),
        tx,
        stats: Arc::clone(&stats),
    };
    let request = CutRequest {
        key: RoundKey::new(epoch(3), 1),
        sent: Arc::from("We open at nine."),
        interrupted_at: Some(SystemClock::shared().now()),
    };
    assert_eq!(observer.cut(&request).await, Cut::Heard(heard));
    let note = rx.recv().await.unwrap();
    assert_eq!(note.key, RoundKey::new(epoch(3), 1));
    assert!(note.barge_in.is_some());
    assert!(matches!(
        note.heard,
        Heard::Partial { ref text, estimated: true } if &**text == "We open"
    ));
    // A full queue drops the note, counts it, and still answers.
    let (tx, _rx) = mpsc::channel(1);
    let observer = HeardObserver {
        inner: Arc::new(Fixed(Cut::Keep)),
        tx,
        stats: Arc::clone(&stats),
    };
    assert_eq!(observer.cut(&request).await, Cut::Keep);
    assert_eq!(observer.cut(&request).await, Cut::Keep);
    assert_eq!(stats.heard_lost(), 1);
}

#[tokio::test]
async fn a_bot_line_with_no_end_event_is_not_written_as_if_it_were_heard() {
    let dir = dir("flush");
    let t = start_transcript(&dir, false, 64);
    bot(&t.ring, 1, "Goodbye, and thanks for calling.");
    drop(t.ring);
    finished(t.task).await;
    let raw = fs::read_to_string(&t.path).unwrap();
    assert!(!raw.contains("Goodbye"), "{raw}");
    assert_eq!(lines(&t.path)[1]["heard"], "unknown");
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn events_already_in_the_ring_when_the_recording_is_stopped_are_still_written() {
    let dir = dir("drain");
    let t = start_transcript(&dir, false, 64);
    // Everything is published and the stop comes before the task ever ran.
    user(&t.ring, "one");
    user(&t.ring, "two");
    bot(&t.ring, 1, "three");
    t.ring
        .publish(EngineEvent::BotStoppedSpeaking { epoch: epoch(1) });
    t.stop.cancel();
    finished(t.task).await;
    let texts: Vec<String> = lines(&t.path)
        .iter()
        .filter_map(|v| v["text"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(texts, ["one", "two", "three"]);
    drop(t.ring);
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_reader_that_falls_behind_loses_the_oldest_events_and_says_so() {
    let dir = dir("lost");
    // A ring of 2 and the task not yet running: the first publishes are overwritten.
    let t = start_transcript(&dir, false, 2);
    for i in 0..10 {
        user(&t.ring, &format!("turn {i}"));
    }
    drop(t.ring);
    finished(t.task).await;
    let l = lines(&t.path);
    let end = l.last().unwrap();
    assert_eq!(end["type"], "end");
    assert!(end["events_lost"].as_u64().unwrap() >= 1, "{end}");
    assert_eq!(t.stats.events_lost(), end["events_lost"].as_u64().unwrap());
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn an_existing_recording_is_never_overwritten() {
    let dir = dir("exists");
    let path = dir.join(file_name_for(1_760_000_000, 7, Kind::Transcript, false));
    fs::write(&path, "precious").unwrap();
    let t = start_transcript(&dir, false, 64);
    user(&t.ring, "hello");
    drop(t.ring);
    finished(t.task).await;
    assert_eq!(fs::read_to_string(&path).unwrap(), "precious");
    assert_eq!(t.stats.write_errors(), 1);
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_very_long_bot_response_is_cut_and_marked() {
    let dir = dir("cut");
    let t = start_transcript(&dir, false, 64);
    let chunk = "word ".repeat(1_000);
    for _ in 0..20 {
        bot(&t.ring, 1, &chunk);
    }
    t.ring
        .publish(EngineEvent::BotStoppedSpeaking { epoch: epoch(1) });
    drop(t.ring);
    finished(t.task).await;
    let l = lines(&t.path);
    let line = &l[1];
    assert_eq!(line["cut"], true);
    assert!(line["text"].as_str().unwrap().len() <= MAX_BOT_TURN_BYTES);
    fs::remove_dir_all(&dir).unwrap();
}

// --- audio ---------------------------------------------------------------------------------

struct AudioRig {
    tx: mpsc::Sender<AudioMsg>,
    stop: CancellationToken,
    stats: Arc<RecordingStats>,
    task: JoinHandle<()>,
    start: Instant,
    caller: PathBuf,
    bot: PathBuf,
}

fn start_audio(dir: &Path, max_audio: Duration, queue: usize) -> AudioRig {
    start_audio_with(dir, max_audio, queue, None)
}

fn start_audio_with(
    dir: &Path,
    max_audio: Duration,
    queue: usize,
    recipients: Option<AgeRecipients>,
) -> AudioRig {
    let encrypted = recipients.is_some();
    let clock = SystemClock::shared();
    let (tx, rx) = mpsc::channel(queue);
    let stop = CancellationToken::new();
    let stats = Arc::new(RecordingStats::default());
    let start = clock.now();
    let caller = dir.join(file_name_for(1_760_000_000, 7, Kind::Caller, encrypted));
    let bot = dir.join(file_name_for(1_760_000_000, 7, Kind::Bot, encrypted));
    let task = tokio::spawn(audio_task(AudioJob {
        rx,
        caller_path: caller.clone(),
        bot_path: bot.clone(),
        clock,
        start,
        max_audio,
        recipients,
        stop: stop.clone(),
        stats: Arc::clone(&stats),
    }));
    AudioRig {
        tx,
        stop,
        stats,
        task,
        start,
        caller,
        bot,
    }
}

fn pcm(samples: usize, fill: u8) -> Bytes {
    Bytes::from(vec![fill; samples * 2])
}

async fn send(tx: &mpsc::Sender<AudioMsg>, msg: AudioMsg) {
    tx.send(msg).await.unwrap();
}

#[tokio::test]
async fn caller_audio_becomes_a_wav_with_silence_where_frames_are_missing() {
    let dir = dir("caller");
    let rig = start_audio(&dir, Duration::from_secs(60), 64);
    let rate = SampleRate::Hz16000;
    for seq in [0, 1, 4] {
        // seq 2 and 3 never arrived
        send(
            &rig.tx,
            AudioMsg::Caller {
                rate,
                seq,
                pcm: pcm(320, 5),
            },
        )
        .await;
    }
    drop(rig.tx);
    finished(rig.task).await;

    let data = wav_data(&rig.caller, 16_000);
    assert_eq!(data.len(), 320 * 5 * 2, "3 frames + 2 frames of silence");
    let fives = data.iter().filter(|&&b| b == 5).count();
    assert_eq!(fives, 3 * 320 * 2);
    assert!(!rig.bot.exists(), "no bot audio: no bot file");
    assert_eq!(rig.stats.audio_files(), 1);
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn bot_audio_is_laid_out_on_the_timeline_and_a_barge_in_cuts_what_was_unplayed() {
    let dir = dir("bot");
    let rig = start_audio(&dir, Duration::from_secs(60), 64);
    let rate = SampleRate::Hz8000;
    // One second of speech produced at t = 0.2 s, then a barge-in at t = 0.7 s.
    send(
        &rig.tx,
        AudioMsg::Bot {
            at: rig.start + Duration::from_millis(200),
            rate,
            pcm: pcm(8_000, 9),
        },
    )
    .await;
    send(
        &rig.tx,
        AudioMsg::BotInterrupt {
            at: rig.start + Duration::from_millis(700),
        },
    )
    .await;
    drop(rig.tx);
    finished(rig.task).await;

    let data = wav_data(&rig.bot, 8_000);
    // 0.2 s of silence, then the 0.5 s that was heard.
    assert_eq!(data.len(), (1_600 + 4_000) * 2);
    assert!(data[..3_200].iter().all(|&b| b == 0));
    assert!(data[3_200..].iter().all(|&b| b == 9));
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn the_stop_drains_the_queue_and_finishes_the_wav_header() {
    let dir = dir("stop");
    let rig = start_audio(&dir, Duration::from_secs(60), 64);
    for seq in 0..10 {
        send(
            &rig.tx,
            AudioMsg::Caller {
                rate: SampleRate::Hz8000,
                seq,
                pcm: pcm(160, 3),
            },
        )
        .await;
    }
    // The queue's sender is still alive (the pipeline has not dropped its taps yet).
    rig.stop.cancel();
    finished(rig.task).await;
    assert_eq!(wav_data(&rig.caller, 8_000).len(), 10 * 160 * 2);
    drop(rig.tx);
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn audio_past_the_length_cap_is_not_written() {
    let dir = dir("cap");
    let rig = start_audio(&dir, Duration::from_secs(1), 256);
    for seq in 0..100 {
        send(
            &rig.tx,
            AudioMsg::Caller {
                rate: SampleRate::Hz8000,
                seq,
                pcm: pcm(160, 3),
            },
        )
        .await;
    }
    drop(rig.tx);
    finished(rig.task).await;
    assert_eq!(wav_data(&rig.caller, 8_000).len(), 8_000 * 2, "one second");
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_file_that_cannot_be_created_disables_that_track_only() {
    let dir = dir("blocked");
    let rig = start_audio(&dir, Duration::from_secs(60), 64);
    fs::write(&rig.caller, "in the way").unwrap();
    for seq in 0..3 {
        send(
            &rig.tx,
            AudioMsg::Caller {
                rate: SampleRate::Hz8000,
                seq,
                pcm: pcm(160, 3),
            },
        )
        .await;
    }
    send(
        &rig.tx,
        AudioMsg::Bot {
            at: rig.start,
            rate: SampleRate::Hz8000,
            pcm: pcm(160, 4),
        },
    )
    .await;
    drop(rig.tx);
    finished(rig.task).await;
    assert_eq!(fs::read_to_string(&rig.caller).unwrap(), "in the way");
    assert_eq!(rig.stats.write_errors(), 1, "one failure, logged once");
    assert!(rig.bot.exists(), "the bot track still recorded");
    fs::remove_dir_all(&dir).unwrap();
}

// --- taps ----------------------------------------------------------------------------------

fn frame(seq: u64, epoch: TurnEpoch) -> AudioFrame {
    AudioFrame::new(
        pcm(160, 1),
        SampleRate::Hz8000,
        SystemClock::shared().now(),
        seq,
        epoch,
    )
    .unwrap()
}

#[test]
fn a_full_queue_drops_the_newest_frame_and_counts_it() {
    let stats = Arc::new(RecordingStats::default());
    let (tx, mut rx) = mpsc::channel(INTERRUPT_RESERVE + 2);
    let mut tap = CallerTap {
        tx,
        stats: Arc::clone(&stats),
    };
    for seq in 0..5 {
        tap.observe(&frame(seq, TurnEpoch::ZERO));
    }
    assert_eq!(stats.frames_dropped(), 3);
    // The OLDEST two are the ones kept.
    for expected in 0..2 {
        match rx.try_recv().unwrap() {
            AudioMsg::Caller { seq, .. } => assert_eq!(seq, expected),
            other => panic!("{other:?}"),
        }
    }
    assert!(rx.try_recv().is_err());
}

#[test]
fn a_barge_in_marker_gets_through_a_queue_that_refuses_audio() {
    let stats = Arc::new(RecordingStats::default());
    let (tx, mut rx) = mpsc::channel(INTERRUPT_RESERVE + 1);
    let mut tap = BotTap {
        tx,
        clock: SystemClock::shared(),
        epoch: TurnEpoch::ZERO,
        stats: Arc::clone(&stats),
    };
    // One audio frame fits; the rest of the queue is the markers' reserve.
    for seq in 0..4 {
        tap.observe(&Frame::Data(DataFrame::Audio(frame(seq, TurnEpoch::ZERO))));
    }
    assert_eq!(stats.frames_dropped(), 3);
    tap.observe(&Frame::System(SystemFrame::Interrupt {
        new_epoch: epoch(1),
    }));
    assert_eq!(stats.interrupts_lost(), 0);
    assert!(matches!(rx.try_recv().unwrap(), AudioMsg::Bot { .. }));
    assert!(matches!(
        rx.try_recv().unwrap(),
        AudioMsg::BotInterrupt { .. }
    ));
}

#[test]
fn the_bot_tap_drops_audio_of_epochs_a_barge_in_ended_and_reports_the_barge_in() {
    let stats = Arc::new(RecordingStats::default());
    let (tx, mut rx) = mpsc::channel(64);
    let mut tap = BotTap {
        tx,
        clock: SystemClock::shared(),
        epoch: TurnEpoch::ZERO,
        stats,
    };
    let one = epoch(1);
    let two = epoch(2);
    tap.observe(&Frame::Data(DataFrame::Audio(frame(0, one))));
    tap.observe(&Frame::System(SystemFrame::Interrupt { new_epoch: two }));
    // Audio of the ended epoch still on its way to TransportOut: not recorded.
    tap.observe(&Frame::Data(DataFrame::Audio(frame(1, one))));
    tap.observe(&Frame::Data(DataFrame::Audio(frame(2, two))));
    let kinds: Vec<&'static str> = std::iter::from_fn(|| rx.try_recv().ok())
        .map(|m| match m {
            AudioMsg::Bot { .. } => "bot",
            AudioMsg::BotInterrupt { .. } => "interrupt",
            AudioMsg::Caller { .. } => "caller",
        })
        .collect();
    assert_eq!(kinds, ["bot", "interrupt", "bot"]);
}

// --- lifetime ------------------------------------------------------------------------------

#[tokio::test]
async fn dropping_a_call_recording_finishes_its_files_without_finish() {
    let dir = dir("drop");
    let rig = start_audio(&dir, Duration::from_secs(60), 64);
    send(
        &rig.tx,
        AudioMsg::Caller {
            rate: SampleRate::Hz8000,
            seq: 0,
            pcm: pcm(160, 3),
        },
    )
    .await;
    let recording = CallRecording {
        stop: rig.stop.clone(),
        tasks: vec![rig.task],
    };
    drop(recording);
    // The task was told to stop though the queue's sender is alive.
    let mut waited = 0;
    while !rig.caller.exists() || fs::metadata(&rig.caller).map_or(true, |m| m.len() < 44 + 320) {
        tokio::time::sleep(Duration::from_millis(10)).await;
        waited += 1;
        assert!(waited < 500, "the file was never finished");
    }
    // The header was finalized by the stop.
    let mut tries = 0;
    loop {
        let bytes = fs::read(&rig.caller).unwrap();
        if u32::from_le_bytes(bytes[40..44].try_into().unwrap()) == 320 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        tries += 1;
        assert!(tries < 500, "the header was never finalized");
    }
    drop(rig.tx);
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn finish_waits_for_the_tasks_and_returns() {
    let dir = dir("finish");
    let rig = start_audio(&dir, Duration::from_secs(60), 64);
    send(
        &rig.tx,
        AudioMsg::Caller {
            rate: SampleRate::Hz8000,
            seq: 0,
            pcm: pcm(160, 3),
        },
    )
    .await;
    let recording = CallRecording {
        stop: rig.stop.clone(),
        tasks: vec![rig.task],
    };
    tokio::time::timeout(WAIT, recording.finish())
        .await
        .unwrap();
    assert_eq!(wav_data(&rig.caller, 8_000).len(), 320);
    assert_eq!(rig.stats.audio_files(), 1);
    drop(rig.tx);
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn prepare_creates_the_directory_and_refuses_a_file() {
    let base = dir("prepare");
    let nested = base.join("a").join("b");
    let recording = Recording::new(RecordingSettings::new(&nested), SystemClock::shared()).unwrap();
    recording.prepare().await.unwrap();
    assert!(nested.is_dir());

    let file = base.join("plain");
    fs::write(&file, "x").unwrap();
    let recording = Recording::new(RecordingSettings::new(&file), SystemClock::shared()).unwrap();
    assert!(recording.prepare().await.is_err());
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn inconsistent_settings_are_refused() {
    let mut settings = RecordingSettings::new("rec");
    settings.transcripts = false;
    assert!(Recording::new(settings, SystemClock::shared()).is_err());
}

#[tokio::test]
async fn the_sweeper_handle_is_kept_and_joins_after_cancellation() {
    let dir = dir("sweeperjoin");
    let clock = SystemClock::shared();
    let recording = Recording::new(RecordingSettings::new(&dir), clock.clone()).unwrap();
    let cancel = CancellationToken::new();
    let sweeper = recording.spawn_sweeper(&cancel).unwrap();
    let stats = sweeper.stats();
    // The first sweep happens at once.
    let mut waited = 0;
    while stats.sweeps() < 1 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        waited += 1;
        assert!(waited < 500, "no sweep");
    }
    cancel.cancel();
    tokio::time::timeout(WAIT, sweeper.join(&*clock, Duration::from_secs(5)))
        .await
        .unwrap();
    assert_eq!(stats.panics(), 0);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn no_retention_means_no_sweeper() {
    let mut settings = RecordingSettings::new("rec");
    settings.retention = None;
    let recording = Recording::new(settings, SystemClock::shared()).unwrap();
    // No runtime needed: nothing is spawned.
    assert!(recording.spawn_sweeper(&CancellationToken::new()).is_none());
}

// --- encryption at rest ----------------------------------------------------------

#[cfg(feature = "recording-encryption")]
mod encrypted {
    use std::io::Read as _;

    use super::*;

    fn keys() -> (age::x25519::Identity, AgeRecipients) {
        let id = age::x25519::Identity::generate();
        let recipients = AgeRecipients::parse(&id.to_public().to_string()).unwrap();
        (id, recipients)
    }

    fn decrypt(path: &Path, id: &age::x25519::Identity) -> Vec<u8> {
        let bytes = fs::read(path).unwrap();
        let decryptor = age::Decryptor::new(&bytes[..]).unwrap();
        let mut reader = decryptor
            .decrypt(std::iter::once(id as &dyn age::Identity))
            .unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        out
    }

    #[tokio::test]
    async fn an_encrypted_transcript_decrypts_to_the_same_lines_and_leaves_no_plaintext() {
        let dir = dir("age-transcript");
        let (id, recipients) = keys();
        let t = start_transcript_with(&dir, false, 64, Some(recipients));
        user(&t.ring, "hello there");
        bot(&t.ring, 1, "Hi, how can I help?");
        t.ring
            .publish(EngineEvent::BotStoppedSpeaking { epoch: epoch(1) });
        drop(t.ring);
        finished(t.task).await;

        assert!(t.path.to_string_lossy().ends_with(".transcript.jsonl.age"));
        let raw = fs::read(&t.path).unwrap();
        assert!(!raw.windows(11).any(|w| w == b"hello there"));
        let plain = String::from_utf8(decrypt(&t.path, &id)).unwrap();
        let l: Vec<Value> = plain
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let kinds: Vec<&str> = l.iter().map(|v| v["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["call", "user", "bot", "end"]);
        assert_eq!(l[1]["text"], "hello there");
        assert_eq!(l[2]["text"], "Hi, how can I help?");
        assert_eq!(t.stats.transcript_lines(), 4);
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_encrypted_wav_keeps_the_streaming_header_and_all_its_samples() {
        let dir = dir("age-wav");
        let (id, recipients) = keys();
        let rig = start_audio_with(&dir, Duration::from_secs(60), 512, Some(recipients));
        let rate = SampleRate::Hz16000;
        // 300 frames of 20 ms: ~192 KB, more than two age chunks and three WAV batches.
        for seq in 0..300 {
            send(
                &rig.tx,
                AudioMsg::Caller {
                    rate,
                    seq,
                    pcm: pcm(320, (seq % 200) as u8 + 1),
                },
            )
            .await;
        }
        drop(rig.tx);
        finished(rig.task).await;

        assert!(rig.caller.to_string_lossy().ends_with(".caller.wav.age"));
        let wav = decrypt(&rig.caller, &id);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(
            u32::from_le_bytes(wav[40..44].try_into().unwrap()),
            u32::MAX,
            "no seek-back: the streaming length stays"
        );
        assert_eq!(wav.len(), 44 + 300 * 640);
        for seq in 0..300usize {
            let at = 44 + seq * 640;
            assert!(
                wav[at..at + 640]
                    .iter()
                    .all(|&b| usize::from(b) == seq % 200 + 1)
            );
        }
        assert_eq!(rig.stats.audio_files(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
