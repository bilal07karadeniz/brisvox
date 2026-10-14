# Agent definitions (configuration)

TOML and YAML. Code: `crates/brisvox-server/src/config/`. Examples:
`examples/agents/*.toml` and their YAML twins `*.yaml` (each one is loaded by the test
`config::tests::the_examples_are_valid`, and `config::yaml::tests::the_yaml_examples_are_twins_of_the_toml_ones`
checks that every twin reads into the same definition, so they cannot rot or drift).

One TOML or YAML file describes one agent: who it is (system prompt, language, tools by name), how turns
and barge-ins are judged, and one provider per stage (STT, LLM, TTS) with its options. Absent
keys keep the engine's and the provider adapters' documented defaults; the defaults live next to
the code that uses them (`EngineConfig::DEFAULT`, `XxxConfig::new`), not in this file.

## Loading

```rust
use brisvox_server::config::{self, ProcessEnv};

// Blocking file read: at startup, or inside spawn_blocking.
let agent = config::load_file(Path::new("examples/agents/support-en.toml"), &ProcessEnv)?;
// Only the tools the definition names (all must be registered): the LLM never sees the rest.
let tools = Arc::new(agent.tool_registry(&server_registry)?);
let engine = agent.engine();                   // EngineConfig, already validated
let stt = agent.stt()?;                        // SttProviderConfig::Deepgram(DeepgramConfig) ...
let llm = agent.llm()?;                        // LlmProviderConfig::OpenAi(OpenAiConfig) ...
let tts = agent.tts(clock, shutdown)?;         // TtsProviderConfig::Cartesia(CartesiaConfig) ...
let builder = agent
    .configure(SessionBuilder::new(engine, clock, timeline, offload, s, l, t)?)
    .with_tools(Arc::clone(&tools), Arc::new(ToolEngine::new(tools, /* ... */)));
```

`load_file` picks the format by extension: `.yaml`/`.yml` (any case) is YAML, anything else is
TOML. `load_str(text, source_name, &env)` does the same for TOML text held in memory, and
`load_str_as(text, source_name, Format::Yaml, &env)` for either format; `parse` / `parse_as` read
the file format only (no cross-field checks, no secrets). `Env` is the secret source: `ProcessEnv`
in production, a `BTreeMap`/`HashMap` in tests.

Loading has three steps:

1. **Syntax and types.** Every table rejects unknown keys (`deny_unknown_fields`): a typo is an
   error, never a silently ignored setting. Values are type- and range-checked while they are
   read. The first such error stops reading.
2. **Cross-field checks** (all reported together, in file order), see below.
3. **Secrets.** Each `api_key_env` names an environment variable; its value is read into a
   `brisvox_core::Secret<String>` (`Debug`/`Display` print `[REDACTED]`). A missing, empty or
   non-UTF-8 variable is an error naming the variable and the key, never a value. Surrounding
   whitespace (the trailing newline of an env-file or a mounted secret file) is trimmed;
   whitespace or control characters inside the value are an error. A key pasted into
   `api_key_env` by mistake (longer than 32 bytes, containing `-`, or mixing lower-case letters
   and digits) is never echoed: messages show its first 4 characters and its length, with a hint.

After that the mapped `EngineConfig` goes through `EngineConfig::validate` as a last net.

## Errors

Every problem names the file, the line and column, the key path and what is wrong (the same
key paths and messages for TOML and YAML; positions refer to the file as written):

```text
invalid agent definition agents/tr.toml (3 problems)
  agents/tr.toml:4:12: `agent.language`: Deepgram Aura has no voice for `tr` (it speaks en, es, de, fr, nl, it, ja); use Cartesia or ElevenLabs for this language
  agents/tr.toml:21:15: `tts.sample_rate`: Deepgram Aura does not deliver PCM16 at 44100 Hz (8000, 16000, 24000 or 48000)
  agents/tr.toml:25:16: `tts.api_key_env`: environment variable `DEEPGRAM_API_KEY` (named by tts.api_key_env) is not set; secrets are read from the environment, never from the file
```

`ConfigError { source_name, issues: Vec<Issue> }`, `Issue { key, position: Option<(line,
column)>, message }`; columns count characters, not bytes. The key path of a read error is found
by walking the parsed document for the most specific key or value whose span contains the error
position (toml's own error carries the span but does not expose its key path). The YAML reader
tracks the key path itself while it reads. An `api_key` literal gets the hint to use
`api_key_env` in both formats.

## Checking a file

`brisvox-server check <agent-file> [--require-secrets]` validates a definition offline, before a
deploy or inside CI. `brisvox-server check --env` does the same for the definition the environment
names (`BRISVOX_AGENT_DEFINITION`, or the file `BRISVOX_AGENT_FILE` names). It runs the same loader the server runs at startup, so it reports every
problem the server would refuse the file for, each with its key path and line and column. It
opens no sockets, starts no runtime and makes no network call.

```text
$ brisvox-server check agents/support.toml
error: agents/support.toml:17:1: `stt.deepgram.endpointing_msx`: unknown field `endpointing_msx`, expected one of ...
FAILED: agents/support.toml has 1 error
```

```text
$ brisvox-server check agents/support.toml
warning: agents/support.toml:31:15: `llm.api_key_env`: environment variable `OPENAI_API_KEY` is not set; ...
OK: agent "support" (stt deepgram, llm openai, tts cartesia) (1 warning)
```

```text
$ brisvox-server check agents/support.toml
error: agents/support.toml:17:10: `agent.tools[0]`: tool `lookup_order` is not registered by this binary, and the server refuses to start with it; ...
FAILED: agents/support.toml has 1 error
```

The exit status is 0 when the file has no errors (warnings do not count) and 1 when it has any.
A wrong command line exits 2. Besides the errors the loader reports (unknown or misplaced keys,
values out of range, a language or sample rate the provider does not support, a provider whose
cargo feature is not compiled into this binary, with the feature to turn on), `check` reports:

| finding | level |
|---|---|
| an environment variable named by `api_key_env` / `token_env` is not set | warning; an error with `--require-secrets` (use it on the machine or CI job that has the real environment) |
| a variable is set but empty, or has spaces or control characters inside | error |
| a tool in `agent.tools` that this binary does not register: the built-in telephony tools need the `call-control` feature and `BRISVOX_CALL_CONTROL` set to a carrier (with both they count as registered; `check` does not verify the carrier's credentials, which the server needs at start), the demo tools the `demo-tools` feature, your own tools `AppTools` (`docs/TOOLS.md`) | **error**: the server refuses to start with it. If it is a Python/TypeScript SDK tool, remove it from `agent.tools`: the SDK agent declares it over the agent link |
| adaptive turns, and no Smart Turn model: the binary lacks the `smart-turn-ort` feature, or the model file (`BRISVOX_SMART_TURN_MODEL`, default `$BRISVOX_MODELS_DIR/smart-turn-v3.2-cpu.onnx`) does not exist | warning (without the model, turns end at their fallback deadline, or the start fails if the feature is on and the file is missing); `BRISVOX_SMART_TURN_MODEL=off` silences it |

`check` answers for the binary it runs in: a server built with fewer provider features than the
one you deploy reports errors the deployed one would not, so run it with the build you ship. A
binary of your own that embeds the server with its own tools (`brisvox_server::app::run_with_tools`,
`docs/TOOLS.md`) has the same `check` command and knows those tools' names. In Rust,
`brisvox_server::check::check_file` returns the report as data (`errors`, `warnings`).

## Editor support (JSON Schema)

`docs/agent.schema.json` is a JSON Schema (draft 2020-12) of the agent file.
With it an editor completes keys, shows each key's description from this reference, and underlines
a typo, an unknown table, a number out of range, a wrong enumeration value or a URL with the wrong
scheme while you type, before `brisvox-server check` runs. The schema is generated from the same
Rust types the loader reads (`deny_unknown_fields` is `additionalProperties: false`; the ranges
of the millisecond, count, real-number and sample-rate settings are `minimum`/`maximum`/`enum`;
names, environment variable names, language tags and URLs are `pattern`s). A test keeps the
committed file equal to the generated one and every file in `examples/` valid against it.

**TOML**, with [Taplo](https://taplo.tamasfe.dev) (the "Even Better TOML" VS Code extension, the
`taplo` CLI, Helix, Neovim): put the schema directive on the first line of the file,

```toml
#:schema ./agent.schema.json
[agent]
name = "support"
```

(a path relative to the file, or the URL
`https://raw.githubusercontent.com/bilal07karadeniz/brisvox/main/docs/agent.schema.json`, once
published), or
bind it to all your agent files once in `.taplo.toml`:

```toml
[[rule]]
include = ["agents/**/*.toml"]
schema.path = "docs/agent.schema.json"
```

**YAML**, with the YAML language server (the Red Hat "YAML" VS Code extension, `yaml-language-server`
in Neovim, Helix and others): a modeline comment on the first line,

```yaml
# yaml-language-server: $schema=./agent.schema.json
agent:
  name: support
```

or, in VS Code `settings.json`, `"yaml.schemas": {"docs/agent.schema.json": "agents/*.yaml"}`.

`brisvox-server --print-schema` prints the schema of the binary you have, so a copy always matches
its version. It needs the `schema` cargo feature; without it the flag is an error that names the
feature. To regenerate the committed file after a change to the config types:

```text
cargo run -p brisvox-server --features schema -- --print-schema > docs/agent.schema.json
```

What the schema cannot say stays with `brisvox-server check`, which remains the authority: the
cross-field rules (`turn.silence_ms` is required with `fixed_silence`, `confirm_tools` must be
listed in `tools`), the environment variables and the tools of your deployment, and the provider
cargo features of the binary. Two small differences: the schema takes no `null` (the YAML reader
treats an empty `turn:` or `silence_ms: ~` as an absent key, and the schema flags it; leave the
key out), and its length limits count characters where the loader counts bytes.

## YAML

The same file in YAML (`examples/agents/support-en.yaml` is the twin of `support-en.toml`):
every TOML table is a mapping, every `[stt.deepgram]` options table is a `deepgram:` mapping under
`stt:`, and the keys, values, defaults and checks are identical.

```yaml
agent:
  name: support-en
  language: en
  system_prompt: |
    You are the voice assistant of Example Shop. Answer in one or two short spoken sentences.
  tools: [lookup_order]

stt:
  provider: deepgram
  api_key_env: DEEPGRAM_API_KEY
  deepgram:
    model: nova-3
    endpointing_ms: 300

llm:
  provider: openai
  api_key_env: OPENAI_API_KEY

tts:
  provider: cartesia
  api_key_env: CARTESIA_API_KEY
  voice: replace-with-a-cartesia-voice-id
```

Rules:

- **Scalars follow the YAML 1.2 core schema.** `true`/`false`, integers (`300`, `+5`, `0x1F`,
  `0o17`), floats (`0.7`, `1e3`) and `null`/`~`/nothing are typed; every other plain scalar and
  every quoted or block (`|`, `>`) scalar is a string. `yes`/`no`/`on`/`off` are strings, so
  `enabled: yes` is an error (`invalid type: string "yes", expected a boolean`), exactly as
  `enabled = "yes"` is in TOML. A voice id that looks like a number must be quoted
  (`voice: "12345"`); TOML needs the quotes anyway.
- **Null is the same as leaving the key out.** An empty `turn:` is an empty `[turn]` table; an
  empty `backchannels:` keeps the preset's phrase sets (`[]` turns them off). A required value
  left empty (`voice:`) is `invalid type: null, expected a string`.
- **Not supported**, each an error at its position: anchors (`&x`) and aliases (`*x`), tags
  (`!!str`, `!x`; quote the value instead), a second document (`---`), keys that are lists or
  mappings, duplicate keys (reported at the second one), nesting deeper than 16 levels.
- Positions: line and character column of the value (or of the key, for an unknown key or a
  problem of a whole mapping such as an options table for another provider). A byte order mark
  and Windows line ends are fine; a lone carriage return (classic Mac line ends) is an error at
  its position, as in TOML. Reading is linear in the file size, also for one long line.
- Syntax errors read `YAML syntax error: <parser message>` with the key path of the enclosing
  value.
- The parser is `saphyr-parser` (pure Rust, YAML 1.2, MIT OR Apache-2.0) behind the
  `brisvox-server` feature `yaml`, **on by default**. A build with `--no-default-features` reads
  TOML only and rejects a YAML definition with a message naming the feature.

## File reference

Durations are whole milliseconds (`*_ms`). Ranges are inclusive.

### `[agent]` (required)

| key | type | notes |
|---|---|---|
| `name` | string | 1-64 of `A-Z a-z 0-9 _ -` |
| `language` | string | `en` or `tr`, region allowed (`en-US`, `tr-TR`). Selects the segmenter and speech normalizer; default STT and TTS language |
| `system_prompt` | string | non-empty, at most 100 000 bytes |
| `tools` | array of strings | tool names (same rule as `name`), no duplicates; `AgentConfig::tool_registry` builds the session's registry from exactly these (unlisted server tools are not offered to the LLM), `AgentConfig::check_tools` only checks them. The server binary refuses a definition naming a tool it does not provide (`docs/TOOLS.md` "In the server binary") |
| `confirm_tools` | array of strings | optional: tools of `tools` that are held for the user's confirmation: the model's call does not run them, it asks the user, and they run only after the user's next reply confirms (`docs/TOOLS.md`, "Confirmation gate"). Each must be listed in `tools` and be a SideEffect tool (`AgentConfig::tool_registry` reports a ReadOnly one at `agent.confirm_tools[i]`). Example: `confirm_tools = ["transfer_call"]` |

### `[turn]`

| key | type | default | notes |
|---|---|---|---|
| `policy` | `"adaptive"` / `"fixed_silence"` | `adaptive` | Smart Turn, or a fixed silence |
| `silence_ms` | 1..=10000 | | `fixed_silence` only, required there |
| `fallback_silence_ms` | 100..=30000 | 3000 | `adaptive` only |
| `require_final` | bool | false | `adaptive` only |
| `final_grace_ms` | 0..=10000 | 800 | `adaptive` only |
| `final_wait_ms` | 1..=30000 | 1500 | must stay below the STT finalize timeout (3000) |
| `response_deadline_ms` | 1000..=300000 | 20000 | must exceed `final_wait_ms` |

### `[interruption]`

`preset` (`"recommended"` default, or `"off"`), then per-field overrides: `min_speech_ms`
(0..=5000), `min_words` (0..=20), `confirm` (`"all"`/`"any"`), `echo_guard_ms` (0..=5000),
`echo_min_words` (1..=20), `backchannels` (array of `"en"`, `"tr"`; `[]` turns them off),
`backchannel_wait_ms` (0..=5000), `backchannel_max_speech_ms` (0..=10000), `vad_hangover_ms`
(0..=2000). Meanings: `InterruptionConfig` in `crates/brisvox-engine/src/config.rs`.

### `[speculation]` and `[early_analysis]`

`[speculation]`: `enabled` (false), `on_partial` (true), `partial_stable_ms` (0..=60000, 0; at
most `max_age_ms`: the stable-partial gate, the partial of a VAD stop is offered only after the
caller stayed silent and the text did not change for this long), `max_per_turn` (1..=16, 3),
`max_age_ms` (100..=60000, 5000), `max_buffered_events` (1..=4096, 256).

`[early_analysis]`: `enabled` (false; needs `turn.policy = "adaptive"`), `fall_delay_ms`
(0..=1000, 64), `max_tail_ms` (1..=5000, 300), `max_per_turn` (1..=32, 4). The assembly applies
the VAD half itself (`SessionBuilder::with_vad`).

### `[audio]`

`frame_ms` (5..=100, 20) must be a whole number of samples at 16 kHz and at `call_sample_rate`;
`call_sample_rate` (8000, 16000, 22050, 24000, 44100 or 48000; default 16000) is only the
fallback for a transport that reports no rate (telephony transports report 8000 themselves).

### `[filler]`

When a slow tool round is announced with a filler phrase (`docs/TOOLS.md` "Tool UX"):
`enabled` (true), `delay_ms` (100..=10000, 1200: the same delay for every tool that has a
filler), `per_tool` (false; true takes each tool's own `filler_after`, 700 ms unless the tool
sets another, and cannot be combined with `delay_ms`). The session default is the fixed 1.2 s
(why: `docs/TOOLS.md`, "Why the default is a fixed 1.2 s"). The phrases and their pre-rendered clips are set in code
(`SessionBuilder::with_filler_policy`, `with_filler_clips`).

### `[recording]`

What call recording keeps and for how long (`docs/PRIVACY.md`). Recording itself is
switched on by the environment (`BRISVOX_RECORDING_DIR`; the directory is infrastructure, not an
agent setting), and a key set here **wins over** the matching environment variable. The table
without `BRISVOX_RECORDING_DIR` is a start error: a privacy setting is never silently ignored.
TOML and YAML read it identically (`recording:` in YAML).

| key | type | default | environment variable it overrides |
|---|---|---|---|
| `transcript` | bool | `true` | `BRISVOX_RECORDING_TRANSCRIPTS` |
| `audio` | bool | `false` | `BRISVOX_RECORDING_AUDIO` (audio is never redacted) |
| `redact` | bool | `true` | `BRISVOX_RECORDING_REDACT_PII` |
| `redact_kinds` | list of `email`, `iban`, `id`, `card`, `phone`, `number` | all | `BRISVOX_RECORDING_REDACT_KINDS`; not empty while `redact` is on, and not combined with `redact = false` |
| `retention_days` | integer 1..=3650 | 30 | `BRISVOX_RECORDING_RETENTION` (which also accepts `90s`..`forever`) |

```toml
[recording]
audio = false
redact_kinds = ["email", "iban", "card", "phone"]
retention_days = 14
```

The merged result is checked like the environment's alone (something must be recorded; the
retention must cover the longest recorded call).

Encryption at rest has no file key: `BRISVOX_RECORDING_AGE_RECIPIENTS` (age public keys, cargo
feature `recording-encryption`) is environment-only, like the directory, because it belongs to the
deployment and not to an agent. With it set, every file the table above governs is written as
`<name>.age` (`docs/PRIVACY.md`, "Encryption at rest").

### `[telephony]`

What the built-in telephony tools may do (`docs/TELEPHONY.md`). They exist only in a
server with a carrier's call control (`BRISVOX_CALL_CONTROL`, `docs/OPERATIONS.md`); an agent
lists the ones it uses in `agent.tools` like any other tool.

| key | type | notes |
|---|---|---|
| `destinations` | array of tables | where `transfer_call` may send the call, at most 32: `name` (1-32 of `A-Z a-z 0-9 _ -`, unique; the model picks a name, never a number), `number` (E.164, `+` and 8 to 15 digits), `description` (optional, shown to the model, cut to 120 characters). Without any, `transfer_call` is not offered |
| `dtmf` | bool | offer `send_dtmf` (default `true`); it is offered only when every configured carrier binding can send tones (Twilio can when the server hosts the Twilio listener: `docs/TELEPHONY.md` 5) |
| `warm` | bool | offer warm transfers: `transfer_call` with `mode = "warm"`, where the receiving party hears a summary first (default `false`). Offered only when the callback listener runs (`BRISVOX_CALLBACK_ADDR`), which finishes a transfer, and every configured carrier binding can do one (a caller id; Telnyx also `BRISVOX_TELNYX_CONNECTION_ID`); otherwise the server logs a warning and offers cold transfers only |
| `voicemail` | table | answering-machine handling, below |

```toml
[telephony]
warm = true
destinations = [
  { name = "front_desk", number = "+15555550100", description = "the clinic's front desk" },
]
```

#### `[telephony.voicemail]`

For an agent that makes outbound calls (`docs/TELEPHONY.md` 6). The carrier's
answering-machine detector classifies the answer, and the verdict reaches the call as the stream
parameter `answered_by` or as a callback to the callback listener. A machine whose greeting
ended is played `message`, then the call is hung up through the carrier; a person gets the
normal conversation. A call that never gets a verdict (an inbound call) is not touched.

| key | type | notes |
|---|---|---|
| `message` | string | required; the fixed message left on a machine, spoken as it is (1 to 1000 characters). Put nothing in it the callee did not consent to hear on a recording |
| `on_unknown` | string | `treat_as_human` (default), `treat_as_machine` (wait for the greeting to end by a fixed time, then leave the message) or `hang_up` |
| `detect_only` | bool | the detector never reports the end of a greeting (Twilio `MachineDetection=Enable`, Telnyx `detect`): hang up on machines instead of speaking over a greeting. Default `false` (Twilio `DetectMessageEnd`, Telnyx `greeting_end`) |

```toml
[telephony.voicemail]
message = "Hello, this is the clinic. Please call us back tomorrow."
on_unknown = "treat_as_human"
```

The bot speaks the message through the engine's say input, so it needs the call's engine to take
lines: with an out-of-process agent (`BRISVOX_AGENT_URL`) the link owns that input and a machine
verdict can only hang up.

### `[agent_link]` (optional)

Where the out-of-process agent of this definition's calls listens (`docs/AGENT_PROTOCOL.md`).
Without the section the server-wide `BRISVOX_AGENT_URL` applies.

```toml
[agent_link]
url = "wss://agent.internal/ws"       # wins over BRISVOX_AGENT_URL
token_env = "AGENT_TOKEN"             # default: BRISVOX_AGENT_TOKEN
# allow_insecure = true               # accept plaintext ws:// to a host outside the private networks
# caller_identity = true              # tell the agent process who is calling (telephony calls)

[[agent_link.routes]]                 # picked by name in a call's start message
name = "billing"
url = "wss://billing-agent.internal/ws"
token_env = "BILLING_AGENT_TOKEN"     # a route never gets the default token
# caller_identity = true              # a route has its own switch
```

`url` and every route `url` must start with `ws://` or `wss://`. A plaintext `ws://` URL whose
host is not on a private network (loopback, RFC 1918, link-local, `localhost`, `*.internal`,
`*.local`, a single-label service name; a resolver's search list can turn the last into a public
name, so it is accepted with a one-time warning) is refused at startup unless `allow_insecure = true`
(on the section, or on one `[[agent_link.routes]]` entry) or `BRISVOX_AGENT_ALLOW_INSECURE=1` is
set (`docs/AGENT_PROTOCOL.md`, "Transport and connection"); route names are 1 to 32 letters,
digits, `_` or `-`, each once, at most 32 routes. A named `token_env` that is not set fails the
start. A call picks a route with the websocket `start` message's metadata key `agent_route`, or
the TwiML parameter `agent_route`; a start message never carries a URL, and an unknown route
falls back to the default process.

`caller_identity` (default `false`, or `BRISVOX_AGENT_CALLER_IDENTITY=1` when the key is not set)
sends the caller's number, the called number and the carrier's call id to the agent process in
`session.start.call` for Twilio, Telnyx and SIP calls (`docs/AGENT_PROTOCOL.md`, "Caller
identity"). A route has its own `caller_identity` and never inherits the section's or the
environment's. The numbers are withheld while the server redacts phone numbers (`docs/PRIVACY.md`).

### `[stt]`, `[llm]`, `[tts]` (required)

Each stage has `provider`, `api_key_env` and an optional options table named after the provider
(`[stt.deepgram]`). An options table for a provider other than the chosen one is an error.

| stage | key | notes |
|---|---|---|
| stt | `provider` | `deepgram`, `assemblyai`, `gladia`, `sherpa_onnx`, `openai_realtime_stt` |
| stt | `api_key_env` | required for `deepgram`, `assemblyai`, `gladia` and the `openai` dialect of `openai_realtime_stt`; optional for `sherpa_onnx` and the local dialects (then it names a bearer token for a proxy, or the server's key) |
| stt | `language` | default `agent.language` |
| llm | `provider` | `openai`, `anthropic`, `gemini`, `openai_compatible` |
| llm | `api_key_env` | required except for `openai_compatible` (a local vLLM/Ollama has no key; set it for a hosted service such as OpenRouter or Groq) |
| llm | `max_tool_rounds` | 1..=32, default 6 |
| llm | `context_window_tokens` | 256..=2000000, default 4000 |
| tts | `provider` | `cartesia`, `elevenlabs`, `deepgram_aura`, `openai_speech` |
| tts | `api_key_env` | required except for `openai_speech` (a local Kokoro/Chatterbox server has no key) |
| tts | `voice` | provider voice id; for Aura the model string (`aura-2-thalia-en`); for `openai_speech` the server's voice (`af_heart` on Kokoro) |
| tts | `sample_rate` | rate the provider delivers, default 24000; the engine resamples to the call |
| tts | `language` | default `agent.language` |

Options tables:

- `[stt.deepgram]`: `model` (nova-3), `endpointing_ms` (0..=10000, 0 = off, 300),
  `utterance_end_ms` (1000..=5000), `smart_format` (true), `keyterms` (<= 100),
  `region` (`global`/`eu`/`au`/`india`), `base_url`.
- `[stt.assemblyai]`: `speech_model` (universal-3-6-pro), `region` (`edge`/`us`/`eu`),
  `keyterms` (<= 100), `format_turns`, `min_turn_silence_ms`, `max_turn_silence_ms` (>= min),
  `base_url`.
- `[stt.gladia]`: `model` (solaria-1), `region` (`us_west`/`eu_west`), `code_switching`,
  `vocabulary` (<= 100), `endpointing_ms` (10..=10000), `base_url`.
- `[stt.sherpa_onnx]` (self-hosted, `docs/providers/sherpa-onnx.md`): `base_url`
  (`ws://host[:port]`, default `ws://127.0.0.1:6006`; any host, no path), `path` (`/`),
  `sample_rate` (the model's rate, 16000), `finalize_timeout_ms` (100..=10000, 1500),
  `finalize_padding_ms` (0..=5000, 300: silence sent before `Done`, 0 for the C++ server). The model
  decides the language. A token (`api_key_env`) is only allowed over `wss://` or to a loopback host.
- `[stt.openai_realtime_stt]` (`docs/providers/openai-realtime-stt.md`): `dialect`
  (`openai`, `nim`, `speaches`, `vllm`; default `openai`), `base_url` (default per dialect),
  `path_and_query`, `model` (required for `speaches` and `vllm`; OpenAI default
  `gpt-live-transcribe`), `prompt` (<= 4096 bytes), `punctuation` (NIM, true), `delta_replaces`,
  `min_commit_ms` (0..=2000), `finalize_timeout_ms` (100..=10000, 2000), `sample_rate`.
- `[llm.openai]`: `model`, `reasoning_effort` (`disabled`/`low`/`medium`/`high`/`xhigh`/`max`),
  `max_output_tokens`, `parallel_tool_calls`, `service_tier`, `base_url`.
- `[llm.anthropic]`: `model`, `max_tokens` (1024), `effort` (`low`..`max`), `thinking`
  (`disabled`/`adaptive`/`between_tools`/`model_default`), `base_url`.
- `[llm.gemini]`: `model`, `thinking_level` (`minimal`/`low`/`medium`/`high`),
  `max_output_tokens`, `temperature` (0..=2), `base_url`.
- `[llm.openai_compatible]` (required for that provider): `preset`
  (`groq`/`together`/`fireworks`/`vllm`/`ollama`/`openrouter`), or `base_url` and `model`; `max_tokens`,
  `temperature` (0..=2), `reasoning_effort` (sent as the request field of that name, e.g.
  `none` or `low` for a reasoning model that thinks by default), `openrouter_order`
  (OpenRouter provider slugs, sent as `provider.order`) with `openrouter_allow_fallbacks`
  (`provider.allow_fallbacks`; `false` = served by the listed provider or refused). Both are
  **OpenRouter-only**: setting either one without `preset = "openrouter"` or a `base_url` whose host
  is `openrouter.ai` (or a subdomain) is a configuration **error** (`brisvox-server check`
  prints it with the line of the key and exits 1), because any other server would silently ignore
  the pin or refuse the request. A hosted service
  without a preset is this table with `base_url` and `model`. **OpenRouter** has the preset
  `openrouter` (`base_url = "https://openrouter.ai/api/v1"`, default model `openai/gpt-6-luna`):
  `preset = "openrouter"`, optionally `model = "<author>/<model>"` (the id from OpenRouter's model
  page) and `api_key_env` naming the variable that holds your OpenRouter key; the full recipe is
  "OpenRouter" in `docs/PROVIDERS.md`.
- `[tts.cartesia]`: `model` (sonic-3.6), `max_buffer_delay_ms` (0..=5000), `url`.
- `[tts.elevenlabs]`: `model` (eleven_flash_v2_5), `stability`, `similarity_boost`, `style`
  (0..=1), `speed` (0.7..=1.2), `use_speaker_boost`, `url`.
- `[tts.deepgram_aura]`: `speed` (0.7..=1.5), `url`.
- `[tts.openai_speech]` (required for that provider; a local server speaking
  `POST {base_url}/audio/speech`, docs/providers/openai-speech.md): `base_url` (required, with the
  `/v1` prefix; `http://` to any host is accepted, but not together with an `api_key_env`),
  `preset` (`generic` default, `kokoro`, `chatterbox`, `speaches`, `vllm_omni`; sets the model,
  the audio format and the request fields), `model` (required for `generic`), `speed`
  (0.25..=4), `format` (`pcm` or `wav`), `first_audio_timeout_ms` (500..=120000, default 15000),
  `warm_text` (text of the background model warm-up request started at call start, audio
  discarded; `""` = off; default `"Hi."` for `kokoro`, off for the other presets),
  `warm_interval_ms` (1000..=3600000, 60000: at most one warm-up per interval across all calls).
  The server delivers 24 kHz: leave `tts.sample_rate` unset or set it to exactly `24000` (the
  example agents under `examples/agents/local/` set it, which is the same thing written out);
  any other value is a load error, because only the `speaches` preset can resample on the server.
  The engine resamples 24 kHz to the call's rate itself. `examples/agents/local-kokoro.toml` is a complete agent.

### Secondary providers (`[stt.secondary]`, `[llm.secondary]`, `[tts.secondary]`)

Optional. A second provider of the same stage that calls fail over to when the primary is down
(the mechanism is described in `docs/PIPELINE.md`, "Resilience layer"): a circuit
breaker per provider is shared by all calls, a failed connect or a session that dies is retried on
the other provider within the call's and the process's retry budgets, and an LLM request that
failed before any content is re-issued to the secondary. Without a secondary every call still
runs behind the breaker, the retry budgets and the TTS idle refresh; it just has nowhere to go.

A secondary table takes the same keys as its stage (`provider`, `api_key_env`, the options table
named after the provider; `voice` for TTS), so it can be a different provider or the same one with
another key or region. These keys belong to the call and come from the primary's table, so a
secondary may not set them: `language` (STT, TTS), `sample_rate` (TTS), `max_tool_rounds` and
`context_window_tokens` (LLM), and `secondary` itself. A TTS secondary needs its own `voice`: a
voice id belongs to one provider, and the engine's voice id is the primary's. The secondary is
checked like a primary against the call's language and rate (for example, a Turkish call cannot
name a secondary Deepgram Aura voice, and Aura must deliver the primary's `sample_rate`).

Its key is read from its own `api_key_env` at startup: a missing key stops the start like a
primary's. Keys are never in the file. `examples/agents/failover-en.toml` (and `.yaml`) shows all
three:

```toml
[stt]
provider = "deepgram"
api_key_env = "DEEPGRAM_API_KEY"

[stt.secondary]
provider = "assemblyai"
api_key_env = "ASSEMBLYAI_API_KEY"

[tts.secondary]
provider = "elevenlabs"
api_key_env = "ELEVENLABS_API_KEY"
voice = "voice-id-at-elevenlabs"
```

Endpoint overrides are checked while the file is read, at the value's position, with the rule the
adapter applies when it is built: Deepgram and AssemblyAI STT `base_url` and every TTS `url` are
WebSocket URLs (`wss://`), Gladia STT and every LLM `base_url` are HTTP URLs (`https://`). The
plain scheme (`ws://`, `http://`) is accepted only to a loopback host (`localhost`, `127.0.0.x`,
`::1`), since the API key would otherwise travel unencrypted. Messages never repeat the URL (it may
carry credentials). Path and query rules (some adapters take a base URL without a path) are still
checked by the adapter.

Not exposed on purpose: STT/TTS audio encodings (the engine always sends 16 kHz PCM16 to STT and
expects PCM16 from TTS, resampling at the edges) and the adapters' internal timeouts and replay
settings (their defaults are tuned in the adapters; change them in code if needed).

Language subtags: Deepgram STT receives the tag as written (`tr-TR` is accepted there);
AssemblyAI, Gladia, Cartesia and ElevenLabs receive the primary subtag (`tr`).

## WebRTC reachability (environment variables)

Not part of the agent file (`[turn]` above is turn-taking, unrelated to TURN relays): these are
process environment variables read by `brisvox-server` with the `webrtc` feature, next to the
`BRISVOX_WEBRTC_*` ones in `docs/OPERATIONS.md`. How to choose and deploy: `docs/DEPLOY.md`
"WebRTC: making the server reachable from browsers"; protocol facts and sources:
`docs/providers/webrtc.md` section 11. A bad value refuses to start and names the variable.

| Variable | Default | Meaning |
|---|---|---|
| `BRISVOX_WEBRTC_WS_PATH` | `/ws` | Path of the WebSocket fallback on the signaling port, or `off`. Starts with `/`, no query or trailing `/`, not under `/whip`. A WebSocket upgrade there is authenticated like `POST /whip` (same static token or session tokens, audience `brisvox-webrtc`; `Authorization: Bearer`, the subprotocol `brisvox-token.<token>` or `?token=`), judged by the `Origin` rule (`BRISVOX_WEBRTC_CORS_ORIGIN`, else the page's own host), and counted against the same offer rate, per-address call cap, handshake pool and call limit. Bounds: HTTP head 8 KiB, frames 64 KiB audio / 16 KiB control, `start` within 5 s, pings every 10 s. |
| `BRISVOX_WEBRTC_TCP_ADDR` | unset (`off`) | Address of the ICE-TCP listener, e.g. `0.0.0.0:50000`. Adds a passive TCP host candidate (RFC 6544; RFC 4571 framing) to every answer; UDP stays preferred. Needs `BRISVOX_WEBRTC_CANDIDATE_IP` when bound to an unspecified address. |
| `BRISVOX_WEBRTC_TCP_CANDIDATE_PORT` | the listener's port | Port written into the TCP candidate when a proxy/port mapping in front of the server uses another (1 to 65535). |
| `BRISVOX_WEBRTC_TCP_MAX_CONNECTIONS` | `256` | ICE-TCP connections served at once (1 to 65536); more are closed unanswered. |
| `BRISVOX_WEBRTC_TCP_MAX_PER_IP` | `16` | ICE-TCP connections one source IP may hold (`0` = no limit). Set `0` when a TCP proxy sits in front of the server: every peer then has the proxy's address and the limit would apply to all browsers together. |
| `BRISVOX_WEBRTC_TCP_IDLE_TIMEOUT` | `10s` | How long a connection may take to name a live call (its first STUN request) before it is closed. The deadline is absolute; junk does not extend it. |
| `BRISVOX_WEBRTC_TCP_MAX_FRAME` | `2048` | Largest RFC 4571 frame in bytes (512 to 65535); a larger one closes the connection. |
| `BRISVOX_WEBRTC_TCP_QUEUE` | `20` | Frames queued per connection towards the browser (8 to 4096, drop-oldest). A writer stuck longer than the write timeout closes the connection. |
| `BRISVOX_TURN_PROVIDER` | unset (`off`) | `coturn`, `cloudflare` or `twilio`. Unset: the server offers no relay and `GET /whip/ice` returns `{"iceServers":[]}`. |
| `BRISVOX_TURN_TTL` | `600` | Lifetime of issued credentials, seconds, 30 to 86400. Fetched credentials are cached for 60 s (at most half of it). |
| `BRISVOX_TURN_URLS` | required for `coturn` | Comma-separated `turn:` / `turns:` URLs of your coturn server (up to 16). |
| `BRISVOX_TURN_SECRET` | required for `coturn` | coturn's `static-auth-secret` (`use-auth-secret` mode). Never sent to browsers, never logged. |
| `BRISVOX_TURN_CLOUDFLARE_KEY_ID` | required for `cloudflare` | The TURN key id (Cloudflare dashboard > Realtime > TURN). |
| `BRISVOX_TURN_CLOUDFLARE_API_TOKEN` | required for `cloudflare` | The TURN key API token. |
| `BRISVOX_TURN_TWILIO_ACCOUNT_SID` | required for `twilio` | The account SID (in the request path). |
| `BRISVOX_TURN_TWILIO_AUTH_TOKEN` | required for `twilio` | The account auth token, or the API key secret when `BRISVOX_TURN_TWILIO_API_KEY_SID` is set. |
| `BRISVOX_TURN_TWILIO_API_KEY_SID` | unset | Authenticate with this API key SID instead of the account SID. |
| `BRISVOX_WEBRTC_TURN_RELAY` | unset (`off`) | The server's own TURN relay: `on`, `only` or `off`. Each call opens a TLS (or TCP) connection to the TURN service of `BRISVOX_TURN_PROVIDER`, allocates a relayed address and advertises it as a host candidate. `on`: next to the direct candidates (lower priority; a failed allocation is logged and the call answers normally). `only`: no direct UDP candidate, `BRISVOX_WEBRTC_CANDIDATE_IP` not required, a failed allocation refuses the offer with `503`. Requires `BRISVOX_TURN_PROVIDER`. For hosts without UDP ingress (a one-port PaaS such as Railway): `docs/DEPLOY.md`. |
| `BRISVOX_WEBRTC_TURN_RELAY_TIMEOUT` | `3s` | Longest the relay setup of one call may take (credentials, DNS, TCP, TLS, allocate, permissions). |

Library-level knobs of the server's relay (`RelayConfig`, no environment variable): `max_permissions`
(8: offer IPs a permission is created for), `queue` (20 packets per direction, 400 ms of audio, drop-oldest;
8 to 4096), `allow_local_peers` (false: loopback and link-local offer IPs get no permission; tests only),
`max_frame` (4096: a larger or malformed frame from the TURN server ends that allocation),
`write_timeout` (2 s), `release_timeout` (1 s: the `Refresh` of lifetime 0 at call end) and
`extra_roots` (DER certificates trusted next to the compiled-in webpki roots, for a private TURN CA).
Counters: `relay_*` in `WebRtcStats` (including the queue dwell and slow-write high-water marks). The
TURN client crates (`turn_client_proto`, `stun_proto`, `turn_types`) are held at `warn` **whatever
`RUST_LOG` says**; `BRISVOX_LOG_TURN_CLIENT=1` lifts that (debugging only: at `trace` they log whole
STUN messages with the nonce and the response's integrity HMAC). The SIP stack (`rsipstack`) is held
at `warn` the same way (it logs whole SIP messages at `info`, SDES SRTP master keys included);
`BRISVOX_LOG_SIP_STACK=1` lifts it (`docs/TELEPHONY.md`, "SIP over TLS and SRTP"). The log writer
also removes the key of any SDES `a=crypto:... inline:<base64>` line, whoever wrote it. The
SIPS idle timeout (`BRISVOX_SIP_TLS_IDLE_TIMEOUT`, seconds, default 300, `0` = off) and the SRTP
authentication-failure limit (`BRISVOX_SIP_SRTP_AUTH_FAIL_LIMIT`, per call and second, default 50,
`0` = off) are in `docs/OPERATIONS.md`; library defaults are `SipTlsConfig::idle_timeout` and
`SipConfig::srtp_auth_failure_limit`.

Library-level knobs (no environment variable, defaults in `WebRtcConfig`): `turn.cache` (60 s,
at most half the TTL) and `turn.fetch_timeout` (5 s) for the credential fetch. `POST /whip` waits
at most 1.5 s for credentials (a provider outage cannot stall an offer: it gets the last
credentials that still have a quarter of their life left, or none); `GET /whip/ice` waits the full
fetch timeout.

**Which endpoint hands out credentials.** The bundled pages read `GET /whip/ice` before
`createOffer`, because the `Link: rel="ice-server"` headers of the WHIP `201` arrive after the
offer is built (a page that wants them must create a second peer connection). The `Link` headers
are there for standard WHIP clients (RFC 9725 section 4.6).

**Security: who may mint relay credentials.** Both endpoints issue credentials to whoever asks.
Without `BRISVOX_WEBRTC_TOKEN` or `BRISVOX_SESSION_SECRET` (allowed only on a loopback address or
with `BRISVOX_INSECURE_LISTENERS=1`), anyone who can reach the signaling port can obtain
TURN credentials: for `coturn` they are computed locally and valid for the whole TTL, and every byte
relayed with them is yours (bandwidth cost, abuse as an open relay for that TTL). `GET /whip/ice`
is rate limited per client address (a burst of 5, then one request per 2 s; IPv6 clients by /64;
`429` with `Retry-After`), which bounds minting but does not authenticate it. For `coturn` a request
that carried a session token gets the username `<expiry>:<label>` with the label the first 16 hex
digits of SHA-256 of the token's `sid`, so relay traffic in coturn's logs traces back to the session
your backend issued; without a session token the label is a random id. **Cloudflare and Twilio
credentials are fetched once and shared by every caller** for the cache period (60 s): the provider
cannot tell callers apart, and a visitor who copies them can use them until they expire. Use coturn
when relay usage must be attributable. When relay traffic
costs money, set `BRISVOX_WEBRTC_TOKEN` (the page then needs the token, so serve it from your own
backend), keep the TTL short, and use the provider's usage caps. Behind a reverse proxy every
client has the proxy's address: the limiter then applies to all of them together, so put the limit
at the proxy as well.

## Process environment: security

Authentication and flood limits are infrastructure, not agent settings, so they are environment
variables of the server process, not keys of the agent file. They are all described in
`docs/SECURITY.md` and listed in `docs/OPERATIONS.md`:

| Variable | Meaning |
|---|---|
| `BRISVOX_SESSION_SECRET` / `BRISVOX_SESSION_SECRET_PREV` | Secret (at least 32 bytes) that signs the short-lived single-use session tokens of the generic websocket, RTVI and WebRTC listeners, and the previous one still accepted during a rotation. |
| `BRISVOX_WS_TOKEN` | Static token of the generic websocket listener (development). The RTVI, WebRTC and Telnyx ones are `BRISVOX_RTVI_TOKEN`, `BRISVOX_WEBRTC_TOKEN`, `BRISVOX_TELNYX_STREAM_TOKEN`. |
| `BRISVOX_INSECURE_LISTENERS` | Lets those listeners bind a non-loopback address without any of the above (logged loudly), and serves `/metrics` without a token. |
| `BRISVOX_TRUSTED_PROXIES` | CIDR list of reverse proxies whose forwarding header the WebRTC listener believes (never `0.0.0.0/0`). |
| `BRISVOX_CLIENT_IP_HEADER` | The one forwarding header believed behind them: `x-forwarded-for` (default), `forwarded` or `x-real-ip` (one address; Railway). |
| `BRISVOX_WEBRTC_HTTP_MAX_PER_IP` | Signaling connections one address may hold (default 8, `0` = no limit; trusted proxies exempt). |
| `BRISVOX_WEBRTC_MAX_CALLS_PER_IP` / `BRISVOX_WEBRTC_MAX_HANDSHAKING` / `BRISVOX_WEBRTC_OFFERS_PER_MINUTE` | WebRTC flood limits: calls per address, calls still handshaking, offers per address per minute. |
| `BRISVOX_METRICS_TOKEN` | Bearer token `/metrics` requires when the health address is not loopback. |

A session token's `agent` and `meta` claims arrive in the call's metadata (`session.agent`,
`session.meta.<key>`); the agent file is chosen by `BRISVOX_AGENT_FILE` as before.

### The agent definition from the environment

A host that takes environment variables but no file mount (a one-port PaaS such as Railway) can
carry the definition itself in `BRISVOX_AGENT_DEFINITION`: the text of the TOML file. It goes
through the same loader as the file (same 1 MiB limit, same checks and messages; errors say
`$BRISVOX_AGENT_DEFINITION:LINE:COL`), and it is mutually exclusive with `BRISVOX_AGENT_FILE`
(both set is a startup error). `BRISVOX_AGENT_FORMAT=yaml` says the text is YAML (default `toml`;
the format is named, never guessed). Secrets still do not go into it: the definition names the
variables that hold them with `api_key_env`, and those are variables of their own. The operating
system limits one variable well below 1 MiB (about 128 KiB on Linux, about 32 KB on Windows); an
agent definition is a few kilobytes.

```sh
BRISVOX_AGENT_DEFINITION='
[agent]
name = "demo"
language = "en"
system_prompt = "You are a helpful voice assistant."
[stt]
provider = "deepgram"
api_key_env = "DEEPGRAM_API_KEY"
[llm]
provider = "openai_compatible"
api_key_env = "OPENROUTER_API_KEY"
[llm.openai_compatible]
preset = "openrouter"
model = "openai/gpt-6-luna"
reasoning_effort = "none"
[tts]
provider = "cartesia"
api_key_env = "CARTESIA_API_KEY"
voice = "a-cartesia-voice-id"
'
```

`brisvox-server check --env` validates what the server would start with (the variable, or the
file `BRISVOX_AGENT_FILE` names) and prints the same report as `check <file>`.

### Port placeholder in listener addresses

The variables that bind a listener (`BRISVOX_WS_ADDR`, `BRISVOX_TWILIO_ADDR`,
`BRISVOX_TELNYX_ADDR`, `BRISVOX_RTVI_ADDR`, `BRISVOX_SIP_ADDR`, `BRISVOX_SIP_TLS_ADDR`,
`BRISVOX_WEBRTC_ADDR`, `BRISVOX_WEBRTC_UDP_ADDR`, `BRISVOX_WEBRTC_TCP_ADDR`,
`BRISVOX_CALLBACK_ADDR`, `BRISVOX_HEALTH_ADDR`) accept the exact token `${PORT}`, replaced by the
value of the `PORT` variable that PaaS hosts set: `BRISVOX_WS_ADDR=[::]:${PORT}`. Nothing else is
expanded (`$PORT` and `${port}` are not). If the token is used and `PORT` is unset or not a number
from 1 to 65535, the start fails with a message naming both variables.

## Cross-field checks

| rule | reported at |
|---|---|
| the chosen provider's cargo feature is compiled in | `<stage>.provider` |
| no options table for another provider | `<stage>.<other>` |
| `agent.language` is `en` or `tr` | `agent.language` |
| Deepgram Aura speaks the TTS language (en, es, de, fr, nl, it, ja; no Turkish) | `tts.language` or `agent.language` |
| an Aura voice's language suffix matches the TTS language | `tts.voice` |
| Aura delivers the `tts.sample_rate` (8000, 16000, 24000, 48000) | `tts.sample_rate` |
| `tts.api_key_env` present (except `openai_speech`); `openai_speech` has `base_url` (and a `model` for `generic`), no key over plain `http://` to a remote host, and `tts.sample_rate` 24000 (except `speaches`) | `tts.*` |
| `eleven_flash_v2` only with English | `tts.elevenlabs.model` |
| Turkish STT: Deepgram nova-3/nova-2/base, AssemblyAI `*-pro` models | the model key, or the language |
| `turn` keys match the policy; `silence_ms` with `fixed_silence` | the key |
| early analysis needs the adaptive policy | `early_analysis.enabled` |
| `final_wait_ms` < STT finalize timeout; `response_deadline_ms` > `final_wait_ms` | the key |
| `frame_ms` whole samples at 16 kHz and `call_sample_rate` | `audio.frame_ms` |
| `filler.delay_ms` not given with `filler.per_tool = true` | `filler.delay_ms` |
| `recording.retention_days` 1 to 3650; `recording.redact_kinds` not empty and not given with `recording.redact = false` | the key |
| `stt.api_key_env` present (except `sherpa_onnx` and the local dialects); `speaches`/`vllm` have a `model`; a token is never sent over plain `ws://` to another machine | `stt.*` |
| tool names valid and unique; keyterm/vocabulary lists bounded | the entry |
| `llm.api_key_env` present (except `openai_compatible`); compatible server has a preset or URL+model | `llm.*` |
| a secondary is checked like a primary (same rules, with the primary's language and rate) | `<stage>.secondary.*` |
| a secondary sets no `language`, `sample_rate`, `max_tool_rounds`, `context_window_tokens` and has no `secondary` | the key |

The provider facts behind these rules come from `docs/providers/*.md` (fetched 2026-10).

## Provider features

`brisvox-server` features `provider-deepgram`, `provider-assemblyai`, `provider-gladia`,
`provider-sherpa-onnx`, `provider-openai-realtime-stt`,
`provider-openai`, `provider-anthropic`, `provider-gemini`, `provider-openai-compatible`,
`provider-cartesia`, `provider-elevenlabs`, `provider-deepgram-aura`, `provider-openai-speech`, and
`all-providers`. None is
on by default (light builds); a definition naming a provider whose feature is off is rejected with
the feature to enable. `SttProviderConfig` / `LlmProviderConfig` / `TtsProviderConfig` have one
variant per compiled adapter.

## Why TOML is the reference and how YAML is read

TOML came first: the definition types carry `toml::Spanned` positions for the cross-field
checks, so YAML needed its own span story, not just a second `from_str`. YAML is read like this:

- The `serde_yaml` successors (checked on crates.io 2026-10-13: `yaml_serde` 0.10.7 by The YAML
  Organization, MIT OR Apache-2.0, last release 2026-08; `serde_yaml_ng` 0.10.0, MIT, last release
  2024-05; `serde_norway` 0.9.42, last release 2024-12; `serde_yaml` itself 0.9.34+deprecated) are
  serde-only: a position appears in an error but never on a value, so a cross-field check (Turkish
  on Aura at `agent.language`) could not say where the value is. They also cannot fill `Spanned`.
- `saphyr-parser` 0.2.1 (MIT OR Apache-2.0, pure safe Rust, YAML 1.2, active: releases 2026-07 to
  2026-10) emits events with line and column. `config::yaml` builds a small tree with byte ranges
  from them and runs the derived `Deserialize` of `AgentFile` over it; `Spanned<T>` is filled
  through `serde_spanned::de` (the same crate `toml::Spanned` comes from, already in the tree).
  So both formats produce the same `AgentFile`, and the cross-field checks, secret lookups and
  mappings are shared code with shared messages; parity tests load each broken TOML definition
  next to its YAML form and compare key paths and messages.
