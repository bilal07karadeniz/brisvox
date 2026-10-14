//! The scripted caller as the production pipeline sees it: audio frames in through a loopback
//! client, a real `TransportSession` and the pump, into the pipeline's first stage. Shared by the
//! hybrid-gate tests that start from the engine's input chain.
#![allow(dead_code, clippy::unwrap_used)]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use brisvox_core::frame::AudioFrame;
use brisvox_core::{FrameSender, SharedClock};
use brisvox_engine::{
    CallRatesCell, EngineConfig, PlayoutBook, Pump, PumpConfig, PumpHandle, PumpLinks,
};
use brisvox_sim::input::{InputTarget, SendFuture};
use brisvox_transport::{
    CommandSender, LoopbackClient, LoopbackConfig, LoopbackTransport, MarkAcks, SessionConfig,
    Transport, TransportSession,
};
use tokio_util::sync::{CancellationToken, DropGuard};

use super::MS;

/// Audio frames in through a loopback client, the transport session and the pump. The pump
/// starts when the runner binds the sink to the started pipeline.
pub struct LoopbackInput {
    client: tokio::sync::Mutex<LoopbackClient>,
    session: Mutex<Option<TransportSession>>,
    /// The running pump (after the sink was bound).
    pub pump: Mutex<Option<PumpHandle>>,
    /// The book the pump writes marks and positions to.
    pub book: Arc<PlayoutBook>,
    clock: SharedClock,
    config: PumpConfig,
    cancel: CancellationToken,
    /// Set for a call whose output goes through the transport: the pump also
    /// publishes the call's rates into the cell and delivers mark echoes to the pipeline's
    /// upstream input, so it starts once both inputs are known.
    output: Option<CallRatesCell>,
    input_sender: Mutex<Option<FrameSender>>,
    _stop_on_drop: DropGuard,
}

impl LoopbackInput {
    /// The next thing the transport sent to the loopback client (audio, marks, clears), waiting
    /// at most `timeout`.
    pub async fn recv_client(&self, timeout: Duration) -> Option<brisvox_transport::ClientEvent> {
        let mut client = self.client.lock().await;
        tokio::time::timeout(timeout, client.recv()).await.ok()?
    }
}

impl InputTarget for LoopbackInput {
    fn send(&self, frame: AudioFrame) -> SendFuture<'_> {
        Box::pin(async move {
            let mut client = self.client.lock().await;
            client
                .send_audio(frame.data().clone(), frame.sample_rate())
                .await
                .is_ok()
        })
    }

    fn bind(&self, input: &FrameSender) {
        if self.output.is_some() {
            // The pump needs the upstream input as well: it starts in `bind_upstream`.
            *self
                .input_sender
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(input.clone());
            return;
        }
        let session = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(session) = session else { return };
        let handle = Pump::spawn(
            session,
            PumpLinks {
                input: input.clone(),
                // The reference output has no transport marks to echo.
                upstream: None,
            },
            Arc::clone(&self.book),
            self.clock.clone(),
            self.config,
            &self.cancel,
        );
        *self.pump.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }

    fn bind_upstream(&self, upstream: &FrameSender) {
        let Some(rates) = &self.output else { return };
        let input = self
            .input_sender
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let session = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let (Some(input), Some(session)) = (input, session) else {
            return;
        };
        let handle = Pump::spawn_publishing(
            session,
            PumpLinks {
                input,
                upstream: Some(upstream.clone()),
            },
            Arc::clone(&self.book),
            self.clock.clone(),
            self.config,
            &self.cancel,
            rates.clone(),
        );
        *self.pump.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }
}

/// How [`open_with`] differs from [`open`].
#[derive(Debug, Clone, Default)]
pub struct LoopbackOptions {
    /// Capacity of the loopback client's queues (default: the loopback's own). The hybrid gate
    /// of the output chain reads the client's events after the run, so its queue must hold them.
    pub client_queue: Option<usize>,
    /// Route mark echoes to the pipeline's upstream input and publish the call's rates here.
    pub output: Option<CallRatesCell>,
}

/// Opens the loopback call and returns the caller side together with the session's
/// `CommandSender` (what the turn driver clears the transport with).
pub async fn open(
    clock: &SharedClock,
    config: &EngineConfig,
) -> Result<(Arc<LoopbackInput>, CommandSender), String> {
    open_with(clock, config, LoopbackOptions::default()).await
}

/// Like [`open`], with options.
pub async fn open_with(
    clock: &SharedClock,
    config: &EngineConfig,
    options: LoopbackOptions,
) -> Result<(Arc<LoopbackInput>, CommandSender), String> {
    let defaults = LoopbackConfig::default();
    let (mut transport, client) = LoopbackTransport::new(
        clock.clone(),
        LoopbackConfig {
            latency: Duration::ZERO,
            mark_acks: MarkAcks::Auto {
                delay: Duration::ZERO,
            },
            client_queue: options.client_queue.unwrap_or(defaults.client_queue),
            max_buffered: options.client_queue.unwrap_or(defaults.max_buffered),
            ..defaults
        },
    );
    let cancel = CancellationToken::new();
    let link = transport
        .accept(&cancel)
        .await
        .map_err(|e| format!("accept: {e}"))?
        .ok_or_else(|| "accept: no call".to_owned())?;
    let session = TransportSession::open(
        clock.clone(),
        link,
        SessionConfig {
            position_interval: 20 * MS,
            ..SessionConfig::default()
        },
        &cancel,
    )
    .await
    .map_err(|e| format!("open: {e}"))?;
    let commands = session.commands();
    let input = Arc::new(LoopbackInput {
        client: tokio::sync::Mutex::new(client),
        session: Mutex::new(Some(session)),
        pump: Mutex::new(None),
        book: PlayoutBook::shared(16),
        clock: clock.clone(),
        config: PumpConfig::from_engine(config),
        cancel: cancel.clone(),
        output: options.output,
        input_sender: Mutex::new(None),
        _stop_on_drop: cancel.drop_guard(),
    });
    Ok((input, commands))
}
