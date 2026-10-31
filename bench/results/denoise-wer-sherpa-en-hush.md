# Noise suppression: word error rate

- command: `brisvox-bench denoise transcribe --stt-url ws://127.0.0.1:6007 --backends none,hush --noise-limit 9 --language en --fixtures en_two_utterances --out-jsonl C:/Users/bilal/AppData/Local/Temp/claude/C--Projelerim-brisvox/6896981b-5214-4731-aa83-a0ca16b3ac07/scratchpad/wer-hush.jsonl --out bench/results/denoise-wer-sherpa-en-hush.md`
- brisvox 0.1.2 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- STT server ws://127.0.0.1:6007
- **not a Linux run** (windows): timers and scheduling differ from the Linux servers the engine targets, so these numbers show trends only; numbers for comparisons (with other engines, or against latency targets) come from a Linux run

The clips are sent to the server as the audio of one utterance stream (20 ms chunks, then a finalize). `none` is the audio as it is (what the provider hears with `denoise_stt = false`); the other columns are the audio enhanced by the backend first (`denoise_stt = true`). WER = (substitutions + deletions + insertions) / reference words, lowercased words only, summed over the noise recordings of the row.

| fixture | path | SNR | WER none | WER hush |
|---|---|---|---|---|
| en_two_utterances | telephone | 0 dB | 98.9 % | 98.1 % |
| en_two_utterances | telephone | 10 dB | 91.2 % | 95.8 % |
| en_two_utterances | telephone | 20 dB | 74.3 % | 96.6 % |
| en_two_utterances | telephone | 5 dB | 92.0 % | 96.2 % |
| en_two_utterances | telephone | clean | 31.0 % | 96.6 % |
| en_two_utterances | wideband | 0 dB | 51.7 % | 60.2 % |
| en_two_utterances | wideband | 10 dB | 25.3 % | 28.7 % |
| en_two_utterances | wideband | 20 dB | 17.6 % | 19.2 % |
| en_two_utterances | wideband | 5 dB | 34.9 % | 40.6 % |
| en_two_utterances | wideband | clean | 17.2 % | 17.2 % |
