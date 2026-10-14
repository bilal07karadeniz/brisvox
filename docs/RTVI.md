# RTVI: connecting a Pipecat client to brisvox

`brisvox_transport::rtvi` (feature `rtvi`, on by default) lets the Pipecat client SDKs talk to a
brisvox server: `@pipecat-ai/client-js` with `@pipecat-ai/websocket-transport`. Protocol facts,
sources and the list of unverified points are in `docs/providers/rtvi.md` (RTVI 2.1.0, fetched and
fact-checked 2026-10-08). The module documentation (`crates/brisvox-transport/src/rtvi/mod.rs`)
is the reference for behavior; this page is how to use it and how to check it by hand.

> **Not the same listener.** The WebSocket fallback of the WebRTC signaling port
> (`docs/DEPLOY.md`) carries the generic protocol of `docs/WEBSOCKET_PROTOCOL.md`, not RTVI. A
> Pipecat client SDK keeps using `BRISVOX_RTVI_ADDR`, which is already one HTTP(S) port and works
> on one-port hosts (such as Railway) without further steps.

## Server side

```rust
let mut server = RtviServer::bind(ServerConfig::default(), RtviConfig::default(),
                                  SystemClock::shared(), &cancel).await?;
while let Some(call) = server.accept_call(&cancel).await? {
    // call.link   -> TransportSession::open(clock, call.link, SessionConfig::default(), &cancel)
    // call.sender -> RtviSender: send(RtviEvent::..), respond(id, t, d), respond_error(id, msg)
    // call.inbox  -> RtviInbox: client-message / send-text / llm-function-call-result requests
}
```

- One WebSocket path (`RtviConfig::path`, default `/rtvi`); the client accepts any path.
- Audio: 16 kHz in (the JS client's `recorderSampleRate` default), 24 kHz out (its
  `playerSampleRate` default). The JS client ignores the `sample_rate` of bot audio and plays at its
  own rate, so `RtviConfig::output_rate` must equal the client's `playerSampleRate`. The engine reads
  both rates from `CallInfo::sample_rate()` (`input_sample_rate` / `output_sample_rate` metadata).
- `Transport::accept` also works and gives only the `CallLink` (no RTVI events, client requests are
  answered with errors).
- Terminate TLS in front of the listener (`wss://`). The protocol has no authentication of its
  own; the upgrade checks are `RtviConfig::sessions` (short-lived single-use session tokens,
  `BRISVOX_SESSION_SECRET`, the production way), `RtviConfig::token` (a shared static token,
  development) and `allowed_origins`. The server binary refuses to bind a non-loopback RTVI
  address with neither a session secret nor a token (`BRISVOX_INSECURE_LISTENERS=1` overrides;
  `docs/SECURITY.md`).
- The token travels in `?token=` (what the Pipecat client SDKs append; it lands in proxy logs and
  browser history), in `Authorization: Bearer <token>`, or in the WebSocket subprotocol
  `brisvox-token.<token>`: a browser page offers `["brisvox", "brisvox-token." + token]` and the
  server selects `brisvox`. A missing token is HTTP 401, an invalid, expired, reused or
  wrong-audience one 403, an oversize one (over 4 KiB) 413. Every refusal and the accepted
  upgrade carry `Cache-Control: no-store`. A session token's claims reach the call's metadata
  (`session_id`, `session.agent`, `session.meta.<key>`); `max_s` is reported, not enforced, here.

### With the engine

`brisvox_engine::rtvi::RtviBridge` connects a call's `RtviSender` and `RtviInbox` to the engine:

```rust
let config = RtviConfig::default(); // the bridge switches the adapter's speaking events off per call
// ... accept_call as above, then for each call:
let (builder, bridge) = RtviBridge::attach(builder, call.sender, call.inbox,
                                           RtviBridgeConfig::default(), Some(handler), clock.clone(), &cancel);
let session = TransportSession::open(clock, call.link, SessionConfig::default(), &cancel).await?;
let call = run_call(session, builder, &cancel).await?; // keep `bridge` alive as long as the call
```

- The client gets `user-started/stopped-speaking` (the user's turn), `user-transcription`,
  `user-llm-text`, `bot-llm-started/text/stopped`, `bot-tts-started/text/stopped`, `bot-output`,
  `bot-started/stopped-speaking` (the transport's playout), `bot-interrupted`, `metrics` (one per
  turn) and `error`, all on one queue. The mapping table is in the module docs
  (`crates/brisvox-engine/src/rtvi.rs`); the events come from `brisvox_engine::events`. Every
  `user-started-speaking` gets its `user-stopped-speaking`, also for a turn that ends without
  words (noise, an empty STT final), a turn superseded while its end waited for a final, and the
  hang-up.
- `send-text` becomes a user turn (`brisvox_engine::text_input`): it interrupts the bot if it is
  thinking or speaking, the runner appends the text to the context as a user message and answers
  it; while the user is speaking it joins that turn; `run_immediately: false` holds it for the next
  turn (Pipecat appends it to the context at once instead). A typed turn sends no
  `user-started/stopped-speaking` (Pipecat neither), only `bot-interrupted` if the bot was busy and
  the `user-llm-text`. `client-message` goes to the `ClientMessageHandler` (`server-response` with
  its answer, `error-response` on its error, its timeout or without a handler); up to 8 replies run
  at once beside the inbox, so a slow handler never delays a `send-text` barge-in.
- With `RtviBridgeConfig::bot_speaking_events` on (the default), `attach` switches the adapter's
  own speaking events off for the call (`RtviSender::set_auto_events(false)`), so the client never
  gets them twice whatever `RtviConfig::auto_events` says. With it off the adapter's derivation
  stays as configured.
- `RtviBridgeConfig::event_capacity` is clamped to `MAX_EVENT_CAPACITY` (65 536) and
  `text_capacity` to `MAX_TEXT_CAPACITY` (256).

Run the demo bot (no provider keys, energy-based speech detection, answers with a beep):

```text
cargo run -p brisvox-transport --example rtvi_server -- 127.0.0.1:8765 /rtvi
```

## Client side (`@pipecat-ai/client-js` 1.13.x, `@pipecat-ai/websocket-transport` 1.7.x)

```ts
import { PipecatClient, RTVIEvent } from "@pipecat-ai/client-js";
import {
  WebSocketTransport,
  ProtobufFrameSerializer,
} from "@pipecat-ai/websocket-transport";

const client = new PipecatClient({
  transport: new WebSocketTransport({
    serializer: new ProtobufFrameSerializer(), // the default
    recorderSampleRate: 16000,                 // must match what the server expects as input
    playerSampleRate: 24000,                   // must equal RtviConfig::output_rate
  }),
  enableMic: true,
  callbacks: {
    onBotReady: () => console.log("bot ready"),
    onUserTranscript: (t) => console.log("user:", t.text, t.final),
    onBotOutput: (o) => console.log("bot:", o.text),
    onBotStartedSpeaking: () => console.log("bot speaking"),
    onError: (e) => console.error(e),
  },
});

// Direct connection: the connection parameters are exactly { wsUrl } and optionally { token }.
await client.connect({ wsUrl: "ws://127.0.0.1:8765/rtvi" });
// With RtviConfig::token or ::sessions set: { wsUrl: "wss://host/rtvi", token: "..." }
// (sent as ?token=). A session token is minted by your backend per call and is single use.

// Application messages (answered by RtviRequest::ClientMessage on the server):
const reply = await client.sendClientRequest("ping", { n: 1 });
client.sendText("hello");

await client.disconnect(); // closes the socket; the server sees a hang-up
```

The snippet is a sketch, not a tested app. The names `ProtobufFrameSerializer`, `recorderSampleRate`,
`playerSampleRate`, `connect({ wsUrl })`, `sendClientRequest`, `sendText` and the callbacks come from
the sources recorded in `docs/providers/rtvi.md`; the exact shape of the `WebSocketTransport`
constructor options and of the `PipecatClient` options (`enableMic`, `callbacks`) was not fetched
(UNVERIFIED), so check them against the types of the installed package versions.

Any other key in the connection parameters makes the client throw
`Unrecognized connection parameter`. If the app starts the bot through an HTTP endpoint
(`startBotAndConnect({ endpoint })`), that endpoint has to return `{ wsUrl }` for the URL above; the
response shape is not verified (`docs/providers/rtvi.md`, UNVERIFIED 8).

## What the tests prove

`crates/brisvox-transport/tests/rtvi.rs` plays the JS client with a scripted browser that has its
own protobuf codec, so it checks the bytes of `docs/providers/rtvi.md` and not just a round trip:

- every WebSocket message is one binary frame with one `pipecat.Frame`; RTVI JSON is the string in
  `MessageFrame.data` (field 4, field 1); audio is `AudioRawFrame` (field 2; audio 3, sample_rate
  4, num_channels 5); the server never sends a text frame or `InterruptionFrame` (field 5);
- `client-ready` with the exact `RTVIMessage.clientReady()` shape is answered with `bot-ready`
  echoing the `id`, version `2.1.0`; version warnings follow Pipecat; the call's metadata carries
  both sample rates;
- 16 kHz client audio reaches the engine as PCM16; 24 kHz bot audio reaches the client as exactly
  one 960-byte (20 ms) message per engine frame; a frame larger than `max_audio_bytes` is split
  into whole PCM16 samples (448 bytes per message at a limit of 512) that reassemble to the
  original; bot audio at the wrong rate is dropped and reported to the engine exactly once per
  call (three bad frames, one error);
- every `RtviEvent` has the JSON of the doc, server events carry no `id` and payload-less events
  no `data`;
- barge-in: `bot-interrupted` and `bot-stopped-speaking` are sent, no audio of the old epoch (a
  distinct fill byte) reaches the client after `bot-interrupted`, stale audio sent after the clear
  never goes out, marks are reported as cut and the engine is told the lead is zero again;
- a control message above `max_control_bytes` (a 4 KiB `client-message` against a 1 KiB limit, so
  the websocket layer lets it through) closes the call with 1009, before and after `client-ready`;
- the token check rejects near misses (a prefix, the secret plus a character, plus a NUL, another
  case) with 403 and an undecodable value (`%zz`, a truncated or signed escape) with 401;
- the writer drops audio of an epoch older than the last clear on its own, even when it arrives
  behind `WireOut::Clear` (a crate-internal test, because the session layer normally drops it
  first), and the playout lead reported to the engine is positive after audio is sent, never above
  what was sent, and runs down to zero;
- two calls side by side stay apart: neither browser receives the other call's messages, audio or
  speaking events, and caller audio reaches only its own engine;
- text frames are rejected (`1003` before the handshake, a fault after), bad JSON, a missing `id`,
  a foreign `label` and unsupported types are answered as Pipecat answers them;
- `client-message` / `send-text` / `llm-function-call-result` reach the inbox, `dtmf` becomes DTMF
  events, `disconnect-bot` a hang-up; token, origin, path, call limit, timeouts and shutdown
  (1001 also for a browser that has not sent `client-ready` yet);
- the bounds: a client that stops reading ends the call after `write_timeout`, `RtviSender::send`
  times out on a full queue, a full inbox answers a `client-message` with "server busy" (and drops
  `send-text` silently), the reply queue keeps exactly its 16 replies and drops the rest, an engine that takes no calls or no
  input is told 1013, an oversized audio frame closes with 1009, a flood before `client-ready`
  closes with 1008, pings are sent and a quiet client that answers them stays connected.

## Manual check with a real JS client (cannot run in CI)

The automated tests play the client with a scripted browser written from the same document as the
server (`docs/providers/rtvi.md`), so they cannot catch a misreading of that document. The check
below is still pending and needs a person with a browser and Node. Versions: `@pipecat-ai/client-js`
1.13.x and `@pipecat-ai/websocket-transport` 1.7.x (the versions the protocol notes were taken
from); record the versions actually installed.

1. Start the server: `cargo run -p brisvox-transport --example rtvi_server -- 127.0.0.1:8765 /rtvi`
   (it logs `rtvi call started` / `rtvi call ended`).
2. In an empty directory: `npm init -y && npm i @pipecat-ai/client-js @pipecat-ai/websocket-transport
   vite`, put the TypeScript above in `main.ts` with `wsUrl: "ws://127.0.0.1:8765/rtvi"`,
   `recorderSampleRate: 16000`, `playerSampleRate: 24000`, run `npx vite` and open
   `http://localhost:5173` (a microphone needs a secure context; `localhost` counts). Open the
   browser console.
3. Expected, in this order:
   - `connect()` resolves and `onBotReady` fires (the `bot-ready` answer carries version `2.1.0`);
     no `Unrecognized connection parameter` and no console warning about the protocol version;
   - speaking into the microphone logs `onUserStartedSpeaking` / `onUserStoppedSpeaking`;
   - the page plays the server's two-tone beep at the right pitch, logs `onBotStartedSpeaking`
     when it begins and `onBotStoppedSpeaking` about when it ends, and logs a `bot-output` text;
   - `await client.sendClientRequest("ping", { n: 1 })` resolves with the same payload;
   - `client.sendText("hello")` produces no error;
   - `client.disconnect()` ends the call; the server logs `rtvi call ended`.
4. If the beep is pitched wrong or the page plays chipmunk audio, `playerSampleRate` does not match
   `RtviConfig::output_rate`. If the server logs a mismatched-input-rate error,
   `recorderSampleRate` does not match `RtviConfig::input_rate`.
5. Record the answers to the open points (each is UNVERIFIED in `docs/providers/rtvi.md` until
   then), in `docs/providers/rtvi.md` with the date and package versions:
   - does the client send a `Sec-WebSocket-Protocol` header (the server accepts none and would
     answer without a subprotocol)? Look at the upgrade request in the browser's network tab;
   - does the client survive a protobuf field it does not know (the server only sends fields 2 and
     4, so this is expected to be a non-issue; note any `Unknown frame kind`);
   - barge-in: speak while the bot beeps and note whether the already buffered beep stops
     (`WavStreamPlayer.interrupt()`) or plays out; the server sends `bot-interrupted`;
   - the default timeout of `sendClientRequest` (call it with a `client-message` type the example
     does not answer and time the rejection).
   Count the check as done only when steps 3 and 5 are recorded; any deviation becomes a fix.

The points that only a real client run can settle are listed in `docs/providers/rtvi.md`
(UNVERIFIED): whether the client demands a subprotocol, what it does with a protobuf field it does
not know, whether `WavStreamPlayer.interrupt()` drops buffered audio on barge-in, and the default
timeout of `sendClientRequest`.

## Known gaps

- Ordering: RTVI messages from the `RtviSender` are ordered among themselves only. They are not
  ordered against bot audio or the auto-derived `bot-started-speaking` / `bot-stopped-speaking`
  (separate queues, RTVI messages first), so an event can overtake the audio queued before it.
  Turn `auto_events` off and send the speaking events yourself if the UI needs a strict order
  (the engine's `RtviBridge` does exactly that).
- With the engine bridge: `send-text` with `audio_response: false` is answered with audio anyway
  (counted as `text_only_requests`), and the `llm-function-call-*` events are not sent (the
  engine runs its tools itself; a client's `llm-function-call-result` is counted as unsupported).
- Input rate: client audio at a rate other than `input_rate` is passed on at its own rate (no
  resampling); the first such frame is reported once as an engine error. Set the client's
  `recorderSampleRate` to the configured input rate.

- `TransportEvent` has no variant for `client-message`, `send-text` and
  `llm-function-call-result` (adding one would break every exhaustive match in the engine), so they
  travel on `RtviInbox`. A follow-up can fold them into the core's event enum.
- RTVI has no playback acknowledgement and no way to flush audio the browser already holds:
  marks and the lead are estimates (`rtvi::playout`), and a barge-in cannot cut the client's
  buffer. The core's real-time pacing keeps the tail short.
- No resampling: the bot's audio rate must equal `output_rate`.
- `raw-audio` / `raw-audio-batch` JSON messages (Python serializer carrier, unused by the JS
  client) are answered with `Unsupported type`; `ui-*` messages are ignored; `bot-transcription`
  (deprecated) is not sent.
