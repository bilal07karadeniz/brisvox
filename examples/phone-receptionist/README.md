# Phone receptionist (Twilio)

An AI receptionist for a fictional dental clinic that answers a real phone number through
Twilio Media Streams. It answers questions, puts the caller through to a person
(`transfer_call`, by destination name) and hangs up politely (`end_call`). A second definition
makes **outbound** reminder calls and leaves a message when an answering machine picks up.

| File | What it is |
|---|---|
| `agent.toml` | The inbound receptionist: AssemblyAI (STT) + Groq (LLM) + Deepgram Aura (TTS), 8 kHz, `[telephony]` destinations `front_desk` and `billing`. |
| `reminder.toml` | The outbound reminder agent: same providers, `[telephony.voicemail]` for answering machines. |
| `twilio.env.example` | Every environment variable of the server and the two scripts, no values. |
| `twiml_server.py` | A small voice webhook (standard library only): returns the `<Connect><Stream>` TwiML with a signed per-call stream URL, passes the machine-detection verdict and the caller's `From` / `To` on, verifies `X-Twilio-Signature`. |
| `place_call.py` | Places an outbound reminder call with answering-machine detection (standard library only; dry run unless `--yes`). |
| `test_phone_receptionist.py` | Tests of the two scripts, no network beyond loopback. |

Nothing here contains a key: the provider keys, the Twilio account SID and Auth Token come from
the environment (`api_key_env` in the definitions names the variable).

## What runs where

```
caller --PSTN--> Twilio --wss://YOUR-HOST/twilio/stream--> brisvox-server --> STT, LLM, TTS
                    |                                        (agent.toml)
                    +--REST (transfer_call, end_call)<-------+
```

Twilio needs a **public `wss://` URL** for the stream. On a laptop use a tunnel such as ngrok;
on a server put a TLS reverse proxy in front of the listener (`docs/DEPLOY.md`). The server
checks `X-Twilio-Signature` of every stream against `BRISVOX_TWILIO_PUBLIC_URL` plus the call's
own path segment, so that value must be the public URL exactly as the webhook writes it into the
TwiML.

## Run it

You need: a Twilio account with a voice-capable number, keys for AssemblyAI, Groq and
Deepgram, Rust, and a tunnel tool (ngrok here; any `https`/`wss` forwarder works).

1. **Build the server** with the providers, the Twilio call control (transfer, hang-up) and
   Smart Turn (the receptionist uses the adaptive turn policy):

   ```sh
   cargo build --release -p brisvox-server --features all-providers,call-control,smart-turn-ort
   python3 scripts/models/fetch_models.py        # the Smart Turn model, into models/
   ```

2. **Expose the stream port and the webhook.** Twilio needs two public URLs on one host:
   the webhook (`/voice`, port 8000 below) and the stream (`/twilio/stream` and the per-call
   path under it, port 8082). On a laptop put a small path-routing proxy in front of both and
   tunnel that one port. A Caddyfile:

   ```
   :8090 {
       handle /voice {
           reverse_proxy 127.0.0.1:8000
       }
       handle /twilio/stream* {
           reverse_proxy 127.0.0.1:8082
       }
   }
   ```

   ```sh
   caddy run --config Caddyfile &
   ngrok http 8090
   # note the forwarding host, e.g. abc123.ngrok-free.app
   ```

   `handle /twilio/stream*` (with the star) matters: every call connects to its own path,
   `/twilio/stream/<segment>`, which an exact `/twilio/stream` match would answer with a 404.

3. **Fill the environment.**

   ```sh
   cp examples/phone-receptionist/twilio.env.example examples/phone-receptionist/twilio.env
   # edit it: the three provider keys, TWILIO_AUTH_TOKEN, BRISVOX_TWILIO_ACCOUNT_SID,
   # BRISVOX_TWILIO_CALLER_ID, BRISVOX_TWILIO_STREAM_SECRET (openssl rand -base64 48),
   # and replace REPLACE-ME.ngrok-free.app (both stream URLs and TWIML_PUBLIC_URL)
   ```

   `twilio.env` is ignored by version control (the name ends in `.env`); keep it that way. The
   secret is required: without it (or the legacy `BRISVOX_TWILIO_ALLOW_STATIC_URL=1`) the server
   refuses to start the Twilio listener and says which variables it wants.

4. **Start the server and the webhook** from the repository root, both with the same
   environment file:

   ```sh
   set -a; . examples/phone-receptionist/twilio.env; set +a
   ./target/release/brisvox-server &
   python3 examples/phone-receptionist/twiml_server.py --port 8000 &
   curl http://127.0.0.1:8081/readyz             # ready
   ```

   `./target/release/brisvox-server check "$BRISVOX_AGENT_FILE" --require-secrets` (same
   environment) validates the agent file offline, including that every key it names is set.

5. **Point your number at the webhook.** In the Twilio console open the number, "Voice
   Configuration", "A call comes in": Webhook, `https://abc123.ngrok-free.app/voice` (the
   value of `TWIML_PUBLIC_URL`, character for character), HTTP POST. Call the number and say
   "Hello" (an agent definition has no scripted greeting: the assistant answers once the
   caller has spoken; an out-of-process agent can open the call, see `../booking-agent`).
   Say "put me through to billing" to try a transfer: the model says one sentence, then
   `transfer_call` redirects the call to the `billing` number of `agent.toml`.

   The webhook (`twiml_server.py`) does one thing per call: it reads the call's `CallSid`
   and answers with `<Connect><Stream>` whose URL is `BRISVOX_STREAM_URL` plus a signed,
   single-use segment `/<CallSid>.<expiry>.<mac>` (valid for 120 s, for that call only).
   The server accepts a stream only on such a URL, so a URL that leaked in a proxy log
   cannot open a second call. With `TWILIO_AUTH_TOKEN` and `TWIML_PUBLIC_URL` set the webhook
   also refuses requests that do not carry Twilio's `X-Twilio-Signature`.

   **In a container, bind it.** `twiml_server.py` listens on `127.0.0.1` by default, which a
   reverse proxy in another container cannot reach. Pass `--host 0.0.0.0` (for example
   `python twiml_server.py --host 0.0.0.0 --port 8000`) and publish the port only to the proxy's
   network, never to the internet.

   **Caller identity.** The webhook also passes the call's Twilio `From` and `To` into the stream
   as `<Parameter name="from" .../>` and `<Parameter name="to" .../>` (a value that is not a
   number or a short identifier is dropped). The server forwards them to an SDK agent process
   only when you opt in (`[agent_link] caller_identity = true` in the agent file, or
   `BRISVOX_AGENT_CALLER_IDENTITY=1`; withheld while phone numbers are redacted,
   `docs/AGENT_PROTOCOL.md`, "Caller identity"). They are not covered by the stream URL's
   signature: treat them as a hint, like a caller ID, and never paste them unquoted into a
   prompt.

   *Legacy: a static TwiML Bin.* A fixed `<Stream url="wss://abc123.ngrok-free.app/twilio/stream"/>`
   in a TwiML Bin works only when the server runs with `BRISVOX_TWILIO_ALLOW_STATIC_URL=1`
   (leave the stream secret unset). The fixed URL has one constant signature that anyone who
   saw it once can replay, so use it for a first try at most.

## Transfer

`transfer_call` takes a destination **name** (`front_desk`, `billing`); the numbers live only in
`[telephony].destinations` of `agent.toml`, so the model can never dial a number of its own.
Replace the `+1555555…` placeholders with E.164 numbers (`+` and 8 to 15 digits).

* **Cold transfer** (what you get as shipped): the call is handed to the destination, the
  receptionist drops out.
* **Warm transfer** (`warm = true`): the receiving person hears a summary first, and the application
  decides when they accepted. It needs the callback listener (`BRISVOX_CALLBACK_ADDR`,
  `BRISVOX_CALLBACK_TOKEN`) and `BRISVOX_TWILIO_CALLER_ID`; the flow, the two endpoints that
  finish a transfer and the limits are in `docs/TELEPHONY.md` ("Warm transfers in the binary")
  and `docs/OPERATIONS.md` ("Callback listener"). Without the listener the server logs a
  warning and offers cold transfers only.

## Voicemail: outbound reminders

An answering machine only exists on a call **you place**, so voicemail handling belongs to
`reminder.toml`, not to the inbound receptionist. An inbound call never gets a verdict and is
never touched by `[telephony.voicemail]`.

1. Run a second server for the reminder agent (one definition per process), e.g. on port 8083:

   ```sh
   BRISVOX_AGENT_FILE=examples/phone-receptionist/reminder.toml \
   BRISVOX_TWILIO_ADDR=127.0.0.1:8083 BRISVOX_HEALTH_ADDR=127.0.0.1:8084 \
   ./target/release/brisvox-server       # with twilio.env loaded, as above
   ```

2. Run `twiml_server.py` behind a public `https` URL (`TWIML_PUBLIC_URL`) and point
   `BRISVOX_STREAM_URL` / `BRISVOX_TWILIO_PUBLIC_URL` at the reminder server's public
   `wss://` URL (the same `BRISVOX_TWILIO_STREAM_SECRET` for the server and the webhook).
3. Place a call (it only prints what it would do until you add `--yes`; call only people who
   agreed to be called):

   ```sh
   python3 examples/phone-receptionist/place_call.py +15555550123 --yes
   ```

   The request asks Twilio for synchronous machine detection (`MachineDetection=DetectMessageEnd`),
   so the webhook runs once the verdict is known; `twiml_server.py` puts it into the stream as the
   parameter `answered_by`. A person gets the conversation of `reminder.toml`. A machine is played
   `[telephony.voicemail].message` **after its greeting ended**, then the call is hung up through
   Twilio. No verdict (`on_unknown = "treat_as_human"`) means a normal conversation.

Details and limits (the bot is not silenced while the verdict is pending, a beep can cut the
message, asynchronous detection through the callback listener): `docs/TELEPHONY.md` section 6.

## Tests

```sh
python3 -m unittest discover -s examples/phone-receptionist -p 'test_*.py'
cargo nextest run -p brisvox-server --all-features -E 'binary(examples)'
```

The Python tests cover the TwiML (escaping, only a well-formed `answered_by` is passed on), the
per-call stream URL (the server's known answer, one segment per call, a call without a usable
`CallSid` refused), the signature check (good, forged, unsigned, a changed parameter) and the call request (machine
detection on, bad numbers refused, nothing sent without `--yes`). The Rust test loads both
definitions through the server's config loader, builds their provider adapters with placeholder
keys, starts them on the binary's tool setup and reads `twilio.env.example` through the server's
settings parsers. `scripts/verify.sh` runs both.

## Not covered offline

A real call is the only test of the audio path: Twilio's behaviour on a stream that ends on
purpose (transfers), the quality of the 8 kHz speech and the detector's verdicts depend on your
account and carrier. The transport's live checks are listed in `docs/TELEPHONY.md` section 7.
