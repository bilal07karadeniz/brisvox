//! The provider-neutral transport trait.

use std::future::Future;

use tokio_util::sync::CancellationToken;

use crate::call::CallLink;
use crate::error::Result;

/// A source of calls: a websocket server, a telephony webhook, a WebRTC signalling endpoint, or
/// the in-memory [`LoopbackTransport`](crate::LoopbackTransport).
///
/// An implementation turns each call into a [`CallLink`]: it creates the pair with
/// [`call_pair`](crate::call_pair), keeps the [`CallEnds`](crate::CallEnds) for its protocol task
/// (spawned by the implementation), and returns the link. The protocol task must end when
/// `cancel` fires or [`CallEnds::ended`](crate::CallEnds::ended) is cancelled, also while it is
/// blocked sending to its client or to the core, and every such send needs its own timeout. The engine then opens a session on the link with
/// [`TransportSession::open`](crate::TransportSession::open).
pub trait Transport: Send + 'static {
    /// Waits for the next incoming call. Returns `Ok(None)` once the transport takes no more
    /// calls. Must return promptly once `cancel` fires.
    fn accept(
        &mut self,
        cancel: &CancellationToken,
    ) -> impl Future<Output = Result<Option<CallLink>>> + Send;
}
