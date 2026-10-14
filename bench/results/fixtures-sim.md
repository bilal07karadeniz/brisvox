# Real-speech fixture calls

- command: `brisvox-bench fixtures --sessions 10 --workers 4 --out bench/results/fixtures-sim.md`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 4 tokio workers (threads, not a CPU limit: other threads may use any core)
- mock providers in-process (brisvox-sim), real time (SystemClock), one process, FLEURS real-speech fixtures (fixtures/NOTICE)
- replies: English utterances get "Okay, got it.", Turkish ones "Peki, anladım"; mock latency medians STT final 150 / LLM first token 350 / TTS first byte 150 ms (log-normal, sigma 0.3); the mock STT returns the fixture transcript, so recognition quality is not measured

Response = server-side voice-to-voice (`FirstAudioOut - UserSpeechEnd`); overhead = internal overhead as defined in `docs/LATENCY.md`. Times in ms; percentiles below 100 samples are coarse (p99 = max).

| fixture | languages | calls | answered | failed | barge-ins | response p50 | response p90 | response p99 | overhead p50 | overhead p99 | wall s |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| en_two_utterances | en, en | 10 | 20/20 | 0 | 0 | 995.217 | 1173.194 | 1263.011 | 0.034 | 0.080 | 17.6 |
| tr_two_utterances | tr, tr | 10 | 20/20 | 0 | 0 | 1009.130 | 1180.907 | 1273.160 | 0.055 | 0.124 | 21.2 |
| en_tr_mixed | en, tr | 10 | 20/20 | 0 | 0 | 994.223 | 1181.991 | 1242.217 | 0.038 | 0.082 | 18.1 |

| fixture | utterance language | turns | response p50 | response p90 | response p99 | response max |
|---|---|---:|---:|---:|---:|---:|
| en_two_utterances | en | 20 | 995.217 | 1173.194 | 1263.011 | 1263.011 |
| tr_two_utterances | tr | 20 | 1009.130 | 1180.907 | 1273.160 | 1273.160 |
| en_tr_mixed | en | 10 | 1038.539 | 1211.831 | 1242.217 | 1242.217 |
| en_tr_mixed | tr | 10 | 931.503 | 1150.203 | 1181.991 | 1181.991 |
