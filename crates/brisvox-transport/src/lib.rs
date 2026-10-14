//! Transport layer: moves audio and control between a caller and a brisvox session.
//!
//! The provider-neutral pieces:
//! - [`Transport`]: a source of calls. Each call becomes a [`CallLink`]; the adapter that speaks
//!   the provider protocol (websocket, Twilio, ...) owns the matching [`CallEnds`] (see [`call`]).
//! - [`TransportSession`]: the engine's handle on one call. It reads [`TransportEvent`]s (caller
//!   audio, DTMF, mark echoes, playback positions, the stop reason) and sends
//!   [`TransportCommand`]s (paced audio, clears, marks, hang-up).
//! - [`LoopbackTransport`]: an in-memory transport with a simulated client, for tests and the
//!   simulator.
//! - `websocket` (module, default feature): the generic WebSocket protocol adapter, a
//!   reference client and the JSON control protocol (`docs/WEBSOCKET_PROTOCOL.md`).
//! - `twilio` (module, default feature): Twilio Media Streams (mu-law 8 kHz, marks, clear,
//!   signature check) on the same server infrastructure (`docs/providers/twilio.md`).
//! - `telnyx` (module, default feature): Telnyx media streaming (G.711 8 kHz, marks, clear, reorder
//!   buffer, optional stream auth token, host-supplied hang-up) on the same server infrastructure
//!   (`docs/providers/telnyx.md`).
//! - `webrtc` (module, cargo feature `webrtc`, off by default): browsers over WebRTC with
//!   WHIP-style HTTP signaling, a demo page and Opus (`docs/providers/webrtc.md`).
//! - `sip` (module, cargo feature `sip`, off by default): a SIP user agent for carriers and PBXs
//!   with G.711 over RTP, RFC 4733 DTMF, inbound and outbound calls (`docs/providers/sip.md`).
//!
//! - `rtvi` (module, default feature): RTVI 2.1.0 over WebSocket with protobuf `pipecat.Frame`
//!   framing, for the Pipecat client SDKs (`@pipecat-ai/client-js`;
//!   `docs/providers/rtvi.md`, `docs/RTVI.md`).
//!
//! - `call_control` (module, cargo feature `call-control`, off by default): REST call control
//!   next to the media stream, for Twilio and Telnyx: DTMF toward the far end, cold and warm
//!   transfer, hang-up and answering-machine detection
//!   (`docs/providers/call-control.md`, `docs/TELEPHONY.md`).
//!
//! Output audio is released at real time by the production [`OutputPacer`](brisvox_audio::OutputPacer).
//! Every queue is bounded and says what it does when full ([`channel::Overflow`]). Design notes and
//! latency budget: `docs/ARCHITECTURE.md`, "Transport".

pub mod admission;
#[cfg(feature = "auth")]
pub mod auth;
pub mod call;
#[cfg(feature = "call-control")]
pub mod call_control;
pub mod channel;
pub mod cidr;
pub mod error;
pub mod event;
pub mod loopback;
#[cfg(feature = "rtvi")]
pub mod rtvi;
pub mod session;
#[cfg(any(feature = "twilio", feature = "telnyx"))]
pub(crate) mod telephony;
#[cfg(feature = "telnyx")]
pub mod telnyx;

#[cfg(feature = "sip")]
pub mod sip;
pub mod transport;
#[cfg(feature = "twilio")]
pub mod twilio;
pub mod wav;
#[cfg(feature = "webrtc")]
pub mod webrtc;
#[cfg(feature = "websocket")]
pub mod websocket;

pub use call::{CallEnds, CallLink, WireEvent, WireOut, call_pair};
pub use cidr::Cidr;
pub use error::{Error, Result};
pub use event::{CallInfo, CallRates, StopReason, TransportCommand, TransportEvent, UtteranceId};
pub use loopback::{
    ClientEvent, LoopbackCaller, LoopbackClient, LoopbackConfig, LoopbackListener,
    LoopbackTransport, MarkAcks,
};
pub use session::{CallStarted, CommandSender, SessionConfig, TransportSession};
pub use transport::Transport;
