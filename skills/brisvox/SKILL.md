---
name: brisvox
description: Build, configure, validate and deploy real-time voice agents with brisvox (Rust voice agent server; phone calls via Twilio/Telnyx/SIP and browser calls via WebRTC with WebSocket fallback; STT/LLM/TTS providers incl. local sherpa-onnx, Ollama, Kokoro; tools in Python, TypeScript or Rust). Use when writing a brisvox agent file (TOML/YAML), adding tools, wiring a website voice button, choosing cargo features, or deploying brisvox-server (Docker, one-port PaaS hosts such as Railway, VPS).
---

# Building with brisvox

brisvox-server runs one **agent definition** (a TOML or YAML file) for every call. Speech in,
LLM with tools, speech out; the caller can interrupt. Everything else about the server is
**environment variables** (`BRISVOX_*`). Read `llms.txt` at the repository root for the map of
the docs; this skill is the workflow and the traps.

## Workflow

1. **Pick the closest example** and copy it; do not write an agent file from memory.
   - `examples/agents/*.toml|yaml` (each has a YAML twin), `examples/agents/local/` (fully local),
     `examples/rust-tools/agent.toml`, `examples/booking-agent` (SDK tools),
     `examples/phone-receptionist` (Twilio), `examples/web-assistant` (browser).
   - Every key is in `docs/CONFIG.md`; every server variable in `docs/OPERATIONS.md`.
2. **Choose cargo features** from the "What to build with" table in `README.md`. Providers and
   transports are features (`all-providers` or `provider-<name>`, `webrtc`, `sip`,
   `call-control`, `smart-turn-ort`). The Docker image takes them as the `BRISVOX_FEATURES`
   build argument, whose default has **no `webrtc`**.
3. **Validate offline** before running anything:
   `brisvox-server check <agent-file> [--require-secrets]`. It runs the real loader against the
   binary you built and prints each problem with its key path and line. Fix until it says OK.
   Editors can check as you type with the JSON Schema (`docs/agent.schema.json` in the repository,
   or `brisvox-server --print-schema > agent.schema.json`; `#:schema ./agent.schema.json` in TOML, `# yaml-language-server: $schema=...` in YAML; `docs/CONFIG.md` "Editor support").
4. **Run**: export the keys the file names (`api_key_env`), set `BRISVOX_AGENT_FILE` and the
   listener variables, start `brisvox-server`. It does **not** read `.env`.
5. **Connect a caller**: browser through `sdk/web` (module `brisvox-call.js`), phone through
   Twilio/Telnyx/SIP (`docs/TELEPHONY.md`), or your own client over the WebSocket protocol
   (`docs/WEBSOCKET_PROTOCOL.md`).

## Where tools live (the most common mistake)

| Your tools are in | Agent file `agent.tools` | How |
|---|---|---|
| Python or TypeScript (another process) | **do not list them** | SDK agent (`sdk/python`, `sdk/typescript`); the server connects with `BRISVOX_AGENT_URL`. Listed in `agent.tools`, they stop the server at startup ("not registered in this server"), and `brisvox-server check` reports it as an error. |
| Rust, in your binary | list them | `brisvox_server::app::run_with_tools(Arc::new(MyTools))` in a 3-line `main`; see `examples/rust-tools` |
| Built-in telephony tools (transfer, DTMF, hang up) | list them | feature `call-control` plus `BRISVOX_CALL_CONTROL` |

- Tools that change the world are `SideEffect`: never run speculatively, never twice for the same
  call and turn. Across calls (a caller phones twice) use your own idempotency key in your
  database. `docs/TOOLS.md` "What never twice covers".
- To ask the caller before a SideEffect runs: `confirm=True` (Python) / `confirm: true`
  (TypeScript) on an SDK tool, or `agent.confirm_tools` for tools listed in the agent file.

## Traps

- **Adaptive turns need the model.** Without the `smart-turn-ort` feature and the model file
  (`python3 scripts/models/fetch_models.py`), every turn waits the 3 s fallback. `check` warns.
- **Placeholders are not values.** Voice ids such as `replace-with-a-...` must be replaced with a
  real voice of the TTS provider.
- **Listeners fail closed.** Off loopback the server refuses to start without authentication:
  `BRISVOX_SESSION_SECRET` (short-lived signed tokens, minted by your backend; see
  `sdk/web/README.md` for a Next.js route) or a static token. Never ship the secret to a browser.
- **Browsers ignore loopback ICE candidates.** For a local test open the page with `?force=ws`
  or set the media address to the LAN IP (`examples/web-assistant/README.md`).
- **SDK agents bind loopback by default.** In a container or PaaS (a private network between services)
  bind `0.0.0.0` / `::`, or the server cannot reach your tools.
- **One-port hosts (Railway, Render, Fly without UDP)**: WebRTC media cannot pass; the browser
  module falls back to WebSocket audio on the same port after 4 s, so on such a host pass
  `transport: 'websocket'` to skip the WebRTC attempt (no wait, no second token). Write listener addresses
  as `0.0.0.0:${PORT}` (the server substitutes the exact token), put the agent file's text in
  `BRISVOX_AGENT_DEFINITION` when no file can be mounted (check it with `brisvox-server check
  --env`), and follow the section "Platforms with one HTTPS port and no UDP (PaaS)" of `docs/DEPLOY.md` (the
  platform's drain time, client IP header, health check, a second service for tools; Railway is its
  worked example).
- **Twilio** streams use a per-call signed URL minted by your webhook
  (`examples/phone-receptionist/twiml_server.py`); a reverse proxy must route
  `/twilio/stream*`, not the exact path.
- **OpenRouter, Groq, Together, vLLM, Ollama** are the `openai_compatible` LLM: a `preset`
  (`openrouter`, `ollama`, ...) or a `base_url`, plus `model`; for voice set
  `reasoning_effort = "none"` on reasoning models (`docs/PROVIDERS.md`).

## Answering "which provider/model"

Do not invent model names or voice ids: take them from `examples/agents/` or the provider's
page under `docs/providers/`, which records what was verified live and when.
