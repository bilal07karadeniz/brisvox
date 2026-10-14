import assert from "node:assert";
import { BrisvoxCall, BrisvoxCallError, WORKLET_SOURCE, connect } from "../brisvox-call.js";
import { advance, drive, ftest, makeEnv, record, response, until } from "./fakes.mjs";

const SERVER = "https://voice.example.com";
const connectD = (options) => drive(connect(options));

function make(env, extra = {}) {
  const call = new BrisvoxCall({ serverUrl: SERVER, platform: env.platform, ...extra });
  return { call, events: record(call) };
}

// Everything a call opened must be closed after hangup().
function assertCleanedUp(env) {
  for (const stream of env.streams) assert(stream.tracks.every((t) => t.stopped), "microphone tracks stopped");
  assert(env.contexts.every((c) => c.closed), "audio context closed");
  assert(env.peers.every((p) => p.closed), "peer connection closed");
  assert(env.sockets.every((s) => s.readyState === 3), "sockets closed");
  assert(env.audioEls.every((a) => a.srcObject === null), "audio element released");
  assert(env.workletNodes.every((n) => n.disconnected), "worklet nodes disconnected");
}

ftest("webrtc: connects with one WHIP request and hangs up with a DELETE", async () => {
  const env = makeEnv({ webrtc: "connect" });
  const { call, events } = make(env, { token: "dev-token" });
  const started = call.start();
  await until(() => env.peers.length === 1, "the peer connection");
  await drive(started);

  assert.strictEqual(call.state, "connected");
  assert.strictEqual(call.transport, "webrtc");
  assert.deepStrictEqual(events.states(), ["connecting", "connected"]);
  assert.deepStrictEqual(events.of("transport"), [{ type: "transport", transport: "webrtc", fallback: false }]);
  assert.strictEqual(env.sockets.length, 0, "no WebSocket when WebRTC works");

  // The microphone is asked for with echo cancellation and noise suppression.
  assert.deepStrictEqual(env.constraints[0], {
    audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true, channelCount: 1 },
    video: false,
  });
  // The microphone track goes to the peer; the offer goes to /whip with the bearer token.
  assert.strictEqual(env.peers[0].tracks.length, 1);
  const post = env.methods("POST")[0];
  assert.strictEqual(post.url, `${SERVER}/whip`);
  assert.strictEqual(post.headers.Authorization, "Bearer dev-token");
  assert.strictEqual(post.headers["Content-Type"], "application/sdp");
  assert.strictEqual(post.body, "v=0 fake offer");
  assert.strictEqual(env.requests[0].url, `${SERVER}/whip/ice`, "the ICE servers are asked for first");
  assert.strictEqual(env.peers[0].remote.sdp, "v=0 fake answer");

  // The assistant's audio plays through an audio element.
  env.peers[0].remoteTrack();
  assert.strictEqual(env.audioEls.length, 1);
  assert(env.audioEls[0].srcObject, "the remote stream is attached");
  // Levels come from analysers.
  env.analysers.forEach((a) => { a.level = 192; });
  const levels = call.getLevels();
  assert(levels.mic > 0.5 && levels.bot > 0.5, JSON.stringify(levels));

  await call.hangup();
  assert.strictEqual(call.state, "ended");
  assert.deepStrictEqual(events.states(), ["connecting", "connected", "ended"]);
  const del = env.methods("DELETE");
  assert.strictEqual(del.length, 1, "exactly one DELETE");
  assert.strictEqual(del[0].url, `${SERVER}/whip/session1`);
  assert.strictEqual(del[0].headers.Authorization, "Bearer dev-token");
  assert.strictEqual(del[0].keepalive, true);
  assertCleanedUp(env);

  await call.hangup();    // twice is fine
  assert.strictEqual(env.methods("DELETE").length, 1);
});

ftest("webrtc: the TURN servers from the server reach the peer connection", async () => {
  const iceServers = [{ urls: "turn:turn.example.com", username: "u", credential: "c" }];
  const env = makeEnv({ webrtc: "connect", iceServers });
  const call = await connectD({ serverUrl: SERVER, platform: env.platform });
  assert.deepStrictEqual(env.peers[0].config.iceServers, iceServers);
  await call.hangup();
});

ftest("webrtc times out: the websocket takes over with a fresh token", async () => {
  const env = makeEnv({ webrtc: "hang" });
  const tokens = [];
  const tokenProvider = async () => { tokens.push(`t${tokens.length + 1}`); return tokens[tokens.length - 1]; };
  const { call, events } = make(env, { tokenProvider, fallbackAfterMs: 40 });
  await drive(call.start());

  assert.strictEqual(call.transport, "websocket");
  assert.strictEqual(call.state, "connected");
  assert.deepStrictEqual(tokens, ["t1", "t2"], "one token per attempt");
  // The WebRTC attempt used t1 (offer and DELETE); the WebSocket presents t2.
  assert.strictEqual(env.methods("POST")[0].headers.Authorization, "Bearer t1");
  assert.strictEqual(env.methods("DELETE")[0].headers.Authorization, "Bearer t1");
  assert.strictEqual(env.methods("DELETE")[0].url, `${SERVER}/whip/session1`, "the WebRTC session is freed first");
  assert(env.peers[0].closed);
  const ws = env.ws();
  assert.strictEqual(ws.url, "wss://voice.example.com/ws");
  assert.deepStrictEqual(ws.protocols, ["brisvox", "brisvox-token.t2"]);

  const fallback = events.of("fallback");
  assert.strictEqual(fallback.length, 1);
  assert.match(fallback[0].reason, /did not connect within 0\.04 s/);
  const transport = events.of("transport");
  assert.strictEqual(transport.length, 1);
  assert.strictEqual(transport[0].transport, "websocket");
  assert.strictEqual(transport[0].fallback, true);

  // The start message follows the protocol.
  const start = ws.json()[0];
  assert.deepStrictEqual(
    { ...start, metadata: undefined },
    { type: "start", version: 1, sample_rate: 24000, encoding: "pcm16", metadata: undefined },
  );
  assert.strictEqual(start.metadata.client, "brisvox-web");

  await call.hangup();
  assert.deepStrictEqual(ws.json().at(-1), { type: "stop" });
  assert.strictEqual(ws.closedWith, 1000);
  assertCleanedUp(env);
});

ftest("webrtc fails outright: the websocket takes over at once", async () => {
  const env = makeEnv({ webrtc: "fail" });
  const { call, events } = make(env, { token: "v1.abc.def", fallbackAfterMs: 5000 });
  await drive(call.start());
  assert.strictEqual(call.transport, "websocket");
  assert.match(events.of("fallback")[0].reason, /could not connect/);
  assert.deepStrictEqual(env.ws().protocols, ["brisvox", "brisvox-token.v1.abc.def"]);
  await call.hangup();
});

ftest("no RTCPeerConnection: straight to the websocket", async () => {
  const env = makeEnv({ noRtc: true });
  const { call, events } = make(env);
  await drive(call.start());
  assert.strictEqual(call.transport, "websocket");
  assert.match(events.of("fallback")[0].reason, /RTCPeerConnection is not available/);
  assert.strictEqual(env.requests.length, 0, "no signaling request");
  await call.hangup();
});

ftest("the signaling request failing at the network level falls back", async () => {
  const env = makeEnv({
    fetch: (url, init) => { if (init.method === "POST") throw new TypeError("network down"); },
  });
  const { call, events } = make(env);
  await drive(call.start());
  assert.strictEqual(call.transport, "websocket");
  assert.match(events.of("fallback")[0].reason, /the signaling request failed: network down/);
  await call.hangup();
});

ftest("transport websocket: no WebRTC at all", async () => {
  const env = makeEnv();
  const { call, events } = make(env, { transport: "websocket", serverUrl: "http://localhost:8088" });
  await drive(call.start());
  assert.strictEqual(call.transport, "websocket");
  assert.strictEqual(env.peers.length, 0);
  assert.strictEqual(env.requests.length, 0);
  assert.strictEqual(env.ws().url, "ws://localhost:8088/ws");
  assert.deepStrictEqual(env.ws().protocols, ["brisvox"], "no token, no token subprotocol");
  assert.strictEqual(events.of("fallback").length, 0);
  assert.strictEqual(events.of("transport")[0].fallback, false);
  await call.hangup();
  assertCleanedUp(env);
});

ftest("a token a subprotocol cannot carry goes in the query", async () => {
  const env = makeEnv();
  const call = await connectD({ serverUrl: SERVER, transport: "websocket", token: "a b/c", platform: env.platform });
  assert.strictEqual(env.ws().url, "wss://voice.example.com/ws?token=a%20b%2Fc");
  assert.deepStrictEqual(env.ws().protocols, ["brisvox"]);
  await call.hangup();
});

ftest("transport webrtc never falls back", async () => {
  const env = makeEnv({ webrtc: "fail" });
  const { call, events } = make(env, { transport: "webrtc" });
  await assert.rejects(drive(call.start()), (error) => error instanceof BrisvoxCallError && error.code === "webrtc");
  assert.strictEqual(env.sockets.length, 0);
  assert.strictEqual(call.state, "failed");
  assert.strictEqual(events.of("error").length, 1);
  assertCleanedUp(env);
});

ftest("transport webrtc: a connection that never completes times out", async () => {
  const env = makeEnv({ webrtc: "hang" });
  const { call } = make(env, { transport: "webrtc", connectTimeoutMs: 30 });
  await assert.rejects(drive(call.start()), (error) => error.code === "timeout");
  assert.strictEqual(call.state, "failed");
  assertCleanedUp(env);
});

ftest("an HTTP answer from the signaling endpoint is shown, not answered with a second transport", async () => {
  const env = makeEnv({ whip: 401 });
  const { call, events } = make(env, { token: "bad" });
  await assert.rejects(drive(call.start()), (error) => error.code === "signaling" && error.status === 401);
  assert.strictEqual(env.sockets.length, 0, "no fallback after a 401");
  assert.strictEqual(call.state, "failed");
  const [error] = events.of("error");
  assert.strictEqual(error.code, "signaling");
  assert.match(error.message, /server answered 401/);
  assert.strictEqual(events.at(-1).type, "state");
  assert.strictEqual(events.at(-1).state, "failed");
  assertCleanedUp(env);
});

ftest("barge-in: clear empties the playback buffer, marks are echoed once played", async () => {
  const env = makeEnv();
  const { call, events } = make(env, { transport: "websocket" });
  await drive(call.start());
  const ws = env.ws();
  const playback = env.workletNodes.find((n) => n.name === "brisvox-playback");
  const capture = env.workletNodes.find((n) => n.name === "brisvox-capture");

  // Bot audio goes to the playback worklet; the microphone goes to the socket.
  ws.serverAudio(960);
  assert.strictEqual(playback.posted.length, 1);
  assert.strictEqual(playback.posted[0].message.pcm.byteLength, 960);
  const frame = new Int16Array(480).buffer;
  capture.port.onmessage({ data: frame });
  assert.deepStrictEqual(ws.binary(), [frame]);

  // A mark is handed to the worklet and echoed when it reports the audio before it as played.
  ws.serverSend({ type: "mark", id: "m1" });
  assert.deepStrictEqual(playback.posted.at(-1).message, { mark: "m1" });
  playback.port.onmessage({ data: { markPlayed: "m1" } });
  assert.deepStrictEqual(ws.json().at(-1), { type: "mark", id: "m1" });

  // Barge-in.
  ws.serverSend({ type: "clear", epoch: 3 });
  assert.deepStrictEqual(playback.posted.at(-1).message, { clear: true });
  assert.deepStrictEqual(events.of("clear"), [{ type: "clear", epoch: 3 }]);
  assert.deepStrictEqual(events.of("mark"), [{ type: "mark", id: "m1" }]);

  await call.hangup();
});

ftest("websocket: audio is bounded in both directions", async () => {
  const env = makeEnv();
  const call = await connectD({ serverUrl: SERVER, transport: "websocket", platform: env.platform });
  const ws = env.ws();
  const playback = env.workletNodes.find((n) => n.name === "brisvox-playback");
  const capture = env.workletNodes.find((n) => n.name === "brisvox-capture");
  // Microphone frames are dropped while the socket is far behind.
  ws.bufferedAmount = 200 * 1024;
  capture.port.onmessage({ data: new Int16Array(480).buffer });
  assert.strictEqual(ws.binary().length, 0);
  ws.bufferedAmount = 0;
  capture.port.onmessage({ data: new Int16Array(480).buffer });
  assert.strictEqual(ws.binary().length, 1);
  // Bot audio is no longer posted while the worklet lags behind by more than 96 KiB.
  for (let i = 0; i < 120; i++) ws.serverAudio(960);
  assert(playback.posted.length < 120, `posted ${playback.posted.length}`);
  assert(playback.posted.length >= 100);
  playback.port.onmessage({ data: { handled: 120 * 960 } });
  const before = playback.posted.length;
  ws.serverAudio(960);
  assert.strictEqual(playback.posted.length, before + 1, "playback resumes once the worklet catches up");
  await call.hangup();
});

ftest("microphone frames before started are not sent", async () => {
  const env = makeEnv({ autoStart: false });
  const { call } = make(env, { transport: "websocket" });
  const started = call.start();
  await until(() => env.sockets.length === 1 && env.ws().readyState === 1, "the socket to open");
  const capture = env.workletNodes.find((n) => n.name === "brisvox-capture");
  capture.port.onmessage({ data: new Int16Array(480).buffer });
  assert.strictEqual(env.ws().binary().length, 0);
  env.ws().serverSend({ type: "started", version: 1, sample_rate: 24000, encoding: "pcm16" });
  await drive(started);
  await call.hangup();
});

ftest("error: a server that picks another format fails the call", async () => {
  const env = makeEnv({ autoStart: false });
  const { call } = make(env, { transport: "websocket" });
  const started = call.start();
  await until(() => env.sockets.length === 1 && env.ws().readyState === 1, "the socket to open");
  env.ws().serverSend({ type: "started", version: 1, sample_rate: 8000, encoding: "mulaw" });
  await assert.rejects(drive(started), /not 24000 Hz pcm16/);
  assert.strictEqual(call.state, "failed");
  assertCleanedUp(env);
});

ftest("error: a refused WebSocket upgrade", async () => {
  const env = makeEnv({ autoStart: false, autoOpen: false });
  const { call, events } = make(env, { transport: "websocket", token: "v1.used.token" });
  const started = call.start();
  await until(() => env.sockets.length === 1, "the socket");
  env.ws().serverClose(1006);
  await assert.rejects(drive(started), (error) => error.code === "websocket" && /close code 1006/.test(error.message));
  assert.strictEqual(events.of("error")[0].code, "websocket");
  assert.strictEqual(call.state, "failed");
  assertCleanedUp(env);
});

ftest("error: a refused upgrade after a WebRTC attempt with a fixed session token explains the likely cause", async () => {
  const env = makeEnv({ webrtc: "fail", autoOpen: false });
  const { call } = make(env, { token: "v1.used.token" });
  const started = call.start();
  await until(() => env.sockets.length === 1, "the socket");
  env.ws().serverClose(1006);
  await assert.rejects(drive(started), /session token was already used/);
});

ftest("error: the token provider failing fails the call before anything is opened", async () => {
  const env = makeEnv();
  const { call, events } = make(env, { tokenProvider: async () => { throw new Error("backend down"); } });
  await assert.rejects(drive(call.start()), (error) => error.code === "token" && /backend down/.test(error.message));
  assert.strictEqual(env.peers.length, 0);
  assert.strictEqual(events.of("error")[0].code, "token");
  assertCleanedUp(env);
});

ftest("error: the token provider failing on the fallback attempt", async () => {
  const env = makeEnv({ webrtc: "fail" });
  let n = 0;
  const tokenProvider = async () => { if (++n > 1) throw new Error("second mint failed"); return "t1"; };
  const { call } = make(env, { tokenProvider });
  await assert.rejects(drive(call.start()), (error) => error.code === "token");
  assert.strictEqual(env.sockets.length, 0);
  assertCleanedUp(env);
});

ftest("error: microphone permission denied", async () => {
  const denied = Object.assign(new Error("denied"), { name: "NotAllowedError" });
  const env = makeEnv({ micError: denied });
  const { call, events } = make(env);
  await assert.rejects(drive(call.start()), (error) => error.code === "microphone" && /permission/.test(error.message));
  assert.strictEqual(call.state, "failed");
  assert.strictEqual(events.of("error").length, 1);
  assertCleanedUp(env);
});

ftest("error: no getUserMedia, bad options", async () => {
  const env = makeEnv();
  const noMic = new BrisvoxCall({ serverUrl: SERVER, platform: { ...env.platform, getUserMedia: undefined } });
  // (in Node there is no navigator.mediaDevices either)
  await assert.rejects(drive(noMic.start()), (error) => error.code === "unsupported" || error.code === "microphone");
  assert.throws(() => new BrisvoxCall({}), /serverUrl is required/);
  assert.throws(() => new BrisvoxCall({ serverUrl: SERVER, transport: "carrier-pigeon" }), /transport must be/);
  assert.throws(() => new BrisvoxCall({ serverUrl: SERVER, token: "a", tokenProvider: () => "b" }), /not both/);
  const bad = new BrisvoxCall({ serverUrl: "ftp://x", platform: env.platform });
  await assert.rejects(drive(bad.start()), /http:\/\/ or https:\/\//);
  const twice = new BrisvoxCall({ serverUrl: SERVER, transport: "websocket", platform: env.platform });
  await drive(twice.start());
  await assert.rejects(drive(twice.start()), /already called/);
  await twice.hangup();
});

ftest("server events: notices, stop, unknown messages, and the call ending by itself", async () => {
  const env = makeEnv();
  const { call, events } = make(env, { transport: "websocket" });
  await drive(call.start());
  const ws = env.ws();
  ws.serverSend({ type: "error", code: "mark_id_too_long", message: "too long", fatal: false });
  ws.serverSend({ type: "transcript", role: "user", text: "hello" });
  ws.serverSend("not json at all");
  assert.deepStrictEqual(events.of("notice"), [{ type: "notice", code: "mark_id_too_long", message: "too long", fatal: false }]);
  assert.deepStrictEqual(events.of("message"), [{ type: "message", message: { type: "transcript", role: "user", text: "hello" } }]);
  assert.strictEqual(call.state, "connected");

  ws.serverSend({ type: "stop", reason: "ended" });
  await until(() => call.state === "ended", "the call to end");
  assert.deepStrictEqual(events.of("stop"), [{ type: "stop", reason: "ended" }]);
  assert.match(events.of("state").at(-1).reason, /assistant ended the call/);
  assertCleanedUp(env);
});

ftest("a connection lost mid-call ends the call and cleans up", async () => {
  const env = makeEnv();
  const { call, events } = make(env, { transport: "websocket" });
  await drive(call.start());
  env.ws().serverClose(1006);
  await until(() => call.state === "ended", "the call to end");
  assert.match(events.of("state").at(-1).reason, /connection closed \(1006\)/);
  assertCleanedUp(env);
});

ftest("webrtc: media lost after connecting fails the call, a hiccup does not", async () => {
  const env = makeEnv({ webrtc: "connect" });
  const { call, events } = make(env);
  await drive(call.start());
  const peer = env.peers[0];
  peer.setConnectionState("disconnected");
  assert.strictEqual(call.state, "connecting");
  peer.setConnectionState("connected");
  assert.strictEqual(call.state, "connected");
  assert.strictEqual(events.of("transport").length, 1, "the transport is announced once");
  peer.setConnectionState("failed");
  await until(() => call.state === "failed", "the call to fail");
  assert.strictEqual(events.of("error")[0].code, "connection");
  assert.strictEqual(env.methods("DELETE").length, 1);
  assertCleanedUp(env);
});

ftest("hanging up while connecting aborts start() and releases everything", async () => {
  const env = makeEnv({ webrtc: "hang" });
  const { call, events } = make(env, { fallbackAfterMs: 5000 });
  const started = call.start();
  await until(() => env.peers.length === 1 && env.requests.some((r) => r.method === "POST"), "the offer");
  await call.hangup();
  await assert.rejects(drive(started), (error) => error.name === "AbortError");
  assert.strictEqual(call.state, "ended");
  assert.strictEqual(env.sockets.length, 0, "the pending fallback never opens a socket");
  assert.strictEqual(env.methods("DELETE").length, 1);
  assert.strictEqual(events.of("error").length, 0, "hanging up is not an error");
  assertCleanedUp(env);
  await advance(20);
  assert.strictEqual(env.sockets.length, 0);
});

ftest("hanging up while the websocket is opening", async () => {
  const env = makeEnv({ autoStart: false, autoOpen: false });
  const { call } = make(env, { transport: "websocket" });
  const started = call.start();
  await until(() => env.sockets.length === 1, "the socket");
  await call.hangup();
  await assert.rejects(drive(started), (error) => error.name === "AbortError");
  assertCleanedUp(env);
});

ftest("hangup before start and mute", async () => {
  const env = makeEnv();
  const idle = new BrisvoxCall({ serverUrl: SERVER, platform: env.platform });
  await idle.hangup();
  assert.strictEqual(idle.state, "idle");

  const { call, events } = make(env, { transport: "websocket" });
  await drive(call.start());
  call.setMuted(true);
  assert.strictEqual(call.muted, true);
  assert(env.streams[0].tracks.every((t) => t.enabled === false));
  call.setMuted(false);
  assert(env.streams[0].tracks.every((t) => t.enabled === true));
  assert.deepStrictEqual(events.of("muted").map((e) => e.muted), [true, false]);
  await call.hangup();
});

ftest("a failing listener does not break the call", async () => {
  const env = makeEnv();
  const seen = [];
  const call = new BrisvoxCall({
    serverUrl: SERVER,
    transport: "websocket",
    platform: env.platform,
    onEvent: (e) => { seen.push(e.type); throw new Error("listener bug"); },
  });
  const original = console.error;
  console.error = () => {};
  try {
    await drive(call.start());
    await call.hangup();
  } finally {
    console.error = original;
  }
  assert(seen.includes("state"));
});

ftest("custom paths and a custom worklet URL", async () => {
  const env = makeEnv();
  const call = await connectD({
    serverUrl: "https://example.com/base/",
    transport: "websocket",
    wsPath: "/voice/socket",
    workletUrl: "/static/worklet.js",
    metadata: { page: "pricing" },
    platform: env.platform,
  });
  assert.strictEqual(env.ws().url, "wss://example.com/voice/socket");
  assert.deepStrictEqual(env.contexts[0].modules, ["/static/worklet.js"]);
  assert.strictEqual(env.ws().json()[0].metadata.page, "pricing");
  await call.hangup();

  // No WebSocket path: the fallback is off and WebRTC's failure is final.
  const env2 = makeEnv({ webrtc: "fail" });
  const { call: c2 } = make(env2, { wsPath: null });
  await assert.rejects(drive(c2.start()), (error) => error.code === "webrtc");
  assert.strictEqual(env2.sockets.length, 0);
});

ftest("the worklet module is added once per context, from a Blob URL by default", async () => {
  const env = makeEnv();
  const call = await connectD({ serverUrl: SERVER, transport: "websocket", platform: env.platform });
  assert.strictEqual(env.contexts[0].modules.length, 1);
  assert.match(env.contexts[0].modules[0], /^blob:/);
  await call.hangup();
});

// ---- review fixes ----------------------------------------------------------------

ftest("hangup while the microphone prompt is open: the late stream is stopped, nothing leaks", async () => {
  const env = makeEnv({ holdMic: true });
  const { call, events } = make(env);
  const started = call.start();
  await until(() => env.constraints.length === 1, "getUserMedia to be called");
  assert.strictEqual(env.contexts.length, 1, "the AudioContext is made at the click");
  await call.hangup();
  assert(env.contexts[0].closed, "the context is closed by the hangup");
  env.releaseMic();                                       // the user answers the prompt afterwards
  await assert.rejects(drive(started), (error) => error.name === "AbortError");
  assert.strictEqual(env.streams.length, 1);
  assert(env.streams[0].tracks.every((t) => t.stopped), "the late microphone stream is stopped");
  assert.strictEqual(env.peers.length, 0, "no connection is attempted");
  assert.strictEqual(env.requests.length, 0, "no signaling request");
  assert.strictEqual(call.state, "ended");
  assert.strictEqual(events.of("error").length, 0);
  assertCleanedUp(env);
});

ftest("hangup before start(): start() rejects with AbortError and opens nothing", async () => {
  const env = makeEnv();
  const call = new BrisvoxCall({ serverUrl: SERVER, platform: env.platform });
  await call.hangup();
  await assert.rejects(drive(call.start()), (error) => error.name === "AbortError");
  assert.strictEqual(env.contexts.length, 0, "no AudioContext");
  assert.strictEqual(env.constraints.length, 0, "the microphone is never asked for");
  assert.strictEqual(env.requests.length, 0);
  assert.strictEqual(call.state, "ended");
});

ftest("hangup while the WHIP POST is pending: the session the server then creates is deleted", async () => {
  let answer;
  const env = makeEnv({
    fetch: (url, init) => (init.method === "POST" ? new Promise((resolve) => { answer = resolve; }) : undefined),
  });
  const { call, events } = make(env, { token: "tok" });
  const started = call.start();
  await until(() => env.methods("POST").length === 1, "the POST");
  await call.hangup();
  assert.strictEqual(env.methods("DELETE").length, 0, "no session URL is known yet");
  answer(response(201, "v=0 fake answer", { Location: "/whip/session1" }));
  await assert.rejects(drive(started), (error) => error.name === "AbortError");
  const del = env.methods("DELETE");
  assert.strictEqual(del.length, 1, "the late session is deleted exactly once");
  assert.strictEqual(del[0].url, `${SERVER}/whip/session1`);
  assert.strictEqual(del[0].headers.Authorization, "Bearer tok", "the DELETE gives the single-use token back");
  assert.strictEqual(env.peers[0].remote, undefined, "the answer is never applied");
  assert.strictEqual(call.state, "ended");
  assert.strictEqual(events.of("error").length, 0);
  assertCleanedUp(env);
});

// Wires the real playback processor (WORKLET_SOURCE) behind a fake worklet node, both directions.
function realPlayback(node) {
  const processors = {};
  class AudioWorkletProcessor {
    constructor() { this.port = { onmessage: null, postMessage: (m) => { if (node.port.onmessage) node.port.onmessage({ data: m }); } }; }
  }
  new Function("AudioWorkletProcessor", "registerProcessor", "sampleRate", WORKLET_SOURCE)(
    AudioWorkletProcessor, (name, cls) => { processors[name] = cls; }, 48000);
  const processor = new processors["brisvox-playback"]({ processorOptions: { rate: 24000, jitterMs: 80, maxBufferMs: 400 } });
  node.port.postMessage = (message) => { node.posted.push({ message }); processor.port.onmessage({ data: message }); };
  return processor;
}

ftest("playback backlog accounting never wedges, even with big (20 KiB) frames", async () => {
  const env = makeEnv();
  const { call, events } = make(env, { transport: "websocket" });
  await drive(call.start());
  const playback = env.workletNodes.find((n) => n.name === "brisvox-playback");
  realPlayback(playback);
  const ws = env.ws();
  for (let i = 0; i < 60; i++) ws.serverAudio(20 * 1024);
  assert.strictEqual(playback.posted.length, 60, "every frame reaches the worklet: its reports keep the backlog near zero");
  await call.hangup();
});
