# @brisvox/web

The browser side of a Brisvox voice call, as **one dependency-free ES module**
(`brisvox-call.js`, with types in `brisvox-call.d.ts`). Put a "Talk to us" button on a website
without writing any audio code:

* **WebRTC first**: one WHIP request, Opus both ways, the browser's own echo cancellation, noise
  suppression and gain control on the microphone.
* **WebSocket fallback, by itself**: when WebRTC media cannot get through (a host with one HTTP
  port, a network that passes only HTTPS, a blocked UDP port), the module switches to the
  generic WebSocket audio protocol on the same port after 4 seconds
  (`docs/WEBSOCKET_PROTOCOL.md`). The microphone is resampled to 24 kHz PCM16 in an AudioWorklet
  and sent in 20 ms frames; the assistant's audio plays through a second worklet with an 80 ms
  jitter buffer (at most 400 ms, the oldest audio dropped beyond that). When the assistant is
  interrupted the server sends `clear` and the buffer is emptied at once, so the caller never
  hears the cut-off sentence finish.
* **One-port hosts: say so.** On a host that cannot receive UDP (a one-port platform such as Railway or Render, a plain
  HTTPS-only proxy) WebRTC never connects. Pass `transport: "websocket"` and the module goes
  straight to `/ws`: no 4 second wait for a WebRTC attempt that cannot succeed and no second
  session token. (`transport: "auto"`, the default, is for pages that must also work where UDP
  is open.)
* **Clean hang-up**: `hangup()` deletes the WHIP session, closes the sockets, stops the
  microphone tracks and closes the audio context. Closing the tab does the same.
* **Session tokens that work**: a token is single use, so the module asks your `tokenProvider`
  for a new one for every connection attempt (WebRTC, then the WebSocket).

The module is also what the server's built-in `/demo` page and `examples/web-assistant/web/index.html`
run, so those pages are a worked example of every option below.

Not on npm yet: copy `brisvox-call.js` and `brisvox-call.d.ts` into your project (they are two
plain files), or depend on the folder (`"@brisvox/web": "file:../brisvox/sdk/web"`).

## Plain HTML

Serve `brisvox-call.js` next to your page. The server URL is the signaling port of
`brisvox-server` (`BRISVOX_WEBRTC_ADDR`: WebRTC at `/whip`, the WebSocket fallback at `/ws`), and
your backend hands out the token (see "Tokens" below).

```html
<button id="talk">Talk to us</button>
<span id="status"></span>

<script type="module">
  import { BrisvoxCall } from "/brisvox-call.js";

  const talk = document.getElementById("talk");
  const status = document.getElementById("status");
  let call = null;

  talk.addEventListener("click", async () => {
    if (call) { await call.hangup(); return; }     // second click: hang up

    call = new BrisvoxCall({
      serverUrl: "https://voice.example.com",
      // Called for every attempt: a session token is single use.
      tokenProvider: async () => {
        const res = await fetch("/api/voice-token", { method: "POST" });
        if (!res.ok) throw new Error(`token request failed (${res.status})`);
        return (await res.json()).token;
      },
      onEvent(event) {
        if (event.type === "state") {
          status.textContent = event.state === "connected" ? `Talking (${event.transport})` : event.state;
          if (event.state === "ended" || event.state === "failed") { call = null; talk.textContent = "Talk to us"; }
          if (event.state === "connected") talk.textContent = "Hang up";
        }
        if (event.type === "error") console.warn(event.message);
      },
    });
    try {
      await call.start();            // asks for the microphone, then connects
    } catch (error) {
      // already reported through onEvent ("error", then state "failed")
    }
  });
</script>
```

Call `start()` from a click handler: browsers only start audio after a user gesture, and the
module creates its audio context at that moment. The page must be on `https://` (or
`http://localhost`) for the microphone, and an `https://` page may only call an `https://` server.
If the page and the server have different origins, set `BRISVOX_WEBRTC_CORS_ORIGIN` on the server
to the page's origin (`docs/DEPLOY.md`).

## Next.js (App Router)

Two files: a route handler that mints a token for the visitor, and a client component with the
button. Put the module in your project, for example `app/lib/brisvox-call.js` and
`app/lib/brisvox-call.d.ts` (or install the folder as a dependency and import `@brisvox/web`).

`app/api/voice-token/route.ts` (server side: the secret never reaches the browser):

```ts
import { createHmac, randomBytes } from "node:crypto";

// The same secret as BRISVOX_SESSION_SECRET on the Brisvox server (at least 32 bytes). A short
// secret can be guessed offline from one token, so refuse to start with one.
const SECRET = process.env.BRISVOX_SESSION_SECRET ?? "";
if (Buffer.byteLength(SECRET) < 32) {
  throw new Error("BRISVOX_SESSION_SECRET must be at least 32 bytes");
}

// The pages that may ask for a token: your own origin(s), exactly.
const ALLOWED_ORIGINS = new Set((process.env.VOICE_ALLOWED_ORIGINS ?? "https://www.example.com").split(","));

// A minimal per-IP limiter: 5 tokens a minute. It lives in this process's memory, so it is per
// server instance and starts over on a restart: enough to stop a loop in a browser tab, not a
// determined attacker. For a real limit use your platform's (Upstash, Redis, your CDN's rules).
const hits = new Map<string, number[]>();
function limited(ip: string, limit = 5, windowMs = 60_000): boolean {
  const now = Date.now();
  const recent = (hits.get(ip) ?? []).filter((t) => now - t < windowMs);
  if (recent.length >= limit) { hits.set(ip, recent); return true; }
  recent.push(now);
  hits.set(ip, recent);
  if (hits.size > 10_000) for (const [key, times] of hits) if (times.every((t) => now - t >= windowMs)) hits.delete(key);
  return false;
}

// A session token: `v1.<payload>.<signature>`, valid for one call, for a minute.
// (sdk/typescript exports the same thing as `mintSessionToken`.)
function mint(audience: string, ttlSeconds: number, claims: object = {}): string {
  const payload = Buffer.from(JSON.stringify({
    aud: audience,
    exp: Math.floor(Date.now() / 1000) + ttlSeconds,
    sid: randomBytes(16).toString("base64url"),
    ...claims,
  }));
  const signature = createHmac("sha256", SECRET).update(payload).digest();
  return `v1.${payload.toString("base64url")}.${signature.toString("base64url")}`;
}

export async function POST(request: Request) {
  // Every token you hand out is a call you pay for: put your own login check here too (the checks
  // below only keep other websites and loops away; a script can fake any header).
  // 1. Only your own pages: the browser always sends Origin on a cross-origin or POST request.
  const origin = request.headers.get("origin");
  if (!origin || !ALLOWED_ORIGINS.has(origin)) {
    return new Response("forbidden", { status: 403 });
  }
  // 2. Not too often from one address (behind a proxy, x-forwarded-for is the client's address
  //    only when YOUR proxy sets it; do not trust it on a server that is reachable directly).
  const ip = request.headers.get("x-forwarded-for")?.split(",")[0]?.trim() || "unknown";
  if (limited(ip)) {
    return new Response("too many calls", { status: 429, headers: { "Retry-After": "60" } });
  }
  const token = mint("brisvox-webrtc", 60, { max_s: 600 });
  return Response.json({ token }, { headers: { "Cache-Control": "no-store" } });
}
```

`app/components/TalkButton.tsx` (client side):

```tsx
"use client";

import { useEffect, useRef, useState } from "react";
import { BrisvoxCall, type CallEvent, type CallState } from "../lib/brisvox-call.js";

export default function TalkButton() {
  const callRef = useRef<BrisvoxCall | null>(null);
  const [state, setState] = useState<CallState>("idle");
  const [transport, setTransport] = useState<string | null>(null);
  const [message, setMessage] = useState("");

  // Leaving the page hangs up (the module also does it on `pagehide`).
  useEffect(() => () => { void callRef.current?.hangup(); }, []);

  async function toggle() {
    if (callRef.current) {
      await callRef.current.hangup();
      return;
    }
    const call = new BrisvoxCall({
      serverUrl: process.env.NEXT_PUBLIC_BRISVOX_URL!,           // https://voice.example.com
      tokenProvider: async () => {
        const res = await fetch("/api/voice-token", { method: "POST" });
        if (!res.ok) throw new Error(`token request failed (${res.status})`);
        return (await res.json()).token as string;
      },
      onEvent(event: CallEvent) {
        if (event.type === "state") {
          setState(event.state);
          setTransport(event.state === "connected" ? event.transport ?? null : null);
          if (event.state === "ended" || event.state === "failed") callRef.current = null;
        }
        if (event.type === "error") setMessage(event.message);
      },
    });
    callRef.current = call;
    setMessage("");
    try {
      await call.start();
    } catch {
      /* reported through onEvent */
    }
  }

  const busy = state === "connecting";
  return (
    <div>
      <button onClick={toggle} disabled={busy}>
        {state === "connected" ? "Hang up" : busy ? "Connecting..." : "Talk to us"}
      </button>
      {state === "connected" && <span> Talking ({transport})</span>}
      {message && <p role="alert">{message}</p>}
    </div>
  );
}
```

Use it from any page: `import TalkButton from "./components/TalkButton"` and render `<TalkButton />`.
The module only touches browser APIs inside `start()`, so importing it during server rendering is
harmless, but keep the component a client component (`"use client"`).

## Options

| Option | Default | |
|---|---|---|
| `serverUrl` | required | The server's `http(s)://` address (the signaling port). |
| `token` / `tokenProvider` | none | A fixed bearer token, or an async function returning one. The provider is called for every attempt. Not both. |
| `transport` | `"auto"` | `"auto"`: WebRTC, then the WebSocket. `"webrtc"`: never fall back. `"websocket"`: skip WebRTC. |
| `fallbackAfterMs` | `4000` | How long WebRTC gets to connect, from the moment the server accepted its offer, before the WebSocket takes over. A failed connection falls back at once. |
| `signalingTimeoutMs` | `10000` | Bound on the WHIP request (a timeout counts as "no way to reach the server": fall back). |
| `connectTimeoutMs` | `20000` | With `transport: "webrtc"`: how long the connection may take. |
| `startTimeoutMs` | `8000` | WebSocket: from connecting to the server's `started`. |
| `whipPath` / `wsPath` | `/whip`, `/ws` | Paths on `serverUrl`. `null` turns that transport off. |
| `audio` | echo cancellation, noise suppression, auto gain, mono | Merged over the microphone constraints, for example `{ deviceId }`. |
| `audioElement` | a new `Audio()` | The element that plays the assistant over WebRTC. |
| `workletUrl` | a Blob URL | Load the AudioWorklet from a URL instead (a strict Content-Security-Policy that forbids `blob:` needs this; the file's content is the exported `WORKLET_SOURCE`). |
| `metadata` | `{}` | Extra string pairs sent in the WebSocket `start` message. |
| `onEvent` | none | Receives every event; `call.subscribe(fn)` adds more listeners. |

`await call.start()` resolves when the call is connected and rejects with a `BrisvoxCallError`
(`error.code` is `config`, `unsupported`, `microphone`, `token`, `signaling`, `webrtc`,
`websocket`, `timeout` or `connection`; `error.status` is the HTTP status when the server
answered one). `connect(options)` is `new BrisvoxCall(options)` plus `start()`.

Other members: `call.state`, `call.transport`, `await call.hangup()`, `call.setMuted(true)`,
`call.getLevels()` (`{ mic, bot }`, each 0 to 1, for a level meter), `call.audioContext`.

## Events

| `event.type` | Fields | When |
|---|---|---|
| `state` | `state` (`connecting`, `connected`, `ended`, `failed`), `previous`, `transport`, `reason`, `error` | Every state change. `connected` carries the transport; `ended` and `failed` carry the reason. Media that drops for a moment shows as `connecting` and then `connected` again. |
| `transport` | `transport` (`webrtc` or `websocket`), `fallback`, `reason` | A transport carries the call. `fallback` is true when WebRTC was tried first. |
| `fallback` | `from`, `to`, `reason` | WebRTC could not connect and the WebSocket is next. |
| `started` | `callId`, `sampleRate`, `encoding` | WebSocket: the server opened the call. |
| `clear` | `epoch` | Barge-in on the WebSocket: the unplayed audio was dropped. (Over WebRTC the server stops sending.) |
| `mark` | `id` | WebSocket: a playback mark; the module echoes it once the audio before it has played. |
| `stop` | `reason` | The server ended the call. |
| `notice` | `code`, `message`, `fatal` | A server `error` message (`docs/WEBSOCKET_PROTOCOL.md`). |
| `message` | `message` | A server message of a type this version does not know. The protocols carry no transcripts today; if a server sends them, they arrive here unchanged. |
| `muted` | `muted` | `setMuted` was called. |
| `error` | `error`, `code`, `message`, `fatal` | The call failed; `state` `failed` follows. |
| `log` | `text` | A line for a connection log. |

## Tokens

A server that listens beyond localhost requires credentials (`docs/SECURITY.md`). For a real site
set `BRISVOX_SESSION_SECRET` on the server and mint a **session token** on your backend for each
visitor (audience `brisvox-webrtc` for the signaling listener, which also serves `/ws`). The
token is short-lived (30 to 120 seconds are enough: it is only needed until the call starts) and
**single use**, which is why `tokenProvider` is called again for the WebSocket fallback instead of
reusing the first token. The module sends it as `Authorization: Bearer` for WebRTC and as the
`brisvox-token.<token>` WebSocket subprotocol (or `?token=` when it has characters a subprotocol
cannot carry). Never put the secret, or a long-lived token, in the page. `token` (a fixed string)
is for development against `BRISVOX_WEBRTC_TOKEN`.

## Behaviour worth knowing

* A refused request is shown, not hidden by a second transport: a `401`/`403` or `503` from the
  signaling endpoint fails the call with `code: "signaling"` and `status`. Only "no WebRTC here",
  "the request could not be made" and "media did not connect" fall back.
* Hanging up while the call is still opening cancels it: `start()` rejects with an error named
  `AbortError` and nothing stays open.
* Over WebSocket the module bounds what it buffers: microphone frames are dropped while the socket
  is more than 128 KiB behind, bot audio is dropped while the playback worklet lags by more than
  96 KiB.
* A browser without `AudioWorklet` (or a page that is not a secure context) can still use WebRTC;
  the fallback then reports `code: "unsupported"`.
* The module opens one `AudioContext` per call and closes it in `hangup()`; make one
  `BrisvoxCall` per call.

## Tests

```sh
cd sdk/web && node scripts/run-tests.mjs        # Node 20.4+, no install step
```

The tests (`test/`) drive the module with fake `WebSocket`, `RTCPeerConnection`, `AudioContext`,
`AudioWorkletNode`, `getUserMedia` and `fetch` objects (passed through the `platform` option):
WebRTC success, WebRTC timeout and the WebSocket fallback with a fresh token, forcing a
transport, hang-up cleanup, barge-in `clear`, marks, back-pressure, error events. `worklet.test.mjs`
runs the AudioWorklet code (resampling, 20 ms framing, jitter buffer, marks, clear) against fake
audio. The tests run on node's mock timers (`ftest`, `drive`, `until` in `test/fakes.mjs`; Node
20.4+), so they never wait for real time. `scripts/verify.sh` runs them next to the TypeScript
SDK's, and then compiles `test/types-check.ts` (a usage sample) with `tsc --noEmit --strict` using
the compiler the TypeScript SDK stage installed; without that compiler the check is skipped with a
warning.
