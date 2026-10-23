# Summary

[Introduction](introduction.md)

# Getting started

- [Quickstart](quickstart.md)
  - [Run the server with an example agent](quickstart/server.md)
  - [Write an agent with the Python or TypeScript SDK](quickstart/sdk-agent.md)
  - [Docker and compose](quickstart/docker.md)
  - [A fully local assistant](quickstart/local.md)

# Concepts

- [The pipeline](concepts/pipeline.md)
- [Turns and interruptions](concepts/turns-and-interruptions.md)
- [Played-text truncation](concepts/played-text.md)
- [Tools](concepts/tools.md)

# Using brisvox

- [Configuration](configuration.md)
- [Telephony](telephony.md)
  - [Twilio Media Streams](telephony/twilio.md)
  - [Telnyx media streaming](telephony/telnyx.md)
  - [SIP trunk](telephony/sip.md)
  - [Call control](telephony/call-control.md)
  - [G.711 audio](telephony/g711.md)
- [Operations](operations.md)
  - [Deploying in a container](operations/deploy.md)
  - [Security and authentication](operations/security.md)
  - [Privacy and recording](operations/privacy.md)
  - [Tracing](operations/tracing.md)

# Benchmarks

- [Benchmark results](benchmarks.md)
  - [Latency design and measurements](benchmarks/latency.md)
  - [Testing and the quality gate](benchmarks/testing.md)

# Reference

- [Architecture](reference/architecture.md)
- [Production pipeline design](reference/pipeline.md)
- [Tools](reference/tools.md)
- [Speech-to-speech models](reference/s2s.md)
- [Protocols](reference/protocols.md)
  - [WebSocket protocol](reference/websocket-protocol.md)
  - [Agent protocol](reference/agent-protocol.md)
  - [RTVI (Pipecat clients)](reference/rtvi.md)
- [Providers](reference/providers.md)
  - [Deepgram (STT)](reference/providers/deepgram.md)
  - [AssemblyAI (STT)](reference/providers/assemblyai.md)
  - [Gladia (STT)](reference/providers/gladia.md)
  - [OpenAI (LLM)](reference/providers/openai.md)
  - [OpenAI-compatible (LLM)](reference/providers/openai-compatible.md)
  - [Anthropic (LLM)](reference/providers/anthropic.md)
  - [Gemini (LLM)](reference/providers/gemini.md)
  - [Cartesia (TTS)](reference/providers/cartesia.md)
  - [ElevenLabs (TTS)](reference/providers/elevenlabs.md)
  - [Deepgram Aura (TTS)](reference/providers/deepgram-aura.md)
  - [OpenAI-compatible speech (local TTS)](reference/providers/openai-speech.md)
  - [sherpa-onnx (local STT)](reference/providers/sherpa-onnx.md)
  - [OpenAI-realtime transcription (local STT dialects)](reference/providers/openai-realtime-stt.md)
  - [Local speech servers (survey)](reference/providers/local-speech.md)
  - [OpenAI Realtime (speech-to-speech)](reference/providers/openai-realtime.md)
  - [Gemini Live (speech-to-speech)](reference/providers/gemini-live.md)
  - [WebRTC](reference/providers/webrtc.md)
  - [RTVI](reference/providers/rtvi.md)
  - [MCP (tools)](reference/providers/mcp.md)
  - [Silero VAD](reference/providers/silero-vad.md)
  - [VAD alternatives](reference/providers/vad-alternatives.md)
  - [Smart Turn](reference/providers/smart-turn.md)
  - [ONNX Runtime for Rust](reference/providers/onnxruntime-rust.md)
  - [rubato (resampling)](reference/providers/rubato.md)
- [Risk register](reference/risks.md)
- [Turkish support](reference/turkish.md)
- [Fixture sources](reference/fixtures-sources.md)
- [Releasing](reference/releasing.md)
