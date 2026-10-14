//! The task behind one open session: a queue of text pieces, at most one HTTP request in flight,
//! its audio handed to the consumer as the body arrives. See the [module docs](super).

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::Error as CoreError;
use brisvox_core::clock::deadline_after;
use brisvox_core::frame::SampleRate;
use bytes::Bytes;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::Inner;
use super::config::{AudioFormat, PROVIDER};
use super::wav::{Fed, WavFormat, WavReader};
use crate::error::Error;
use crate::http_llm;
use crate::timer::Timer;
use crate::tts::{ContextId, TtsEvent, TtsInput};

/// Text pieces and flushes waiting for the request in flight. When the queue is full the task
/// stops reading `input` (the sender waits: backpressure, nothing is dropped).
const MAX_QUEUE: usize = 128;

/// Events waiting for room in the events channel. At this many the response body is not read
/// (TCP backpressure to the server), so a slow consumer cannot make the task buffer a whole
/// utterance.
const OUT_HIGH_WATER: usize = 8;

/// Cancelled contexts remembered, so that input that was already in flight for one is ignored.
const REMEMBERED_CANCELS: usize = 64;

/// Segments in a row the server may refuse (HTTP 400 or 422) before the session fails. One
/// refusal is normally about that text (Kokoro-FastAPI answers 400 to a segment without
/// speakable characters); two in a row look like a wrong model or voice.
const MAX_CONSECUTIVE_REFUSALS: u32 = 2;

/// How long the final error (or the audio before it) may wait for room in the events channel.
const TERMINAL_SEND_GRACE: Duration = Duration::from_secs(5);

/// What the session needs to know about its `connect`.
#[derive(Debug, Clone)]
pub(crate) struct Params {
    pub(crate) voice: Arc<str>,
    pub(crate) rate: SampleRate,
}

enum Job {
    Speak { context: ContextId, text: String },
    Flush { context: ContextId },
}

type Sending = Pin<Box<dyn Future<Output = Result<reqwest::Response, reqwest::Error>> + Send>>;

/// The request in flight.
enum Stage {
    Idle,
    /// The request is being sent; no response headers yet.
    Sending {
        context: ContextId,
        fut: Sending,
    },
    /// Headers are in; the body is being read.
    Streaming {
        context: ContextId,
        response: reqwest::Response,
        /// The first byte of a sample that a chunk boundary cut.
        carry: Option<u8>,
        /// The header reader of a `wav` response, until its header is done.
        wav: Option<WavReader>,
        got_audio: bool,
    },
}

impl Stage {
    fn context(&self) -> Option<ContextId> {
        match self {
            Self::Idle => None,
            Self::Sending { context, .. } | Self::Streaming { context, .. } => Some(*context),
        }
    }
}

/// One thing the stage produced.
enum Step {
    Headers(Result<reqwest::Response, reqwest::Error>),
    Chunk(Result<Option<Bytes>, reqwest::Error>),
}

/// Waits for the stage's next result; never completes while the stage is idle.
async fn step(stage: &mut Stage) -> Step {
    match stage {
        Stage::Idle => std::future::pending().await,
        Stage::Sending { fut, .. } => Step::Headers(fut.as_mut().await),
        Stage::Streaming { response, .. } => Step::Chunk(response.chunk().await),
    }
}

struct State {
    inner: Arc<Inner>,
    params: Params,
    queue: VecDeque<Job>,
    /// Events not yet handed to the consumer, in order.
    out: VecDeque<TtsEvent>,
    stage: Stage,
    /// Deadline of the next sign of progress (`None` while the body is not being read).
    deadline: Option<Instant>,
    /// Deadline of the request as a whole.
    hard_deadline: Instant,
    cancelled: VecDeque<ContextId>,
    refusals: u32,
    fatal: Option<Error>,
}

/// Runs the session until its input and work are done, the consumer goes away, the provider is
/// shut down, or it fails.
pub(crate) async fn run(
    inner: Arc<Inner>,
    params: Params,
    mut input: mpsc::Receiver<TtsInput>,
    mut cancel: mpsc::Receiver<ContextId>,
    events: mpsc::Sender<TtsEvent>,
) {
    let clock = Arc::clone(&inner.config.clock);
    let shutdown = inner.config.shutdown.clone();
    let now = clock.now();
    let mut state = State {
        inner,
        params,
        queue: VecDeque::new(),
        out: VecDeque::new(),
        stage: Stage::Idle,
        deadline: None,
        hard_deadline: now,
        cancelled: VecDeque::new(),
        refusals: 0,
        fatal: None,
    };
    let mut input_open = true;
    let mut cancel_open = true;
    // One pinned timer for the request deadline, reset only when the deadline moves (it used
    // to be rebuilt on every pass, i.e. for every chunk of audio).
    let mut due_timer = Timer::new(&*clock);
    loop {
        state.advance();
        if state.fatal.is_some() {
            break;
        }
        if !input_open
            && state.queue.is_empty()
            && state.out.is_empty()
            && matches!(state.stage, Stage::Idle)
        {
            return;
        }
        let gated = state.out.len() >= OUT_HIGH_WATER;
        state.arm(gated);
        let due = state
            .deadline
            .map_or(state.hard_deadline, |d| d.min(state.hard_deadline));
        let busy = !matches!(state.stage, Stage::Idle);
        due_timer.set(busy.then_some(due));
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            () = events.closed() => return,
            context = cancel.recv(), if cancel_open => match context {
                Some(context) => state.cancel(context),
                None => cancel_open = false,
            },
            permit = events.reserve(), if !state.out.is_empty() => match permit {
                Ok(permit) => {
                    if let Some(event) = state.out.pop_front() {
                        permit.send(event);
                    }
                }
                Err(_) => return,
            },
            result = step(&mut state.stage), if !gated => state.on_step(result),
            () = due_timer.wait(&*clock), if busy => state.on_timeout(),
            message = input.recv(), if input_open && state.queue.len() < MAX_QUEUE => match message {
                Some(message) => state.on_input(message),
                None => input_open = false,
            },
        }
        if state.fatal.is_some() {
            break;
        }
    }
    // A failed session: the audio produced before the failure first, then the error, then the
    // end of the stream (dropping `events`).
    if let Some(error) = state.fatal.take() {
        state.out.push_back(TtsEvent::Error(error));
        let until = deadline_after(clock.now(), TERMINAL_SEND_GRACE);
        for event in state.out.drain(..) {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                sent = events.send(event) => if sent.is_err() { return },
                () = clock.sleep_until(until) => return,
            }
        }
    }
}

impl State {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    /// Starts the next piece of work while no request is in flight: flushes are answered,
    /// pending text of one context is joined, and the request for it is started.
    fn advance(&mut self) {
        while matches!(self.stage, Stage::Idle) && self.fatal.is_none() {
            match self.queue.pop_front() {
                None => return,
                Some(Job::Flush { context }) => self.out.push_back(TtsEvent::Flushed { context }),
                Some(Job::Speak { context, mut text }) => {
                    let limit = self.inner.config.coalesce_chars;
                    while let Some(Job::Speak {
                        context: next_context,
                        text: next,
                    }) = self.queue.front()
                    {
                        if *next_context != context || text.len().saturating_add(next.len()) > limit
                        {
                            break;
                        }
                        text.push_str(next);
                        self.queue.pop_front();
                    }
                    if text.trim().is_empty() {
                        continue;
                    }
                    self.start(context, &text);
                }
            }
        }
    }

    /// Sends the request for `text`. The future is polled by the session loop, so a cancel or a
    /// shutdown drops it (and with it the connection attempt).
    fn start(&mut self, context: ContextId, text: &str) {
        let inner = &self.inner;
        let body = match inner.request_body(&self.params, text) {
            Ok(body) => body,
            Err(error) => {
                self.fatal = Some(error);
                return;
            }
        };
        let mut headers = inner.headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let request = inner
            .client
            .post(&inner.url)
            .headers(headers)
            .body(body)
            .send();
        let now = inner.config.clock.now();
        self.deadline = Some(deadline_after(now, inner.config.first_audio_timeout));
        self.hard_deadline = deadline_after(now, inner.config.max_request_duration);
        self.stage = Stage::Sending {
            context,
            fut: Box::pin(request),
        };
    }

    /// Re-arms the progress deadline after the body was not being read (`gated`) or after a
    /// request started: time the consumer spent not reading is not charged to the server.
    fn arm(&mut self, gated: bool) {
        if gated || matches!(self.stage, Stage::Idle) {
            self.deadline = None;
        } else if self.deadline.is_none() {
            let config = &self.inner.config;
            let got_audio = matches!(
                self.stage,
                Stage::Streaming {
                    got_audio: true,
                    ..
                }
            );
            let after = if got_audio {
                config.idle_timeout
            } else {
                config.first_audio_timeout
            };
            self.deadline = Some(deadline_after(config.clock.now(), after));
        }
    }

    fn on_input(&mut self, message: TtsInput) {
        let context = match &message {
            TtsInput::Text { context, .. } | TtsInput::Flush { context } => *context,
        };
        if self.cancelled.contains(&context) {
            return;
        }
        match message {
            TtsInput::Text { text, .. } => {
                if !text.is_empty() {
                    self.queue.push_back(Job::Speak {
                        context,
                        text: text.to_string(),
                    });
                }
            }
            TtsInput::Flush { .. } => self.queue.push_back(Job::Flush { context }),
        }
    }

    /// Barge-in: forgets everything of `context`, aborts its request (dropping the response
    /// resets the HTTP/2 stream or closes the HTTP/1.1 connection, so the server stops
    /// generating) and acknowledges at once.
    fn cancel(&mut self, context: ContextId) {
        if self.cancelled.len() >= REMEMBERED_CANCELS {
            self.cancelled.pop_front();
        }
        self.cancelled.push_back(context);
        self.queue.retain(|job| match job {
            Job::Speak { context: c, .. } | Job::Flush { context: c } => *c != context,
        });
        self.out.retain(|event| match event {
            TtsEvent::Audio { context: c, .. }
            | TtsEvent::Word { context: c, .. }
            | TtsEvent::Flushed { context: c }
            | TtsEvent::Cancelled { context: c } => *c != context,
            TtsEvent::Error(_) => true,
        });
        if self.stage.context() == Some(context) {
            self.stage = Stage::Idle;
            self.deadline = None;
        }
        self.out.push_back(TtsEvent::Cancelled { context });
    }

    fn on_timeout(&mut self) {
        let config = &self.inner.config;
        let now = config.clock.now();
        let (what, after) = if now >= self.hard_deadline {
            ("tts request", config.max_request_duration)
        } else if matches!(
            self.stage,
            Stage::Streaming {
                got_audio: true,
                ..
            }
        ) {
            ("tts idle", config.idle_timeout)
        } else {
            ("tts first audio", config.first_audio_timeout)
        };
        self.fatal = Some(Error::Core(CoreError::Timeout { what, after }));
    }

    fn on_step(&mut self, step: Step) {
        match step {
            Step::Headers(result) => self.on_headers(result),
            Step::Chunk(result) => self.on_chunk(result),
        }
    }

    fn transport(&self, error: &reqwest::Error, limit: Duration, what: &'static str) -> Error {
        // Never `%error`: reqwest's Display appends the request URL, which may carry
        // credentials. Only the class of the failure is logged.
        tracing::debug!(
            provider = self.name(),
            what,
            timeout = error.is_timeout(),
            connect = error.is_connect(),
            body = error.is_body(),
            decode = error.is_decode(),
            "speech transport error"
        );
        http_llm::transport_error(self.name(), error, limit, what)
    }

    fn on_headers(&mut self, result: Result<reqwest::Response, reqwest::Error>) {
        let Stage::Sending { context, .. } = self.stage else {
            return;
        };
        let config = &self.inner.config;
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                self.stage = Stage::Idle;
                self.fatal = Some(self.transport(&error, config.connect_timeout, "tts connect"));
                return;
            }
        };
        let status = response.status();
        if !status.is_success() {
            // The body is not read: it can echo fragments of the request or the key, and a
            // refused request must free the connection at once.
            let code = status.as_u16();
            tracing::warn!(
                provider = self.name(),
                status = code,
                retry_after_secs = http_llm::retry_after_secs(&response),
                "the speech server refused a request"
            );
            self.stage = Stage::Idle;
            if matches!(code, 400 | 422) {
                self.refusals += 1;
                if self.refusals < MAX_CONSECUTIVE_REFUSALS {
                    return;
                }
            }
            self.fatal = Some(Error::Rejected {
                provider: self.name(),
                status: code,
            });
            return;
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if content_type.starts_with("application/json")
            || content_type.starts_with("text/")
            || content_type.contains("xml")
        {
            // A 200 that carries a document is an error page or a wrong route, never audio.
            self.stage = Stage::Idle;
            self.fatal = Some(http_llm::protocol(
                self.name(),
                "the speech endpoint answered 200 with a non-audio content type",
            ));
            return;
        }
        let wav = (config.format == AudioFormat::Wav).then(WavReader::new);
        self.stage = Stage::Streaming {
            context,
            response,
            carry: None,
            wav,
            got_audio: false,
        };
        // Headers are not audio: the first-audio deadline keeps running.
    }

    fn on_chunk(&mut self, result: Result<Option<Bytes>, reqwest::Error>) {
        let idle = self.inner.config.idle_timeout;
        let Stage::Streaming {
            context,
            carry,
            wav,
            got_audio,
            ..
        } = &mut self.stage
        else {
            return;
        };
        let context = *context;
        match result {
            Ok(Some(chunk)) => {
                let audio = match feed(wav, carry, chunk, self.params.rate) {
                    Ok(audio) => audio,
                    Err(detail) => {
                        self.stage = Stage::Idle;
                        self.fatal = Some(http_llm::protocol(PROVIDER, detail));
                        return;
                    }
                };
                // Any body progress counts, audio or not (a WAV header is progress too).
                self.deadline = Some(deadline_after(self.inner.config.clock.now(), idle));
                if let Some(audio) = audio {
                    *got_audio = true;
                    self.refusals = 0;
                    self.out.push_back(TtsEvent::Audio { context, audio });
                }
            }
            Ok(None) => {
                if wav.is_some() {
                    self.stage = Stage::Idle;
                    self.fatal = Some(http_llm::protocol(
                        PROVIDER,
                        "the response ended inside the WAV header",
                    ));
                    return;
                }
                if !*got_audio {
                    tracing::warn!(
                        provider = PROVIDER,
                        "the speech server answered with no audio for a text piece"
                    );
                }
                self.stage = Stage::Idle;
            }
            Err(error) => {
                self.stage = Stage::Idle;
                self.fatal = Some(self.transport(&error, idle, "tts body"));
            }
        }
    }
}

/// Turns one body chunk into whole PCM16 samples: the WAV header is consumed, an odd trailing
/// byte waits for the next chunk. `Ok(None)`: nothing to hand over yet.
fn feed(
    wav: &mut Option<WavReader>,
    carry: &mut Option<u8>,
    chunk: Bytes,
    rate: SampleRate,
) -> Result<Option<Bytes>, String> {
    let samples = match wav {
        None => chunk,
        Some(reader) => match reader.feed(chunk).map_err(|e| e.describe())? {
            Fed::NeedMore => return Ok(None),
            Fed::Samples(samples, format) => {
                if let Some(WavFormat { sample_rate, .. }) = format
                    && sample_rate != rate.hz()
                {
                    return Err(format!(
                        "the server delivers {sample_rate} Hz audio but the session is \
                         configured for {} Hz (set tts.sample_rate to the server's rate)",
                        rate.hz()
                    ));
                }
                *wav = None;
                samples
            }
        },
    };
    Ok(whole_samples(carry, samples))
}

/// `chunk` (preceded by the byte left over from the previous one) cut to a whole number of
/// 16-bit samples. Zero-copy unless a sample straddles two chunks.
fn whole_samples(carry: &mut Option<u8>, chunk: Bytes) -> Option<Bytes> {
    let joined = match carry.take() {
        None => chunk,
        Some(first) => {
            let mut joined = Vec::with_capacity(chunk.len().saturating_add(1));
            joined.push(first);
            joined.extend_from_slice(&chunk);
            Bytes::from(joined)
        }
    };
    let whole = joined.len() & !1;
    if whole < joined.len() {
        *carry = joined.get(whole).copied();
    }
    if whole == 0 {
        None
    } else {
        Some(joined.slice(..whole))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn samples_are_never_split() {
        let mut carry = None;
        assert_eq!(
            whole_samples(&mut carry, Bytes::from_static(&[1, 2, 3])).unwrap(),
            Bytes::from_static(&[1, 2])
        );
        assert_eq!(carry, Some(3));
        assert_eq!(
            whole_samples(&mut carry, Bytes::from_static(&[4, 5, 6, 7])).unwrap(),
            Bytes::from_static(&[3, 4, 5, 6])
        );
        assert_eq!(carry, Some(7));
        assert!(whole_samples(&mut carry, Bytes::new()).is_none());
        assert_eq!(carry, Some(7));
        assert_eq!(
            whole_samples(&mut carry, Bytes::from_static(&[8])).unwrap(),
            Bytes::from_static(&[7, 8])
        );
        assert_eq!(carry, None);
    }
}
