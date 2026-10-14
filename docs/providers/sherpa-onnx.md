# sherpa-onnx — self-hosted streaming STT (WebSocket server)

Code: `brisvox_providers::sherpa_onnx` (feature `provider-sherpa-onnx`, server
feature of the same name, `provider = "sherpa_onnx"`), on the shared session core
`brisvox_providers::wsstt`. Survey and alternatives: `docs/providers/local-speech.md`.

Read 2026-10-14. Everything marked (source) was read from the server source files below, not
from a summary. **Status.** The protocol sections were written from the server source before any
server ran; the sections "Live" and "First-turn check" at the end report what was run afterwards
(sherpa-onnx 1.13.8, the Python `streaming_server.py`, with the Turkish `seda-v0.1` model and the
English zipformer, on WSL2). Still not run: the C++ server and 8 kHz telephone audio. The "Nothing
below was run" remark in "Models and the commands" and the UNVERIFIED marks refer to what the
live runs did not cover. Mocks in `crates/brisvox-providers/tests/sherpa_onnx.rs` implement the
protocol exactly as read here.

- Python server: https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/master/python-api-examples/streaming_server.py
  (885 lines; `handle_connection_impl`, `recv_audio_samples`, `process_request`, argparse flags)
- Python client: https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/master/python-api-examples/online-websocket-client-decode-file.py
- C++ server: https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/master/sherpa-onnx/csrc/online-websocket-server-impl.cc
  (+ `-impl.h` for the defaults), result JSON in `sherpa-onnx/csrc/online-recognizer.cc`
  (`OnlineRecognizerResult::AsJsonString`)
- C++ server usage: https://k2-fsa.github.io/sherpa/onnx/websocket/online-websocket.html
- English model files: https://k2-fsa.github.io/sherpa/onnx/pretrained_models/online-transducer/zipformer-transducer-models.html
- Turkish model card: https://huggingface.co/atasoglu/seda-v0.1

## Protocol (source)

One WebSocket per recognition stream; no handshake message, no parameters, no authentication.

| | Python `streaming_server.py` | C++ `sherpa-onnx-online-websocket-server` |
|---|---|---|
| client -> server audio | binary message = raw little-endian `float32` samples (`np.frombuffer(message, dtype=np.float32)`) at `--sample-rate` (default 16000); "at present, we assume the sampling rate of the received audio samples equal to `--sample-rate`" (no resampling) | binary message = `float32` samples (`payload.size() / sizeof(float)`) at `--input-sample-rate` (default 16000); the option help says the server resamples to the model's rate |
| end of audio | text `Done` | text `Done` |
| result message | text JSON `{"text": <hypothesis of the open segment>, "segment": n}`, sent after every decode step (a message may repeat the previous text) | text JSON from `AsJsonString`: `text`, `tokens`, `timestamps`, `ys_probs`, `lm_probs`, `context_scores`, `segment`, `words`, `num_trailing_blanks`, `start_time`, `is_final`, `is_eof`; sent per decode batch |
| endpoint | none flagged. After an endpoint the message that carries the endpointed text is sent first, then the stream is reset and `segment` goes up by one: the next message has `segment + 1` | `is_final: true` on the message that carries the endpointed text; the stream is reset |
| after `Done` | the server pads **0.3 s** of zeros (`int(sample_rate * 0.3)`), calls `input_finished()`, decodes the rest, sends **one more** `{text, segment}` and returns, which closes the connection | `end-tail-padding` (default **0.8 s**) of zeros, `InputFinished()`; the last result has `is_final: true` and `is_eof: true`; then the text message `Done!` |
| connection limit | `--max-active-connections` (default 200): the HTTP upgrade of an extra connection is answered **503** | none seen |
| other | `--max-message-size` 1 MiB, `--max-queue-size` 32 messages per connection (backpressure) | `--max-batch-size`, `--loop-interval-ms` (default 10) |

Endpointing (both): `--use-endpoint 1` (default), `--rule1-min-trailing-silence 2.4`,
`--rule2-min-trailing-silence 1.2`, `--rule3-min-utterance-length 20` (seconds, Python flags;
the help texts only say "duration of trailing silence >= this value", which rule needs decoded
text first was not read from the recognizer source).

Not in the protocol: a mid-stream finalize or flush, a keepalive, a language parameter (the
model decides), word timestamps in the Python flavour.

## What the adapter does

- **Audio.** PCM16 from the engine is resampled to `sample_rate` when the rates differ
  (`brisvox_audio::StreamResampler`, 1–2 us per 20 ms frame, `docs/LATENCY.md`) and converted to
  `float32` LE in a reused buffer (frames are cut off the front of one `BytesMut`; the steady
  state does not allocate per frame).
- **Interims.** The hypothesis of the open segment, forwarded when it changes (identical results
  are not repeated). While a `Done` exchange is open the hypotheses are not forwarded: the answer
  carries the text.
- **Server endpoint = final.** Python: a result with a higher `segment` ends the previous
  segment, whose last hypothesis becomes a final transcript with `speech_final = true`
  (`from_finalize = false`). This final is therefore seen one decode step *after* the endpoint
  (as soon as the next audio frame is decoded; audio never stops in the engine). C++: the
  `is_final` message is the final. Both flavours are told apart by the presence of `is_final`.
- **Errors.** HTTP status of the upgrade (503 for the connection limit) -> `Rejected`; close code
  1008 -> `Rejected`; a text message that is not JSON -> `Protocol`; everything else as in
  `wsstt` (reconnect, silence watchdog with a half-way ping, `Core(Timeout)`).

## Finalize (the design)

The engine's `SttInput::Finalize` means "the caller stopped talking: give me the final text of
what you heard now". sherpa-onnx has no such message; the only thing that makes it decode the
tail and emit its last result is `Done`, which ends the connection. Options considered:

1. **`Done`, then a new connection (implemented).** `Finalize` sends `Done`. The server's last
   result is the answer (`from_finalize`, `speech_final`), delivered when the server closes (Python)
   or marks `is_eof` (C++). The spent connection is replaced; the replacement is opened when the
   caller sends again (audio follows within a frame, so this is immediate in practice, and a call
   that ends right after a turn leaves no stray connection). Audio that arrives while the answer is
   pending is held in the bounded ring (`replay_window`, oldest dropped first) and goes to the
   replacement in order. A `Finalize` on a connection that has heard no audio is answered empty
   without a round trip. Further `Finalize` requests queue and are answered in order.
2. *Trailing silence and wait for the server's endpoint (rejected).* Send enough zeros to trip
   the trailing-silence rules (>= 1.2 s for rule2) and treat the next endpoint result as the answer. It keeps the connection but
   depends on the server's endpointing being on and tuned (`--use-endpoint`, the rules), which the
   adapter cannot see; with endpointing off a `Finalize` could never be answered, and the 1.2 s of
   padding is decoded as real audio (~40 ms at the community model's RTF 0.03, but 1.2 s of extra
   stream length per turn).
3. *Per-turn connection with a pre-warmed spare (not needed).* Same effect as 1 with a connection
   parked idle per session; saves one loopback/LAN handshake (~1 ms local, 1–3 RTT on a LAN) at
   the cost of a second server connection per call against `--max-active-connections`.

Latency trade-off of option 1: the answer arrives after the server decoded the audio it had not
yet decoded plus 0.3 s (Python) / 0.8 s (C++) of tail padding. For the zipformer models the model
card quotes a final about 40 ms after the end of audio (RTF 0.027–0.034 on one CPU thread, seda
card; not measured here), so `finalize_timeout` (default 1.5 s) is a safety bound, not the
expected wait. The replacement connection costs one handshake that overlaps the next turn's first
audio; the recognizer state is reset every turn (the models are reset at endpoints anyway, so no
context is lost that the server would have kept).

If the answer does not arrive within `finalize_timeout`, the latest hypothesis is delivered as the
answer and the connection is replaced. A late result cannot arrive (the old connection is gone).

## Models and the commands to start a server

The Python server was run with the Turkish model (section "Live") and the English zipformer
(section "First-turn check"); the command lines below are assembled from the model pages and the
server's flags, and the C++ server line was not run. The flags are those of
`streaming_server.py` (source, `--help` list:
`--encoder --decoder --joiner --tokens --sample-rate --decoding-method --use-endpoint
--rule1/2/3-... --port --nn-pool-size --max-batch-size --max-wait-ms --max-active-connections
--num-threads`); file names are from the model pages.

English (CPU, streaming zipformer, `sherpa-onnx-streaming-zipformer-en-2023-06-26`; the page lists
fp32 and int8 variants of each file):

```sh
git clone https://github.com/k2-fsa/sherpa-onnx          # python-api-examples/ (it imports http_server.py next to it)
pip install sherpa-onnx websockets numpy
wget https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2
tar xvf sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2
python3 ./python-api-examples/streaming_server.py \
  --tokens  ./sherpa-onnx-streaming-zipformer-en-2023-06-26/tokens.txt \
  --encoder ./sherpa-onnx-streaming-zipformer-en-2023-06-26/encoder-epoch-99-avg-1-chunk-16-left-128.int8.onnx \
  --decoder ./sherpa-onnx-streaming-zipformer-en-2023-06-26/decoder-epoch-99-avg-1-chunk-16-left-128.onnx \
  --joiner  ./sherpa-onnx-streaming-zipformer-en-2023-06-26/joiner-epoch-99-avg-1-chunk-16-left-128.int8.onnx \
  --port 6007
```

Turkish, `atasoglu/seda-v0.1` (community model, Apache-2.0 acoustic model; streaming Zipformer2
transducer, 16 kHz, chunk 32 frames = 0.64 s; files from the card: `encoder.int8.onnx` 70 MB,
`decoder.onnx`, `joiner.int8.onnx`, `tokens.txt`, optional `lm/rnnlm.int8.onnx` and `lm/2gram.fst`
under CC BY-SA 4.0):

```sh
git clone https://github.com/k2-fsa/sherpa-onnx          # for python-api-examples/
pip install sherpa-onnx websockets numpy huggingface_hub
huggingface-cli download atasoglu/seda-v0.1 --local-dir ./seda-v0.1
python3 ./python-api-examples/streaming_server.py \
  --tokens  ./seda-v0.1/tokens.txt \
  --encoder ./seda-v0.1/encoder.int8.onnx \
  --decoder ./seda-v0.1/decoder.onnx \
  --joiner  ./seda-v0.1/joiner.int8.onnx \
  --sample-rate 16000 \
  --port 6006
```

UNVERIFIED for seda: the model card gives no server command, so that `streaming_server.py` loads
the files unchanged (it takes the model type from the ONNX metadata of the transducer) is an
inference from the server source, to be confirmed by a live run. `streaming_server.py` has no
language-model flag, so the card's headline WERs (11.52 FLEURS-TR with beam 4 and the LM) do not
apply to this setup; greedy search without the LM is worse by an unmeasured amount. The output
is lowercase, unpunctuated, numbers spelled out. Telephone (8 kHz) and noisy speech are
unmeasured by the card; the adapter upsamples 8 kHz to 16 kHz.

C++ server (from the usage page; arguments must be `--key=value`):
`./build/bin/sherpa-onnx-online-websocket-server --port=6006 --tokens=... --encoder=...
--decoder=... --joiner=... --max-batch-size=5 --loop-interval-ms=20`, optionally
`--input-sample-rate=8000` to let the server resample (then set `sample_rate = 8000` in the
agent file and the adapter sends the telephony audio as it is).

**Ports.** The server's `--port` default is 6006, which is also the adapter's default URL
(`ws://127.0.0.1:6006`). The examples pass the port explicitly so that both can run on one machine:
**6006 for the Turkish model** (`examples/agents/local/local-tr.toml`) and **6007 for the English
one** (`local-en.toml`, the port of the live English run below). Any other port works if the
agent file's `base_url` says so.

**Bind address.** `streaming_server.py` has no host or bind flag (its flags: `--port`,
`--certificate`, `--doc-root`, `--nn-pool-size`, `--max-active-connections` and the model and
decoding options). Its source starts the server with `websockets.serve(self.handle_connection,
host="", port=port, ...)`, which listens on **all interfaces** (fetched from
https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/master/python-api-examples/streaming_server.py,
2026-10-14; the `localhost`/`127.0.0.1` addresses it prints at startup are only log text). The
server has no authentication, so keep its port closed to the network: a host firewall rule, or run
the script in a container that publishes the port with `-p 127.0.0.1:<port>:<port>`.

Agent file: `examples/agents/local/local-en.toml`, `local-tr.toml`; keys in `docs/CONFIG.md`;
step by step with Ollama, Kokoro and the browser page: the book's "A fully local assistant".

## Tests

`crates/brisvox-providers/tests/sherpa_onnx.rs`: mocks of both servers (partials, repeats,
segment endpoint, C++ `is_final`/`is_eof`/`Done!`), the `Done` exchange with audio arriving
meanwhile, unanswered `Done`, finalize without audio, queued finalizes, reconnect with replay and
a pending finalize, disconnect, garbage, refusals, silent-but-alive server (pings), dead server
(timeout), keepalive, 8 kHz resampling, bearer header, the provider contract suite (7 checks).
Live: `BRISVOX_LIVE_TESTS=1 SHERPA_ONNX_URL=ws://127.0.0.1:6006 [SHERPA_ONNX_LANGUAGE=tr] cargo
test -p brisvox-providers --features provider-sherpa-onnx --test sherpa_onnx live_ -- --nocapture`
streams the two-utterance fixture (`fixtures/tr_two_utterances.wav` for `tr`, else
`en_two_utterances.wav`) at real-time pace, finalizes per utterance and prints partial counts,
first-partial time, **finalize latency** and a folded WER against the fixture's reference text.
`SHERPA_ONNX_TR_URL=... live_sherpa_onnx_turkish` runs every Turkish fixture utterance at 16 kHz
and as 8 kHz mu-law phone audio and prints a WER and finalize-latency table (section "Turkish
phone audio" above).

## Open points

- Live wire check against a running server (Python and C++), the real finalize latency and the
  seda model: a live run.
- The C++ server's behaviour for a `Done` on a stream that is already at an endpoint, and whether
  it closes the socket after `Done!` (the adapter does not depend on either).
- Max-queue backpressure of the Python server (32 messages): a replay of a full 8 s ring is 400
  messages; the server reads continuously, so it only slows the replay.

## Live (2026-10-14)

Owner's machine, WSL2 Ubuntu 24.04, sherpa-onnx 1.13.8 Python `streaming_server.py` (needs
`http_server.py` and the `python-api-examples/web` assets next to it), Turkish model
`atasoglu/seda-v0.1` (int8 encoder/joiner, 76 MB, CPU, `--num-threads 2`), the TR FLEURS fixture
through `sherpa_onnx` (`SHERPA_ONNX_LANGUAGE=tr`), 2 runs: WER 0.00 on both utterances, 9-11
partials per turn, first partial ~1.07-1.16 s into the speech, **finalize answered in 1.2-20.6 ms**
(the server's final result after `Done`; Deepgram's Finalize is ~290-330 ms). The first-partial
delay is the model's chunking; the turn's end is what the engine waits for.

### Turkish phone audio (2026-10-14)

Same machine and server (Python `streaming_server.py`, `ws://127.0.0.1:6006`), model
`atasoglu/seda-v0.1` (int8, CPU, 2 threads), adapter defaults (finalize padding 300 ms). Eight
Turkish FLEURS utterances (108 reference words): the two of `fixtures/tr_two_utterances` and the
six of the untracked `tr_long_0` / `tr_long_1` set. Each utterance goes in real time in 20 ms
frames followed by 0.3 s of silence (what the VAD leaves), then `Finalize`; one session per path.

- **16 kHz**: the fixture as recorded, session at 16 kHz.
- **8 kHz mu-law**: what a Twilio call delivers. The fixture is resampled to 8 kHz with
  `brisvox-audio`, G.711 mu-law encoded and decoded, and sent with the session configured for
  8 kHz, so the adapter resamples it back up to the server's rate.

WER is word-level on text folded to lowercase letters and digits (Turkish letters folded to
ASCII, punctuation dropped; `İsa'nın` is scored as two reference words and counts as an error
when the model writes `isanın`).

| utterance | words | WER 16 kHz | finalize ms | WER 8 kHz mu-law | finalize ms |
|---|---:|---:|---:|---:|---:|
| tr_two_utterances #0 | 11 | 0.000 | 24.5 | 0.182 | 30.5 |
| tr_two_utterances #1 | 15 | 0.000 | 21.8 | 0.000 | 24.2 |
| tr_long_0 #0 | 11 | 0.000 | 25.4 | 0.091 | 20.5 |
| tr_long_0 #1 | 15 | 0.000 | 19.0 | 0.000 | 27.1 |
| tr_long_0 #2 | 13 | 0.154 | 25.0 | 0.154 | 23.7 |
| tr_long_1 #0 | 11 | 0.000 | 15.4 | 0.000 | 15.9 |
| tr_long_1 #1 | 14 | 0.143 | 24.0 | 0.143 | 24.9 |
| tr_long_1 #2 | 18 | 0.111 | 25.7 | 0.444 | 14.1 |
| **total / mean** | 108 | **0.056** | **22.6** | **0.139** | **22.6** |

Reading: phone quality costs the seda model about 8 points of WER on this set (5.6 % to 13.9 %).
Seven of the eight utterances are equal or within one word; the damage is concentrated in the
longest, densest sentence (tr_long_1 #2, a run of unusual words: `küresel ısınma` became `kürete
ısınma`) and in proper names (`Orta Amerika ülkelerindeki` became `Orta Amerikan ilçelerindeki`).
Finalize latency is the same for both paths, 14-31 ms (mean 22.6 ms), because the final result
after `Done` is just the tail decode; the adapter's own 8 to 16 kHz resampling adds nothing
measurable. One run, eight utterances: read the WER as an indication, not a benchmark.

Reproduce: `BRISVOX_LIVE_TESTS=1 SHERPA_ONNX_TR_URL=ws://127.0.0.1:6006 cargo test -p
brisvox-providers --features provider-sherpa-onnx --test sherpa_onnx live_sherpa_onnx_turkish --
--nocapture` (the `tr_long_*` files are picked up from `fixtures/large/` or the directory in
`BRISVOX_FIXTURES_LARGE`; without them only the two tracked utterances run).

## First-turn check and the padding before `Done` (2026-10-14)

The maintainers' fully local browser call (sherpa-onnx English zipformer chunk-16, Python server at
`ws://127.0.0.1:6007`, WSL2) ended its first turn with an empty transcript (`llm_rounds=0`).
The turn was reproduced through the production assembly, not the adapter alone:
`crates/brisvox-engine/tests/sherpa_first_turn.rs` runs `run_call` with the real adapter, Silero
VAD on the inference pool, the STT stage and the turn driver, a mock LLM and TTS, and a caller
paced in real time (room tone, then an utterance cut from `fixtures/en_two_utterances.wav` or
spoken by Kokoro, then 3 s of tone); the mock LLM shows what the turn became. Live only
(`BRISVOX_LIVE_TESTS=1 SHERPA_ONNX_URL=...`, feature `silero-tract`).

What was ruled out (none of it is the cause of an empty first turn): the connection opened late
(warm 0 ms, connect 1-12 ms; one 20 ms frame is dropped while the session connects, which only
matters for speech starting at sample 0), Finalize overtaking audio (the stage defers it until
the audio before it has gone out; `audio_sent` equals `audio_in` minus that frame), the
`heard no audio` shortcut (it is keyed on audio written to the connection), the replay ring,
rotation between turns (two utterances 3.5 s apart: both reached the LLM with their text; a loud
noise burst before the words did not make the Silero VAD open a separate turn), and an endpoint of the
server's own (rule2, 1.2 s) racing the `Done` (the segment bump is delivered as a final segment
before the empty answer and the engine joins them; `endpoint_closes=1` in the run that hit it).

What is real is the server's chunking, which the adapter can mitigate only in part:

- A turn that reaches the server with little audio is answered with little text. Live, one
  connection per probe, audio then `Done`: 0.45 s of speech + 0.3 s of silence gave `""`; the same
  with 0.9 s of trailing silence gave `"I"`; Kokoro's "Hello." (0.86 s) decoded as `HELLO` at
  every trailing length; "Hi there." (0.97 s) came back as `HIE`. A first utterance of well under a
  second right at the start of a call, with nothing else on the connection, is therefore the one
  case that can come back empty or partial. The adapter cannot add what the model never saw.
- The last word of a turn is cut. The Python server pads only 0.3 s before `input_finished()`;
  the zipformer's right context needs about 0.6 s of trailing silence (saturates there). Fixture
  sentence cut at the VAD's silence: `... WITH THE IMP` (0.3 s) against `... WITH THE IMPACT`
  (0.6 s); through the engine `... IS ORTHODOX CHRIST` / `... ORTHODOX CH` in some runs, `HIE` for
  "Hi there".

Fix: `SherpaOnnxConfig::finalize_padding` (`[stt.sherpa_onnx] finalize_padding_ms`, 0..=5000,
default 300). On `Finalize`, after the turn's audio and before `Done`, the adapter writes that much
`float32` silence on the connection about to be spent (never into the replay ring; nothing is
written for a connection that heard no audio). Cost, measured through the engine (UserSpeechEnd to
the final transcript, 6 turns each): 0 ms padding 96-151 ms (mean 118), 300 ms 90-161 ms (mean
122), 600 ms 127-199 ms (mean 162). The C++ server pads 0.8 s itself: set 0 there. Larger values
also make the server's rule2 endpoint (1.2 s of trailing silence) fire while the answer is
produced: handled, but pointless.

Tests: wire unit tests (padding then `Done`, none without audio), mock-server tests (a server that
shows text only when it received enough audio: empty at 0 ms padding, text at 300 ms; the padding is
`float32` zeros of exactly the configured length between the audio and `Done`), the live adapter
test `live_sherpa_onnx_keeps_the_last_word_of_a_turn_with_the_default_padding` (fixture sentence +
0.3 s of silence must end `... THE IMPACT`), and the engine test above.

A turn that is empty because the caller made a noise (a pop when the browser opens the
microphone) is not a transcription fault: the VAD reports speech, the server hears no words,
`llm_rounds=0` is the right outcome.
