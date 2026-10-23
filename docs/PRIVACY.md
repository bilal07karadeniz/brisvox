# Recording and privacy

Code: `brisvox_server::privacy`; the engine seam is `brisvox_engine::CallerAudioTap`
(`SessionBuilder::with_caller_audio_tap`). This page says what the server can write to disk, how to
turn it on, what the redaction does and does not do, and what an operator still has to decide.
It is engineering documentation, **not legal advice**: laws on recording calls differ by country
and by who the caller is, and the checklist at the end is a prompt for the conversation with your
own counsel, not a statement of what the law requires.

## What is recorded

**Nothing, by default.** Recording is on only when `BRISVOX_RECORDING_DIR` is set. Per call it can
write up to three files into that directory, which must be **dedicated to the recordings** (see
"Retention"):

| File | Holds | On by default once the directory is set |
|---|---|---|
| `call-<unix start>-<session>.transcript.jsonl` | A header line, one line per user turn and per bot response, an end line. | yes |
| `call-<unix start>-<session>.caller.wav` | The caller's audio, mono PCM16, at the call's own rate (8 kHz telephony, 16 kHz websocket, ...). | **no** |
| `call-<unix start>-<session>.bot.wav` | The bot's audio, mono PCM16, at the TTS rate, laid out on the call's timeline. | **no** (same switch) |

The file name carries the call's start (unix seconds) and the server's session counter, nothing
about the caller: no phone number, name or stream id is written anywhere by this feature. If your
deployment can map a session to a caller (the agent link, telephony metadata), the files become
linkable to a person through that mapping and are personal data either way.

### Transcript format

JSON lines, UTF-8:

```json
{"type":"call","version":2,"session":17,"agent":"support","started_unix":1760000000,"pii_redacted":true}
{"type":"user","t_ms":4210,"text":"my email is [EMAIL] and my number is [PHONE]"}
{"type":"bot","t_ms":6120,"text":"Thanks, I noted [EMAIL].","interrupted":false,"heard":"full","cut":false}
{"type":"user","t_ms":9800,"text":"stop, only the weekday hours"}
{"type":"bot","t_ms":10450,"text":"We open at nine.","interrupted":true,"heard":"partial","estimated":false,"round":0,"barge_in_ms":9300,"cut":false}
{"type":"end","t_ms":31800,"events_lost":0,"summary":{"duration_ms":31900,"turns":2,"interrupted_turns":1,"latency":{"response":{"avg_ms":812.5,"max_ms":905.0}},"usage":{"llm":{"provider":"openai","model":"gpt-6-luna","input_tokens":1840,"cached_input_tokens":1024,"output_tokens":96,"responses_without_usage":0},"stt":{"provider":"deepgram","model":"nova-3","audio_seconds":31.5},"tts":{"provider":"cartesia","model":"sonic-3.6","characters":212}}}}
```

- `t_ms` is milliseconds since the recording of the call started, on the server's clock.
- `user` lines hold the whole text the LLM answered for that turn (spoken finals and typed text);
  interim hypotheses are not recorded.
- `bot` lines hold **only what the caller heard**, never the text that was merely sent to speech
  synthesis (a barge-in leaves sentences nobody heard; writing them would put words in the
  caller's file that were never delivered). See "What a bot line holds" below.
- `events_lost` counts conversation events the writer fell too far behind for (a stalled disk).
  Normally `0`.
- `summary` on the `end` line is the call's usage summary, the same object the agent link sends as
  `session.usage` (`docs/AGENT_PROTOCOL.md`, "Call summary"): the duration, the turns and
  interrupted turns, the average and worst latency of each stage, and the provider units the call
  used (LLM tokens, STT audio seconds, TTS characters, with the configured provider and model).
  Numbers and provider names only: no conversation text and nothing about the caller. It is an
  additive member (the format stays `version` 2; a reader that does not know it ignores it) and
  is absent when the call ended before the server could build it.

### What a bot line holds

| `heard` | Written when | `text` |
|---|---|---|
| `full` | the transport reported the response played to its end (`BotStoppedSpeaking`), or the engine's answer says the whole round was played | the response's text (`"cut": true` if a very long one was truncated at 32 KiB) |
| `partial` | the response was interrupted (`"interrupted": true`) and the engine settled what was played | the heard prefix only, from the same played-text truncation (`PlayoutBook`) that cuts the LLM's own context, so the transcript and the LLM agree on what the caller heard; `"estimated": true` when it is a speaking-rate estimate because the TTS sent no word timestamps |
| `unknown` | the heard extent is not known: the engine had no answer, the answer or a conversation event was lost (`events_lost`, `RecordingStats::heard_lost`), or the call ended before the engine settled | empty. The unheard text is never written as a guess |

The engine settles what was played when the caller's next turn ends, or at the end of the call,
so the line of an interrupted response is written then (a `partial` or `unknown` bot line can
appear after the following `user` line; `barge_in_ms` says when the barge-in itself happened,
and there is one line per LLM round, with `round`). A response whose BotInterrupted event was
lost is never written as complete: without the transport's confirmation of the end, only the
engine's answer or `unknown` is written. Nothing is held back: text that is not going to be
written is dropped from memory as soon as its response ends unconfirmed.


### Audio format

Two separate mono WAV files, so each side keeps its native rate and nothing is mixed or
resampled. Both start at the call's start (to within one frame); the bot file carries silence
between responses and **only the audio that was due to be heard**: when the caller barges in,
what the bot had queued but not yet played is cut from the file (to the accuracy of the
transport's pacing). Missing caller frames (the engine drops the oldest audio when it falls
behind) are filled with silence of the missing length, so timing holds.

The header is written with the streaming length first and rewritten with the true lengths at the
end of the call, so a file left by a crash still plays (decoders read it to the end of the
file). Recorded audio per call and track is capped (`BRISVOX_RECORDING_MAX_MINUTES`). Samples
are written in batches of 64 KiB (one disk operation per batch, not one per 20 ms frame), so a
crash loses at most the last 64 KiB of a track. Only whole 16-bit samples are written: a frame
with an odd byte count loses its last byte (counted), so the rest of the file never shifts.

## Encryption at rest

Optional, and compiled in only on request: build `brisvox-server` with the cargo feature
`recording-encryption` (it adds the `age` crate, 0.12, MIT OR Apache-2.0, and a larger crypto
dependency tree, which is why it is off by default) and set `BRISVOX_RECORDING_AGE_RECIPIENTS` to
one or more age X25519 **public** keys, comma-separated. Every recording file of every call (the
transcript and both WAV files) is then written as a standard [age](https://age-encryption.org)
file with the extra suffix `.age`, for example `call-1760000000-17.transcript.jsonl.age`. **The
server never holds a private key**: it can write recordings but not read them back, so a stolen
disk, backup or volume snapshot exposes nothing without the key you keep elsewhere. No plain file
is written next to it.

Make a key pair on a machine you trust, not on the server:

```sh
age-keygen -o key.txt          # prints "Public key: age1..." ; key.txt holds the private key
```

Put only the `age1...` line on the server (`BRISVOX_RECORDING_AGE_RECIPIENTS=age1...`; several keys
let several people, or a backup key, decrypt). Keep `key.txt` offline or in your secret manager;
anyone who has it can read every recording made for its public key. Losing it loses the
recordings. Decrypt with the stock tool:

```sh
age -d -i key.txt call-1760000000-17.transcript.jsonl.age > transcript.jsonl
age -d -i key.txt call-1760000000-17.caller.wav.age > caller.wav
```

Startup checks: a value that is not an `age1...` X25519 recipient, a private key pasted by mistake
(`AGE-SECRET-KEY-...`, refused without being printed), more than 16 recipients, or the variable set
in a build without the feature (the error names the feature) all stop the start. A blank
`BRISVOX_RECORDING_AGE_RECIPIENTS` (set but empty) means plain recordings, exactly as if it were
unset. Like every recording variable it needs `BRISVOX_RECORDING_DIR`.

Behaviour to know:

- **A WAV file keeps the streaming header** (length fields `0xFFFFFFFF`): an encrypted stream
  cannot be rewritten at the end of the call, so the true lengths are never patched in. Players and
  converters (ffmpeg, sox) read to the end of the file; `ffmpeg -i caller.wav -c copy fixed.wav`
  writes a file with proper lengths if a tool insists on them.
- **An unfinished file is not fully readable.** age releases a 64 KiB chunk only when more data
  follows it, and writes the final chunk with an end mark when the file is closed. A call whose
  process crashes leaves an `.age` file that decrypts up to the last full chunk and then fails with
  a truncation error. A transcript is a few KiB, so it is typically lost entirely; a WAV loses its
  tail from the last full 64 KiB chunk. (Plain files lose at most the last 64 KiB of a WAV.) Files
  of a call that ended normally, including one cut by an orderly shutdown (the server finishes the
  recordings first), are complete. The one exception is the 15 s finish timeout at shutdown: a
  recording that has not finished by then is aborted and its encrypted file is truncated like
  after a crash.
- **Cost.** The encryption of each batch runs on the blocking pool, never on an async worker, and
  every disk operation keeps its timeout; an encryption that stalls past the timeout stops that
  file's recording like a failed write (counted, logged once) and never the call.
- **Retention** treats `X.age` exactly like `X`: the sweeper deletes `call-*.transcript.jsonl.age`,
  `call-*.caller.wav.age` and `call-*.bot.wav.age` by age, and still touches nothing else.
- Encryption protects files at rest. The server process still sees the plaintext in memory while
  the call runs, and the redaction (above) still applies before the transcript is encrypted.

## Settings

Two sources, the agent file winning over the environment:

1. **Environment variables** (`docs/OPERATIONS.md` has the full table). An invalid value, or a
   recording variable set without `BRISVOX_RECORDING_DIR`, stops the start with a message naming
   the variable: a privacy setting is never silently ignored.
2. **The agent file's `[recording]` table** (TOML and YAML, `docs/CONFIG.md`): `transcript`,
   `audio`, `redact`, `redact_kinds`, `retention_days`. A key set there wins over the matching
   variable. The directory is environment-only. A `[recording]` table with any key but no
   `BRISVOX_RECORDING_DIR` is an error at startup.

| Variable | File key | Default | Meaning |
|---|---|---|---|
| `BRISVOX_RECORDING_DIR` | - | unset (nothing recorded) | Directory for the files; created at startup. **Use a directory dedicated to the recordings.** |
| `BRISVOX_RECORDING_TRANSCRIPTS` | `transcript` | `true` | Write the transcript. |
| `BRISVOX_RECORDING_AUDIO` | `audio` | `false` | Write the two WAV files. Audio cannot be redacted. |
| `BRISVOX_RECORDING_REDACT_PII` | `redact` | `true` | Redact the transcript text before it is written. |
| `BRISVOX_RECORDING_REDACT_KINDS` | `redact_kinds` | all six | Which kinds the redaction replaces: `email,iban,id,card,phone,number`. |
| `BRISVOX_RECORDING_RETENTION` | `retention_days` | `30d` | Delete recordings this old (`90s`, `15m`, `36h`, `30d`; at least `1h` and at least the audio cap; `forever` to keep them; the file takes whole days, 1 to 3650). |
| `BRISVOX_RECORDING_SWEEP_INTERVAL` | - | `1h` | How often the sweeper runs (at least `1m`). |
| `BRISVOX_RECORDING_MAX_MINUTES` | - | `120` | Longest audio kept per call and track (1 to 600). |
| `BRISVOX_RECORDING_AGE_RECIPIENTS` | - | unset (plain files) | Comma-separated age X25519 public keys (`age1...`): every recording file is written encrypted as `<name>.age`. Needs the cargo feature `recording-encryption` (a start error otherwise). See "Encryption at rest". |

The first versions of the three switches were named `BRISVOX_RECORD_TRANSCRIPTS`,
`BRISVOX_RECORD_AUDIO` and `BRISVOX_REDACT_PII`. They still work as aliases of the
`BRISVOX_RECORDING_*` names above; setting both names to different values is an error.

The server logs, once at startup, whether recording is on and with which switches, and warns
when audio is recorded or retention is `forever`.

## PII redaction

With redaction on (`BRISVOX_RECORDING_REDACT_PII=true`, the default) every `user` and `bot` line
goes through `redact_pii` **before** it is serialized: the original text never reaches the disk.
`redact_kinds` narrows it to some of the kinds below (turning a kind off leaves that kind in the
file; an empty list is refused). It replaces:

| What | Rule | Placeholder |
|---|---|---|
| Email address | `local@domain.tld` | `[EMAIL]` |
| IBAN | two letters, two digits, up to 30 more characters (single spaces allowed), ISO 7064 mod-97 checksum | `[IBAN]` |
| Turkish national ID (TC kimlik no) | 11 digits with the two official check digits | `[ID]` |
| Payment card number | 13 to 19 digits (spaces, dashes, dots between groups), Luhn checksum | `[CARD]` |
| Phone number | 7 to 15 digits (leading `+`, spaces, dashes, dots, parentheses) | `[PHONE]` |
| Any other long number | 16 or more digits | `[NUMBER]` |

It leaves dates (`2026-10-13`, `13.10.2026`), digits inside words and codes (`A1234567890`),
and numbers of fewer than seven digits alone. It errs toward redacting: an amount like
`1.234.567` or any other run of seven or more digits is replaced even though it is not a phone
number.

### What redaction does not do

- **Audio is never redacted.** `caller.wav` and `bot.wav` contain everything said. If you record
  audio, the transcript's redaction protects nothing that the audio holds. The server warns about
  this at startup.
- **Digits spoken as words** ("five three two ...") are not recognised, nor are spelled-out
  emails ("jane at example dot com"), unless the STT already wrote them as digits and symbols.
- **Names, street addresses, dates of birth, health and other free-text details** are not
  detected. There is no named-entity model in the redactor.
- **Logs are scrubbed of credentials only, unless you opt in** (next section).
- It is a pattern filter with checksum validation for IBANs, IDs and cards, not a proof. Treat
  the redacted transcript as reduced-risk data, not as anonymous data.

## Logs

Log lines never contain transcript text or audio: they carry counts, file names and error text.
Provider error messages and a few diagnostics can still echo user-derived text, so logs have an
opt-in PII scrub on top of the always-on credential scrub (`docs/TRACING.md`):

| Variable | Default | Meaning |
|---|---|---|
| `BRISVOX_LOG_REDACT_PII` | `false` | Also replace emails, IBANs, national IDs, card and phone numbers in every log line, with the same filter and placeholders as the transcripts. |
| `BRISVOX_LOG_REDACT_KINDS` | `email,iban,id,card,phone` | Narrow or widen the kinds (`number` also replaces any 16+ digit run). Setting it without `BRISVOX_LOG_REDACT_PII=true` is an error. |

It is off by default because the filter errs toward redacting: a counter or a duration of seven
or more digits (`elapsed_ns=1234567`) is shown as `[PHONE]`. Turn it on where logs leave the
trust boundary (a hosted log service) and accept the noise.

## The caller's number and the agent process

An SDK agent (`docs/AGENT_PROTOCOL.md`) does not learn who is calling unless you ask: the
caller's number, the called number and the carrier's call id reach it in `session.start.call`
only when the definition sets `[agent_link] caller_identity = true` (or the default process is
opted in with `BRISVOX_AGENT_CALLER_IDENTITY=1`). A route to another agent process has its own
switch and never inherits it. It is the transfer of a personal identifier to another process
(and possibly another team), so it is off by default and the agent process becomes part of your
data map.

The redaction settings above are respected: while the server redacts phone numbers, the numbers
are not given to the agent process either. That is the case when recording is on with redaction
enabled and the `phone` kind (or the `number` kind: it replaces only digit runs of 16 or more
digits, but it counts as covering phone numbers here, on the safe side) selected (the default whenever `BRISVOX_RECORDING_DIR` is set and a
transcript or audio is written), or when `BRISVOX_LOG_REDACT_PII=true` with the `phone` or
`number` kind. The
agent then gets `carrier`, `carrier_call_id` and `pii_redacted: true`, and no `from` or `to`. The
carrier's call id is an opaque identifier, not a number, and stays. If your agent needs the
number to verify or greet the caller, either turn the redaction off for the kinds it conflicts
with (a decision for your counsel, since the number then also appears in transcripts or logs) or
look the caller up by the carrier's call id through the carrier's API.

The server does not write the numbers to its own logs or recordings from this path. Your agent
process should treat them like any other caller data: keep them out of its logs and apply your
retention to whatever it stores.

## MCP servers

An agent file with `[[mcp.servers]]` (`docs/TOOLS.md`, "MCP tools") sends the arguments of every
tool call the model makes to those servers: whatever the model puts there, such as a name, a phone
number, an order id or the text of a question, and the engine's idempotency key of the call. The
results come back and go to the LLM provider as any tool result does. A stdio server is a program on
your machine; an HTTP server is a service you name, and a public third-party one (the DeepWiki
example) receives what callers ask. Treat each as a processor of caller data in your data map, name
only servers you may send that data to, and use `https://` (a plain `http://` URL outside the private
networks needs `allow_insecure = true`). The server writes no tool arguments or results to its logs
or recordings from this path; the server process and its logs are yours to govern. The child's
environment holds none of the server's provider keys.

## Retention

The sweeper runs at startup and then every `BRISVOX_RECORDING_SWEEP_INTERVAL`. It looks at the
files directly in the recording directory (no recursion, symlinks are not followed), considers
only names this feature generates (`call-*.transcript.jsonl`, `call-*.caller.wav`,
`call-*.bot.wav`, each also with the `.age` suffix of an encrypted recording), and deletes those last modified at least `BRISVOX_RECORDING_RETENTION` ago.
Anything else in the directory is never touched. A sweep reads the directory in rounds of 4 096
entries (yielding to the runtime between rounds) until the listing is exhausted, so a file is
reached however many other entries the directory holds; only a listing of more than 10 million
entries stops a sweep early (it is reported as incomplete and the next sweep continues).
**Keep the directory dedicated to the recordings**: every foreign entry is still read and
counted by the sweep, a huge unrelated directory makes each sweep slow, and files the feature
did not name are never cleaned up. Deletion is a plain `unlink`: it does not overwrite the
blocks, and it does not reach backups, snapshots or copies you made. Combine it with disk
encryption.

The sweeper's task handle is kept for the life of the server: at shutdown the server waits for it
and logs its counters; a panic of the sweep is logged as an error and counted
(`SweepStats::panics`), because it means expired recordings are no longer being deleted.

Retention counts from the file's last write, so a call that is still running keeps its files
young; the minimum of one hour (and the audio cap) exists so a sweep can never take the files of
a call in progress.

To delete one call's recording on request, remove its three files by hand (the session number
and start time are in the name and in the transcript's header line).

## How it stays out of the call's way

- The audio taps run on the pipeline's own tasks and only copy a `Bytes` handle into a bounded
  queue of 1 024 frames. A full queue drops the newest frame and counts it
  (`RecordingStats::frames_dropped`); the call is never slowed. The last 16 slots are reserved
  for barge-in markers: audio is refused before they are used, so a stalled disk does not cost
  the bot track its barge-in cut (a marker that still finds no room is counted,
  `RecordingStats::interrupts_lost`).
- The bot track keeps at most 30 s of audio that is still waiting to be due; a response
  synthesized much faster than real time loses the audio past that (the newest is dropped and
  counted) instead of growing memory.
- The answers of the engine about what was played reach the transcript task through a bounded
  queue of 64; a full queue drops the newest answer and counts it (`RecordingStats::heard_lost`),
  and that line is written as `unknown`.
- The transcript reads the call's conversation-event ring, which drops its oldest events for a
  slow reader. Nothing is added to the pipeline.
- Every disk operation has a 10 s timeout (measured on the server's injected clock). A failing file is disabled, logged once (the file
  name and the error, **never text**) and the call continues; recording failures never end or
  fail a call.
- When the call ends, the recording tasks drain what is queued and finish the files; the call's
  teardown waits for that for at most 15 s.
- Files are created exclusively (an existing file is never overwritten) and, on Unix, with mode
  `0600`. Make the directory itself private too.

## Checklist for an operator (a prompt for your counsel, not legal advice)

These are the questions that usually come up under Turkey's KVKK (Law No. 6698) and the EU/UK
GDPR; other regimes ask similar ones.

1. **Do you need to record at all?** Data minimisation points to transcripts with redaction on,
   audio off, and the shortest retention that serves the purpose.
2. **Basis and notice.** Both regimes expect a lawful basis (consent, contract, legitimate
   interest, or a legal obligation, depending on the case) and that callers are told, before
   or at the start of the call, that it is recorded, why, for how long and how to exercise their
   rights. Put the notice in the agent's greeting (`system_prompt`/first message) and in your
   privacy notice. Recording calls can additionally be regulated by telecommunications or
   interception rules that differ by country and by whether one or both parties must consent.
3. **Special categories.** A voice recording used to identify or authenticate a person can be
   biometric data, and a call can contain health or other special-category details. Both laws
   treat these more strictly. Redaction does not detect them.
4. **Processors and transfers.** The STT, LLM and TTS providers receive the call's content
   whether or not you record. Recording adds a second copy under your control. Check your
   agreements with each provider and the rules for sending data abroad (KVKK Art. 9, GDPR
   Chapter V).
5. **Registers and records.** Depending on your size and role, KVKK may require registration
   with VERBIS and GDPR a record of processing activities; recordings belong in them.
6. **Security.** Encrypt the volume, restrict access to the directory, keep backups on the same
   retention, and log who reads recordings (the server does not read them back).
7. **Rights requests.** Be able to find and delete one caller's recordings on request. Because
   file names carry no caller identity, you need your own mapping from caller to session/start
   time (for example from your telephony or CRM logs).
8. **Retention.** Set `BRISVOX_RECORDING_RETENTION` to what you can justify. `forever` is
   possible and logged as a warning; it is rarely defensible.

## Testing

`crates/brisvox-server/src/privacy/**/tests` (redaction, the tracks, the settings, the sinks on
a real file system, the sweeper), `crates/brisvox-engine/tests/audio_in.rs` (the caller-audio tap)
and `crates/brisvox-server/tests/recording.rs` (a call through the engine runner and the RTVI
listener, then the files it left).
