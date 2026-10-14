//! Model tests of the client's bookkeeping (what the server's messages mean for the call).
#![allow(clippy::disallowed_methods)] // a model on a real instant, advanced by hand

use super::super::measure::Outcome;
use super::*;

const MS: Duration = Duration::from_millis(1);

fn log(begin: Instant) -> Log {
    Log {
        begin,
        rate: 16_000,
        silence_peak: 200,
        last_eos: Duration::from_millis(1_000),
        tracker: Tracker::new(
            vec![Turn {
                index: 0,
                lang: "en".to_owned(),
                speech_start: Duration::ZERO,
                eos: Duration::from_millis(1_000),
            }],
            MeasureConfig::default(),
        ),
        audio_frames: 0,
        audible_frames: 0,
        clears: 0,
        errors: Vec::new(),
        errors_dropped: 0,
        answered_last: false,
        playout_end: begin,
        pending_marks: VecDeque::new(),
        stop_reason: None,
        fatal_error: None,
        ended: None,
    }
}

/// `frames` 20 ms frames of PCM16 at 16 kHz with every sample `value`.
fn pcm(frames: usize, value: i16) -> Bytes {
    Bytes::from(
        std::iter::repeat_n(value.to_le_bytes(), 320 * frames)
            .flatten()
            .collect::<Vec<u8>>(),
    )
}

#[test]
fn only_audible_frames_are_arrivals_and_the_one_after_the_last_eos_ends_the_wait() {
    let begin = Instant::now();
    let mut log = log(begin);
    log.apply(Inbound::Audio {
        at: begin + 300 * MS,
        data: pcm(1, 0),
    });
    log.apply(Inbound::Audio {
        at: begin + 320 * MS,
        data: pcm(1, 5_000),
    });
    assert_eq!((log.audio_frames, log.audible_frames), (2, 1));
    assert_eq!(
        log.tracker.results()[0].outcome,
        Outcome::NoResponse,
        "audio long before EOS is no reply"
    );
    assert!(!log.answered_last, "before the last end of speech");
    log.apply(Inbound::Audio {
        at: begin + 1_400 * MS,
        data: pcm(1, -5_000),
    });
    assert!(log.answered_last);
    let result = &log.tracker.results()[0];
    assert_eq!(
        (result.outcome, result.v2v),
        (Outcome::Answered, Some(400 * MS))
    );
}

#[test]
fn a_mark_is_due_when_the_audio_before_it_has_played() {
    let begin = Instant::now();
    let mut log = log(begin);
    // 3 frames (60 ms) arrive at once at t=100 ms, then the mark: due at 160 ms.
    log.apply(Inbound::Audio {
        at: begin + 100 * MS,
        data: pcm(3, 5_000),
    });
    log.apply(Inbound::Mark {
        at: begin + 100 * MS,
        id: "a".to_owned(),
    });
    assert_eq!(log.pending_marks.front().unwrap().0, begin + 160 * MS);
    // More audio queues behind it; a mark with nothing playing is due at once.
    log.apply(Inbound::Audio {
        at: begin + 120 * MS,
        data: pcm(1, 5_000),
    });
    log.apply(Inbound::Mark {
        at: begin + 120 * MS,
        id: "b".to_owned(),
    });
    assert_eq!(log.pending_marks.back().unwrap().0, begin + 180 * MS);
    log.apply(Inbound::Mark {
        at: begin + 900 * MS,
        id: "c".to_owned(),
    });
    assert_eq!(log.pending_marks.back().unwrap().0, begin + 900 * MS);
    // A clear forgets the unplayed audio and every pending mark.
    log.apply(Inbound::Clear {
        at: begin + 950 * MS,
    });
    assert!(log.pending_marks.is_empty());
    assert_eq!((log.clears, log.playout_end), (1, begin + 950 * MS));
}

#[test]
fn the_mark_list_is_bounded() {
    let begin = Instant::now();
    let mut log = log(begin);
    for i in 0..MAX_PENDING_MARKS + 5 {
        log.apply(Inbound::Mark {
            at: begin,
            id: i.to_string(),
        });
    }
    assert_eq!(log.pending_marks.len(), MAX_PENDING_MARKS);
    assert_eq!(
        log.pending_marks.front().unwrap().1,
        "5",
        "the oldest were given up"
    );
}

#[test]
fn stop_fatal_errors_and_the_end_of_the_connection_end_the_call() {
    let begin = Instant::now();
    let mut log = log(begin);
    let warning = Inbound::Error {
        code: "output_rate_mismatch".to_owned(),
        message: "dropped".to_owned(),
        fatal: false,
    };
    assert!(!log.apply(warning));
    assert!(log.fatal_error.is_none());
    let fatal = Inbound::Error {
        code: "overloaded".to_owned(),
        message: "slow".to_owned(),
        fatal: true,
    };
    assert!(log.apply(fatal));
    assert_eq!(log.errors.len(), 2);
    assert_eq!(log.fatal_error.as_deref(), Some("overloaded: slow"));
    assert!(log.apply(Inbound::Stop {
        reason: "ended".to_owned()
    }));
    assert!(log.apply(Inbound::Ended {
        clean: false,
        detail: "reset".to_owned()
    }));
    assert_eq!(log.ended, Some((false, "reset".to_owned())));
}

#[test]
fn urls_are_scrubbed_from_error_text() {
    let config = ClientConfig::new("ws://h:1/call?token=SECRETTOKEN");
    let text = scrub(
        &config,
        "failed to connect to ws://h:1/call?token=SECRETTOKEN: refused",
    );
    assert_eq!(text, "failed to connect to <url>: refused");
    assert!(format!("{config:?}").contains("[REDACTED]"));
    assert!(!format!("{config:?}").contains("SECRETTOKEN"));
}

#[test]
fn server_errors_are_kept_up_to_the_cap_and_the_rest_counted() {
    let begin = Instant::now();
    let mut log = log(begin);
    for n in 0..MAX_SERVER_ERRORS + 10 {
        let over = log.apply(Inbound::Error {
            code: "e".to_owned(),
            message: n.to_string(),
            fatal: false,
        });
        assert!(!over, "a non-fatal error does not end the call");
    }
    assert_eq!(log.errors.len(), MAX_SERVER_ERRORS);
    assert_eq!(log.errors_dropped, 10);
    assert_eq!(log.errors.first().unwrap(), "e: 0");
    assert!(log.fatal_error.is_none());
}
