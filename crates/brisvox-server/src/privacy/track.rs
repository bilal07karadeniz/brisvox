//! The two audio tracks of a recorded call, as pure state machines.
//!
//! A track turns the audio it is shown into the PCM16 chunks to append to its WAV file. No IO,
//! no clock reads: every time comes in as an argument, so the tracks are tested exhaustively
//! with plain numbers.
//!
//! - [`CallerTrack`]: the caller's frames arrive in real time, one after the other. A hole in
//!   the transport's sequence numbers (a frame the first-stage inbox dropped, or the recorder's
//!   own queue) is filled with silence of the missing length, so the file keeps the call's
//!   timing.
//! - [`BotTrack`]: the bot's audio is produced faster than it is played. The track keeps a
//!   virtual playout: audio waits in `pending` and is committed to the file only up to the
//!   moment it was due to be heard; a barge-in drops what was still waiting, so the file holds
//!   what the caller could hear (to the transport's pacing accuracy), with silence in between.
//!
//! Both are bounded: a track stops accepting audio past its length cap, and the bot's pending
//! audio past [`BOT_PENDING_CAP`], counting what it refused. Only whole PCM16 samples are ever
//! appended (a frame with an odd byte length would shift every later sample by a byte and turn
//! the rest of the file into noise): the stray byte is dropped and counted.

use std::collections::VecDeque;
use std::time::Duration;

use brisvox_core::frame::SampleRate;
use bytes::Bytes;
use tokio::time::Instant;

/// Most audio the bot track holds ahead of the playout position. Overflow policy: a frame that
/// would pass it is refused (the NEWEST audio is dropped and counted in
/// [`TrackCounters::refused`]), so a response synthesized much faster than real time loses its
/// tail in the file rather than growing the call's memory; 30 s is far above the lead any real
/// transport sink allows (`docs/PRIVACY.md`).
pub const BOT_PENDING_CAP: Duration = Duration::from_secs(30);

/// Longest silence a sequence hole is filled with; a larger jump is a reset of the stream, not
/// a stretch of lost audio.
pub const MAX_GAP_FILL: Duration = Duration::from_secs(10);

/// Bytes per PCM16 sample.
const BYTES_PER_SAMPLE: u64 = 2;

/// What a track did with a piece of audio, for counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackCounters {
    /// Samples written to the file (audio and silence).
    pub samples: u64,
    /// Samples of silence inserted for a hole or a pause.
    pub silence: u64,
    /// Frames refused: another sample rate than the track's, or past a length cap.
    pub refused: u64,
    /// Samples of bot audio discarded by a barge-in or the end of the call before they were due.
    pub discarded: u64,
    /// Bytes dropped because a frame did not hold a whole number of samples.
    pub odd_bytes: u64,
}

/// Zero bytes the silence chunks borrow from, so a long pause is a run of shared slices of
/// this one buffer and not one big allocation.
static ZEROS: [u8; 64 * 1024] = [0; 64 * 1024];

/// Appends `samples` samples of silence (PCM16) to `out` as chunks of at most 64 KiB.
fn push_silence(out: &mut Vec<Bytes>, samples: u64) {
    let mut left = usize::try_from(samples.saturating_mul(BYTES_PER_SAMPLE)).unwrap_or(0);
    while left > 0 {
        let n = left.min(ZEROS.len());
        if let Some(zeros) = ZEROS.get(..n) {
            out.push(Bytes::from_static(zeros));
        }
        left -= n;
    }
}

/// Samples of `pcm` (PCM16).
fn samples_of(pcm: &Bytes) -> u64 {
    u64::try_from(pcm.len()).unwrap_or(u64::MAX) / BYTES_PER_SAMPLE
}

/// `pcm` cut to whole samples, and the bytes that were cut off (0 or 1).
fn whole_samples(pcm: &Bytes) -> (Bytes, u64) {
    let odd = pcm.len() % 2;
    (pcm.slice(..pcm.len().saturating_sub(odd)), odd as u64)
}

/// The caller's track.
#[derive(Debug)]
pub struct CallerTrack {
    max_duration: Duration,
    /// Cap in samples, set with the rate by the first frame.
    max_samples: u64,
    rate: Option<SampleRate>,
    last_seq: Option<u64>,
    counters: TrackCounters,
}

impl CallerTrack {
    /// A track that holds at most `max_duration` of audio.
    pub const fn new(max_duration: Duration) -> Self {
        Self {
            max_duration,
            max_samples: 0,
            rate: None,
            last_seq: None,
            counters: TrackCounters {
                samples: 0,
                silence: 0,
                refused: 0,
                discarded: 0,
                odd_bytes: 0,
            },
        }
    }

    /// The track's sample rate, known after the first accepted frame.
    pub const fn rate(&self) -> Option<SampleRate> {
        self.rate
    }

    /// Counters so far.
    pub const fn counters(&self) -> TrackCounters {
        self.counters
    }

    /// Shows the track a frame: the chunks to append, in order (empty when refused).
    pub fn push(&mut self, rate: SampleRate, seq: u64, pcm: &Bytes, out: &mut Vec<Bytes>) {
        let (pcm, odd) = whole_samples(pcm);
        let pcm = &pcm;
        self.counters.odd_bytes += odd;
        let hz = u64::from(rate.hz());
        if self.rate.is_none() {
            self.rate = Some(rate);
            self.max_samples = hz.saturating_mul(self.max_duration.as_secs());
        }
        let track_rate = self.rate.unwrap_or(rate);
        let frame_samples = samples_of(pcm);
        if track_rate != rate || frame_samples == 0 {
            self.counters.refused += u64::from(track_rate != rate);
            return;
        }
        let mut fill = 0;
        if let Some(last) = self.last_seq
            && seq > last.wrapping_add(1)
        {
            let missing = seq.saturating_sub(last).saturating_sub(1);
            let cap = hz * MAX_GAP_FILL.as_secs();
            fill = missing.saturating_mul(frame_samples).min(cap);
        }
        self.last_seq = Some(seq);
        if self
            .counters
            .samples
            .saturating_add(fill)
            .saturating_add(frame_samples)
            > self.max_samples
        {
            self.counters.refused += 1;
            return;
        }
        if fill > 0 {
            push_silence(out, fill);
            self.counters.silence += fill;
            self.counters.samples += fill;
        }
        out.push(pcm.clone());
        self.counters.samples += frame_samples;
    }
}

/// The bot's track: a virtual playout over the call's timeline.
#[derive(Debug)]
pub struct BotTrack {
    start: Instant,
    max_samples: u64,
    rate: Option<SampleRate>,
    /// Samples committed to the file.
    written: u64,
    pending: VecDeque<Bytes>,
    pending_samples: u64,
    max_duration: Duration,
    counters: TrackCounters,
}

impl BotTrack {
    /// A track for a call that started at `start`, holding at most `max_duration` of timeline.
    pub fn new(start: Instant, max_duration: Duration) -> Self {
        Self {
            start,
            max_samples: 0,
            rate: None,
            written: 0,
            pending: VecDeque::new(),
            pending_samples: 0,
            max_duration,
            counters: TrackCounters::default(),
        }
    }

    /// The track's sample rate, known after the first accepted frame.
    pub const fn rate(&self) -> Option<SampleRate> {
        self.rate
    }

    /// Counters so far.
    pub const fn counters(&self) -> TrackCounters {
        self.counters
    }

    /// Samples of the timeline at `at`.
    fn position(&self, at: Instant) -> u64 {
        let hz = u128::from(self.rate.map_or(0, SampleRate::hz));
        let nanos = at.saturating_duration_since(self.start).as_nanos();
        u64::try_from(nanos.saturating_mul(hz) / 1_000_000_000).unwrap_or(u64::MAX)
    }

    /// Commits pending audio that is due by `at` to `out`.
    fn flush(&mut self, at: Instant, out: &mut Vec<Bytes>) {
        let due = self.position(at).min(self.written + self.pending_samples);
        let mut room = due.saturating_sub(self.written);
        while room > 0 {
            let Some(mut chunk) = self.pending.pop_front() else {
                break;
            };
            let samples = samples_of(&chunk);
            if samples <= room {
                room -= samples;
                self.written += samples;
                self.pending_samples -= samples;
                self.counters.samples += samples;
                out.push(chunk);
            } else {
                let take = usize::try_from(room.saturating_mul(BYTES_PER_SAMPLE)).unwrap_or(0);
                let head = chunk.split_to(take);
                self.written += room;
                self.pending_samples -= room;
                self.counters.samples += room;
                out.push(head);
                self.pending.push_front(chunk);
                room = 0;
            }
        }
    }

    /// Pads the file with silence up to the timeline position `at` when nothing is waiting.
    fn pad(&mut self, at: Instant, out: &mut Vec<Bytes>) {
        if self.pending_samples > 0 {
            return;
        }
        let target = self.position(at).min(self.max_samples);
        if target > self.written {
            let gap = target - self.written;
            push_silence(out, gap);
            self.written = target;
            self.counters.samples += gap;
            self.counters.silence += gap;
        }
    }

    /// Bot audio produced at `at`: it plays after whatever is already waiting.
    pub fn push(&mut self, at: Instant, rate: SampleRate, pcm: &Bytes, out: &mut Vec<Bytes>) {
        let (pcm, odd) = whole_samples(pcm);
        let pcm = &pcm;
        self.counters.odd_bytes += odd;
        if self.rate.is_none() {
            self.rate = Some(rate);
            let hz = u64::from(rate.hz());
            self.max_samples = hz.saturating_mul(self.max_duration.as_secs());
        }
        if self.rate != Some(rate) {
            self.counters.refused += 1;
            return;
        }
        self.flush(at, out);
        self.pad(at, out);
        let samples = samples_of(pcm);
        let hz = u64::from(rate.hz());
        let over_cap = self.written + self.pending_samples + samples > self.max_samples
            || self.pending_samples + samples > hz.saturating_mul(BOT_PENDING_CAP.as_secs());
        if over_cap || samples == 0 {
            self.counters.refused += u64::from(over_cap);
            return;
        }
        self.pending_samples += samples;
        self.pending.push_back(pcm.clone());
    }

    /// A barge-in at `at`: what was due is committed, what was still waiting is dropped.
    pub fn interrupt(&mut self, at: Instant, out: &mut Vec<Bytes>) {
        self.finish(at, out);
    }

    /// The end of the call at `at`: like an interruption.
    pub fn finish(&mut self, at: Instant, out: &mut Vec<Bytes>) {
        if self.rate.is_none() {
            return;
        }
        self.flush(at, out);
        self.counters.discarded += self.pending_samples;
        self.pending.clear();
        self.pending_samples = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HZ: SampleRate = SampleRate::Hz8000;

    fn pcm(samples: usize, fill: u8) -> Bytes {
        Bytes::from(vec![fill; samples * 2])
    }

    fn total(out: &[Bytes]) -> usize {
        out.iter().map(Bytes::len).sum()
    }

    /// Any instant: the tracks only use differences.
    #[allow(clippy::disallowed_methods)]
    fn now() -> Instant {
        Instant::now()
    }

    fn secs(origin: Instant, ms: u64) -> Instant {
        origin + Duration::from_millis(ms)
    }

    #[test]
    fn caller_frames_in_sequence_are_appended_as_they_are() {
        let mut track = CallerTrack::new(Duration::from_secs(60));
        let mut out = Vec::new();
        for seq in 0..3 {
            track.push(HZ, seq, &pcm(160, 7), &mut out);
        }
        assert_eq!(total(&out), 3 * 320);
        assert_eq!(track.counters().silence, 0);
        assert_eq!(track.rate(), Some(HZ));
    }

    #[test]
    fn a_sequence_hole_is_filled_with_silence_of_the_missing_length() {
        let mut track = CallerTrack::new(Duration::from_secs(60));
        let mut out = Vec::new();
        track.push(HZ, 0, &pcm(160, 1), &mut out);
        track.push(HZ, 4, &pcm(160, 1), &mut out); // frames 1, 2, 3 lost
        assert_eq!(track.counters().silence, 3 * 160);
        assert_eq!(total(&out), (160 + 3 * 160 + 160) * 2);
        // The silence sits between the two frames.
        let silent = out.get(1).unwrap();
        assert!(silent.iter().all(|&b| b == 0));
    }

    #[test]
    fn a_huge_sequence_jump_is_a_reset_not_ten_minutes_of_silence() {
        let mut track = CallerTrack::new(Duration::from_secs(3_600));
        let mut out = Vec::new();
        track.push(HZ, 0, &pcm(160, 1), &mut out);
        track.push(HZ, 1_000_000, &pcm(160, 1), &mut out);
        assert_eq!(
            track.counters().silence,
            8_000 * MAX_GAP_FILL.as_secs(),
            "capped"
        );
    }

    #[test]
    fn a_repeated_or_reordered_sequence_number_adds_no_silence() {
        let mut track = CallerTrack::new(Duration::from_secs(60));
        let mut out = Vec::new();
        track.push(HZ, 5, &pcm(160, 1), &mut out);
        track.push(HZ, 5, &pcm(160, 1), &mut out);
        track.push(HZ, 3, &pcm(160, 1), &mut out);
        assert_eq!(track.counters().silence, 0);
    }

    #[test]
    fn caller_audio_past_the_length_cap_is_refused_and_counted() {
        let mut track = CallerTrack::new(Duration::from_secs(1)); // 8000 samples at 8 kHz
        let mut out = Vec::new();
        for seq in 0..60 {
            track.push(HZ, seq, &pcm(160, 1), &mut out);
        }
        assert_eq!(track.counters().samples, 8_000);
        assert_eq!(track.counters().refused, 10);
        assert_eq!(total(&out), 16_000);
    }

    #[test]
    fn a_frame_at_another_rate_is_refused() {
        let mut track = CallerTrack::new(Duration::from_secs(60));
        let mut out = Vec::new();
        track.push(HZ, 0, &pcm(160, 1), &mut out);
        track.push(SampleRate::Hz16000, 1, &pcm(320, 1), &mut out);
        assert_eq!(total(&out), 320);
        assert_eq!(track.counters().refused, 1);
    }

    #[test]
    fn bot_audio_is_committed_only_as_it_falls_due() {
        let origin = now();
        let mut track = BotTrack::new(origin, Duration::from_secs(60));
        let mut out = Vec::new();
        // One second of speech (8000 samples) produced at once at t = 1 s.
        track.push(secs(origin, 1_000), HZ, &pcm(8_000, 9), &mut out);
        // Silence for the first second, then nothing is due yet.
        assert_eq!(total(&out), 8_000 * 2);
        assert!(out.iter().all(|c| c.iter().all(|&b| b == 0)));
        out.clear();
        // Half a second later, half of the speech has been heard.
        track.push(secs(origin, 1_500), HZ, &pcm(0, 0), &mut out);
        track.interrupt(secs(origin, 1_500), &mut out);
        assert_eq!(total(&out), 4_000 * 2, "the heard half");
        assert!(out.iter().all(|c| c.iter().all(|&b| b == 9)));
        assert_eq!(track.counters().discarded, 4_000, "the half never heard");
    }

    #[test]
    fn consecutive_chunks_play_back_to_back_and_the_end_drops_the_rest() {
        let origin = now();
        let mut track = BotTrack::new(origin, Duration::from_secs(60));
        let mut out = Vec::new();
        track.push(secs(origin, 0), HZ, &pcm(4_000, 1), &mut out); // 0.5 s
        track.push(secs(origin, 10), HZ, &pcm(4_000, 2), &mut out); // plays after the first
        track.finish(secs(origin, 750), &mut out);
        // 6000 samples are due by 0.75 s: all of chunk one and 2000 of chunk two.
        let bytes: Vec<u8> = out.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(bytes.iter().filter(|&&b| b == 1).count(), 8_000);
        assert_eq!(bytes.iter().filter(|&&b| b == 2).count(), 4_000);
        assert_eq!(track.counters().discarded, 2_000);
        assert_eq!(track.counters().silence, 0);
    }

    #[test]
    fn a_pause_between_responses_is_silence_on_the_timeline() {
        let origin = now();
        let mut track = BotTrack::new(origin, Duration::from_secs(60));
        let mut out = Vec::new();
        track.push(secs(origin, 0), HZ, &pcm(800, 1), &mut out); // 0.1 s, due by 0.1 s
        track.push(secs(origin, 2_000), HZ, &pcm(800, 2), &mut out);
        // The first response was committed, then 1.9 s of silence until the second starts.
        assert_eq!(track.counters().samples, 800 + 15_200);
        assert_eq!(track.counters().silence, 15_200);
    }

    #[test]
    fn a_barge_in_drops_what_was_waiting_and_the_next_response_starts_clean() {
        let origin = now();
        let mut track = BotTrack::new(origin, Duration::from_secs(60));
        let mut out = Vec::new();
        track.push(secs(origin, 0), HZ, &pcm(16_000, 1), &mut out); // 2 s of speech
        track.interrupt(secs(origin, 500), &mut out); // heard 0.5 s
        assert_eq!(track.counters().samples, 4_000);
        assert_eq!(track.counters().discarded, 12_000);
        out.clear();
        track.push(secs(origin, 1_000), HZ, &pcm(800, 2), &mut out);
        track.finish(secs(origin, 2_000), &mut out);
        // 0.5 s of silence (the user talking), then the new response.
        assert_eq!(track.counters().silence, 4_000);
        let twos: usize = out
            .iter()
            .flat_map(|c| c.iter())
            .filter(|&&b| b == 2)
            .count();
        assert_eq!(twos, 1_600);
    }

    #[test]
    fn bot_audio_past_the_length_cap_is_refused() {
        let origin = now();
        let mut track = BotTrack::new(origin, Duration::from_secs(1));
        let mut out = Vec::new();
        track.push(secs(origin, 0), HZ, &pcm(8_000, 1), &mut out);
        track.push(secs(origin, 0), HZ, &pcm(160, 1), &mut out);
        assert_eq!(track.counters().refused, 1);
    }

    #[test]
    fn bot_audio_at_another_rate_is_refused() {
        let origin = now();
        let mut track = BotTrack::new(origin, Duration::from_secs(60));
        let mut out = Vec::new();
        track.push(secs(origin, 0), HZ, &pcm(160, 1), &mut out);
        track.push(secs(origin, 0), SampleRate::Hz24000, &pcm(480, 1), &mut out);
        assert_eq!(track.counters().refused, 1);
        assert_eq!(track.rate(), Some(HZ));
    }

    #[test]
    fn a_long_pause_is_run_of_shared_slices_not_one_allocation() {
        let origin = now();
        let mut track = BotTrack::new(origin, Duration::from_secs(3_600));
        let mut out = Vec::new();
        // Ten minutes of nothing, then a word, at 48 kHz.
        track.push(
            secs(origin, 600_000),
            SampleRate::Hz48000,
            &pcm(480, 1),
            &mut out,
        );
        assert_eq!(total(&out), 600 * 48_000 * 2);
        assert!(out.iter().all(|c| c.len() <= 64 * 1024));
        assert!(out.iter().all(|c| c.iter().all(|&b| b == 0)));
    }
    #[test]
    fn a_frame_with_an_odd_byte_length_is_cut_to_whole_samples_and_counted() {
        let mut track = CallerTrack::new(Duration::from_secs(60));
        let mut out = Vec::new();
        let odd = Bytes::from(vec![1u8; 321]);
        track.push(HZ, 0, &odd, &mut out);
        track.push(HZ, 1, &pcm(160, 2), &mut out);
        assert!(out.iter().all(|c| c.len() % 2 == 0), "only whole samples");
        assert_eq!(total(&out), 320 + 320);
        assert_eq!(track.counters().odd_bytes, 1);
        assert_eq!(track.counters().samples, 320);

        let origin = now();
        let mut bot = BotTrack::new(origin, Duration::from_secs(60));
        let mut out = Vec::new();
        bot.push(secs(origin, 0), HZ, &odd, &mut out);
        bot.finish(secs(origin, 1_000), &mut out);
        assert!(out.iter().all(|c| c.len() % 2 == 0));
        assert_eq!(total(&out), 320);
        assert_eq!(bot.counters().odd_bytes, 1);
    }

    #[test]
    fn pending_bot_audio_is_capped_at_thirty_seconds_and_the_newest_is_dropped() {
        let origin = now();
        let mut track = BotTrack::new(origin, Duration::from_secs(3_600));
        let mut out = Vec::new();
        // 40 one-second chunks all produced at once: only the first 30 s fit.
        for _ in 0..40 {
            track.push(secs(origin, 0), HZ, &pcm(8_000, 1), &mut out);
        }
        assert_eq!(track.counters().refused, 10);
        track.finish(secs(origin, 3_600_000), &mut out);
        assert_eq!(total(&out), 30 * 8_000 * 2);
    }
}
