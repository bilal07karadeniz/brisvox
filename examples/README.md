# Examples

Runnable projects, each with its own README. Every one loads and validates offline (no keys, no
network): `scripts/verify.sh` runs their tests, and a Rust test loads each agent definition
through the server's config loader. Keys always come from the environment; no example contains
one.

| Project | What it shows | Needs |
|---|---|---|
| [`phone-receptionist/`](phone-receptionist) | An inbound receptionist on a Twilio number: transfer to named destinations, hang-up, and an outbound reminder agent that leaves a voicemail message. A small voice webhook and a call-placing script. | Twilio account and number, a tunnel (ngrok) or a TLS proxy, AssemblyAI + Groq + Deepgram keys |
| [`web-assistant/`](web-assistant) | A voice assistant in a static web page over WebRTC: one HTML file, no framework; the server settings that host it. | A server built with the `webrtc` feature, Deepgram + OpenAI + Cartesia keys |
| [`booking-agent/`](booking-agent) | A tool-heavy booking agent as an out-of-process Python agent (SDK): five typed tools, `side_effect` booking and cancelling with idempotency keys, per-call prompt; its tools are tested without keys. | Python 3.11+ and `websockets`; the three provider keys of the server to talk to it |
| [`rust-tools/`](rust-tools) | A server with your own Rust tools: a `main` of one line over `brisvox_server::app::run_with_tools`, a read-only `get_time` and a side-effect `save_note` with an idempotency key, and the agent file that names them. A workspace crate: its tests register the tools and load the agent file against them. | Rust; Deepgram + OpenAI + Cartesia keys to run it |
| [`agents/`](agents) | Single-file agent definitions (TOML and YAML twins) for support, Turkish support, telephony and provider failover; `agents/local/` has fully local stacks (sherpa-onnx STT, Ollama, an OpenAI-compatible TTS server), English and Turkish. | See `docs/CONFIG.md`; the local ones: `docs/providers/sherpa-onnx.md` |

Which transport serves which example:

| Example | Listener | Settings |
|---|---|---|
| phone receptionist | Twilio Media Streams | `BRISVOX_TWILIO_ADDR`, `BRISVOX_TWILIO_PUBLIC_URL`, `TWILIO_AUTH_TOKEN`, `BRISVOX_CALL_CONTROL=twilio` |
| web assistant | WebRTC (feature `webrtc`) | `BRISVOX_WEBRTC_ADDR` and friends |
| booking agent | any (the agent process is reached through `BRISVOX_AGENT_URL`) | `BRISVOX_AGENT_URL`, `BRISVOX_AGENT_TOKEN` |

All settings are listed in `docs/OPERATIONS.md`; the agent file format in `docs/CONFIG.md`.

**Voice ids are placeholders** (`replace-with-...`) on purpose: a voice id belongs to a vendor
account and its language coverage differs per voice, and this repository records no voice id
that its live tests verified (they read yours from `CARTESIA_VOICE_ID` / `ELEVENLABS_VOICE_ID`
in the environment, `.env.example`). Pick one from your vendor's voice library, one that lists
your agent's `language`, and run `brisvox-server check` and one call before you rely on it.

## Running the examples' own tests

```sh
scripts/verify.sh                                              # everything, including the examples stage
python3 -m unittest discover -s examples/phone-receptionist -p 'test_*.py'
python3 -m unittest discover -s examples/web-assistant -p 'test_*.py'
PYTHONPATH=sdk/python python3 -m unittest discover -s examples/booking-agent -p 'test_*.py'
cargo nextest run -p brisvox-server --all-features -E 'binary(examples)'
```
