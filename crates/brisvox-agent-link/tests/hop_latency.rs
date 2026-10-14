//! The hop latency of the agent link (`docs/LATENCY.md`): an engine event leaves the
//! call, crosses the WebSocket to the reference agent on 127.0.0.1, the agent answers with a
//! command naming the event (`re`), and the server parses it. The link measures every such hop
//! itself (`LinkStats`: mean, max and a bucketed histogram); this test drives 500 hops on the
//! real clock and prints the distribution (`cargo nextest run -p brisvox-agent-link
//! --test hop_latency --no-capture`, add `--release` for the production build).
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use brisvox_agent_link::client::AgentServer;
use brisvox_agent_link::protocol::{AgentMessage, ServerBody, SessionAccept};
use brisvox_agent_link::{AgentLinkConfig, LinkCall, attach};
use brisvox_core::{SharedClock, SystemClock, TimelineRecorder, TurnEpoch};
use brisvox_engine::{EngineConfig, EngineEvent, EngineEvents, Offload, SessionBuilder};
use brisvox_sim::session::SimSession;
use tokio_util::sync::CancellationToken;

const HOPS: u64 = 500;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hop_from_an_event_to_the_agents_command_is_measured() {
    let clock: SharedClock = SystemClock::shared();
    let server = AgentServer::bind("127.0.0.1:0".parse().unwrap(), clock.clone())
        .await
        .unwrap();
    let config = Arc::new(AgentLinkConfig::new(server.url().unwrap()));
    let sim = SimSession::new(clock.clone());
    let events = EngineEvents::new(1024);
    let builder = SessionBuilder::new(
        EngineConfig::DEFAULT,
        clock.clone(),
        TimelineRecorder::new(),
        Offload::Inline(clock.clone()),
        Arc::new(sim.stt.clone()),
        Arc::new(sim.llm.clone()),
        Arc::new(sim.tts.clone()),
    )
    .unwrap()
    .with_events(events.clone());
    let cancel = CancellationToken::new();
    let call = LinkCall {
        session_id: 1,
        agent: "hop".to_owned(),
        clock: clock.clone(),
        cancel: cancel.clone(),
        ops: None,
        identity: None,
    };
    let agent = async {
        let mut session = server.accept().await.unwrap();
        session
            .accept_session(SessionAccept {
                version: 1,
                events: Some(vec!["user.text".to_owned()]),
                ..SessionAccept::default()
            })
            .await
            .unwrap();
        session
    };
    let ((_builder, link), mut session) = tokio::join!(attach(&config, builder, call), agent);
    let link = link.unwrap();

    // The agent answers every user text at once with a command that names it.
    let (answered_tx, mut answered_rx) = tokio::sync::mpsc::channel::<()>(1);
    let responder = tokio::spawn(async move {
        let mut answered = 0u64;
        while answered < HOPS {
            let message = session.recv().await.unwrap().unwrap();
            if let ServerBody::UserText { .. } = message.body {
                let command = AgentMessage::ToolsSet {
                    id: None,
                    re: Some(message.seq),
                    enabled: None,
                };
                session.send(&command).await.unwrap();
                answered += 1;
                answered_tx.send(()).await.unwrap();
            }
        }
        session
    });
    let text: Arc<str> = Arc::from("what time is it");
    for n in 0..HOPS {
        events.publish(EngineEvent::UserText {
            epoch: TurnEpoch::new(u32::try_from(n).unwrap()),
            text: Arc::clone(&text),
        });
        // One hop at a time: the next event leaves once the agent answered this one.
        tokio::time::timeout(Duration::from_secs(5), answered_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    let _session = tokio::time::timeout(Duration::from_secs(20), responder)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..200 {
        if link.stats().hops == HOPS {
            break;
        }
        clock.sleep(Duration::from_millis(10)).await;
    }
    let stats = link.stats();
    assert_eq!(stats.hops, HOPS, "{stats:?}");
    assert_eq!(stats.events_lost, 0, "{stats:?}");
    let ms = |d: Option<Duration>| d.map_or(f64::NAN, |d| d.as_secs_f64() * 1000.0);
    eprintln!(
        "agent hop over {} events: mean {:.3} ms, p50 <= {:.3} ms, p95 <= {:.3} ms, p99 <= {:.3} ms, max {:.3} ms (build: {})",
        stats.hops,
        ms(stats.hop_mean()),
        ms(stats.hop_quantile(0.50)),
        ms(stats.hop_quantile(0.95)),
        ms(stats.hop_quantile(0.99)),
        ms(Some(Duration::from_micros(stats.hop_max_us))),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    );
    // A loose bound (shared CI machines, debug builds): the median loopback hop is well under
    // a 20 ms audio frame.
    let p50 = stats.hop_quantile(0.50).unwrap();
    assert!(p50 <= Duration::from_millis(20), "{stats:?}");
    link.close("done").await;
    cancel.cancel();
}
