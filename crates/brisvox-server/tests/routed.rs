//! One process, two agents (`BRISVOX_WS_AGENT_FILE`): the generic websocket listener's calls
//! run the websocket runner, admitted through the same manager as every other listener's.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use brisvox_core::{SharedClock, SystemClock};
use brisvox_server::{
    AcceptedCall, HostSettings, ManagerConfig, Server, ServerSettings, SessionContext,
    SessionRunner, ShutdownPolicy,
};
use brisvox_transport::websocket::{ClientMessage, Encoding, Start};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(10);

/// Counts the calls it runs; each call ends at once.
#[derive(Default)]
struct Counting {
    calls: AtomicUsize,
}

impl SessionRunner for Counting {
    type Request = AcceptedCall;
    type Output = ();
    type Error = String;

    async fn run(self: Arc<Self>, _ctx: SessionContext, _call: AcceptedCall) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn serve(
    main: &Arc<Counting>,
    websocket: Option<&Arc<Counting>>,
    clock: SharedClock,
) -> Server {
    let host = HostSettings {
        websocket: Some(([127, 0, 0, 1], 0).into()),
        ..HostSettings::none()
    };
    let settings = ServerSettings {
        manager: ManagerConfig::new(4).unwrap(),
        health_addr: None,
        shutdown: ShutdownPolicy {
            drain_timeout: Duration::from_secs(2),
            cancel_grace: Duration::from_secs(1),
            abort_wait: Duration::from_secs(1),
        },
        ..ServerSettings::default()
    };
    Server::start_hosting_routed(
        settings,
        brisvox_server::AdmissionPolicy::none(),
        &host,
        clock,
        main,
        websocket,
    )
    .await
    .unwrap()
}

/// Dials the generic websocket listener and opens one call.
async fn call(server: &Server) {
    let url = format!("ws://{}/call", server.listener_addrs().websocket.unwrap());
    let (mut ws, _) = tokio::time::timeout(WAIT, tokio_tungstenite::connect_async(url))
        .await
        .unwrap()
        .unwrap();
    let start = ClientMessage::Start(Start::new(16_000, Encoding::Pcm16));
    ws.send(Message::text(start.to_json())).await.unwrap();
    // The runner ends the call at once: read until the server closes.
    let _ = tokio::time::timeout(WAIT, async { while ws.next().await.is_some() {} }).await;
}

async fn until(what: &str, check: impl Fn() -> bool) {
    let clock = SystemClock::shared();
    let done = tokio::time::timeout(WAIT, async {
        while !check() {
            clock.sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    assert!(done, "timed out waiting for {what}");
}

#[tokio::test]
async fn the_websocket_listener_runs_its_own_runner_when_given_one() {
    let clock = SystemClock::shared();
    let main = Arc::new(Counting::default());
    let websocket = Arc::new(Counting::default());
    let server = serve(&main, Some(&websocket), clock).await;
    call(&server).await;
    call(&server).await;
    until("two websocket calls", || {
        websocket.calls.load(Ordering::SeqCst) == 2
    })
    .await;
    assert_eq!(
        main.calls.load(Ordering::SeqCst),
        0,
        "the main agent took none"
    );
    assert_eq!(
        server.manager().stats().started,
        2,
        "one manager admits them"
    );
    drop(server);
}

#[tokio::test]
async fn without_a_websocket_runner_the_main_one_runs_its_calls() {
    let clock = SystemClock::shared();
    let main = Arc::new(Counting::default());
    let server = serve(&main, None, clock).await;
    call(&server).await;
    until("the call", || main.calls.load(Ordering::SeqCst) == 1).await;
    drop(server);
}
