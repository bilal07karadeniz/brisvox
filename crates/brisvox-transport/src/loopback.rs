//! In-memory loopback transport: the test and simulator side of the transport layer.
//!
//! [`LoopbackTransport`] accepts one call. Its adapter task models a client with a playout buffer:
//! every frame and mark the core sends reaches the [`LoopbackClient`] `latency` after it arrives,
//! in order. A clear drops everything still in that buffer at once, as a real adapter must
//! (the telephony adapters do the same). The test drives the caller side through [`LoopbackClient`]:
//! it sends audio, DTMF and mark echoes, and reads what the client would hear, with the time it
//! heard it. [`LoopbackConfig::mark_acks`] chooses who echoes marks: the test by hand (default),
//! the adapter after a delay (standing in for playout time), or nobody ([`MarkAcks`]).

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use brisvox_core::frame::{AudioFrame, DtmfDigit, SampleRate};
use brisvox_core::{SharedClock, TurnEpoch};
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::call::{CallEnds, CallLink, WireEvent, WireOut, call_pair};
use crate::error::{Error, Result};
use crate::event::CallInfo;
use crate::transport::Transport;

/// What the loopback client does with the marks it receives.
///
/// A real client echoes a mark once its playout buffer has played up to it. The loopback
/// models the buffer's arrival time but not playout time, so a test chooses how the echo
/// behaves: it sends it itself ([`LoopbackClient::ack_mark`]), lets the adapter do it after a
/// delay (standing in for the playout time of the audio before the mark), or never echoes (a
/// client that does not support marks, or one that stalls).
///
/// A clear drops the auto-acks still pending: a client that was cleared never plays those marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MarkAcks {
    /// The test echoes marks itself with [`LoopbackClient::ack_mark`] (the default).
    #[default]
    Manual,
    /// The adapter echoes every mark `delay` after the client received it (zero: at once).
    /// Echoes the test sends by hand are still delivered.
    ///
    /// Ordering: an echo is never held back behind an item that falls due later (the next
    /// audio, say), and one that falls due at the same instant goes to the core first, because
    /// its mark reached the client first. (The first is pinned end to end by the transport
    /// tests; the second only by the unit test of `next_wake`, since the echo and the delivery
    /// travel on different channels and a test cannot see which was sent first.) A delay too
    /// long to represent as an instant (for example [`Duration::MAX`]) means the echo never comes.
    Auto {
        /// Time between the client receiving a mark and echoing it.
        delay: Duration,
    },
    /// Marks are never echoed: [`LoopbackClient::ack_mark`] succeeds but the echo is dropped.
    Never,
}

/// Configuration of the loopback client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopbackConfig {
    /// Client-side buffering: how long after the core releases a frame the client hears it.
    pub latency: Duration,
    /// Most frames and marks the client buffer holds. When full, the adapter stops taking audio
    /// from the core, which then backs up (bounded, lossless).
    pub max_buffered: usize,
    /// Capacity of the queues between the client and the adapter (both directions).
    pub client_queue: usize,
    /// How long the adapter waits to hand an item to a peer that is not taking it (the client's
    /// receive queue, or the core). Past it the call ends: the adapter stops, which the core
    /// reports as a lost peer. Every send is also cancellable.
    pub send_timeout: Duration,
    /// How marks are echoed (see [`MarkAcks`]). With [`MarkAcks::Auto`], at most `max_buffered`
    /// echoes wait for their delay; beyond that the oldest is dropped.
    pub mark_acks: MarkAcks,
}

impl Default for LoopbackConfig {
    fn default() -> Self {
        Self {
            latency: Duration::from_millis(30),
            max_buffered: 256,
            client_queue: 256,
            send_timeout: Duration::from_secs(5),
            mark_acks: MarkAcks::Manual,
        }
    }
}

/// What the client receives from the transport, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// Audio the client plays, with the time it arrived at the client.
    Audio {
        /// The frame, as the core tagged it.
        frame: AudioFrame,
        /// When the client received it.
        received_at: Instant,
    },
    /// A mark. Everything sent before it has arrived; echo it with
    /// [`LoopbackClient::ack_mark`] once it has played.
    Mark {
        /// The mark id.
        id: String,
        /// When the client received it.
        received_at: Instant,
    },
    /// The core cleared the client: its buffered audio is gone.
    Clear {
        /// The epoch now current.
        epoch: TurnEpoch,
        /// When the client was cleared.
        at: Instant,
    },
    /// The call ended.
    Hangup {
        /// When the client learned of it.
        at: Instant,
    },
}

/// What the client sends to the transport.
#[derive(Debug)]
enum ClientInput {
    Audio(AudioFrame),
    Dtmf(DtmfDigit),
    MarkAck(String),
    Hangup,
}

/// The loopback transport. Accepts exactly one call.
pub struct LoopbackTransport {
    adapter: Option<Adapter>,
}

/// The adapter task's state: the client-side buffer and the client's channels.
struct Adapter {
    clock: SharedClock,
    config: LoopbackConfig,
    inbound: mpsc::Receiver<ClientInput>,
    outbound: mpsc::Sender<ClientEvent>,
}

/// The test side of a loopback call: the caller and the client.
pub struct LoopbackClient {
    caller: LoopbackCaller,
    listener: LoopbackListener,
}

/// The sending half of a [`LoopbackClient`] ([`LoopbackClient::split`]): the caller's audio,
/// key presses, mark echoes and hang-up.
pub struct LoopbackCaller {
    clock: SharedClock,
    inbound: mpsc::Sender<ClientInput>,
    next_seq: u64,
}

/// The receiving half of a [`LoopbackClient`] ([`LoopbackClient::split`]): what the transport
/// sent to the client. A task that reads it slowly is a slow consumer: the adapter waits for it
/// (each wait bounded by [`LoopbackConfig::send_timeout`]), and the core backs up behind the
/// adapter's bounded buffer.
pub struct LoopbackListener {
    outbound: mpsc::Receiver<ClientEvent>,
}

impl LoopbackTransport {
    /// A loopback transport and the client that talks to it. Both use `clock`.
    pub fn new(clock: SharedClock, config: LoopbackConfig) -> (Self, LoopbackClient) {
        let capacity = config.client_queue.max(1);
        let (inbound_tx, inbound_rx) = mpsc::channel(capacity);
        let (outbound_tx, outbound_rx) = mpsc::channel(capacity);
        let adapter = Adapter {
            clock: Arc::clone(&clock),
            config,
            inbound: inbound_rx,
            outbound: outbound_tx,
        };
        let client = LoopbackClient {
            caller: LoopbackCaller {
                clock,
                inbound: inbound_tx,
                next_seq: 0,
            },
            listener: LoopbackListener {
                outbound: outbound_rx,
            },
        };
        (
            Self {
                adapter: Some(adapter),
            },
            client,
        )
    }
}

impl Transport for LoopbackTransport {
    async fn accept(&mut self, cancel: &CancellationToken) -> Result<Option<CallLink>> {
        let Some(adapter) = self.adapter.take() else {
            return Ok(None);
        };
        let (link, ends) = call_pair();
        tokio::spawn(adapter.run(ends, cancel.clone()));
        Ok(Some(link))
    }
}

/// Hands an item to a peer, giving up when `cancel` fires, the core ended the call (`ended`) or
/// the peer takes nothing for `limit`.
/// Returns whether the item was delivered; the adapter ends the call when it was not.
async fn deliver<E>(
    cancel: &CancellationToken,
    ended: &CancellationToken,
    limit: Duration,
    send: impl Future<Output = std::result::Result<(), E>>,
) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        () = ended.cancelled() => false,
        sent = tokio::time::timeout(limit, send) => matches!(sent, Ok(Ok(()))),
    }
}

/// What the client buffer holds.
enum Item {
    Audio(AudioFrame),
    Mark(String),
}

/// One buffered item and the instant the client hears it.
struct Delivery {
    at: Instant,
    item: Item,
}

/// What the adapter loop does next.
enum Step {
    Stop,
    Out(Option<WireOut>),
    Deliver,
    Ack,
    In(Option<ClientInput>),
}

impl Adapter {
    async fn run(self, ends: CallEnds, cancel: CancellationToken) {
        let CallEnds {
            events,
            mut outgoing,
            ended,
        } = ends;
        let Adapter {
            clock,
            config,
            mut inbound,
            outbound,
        } = self;
        let mut buffer: VecDeque<Delivery> = VecDeque::new();
        // Marks the client received and will echo (`MarkAcks::Auto`), oldest first; their due
        // instants never decrease because the delay is constant.
        let mut pending_acks: VecDeque<(Instant, String)> = VecDeque::new();
        let mut min_epoch = TurnEpoch::ZERO;
        let mut client_open = true;

        let started = WireEvent::Started(CallInfo {
            call_id: "loopback".to_owned(),
            metadata: BTreeMap::new(),
        });
        if !deliver(&cancel, &ended, config.send_timeout, events.send(started)).await {
            return;
        }
        let limit = config.send_timeout;

        loop {
            let due = buffer.front().map(|d| d.at);
            let ack_due = pending_acks.front().map(|a| a.0);
            let (next, ack_first) = next_wake(due, ack_due, clock.now());
            let step = tokio::select! {
                biased;
                () = cancel.cancelled() => Step::Stop,
                () = ended.cancelled() => Step::Stop,
                out = outgoing.recv(), if buffer.len() < config.max_buffered => Step::Out(out),
                () = clock.sleep_until(next), if due.is_some() || ack_due.is_some() => {
                    if ack_first { Step::Ack } else { Step::Deliver }
                }
                input = inbound.recv(), if client_open => Step::In(input),
            };
            match step {
                Step::Stop | Step::Out(None | Some(WireOut::Hangup)) => break,
                Step::Out(Some(WireOut::Audio(frame))) => {
                    if frame.epoch().get() < min_epoch.get() {
                        continue;
                    }
                    let at = arrival_deadline(&clock, config.latency, buffer.back());
                    buffer.push_back(Delivery {
                        at,
                        item: Item::Audio(frame),
                    });
                }
                Step::Out(Some(WireOut::Mark { id })) => {
                    let at = arrival_deadline(&clock, config.latency, buffer.back());
                    buffer.push_back(Delivery {
                        at,
                        item: Item::Mark(id),
                    });
                }
                Step::Out(Some(WireOut::Clear { epoch })) => {
                    min_epoch = min_epoch.max(epoch);
                    buffer.clear();
                    // A cleared client never plays the marks it still had queued.
                    pending_acks.clear();
                    let event = ClientEvent::Clear {
                        epoch,
                        at: clock.now(),
                    };
                    if !deliver(&cancel, &ended, limit, outbound.send(event)).await {
                        break;
                    }
                }
                Step::Deliver => {
                    let Some(delivery) = buffer.pop_front() else {
                        continue;
                    };
                    let received_at = clock.now();
                    let event = match delivery.item {
                        Item::Audio(frame) => ClientEvent::Audio { frame, received_at },
                        Item::Mark(id) => {
                            if let MarkAcks::Auto { delay } = config.mark_acks {
                                if pending_acks.len() >= config.max_buffered.max(1) {
                                    pending_acks.pop_front();
                                }
                                // A delay too long to represent is "never": no echo is queued.
                                if let Some(ack_at) = received_at.checked_add(delay) {
                                    pending_acks.push_back((ack_at, id.clone()));
                                }
                            }
                            ClientEvent::Mark { id, received_at }
                        }
                    };
                    if !deliver(&cancel, &ended, limit, outbound.send(event)).await {
                        break;
                    }
                }
                Step::Ack => {
                    let Some((_, id)) = pending_acks.pop_front() else {
                        continue;
                    };
                    if !deliver(
                        &cancel,
                        &ended,
                        limit,
                        events.send(WireEvent::MarkAck { id }),
                    )
                    .await
                    {
                        break;
                    }
                }
                Step::In(Some(ClientInput::Audio(frame))) => {
                    if !deliver(&cancel, &ended, limit, events.send(WireEvent::Audio(frame))).await
                    {
                        break;
                    }
                }
                Step::In(Some(ClientInput::Dtmf(digit))) => {
                    let event = WireEvent::Dtmf {
                        digit,
                        at: clock.now(),
                    };
                    if !deliver(&cancel, &ended, limit, events.send(event)).await {
                        break;
                    }
                }
                Step::In(Some(ClientInput::MarkAck(_))) if config.mark_acks == MarkAcks::Never => {
                    // A client that never echoes marks: the echo is swallowed.
                }
                Step::In(Some(ClientInput::MarkAck(id))) => {
                    if !deliver(
                        &cancel,
                        &ended,
                        limit,
                        events.send(WireEvent::MarkAck { id }),
                    )
                    .await
                    {
                        break;
                    }
                }
                Step::In(Some(ClientInput::Hangup)) => {
                    // The client hung up; the core ends the call and tells us with `Hangup`.
                    client_open = false;
                    if !deliver(&cancel, &ended, limit, events.send(WireEvent::Hangup)).await {
                        break;
                    }
                }
                Step::In(None) => {
                    // The client went away without a word: drop our end of the wire, which the
                    // core reports as a lost peer.
                    break;
                }
            }
        }
        // Best effort: the client may have stopped reading.
        let _ = outbound.try_send(ClientEvent::Hangup { at: clock.now() });
    }
}

/// When the adapter next has something to hand over, and whether that is an echo (`true`) or a
/// delivery to the client (`false`). An echo falling due at the same instant as a delivery goes
/// first: its mark reached the client before the item that is due with it.
fn next_wake(due: Option<Instant>, ack_due: Option<Instant>, now: Instant) -> (Instant, bool) {
    match (ack_due, due) {
        (Some(ack), Some(delivery)) => (ack.min(delivery), ack <= delivery),
        (Some(at), None) => (at, true),
        (None, Some(at)) => (at, false),
        (None, None) => (now, false),
    }
}

/// When a frame arriving now is heard: `latency` later, and never before the item ahead of it.
fn arrival_deadline(clock: &SharedClock, latency: Duration, ahead: Option<&Delivery>) -> Instant {
    let now = clock.now();
    // A latency too long to represent saturates at the item ahead (or now): never a panic.
    let heard = now.checked_add(latency).unwrap_or(now);
    ahead.map_or(heard, |behind| behind.at.max(heard))
}

impl LoopbackClient {
    /// Sends caller audio at `rate`, timestamped now. The frame is `pcm` as 16-bit little-endian
    /// mono samples; an odd byte count is rejected.
    ///
    /// # Errors
    /// [`Error::Core`] for an invalid buffer; [`Error::Disconnected`] once the call has ended.
    pub async fn send_audio(&mut self, pcm: Bytes, rate: SampleRate) -> Result<()> {
        self.caller.send_audio(pcm, rate).await
    }

    /// Presses a DTMF key.
    ///
    /// # Errors
    /// [`Error::Disconnected`] once the call has ended.
    pub async fn send_dtmf(&self, digit: DtmfDigit) -> Result<()> {
        self.caller.send_dtmf(digit).await
    }

    /// Echoes a mark the client received, once it has played.
    ///
    /// # Errors
    /// [`Error::Disconnected`] once the call has ended.
    pub async fn ack_mark(&self, id: &str) -> Result<()> {
        self.caller.ack_mark(id).await
    }

    /// Hangs up from the client side.
    ///
    /// # Errors
    /// [`Error::Disconnected`] once the call has ended.
    pub async fn hangup(&self) -> Result<()> {
        self.caller.hangup().await
    }

    /// The next event the client receives. `None` once the transport side has gone.
    pub async fn recv(&mut self) -> Option<ClientEvent> {
        self.listener.recv().await
    }

    /// The two halves, for a test that sends on one task and reads on another (a slow
    /// consumer reading at its own pace while the caller keeps talking).
    pub fn split(self) -> (LoopbackCaller, LoopbackListener) {
        (self.caller, self.listener)
    }
}

impl LoopbackCaller {
    /// See [`LoopbackClient::send_audio`].
    ///
    /// # Errors
    /// [`Error::Core`] for an invalid buffer; [`Error::Disconnected`] once the call has ended.
    pub async fn send_audio(&mut self, pcm: Bytes, rate: SampleRate) -> Result<()> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let frame = AudioFrame::new(pcm, rate, self.clock.now(), seq, TurnEpoch::ZERO)?;
        self.push(ClientInput::Audio(frame)).await
    }

    /// See [`LoopbackClient::send_dtmf`].
    ///
    /// # Errors
    /// [`Error::Disconnected`] once the call has ended.
    pub async fn send_dtmf(&self, digit: DtmfDigit) -> Result<()> {
        self.push(ClientInput::Dtmf(digit)).await
    }

    /// See [`LoopbackClient::ack_mark`].
    ///
    /// # Errors
    /// [`Error::Disconnected`] once the call has ended.
    pub async fn ack_mark(&self, id: &str) -> Result<()> {
        self.push(ClientInput::MarkAck(id.to_owned())).await
    }

    /// See [`LoopbackClient::hangup`].
    ///
    /// # Errors
    /// [`Error::Disconnected`] once the call has ended.
    pub async fn hangup(&self) -> Result<()> {
        self.push(ClientInput::Hangup).await
    }

    async fn push(&self, input: ClientInput) -> Result<()> {
        self.inbound
            .send(input)
            .await
            .map_err(|_| Error::Disconnected)
    }
}

impl LoopbackListener {
    /// The next event the client receives. `None` once the transport side has gone.
    pub async fn recv(&mut self) -> Option<ClientEvent> {
        self.outbound.recv().await
    }
}

#[cfg(test)]
mod tests {
    use brisvox_core::Clock;

    use super::*;

    #[test]
    fn an_echo_due_with_a_delivery_goes_first() {
        let now = brisvox_core::SimClock::new().now();
        let later = now + Duration::from_millis(5);
        assert_eq!(next_wake(Some(now), Some(now), now), (now, true));
        assert_eq!(next_wake(Some(now), Some(later), now), (now, false));
        assert_eq!(next_wake(Some(later), Some(now), now), (now, true));
        assert_eq!(next_wake(None, Some(later), now), (later, true));
        assert_eq!(next_wake(Some(later), None, now), (later, false));
        assert_eq!(next_wake(None, None, now), (now, false));
    }
}
