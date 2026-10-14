# Brisvox websocket protocol, version 1

Status: version 1. Code: `brisvox_transport::websocket` (`protocol.rs` is the executable
definition, `connection.rs` the server side, `client.rs` the reference client). Design and
bounds: `docs/ARCHITECTURE.md`, "WebSocket transport".

One websocket connection is one call. **Binary frames carry audio, text frames carry one JSON
object each** with a `type` field. The server speaks plain `ws://`; terminate TLS (`wss://`) in
front of it (a reverse proxy). The server does not look at the request path. The protocol has
no authentication of its own, but the upgrade request can be checked (
`docs/SECURITY.md`): with `BRISVOX_SESSION_SECRET` the client must present a session token your
backend minted (`aud` `brisvox-ws`, short-lived, single use), with `BRISVOX_WS_TOKEN` a static
one (development). The token travels in `Authorization: Bearer <token>`, in the WebSocket
subprotocol `brisvox-token.<token>` (a browser: `new WebSocket(url, ["brisvox",
"brisvox-token." + token])`; the server selects `brisvox`), or in `?token=<token>`. A missing
token is HTTP 401, an invalid, expired, reused or wrong-audience one 403, an oversize one (over
4 KiB) 413; every answer carries `Cache-Control: no-store`. The token's `sid`, `agent`, `max_s`
and `meta` reach the call's metadata as `session_id`, `session.agent`, `session.max_s` and
`session.meta.<key>` (the client's own `start.metadata` cannot set those keys; `max_s` is
reported, not enforced, on this listener). The server binary keeps this listener on loopback,
or off next to another listener, unless `BRISVOX_WS_ADDR` says otherwise, and **refuses a
non-loopback `BRISVOX_WS_ADDR` without `BRISVOX_SESSION_SECRET` or `BRISVOX_WS_TOKEN`** unless
`BRISVOX_INSECURE_LISTENERS=1` (`docs/OPERATIONS.md`).

**Also on the WebRTC signaling port.** With `brisvox-server --features webrtc`, the
signaling listener (`BRISVOX_WEBRTC_ADDR`) serves this same protocol on a WebSocket upgrade at
`BRISVOX_WEBRTC_WS_PATH` (default `/ws`), so a browser whose network cannot carry WebRTC media can
still call over the one HTTP(S) port. The connection and call code is this transport's; what differs is
the listener around it. The upgrade is judged like `POST /whip`: the signaling listener's token
(static `BRISVOX_WEBRTC_TOKEN` or a session token with `aud` `brisvox-webrtc`, in the same three
places as above), its `Origin` rule, its offer rate, per-address call cap and handshake pool, and
the call limit (HTTP 401, 403, 413, 429, 503 before the upgrade), and a session token's `max_s`
**is** enforced here (the call ends with `stop{ended}`), unlike on the standalone `BRISVOX_WS_ADDR` listener above, which only reports it. The call's metadata is as above with
`transport` `webrtc-ws` (the server's logs and metrics label the call the same way). The browser module `sdk/web/brisvox-call.js` (and with it the bundled
pages `/demo` and `examples/web-assistant/web/index.html`)
use it as their fallback with 24 kHz `pcm16`, 20 ms frames, an 80 ms playback buffer, and echo
`mark` once the audio before it has played.

## Audio
- Mono, in both directions, at the `sample_rate` and `encoding` the client gave in `start`:
  - `pcm16`: 16-bit signed little-endian samples. A frame must be whole samples (even length).
  - `mulaw`: G.711 mu-law, one byte per sample (telephony; typically 8000 Hz).
- Sample rates: 8000, 16000, 22050, 24000, 44100 or 48000 Hz.
- A frame may hold any amount of audio up to `max_audio_bytes` (default 64 KiB). Send 20 ms
  frames: that is the unit the engine paces and clears at. An empty binary frame is ignored.
- The server sends bot audio at real time (one 20 ms frame per 20 ms), already in the call's
  rate and encoding. The transport itself does not resample: audio at another rate than the
  call's is dropped, with one `output_rate_mismatch` notice per call. The engine never sends
  such audio: it reads the call's rate from the transport's `Started` event and its TTS stage
  resamples the provider's audio (24 kHz by default) to it in exact 20 ms frames, so any client
  rate works with any provider rate (an 8 kHz mu-law call with a 24 kHz TTS: 160-byte frames,
  about 7 us per frame in release; `crates/brisvox-engine/tests/ws_mulaw_call.rs`).
- Allocation: pcm16 frames pass without a copy in both directions; the mu-law conversion writes
  into a per-connection buffer allocated in 16 KiB chunks. A chunk serves 51 decoded frames (320
  bytes: 20 ms at 8 kHz) or 102 encoded frames (160 bytes), so about one allocation per 50
  frames from the client and one per 100 frames to it (unit-tested in `connection.rs`).

## Client to server

| `type` | fields | meaning |
|---|---|---|
| `start` | `version` (1), `sample_rate`, `encoding` (`pcm16` default, or `mulaw`), `call_id` (optional, at most 128 bytes), `metadata` (optional string map, at most 32 entries, keys up to 64 bytes, values up to 256 bytes) | Opens the call. Must be the first message. A client should send nothing else before it has `started`; the server does not enforce that: audio, `dtmf` and `mark` that arrive between `start` and `started` are accepted and passed on. The rate keys of the call's metadata belong to the server: a client-supplied `sample_rate` is overwritten and `input_sample_rate` / `output_sample_rate` are removed (the call has one symmetric rate). |
| `dtmf` | `digit`: one of `0-9 * # A-D` (`a-d` are accepted too and mean the same) | A key press. |
| `mark` | `id` (at most 256 bytes) | Echo of a server `mark`: everything the server sent before it has been played. |
| `stop` | | The client hangs up. |

Unknown fields are ignored. A text frame larger than `max_control_bytes` (default 16 KiB) is an
error.

## Server to client

| `type` | fields | meaning |
|---|---|---|
| `started` | `version`, `call_id`, `sample_rate`, `encoding` | The call is open. Bot audio can follow from here. |
| `clear` | `epoch` | Barge-in. Drop everything not yet played, including audio and marks buffered on the client; audio sent after this message belongs to the new epoch. Marks that were cut are never echoed (an echo of one is ignored anyway). |
| `mark` | `id` (at most 256 bytes) | Echo this id as a `mark` once all audio sent before it has played. The server does not send a mark with a longer id (non-fatal `mark_id_too_long` instead; the engine gets no `MarkPlayed` for it), because the client's echo of it would be refused. |
| `stop` | `reason`: `ended` (the engine hung up, or the client's own `stop` was processed) or `shutdown` (the server stops) | The call is over; the server closes the connection next. |
| `error` | `code`, `message`, `fatal` | A protocol fault. A fatal error is followed by a close frame; a non-fatal one is informational. |

`message` is for humans and never contains credentials or audio.

## Lifecycle
1. The client connects (HTTP upgrade). If the server is at `max_calls` (or, in
   `brisvox-server`, at `BRISVOX_MAX_SESSIONS`), the answer is HTTP 503 with `Retry-After: 1`,
   and no websocket exists. A `brisvox-server` whose inference pool is overloaded (`5`) or whose
   provider budget is exhausted (`2`) answers 503 with a longer `Retry-After` the same way. A
   `brisvox-server` that is draining for a shutdown answers HTTP 503
   without `Retry-After`: connect to another instance (`docs/OPERATIONS.md`).
2. The client sends `start` within `start_timeout` (default 5 s).
3. The server validates it and answers `started` (or a fatal `error`, then a close).
4. Audio and control flow both ways. The server pings every `ping_interval` (default 10 s); a
   standard client answers pongs automatically. Any frame from the client counts as life. No frame
   for `ping_timeout` (default 30 s) ends the call with `ping_timeout`.
5. The call ends when:
   - the client sends `stop`: the server answers `stop{ended}` and closes (engine: `PeerHangup`);
   - the client sends a close frame (engine: `PeerHangup`). The server answers with a close frame
     (RFC 6455 5.5.1), but it cannot send `stop` after it read the client's close frame, so a client
     that wants the `stop` message should send `stop` and wait for the server's close. `stop`
     followed at once by a close frame (a browser's `send(stop); close()`) works: the server holds
     back reading the close frame until it has sent `stop{ended}`, and the engine sees
     `PeerHangup`;
   - the engine hangs up: `stop{ended}`, close 1000;
   - the server shuts down: `stop{shutdown}`, close 1001;
   - the connection breaks without a close frame, an EOF or a reset (engine: `PeerDisconnected`,
     and no `error` event: a vanished peer is not a protocol violation, there is nobody to report
     it to);
   - a fatal error, a ping timeout or a stuck client (below).

## Errors and close codes
The close codes below are asserted by `tests/websocket.rs` (a client branches on them: 1013 means
retry later); 1000 is the engine's hang-up and 1001 the server's shutdown.

| `code` | fatal | close code | when |
|---|---|---|---|
| `invalid_json` | yes | 1002 | a text frame that is not a JSON object with a string `type` |
| `invalid_message` | yes | 1002 / 1008 | a known `type` with missing or invalid fields (bad digit, bad `start`, too long id, `start` not first) |
| `unknown_message` | no (yes before `start`) | | an unknown `type`; ignored once the call is open. More than 16 faults on one call end it with `too_many_errors` |
| `unsupported_version` | yes | 1008 | `start.version` is not 1 |
| `unsupported_sample_rate` | yes | 1008 | `start.sample_rate` is not a supported rate |
| `unexpected_start` | yes | 1008 | a second `start` |
| `audio_before_start` | yes | 1008 | a binary frame before `start` |
| `start_timeout` | yes | 1008 | no `start` in time |
| `bad_audio` | yes | 1008 | a pcm16 frame with an odd byte count |
| `frame_too_large` | yes | 1009 | a binary or text frame over its limit (or over the websocket layer's limit; that error may be lost to a connection reset) |
| `too_many_errors` | yes | 1008 | see `unknown_message` |
| `output_rate_mismatch` | no | | the engine produced audio at another rate than the call's; dropped, reported once per call |
| `mark_id_too_long` | no | | the engine asked for a mark with an id over 256 bytes; the mark is not sent |
| `server_busy` | yes | 1013 | the engine is not taking new calls |
| `overloaded` | yes | 1013 | the engine takes no input from the call for `send_timeout` |
| `ping_timeout` | yes | 1008 | see Lifecycle |

A client that does not take bot audio (its socket stays full for `write_timeout`, default 5 s) is
disconnected without a message, since nothing more can be written to it. Playout is real time, so
the server never buffers more than a few frames for a slow client. When the engine hangs up or the
server shuts down while the client is stuck, the blocked write is abandoned at once and the
goodbye gets at most `min(write_timeout, close_grace)`.

## Server configuration
`ServerConfig::validate` (called when the server starts) refuses: `ping_timeout` not longer than
`ping_interval`, `ping_interval` under 10 ms, a zero timeout or `close_grace`, `max_calls` or
`accept_queue` of 0, `max_audio_bytes` under 2, `max_control_bytes` under 512. The websocket layer
itself accepts frames up to the larger of `max_audio_bytes` and `max_control_bytes` (a frame above
that is refused as `frame_too_large` by the layer, with a close that may reach the client as a
reset when much of the frame is unread); the smaller limit is enforced by the connection.

## Marks and barge-in, from the client's side
A phone-like client keeps a playout buffer. It plays binary frames in order. When it receives
`mark`, it remembers the id and sends `mark{id}` back when the audio received before the mark has
finished playing. On `clear` it empties the buffer at once and forgets pending marks. This is what
the reference client does, and what makes `MarkPlayed` mean "the caller heard it" and what lets the
engine trim its LLM context to what was actually spoken.

## Reference client
`cargo run -p brisvox-transport --bin ws_client -- <URL> <INPUT.wav> <OUTPUT.wav> [options]`

Streams a mono 16-bit WAV (for instance `fixtures/en_two_utterances.wav`, 16 kHz) as real-time
20 ms frames, answers marks, and writes the bot audio it received to `OUTPUT.wav` (the
concatenation of what played; a `clear` removes the audio that had not played yet). After the
input ends it waits until the server has been silent for `--linger-ms` (default 2000)
(pending marks are echoed first, even if that takes longer), sends `stop` and waits for the server
to close.

Options: `--encoding pcm16|mulaw`, `--no-realtime` (send as fast as the server takes it; the
client keeps reading its socket between and during the sends, so a server that is already
producing audio is not stalled by it),
`--linger-ms N`, `--dtmf KEYS` (pressed right after `started`), `--meta KEY=VALUE` (repeatable).
Exit codes: 0 success, 1 the call failed, 2 bad usage or an unreadable or unwritable file. The call
failed (`ClientReport::failure`) when the server reported a fatal error, the connection broke
(a read or write error, or the stream ended without a close frame) before the server stopped the
call, the server closed the connection without `stop` before all input was sent, or
`max_duration` passed. The recording is written in every case, and the error is printed. A server
`stop` while input was still being sent (the bot hung up) is not a failure.

## Compatibility
The version is the integer in `start.version`/`started.version`. A change that an old peer cannot
ignore (a new required field, a changed meaning) bumps it. Adding message types and fields does
not: peers ignore unknown fields, and an unknown `type` is non-fatal.
