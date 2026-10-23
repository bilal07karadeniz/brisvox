brisvox-server @VERSION@ (@TARGET@)

Prebuilt with: cargo build --release --locked -p brisvox-server \
    --features all-providers,call-control,smart-turn-ort,mcp,webrtc
That is every STT/LLM/TTS provider adapter, call control (transfer_call, send_dtmf, end_call),
Smart Turn on ONNX Runtime and the WebRTC listener. Not included: the SIP listener (feature
"sip"), OpenTelemetry export ("otlp"), encrypted recordings ("recording-encryption"),
the demo tools. Build from source for a different set: cargo install brisvox-server --features ...

Contents
  brisvox-server        the server (Windows: brisvox-server.exe)
  agents/               reference agent definitions to copy and change
  scripts/models/fetch_models.py
                        downloads the Smart Turn model into ./models (pinned SHA-256);
                        needs Python 3 and nothing else
  NOTICE-models.txt     notices of the two on-device models
  LICENSE-MIT, LICENSE-APACHE

Quick start
  python3 scripts/models/fetch_models.py
  ./brisvox-server --version
  ./brisvox-server check agents/web.toml
  export DEEPGRAM_API_KEY=... OPENAI_API_KEY=... CARTESIA_API_KEY=...
  BRISVOX_AGENT_FILE=agents/web.toml ./brisvox-server

The Smart Turn model is not part of this archive. Without it, adaptive turns end at their fallback
deadline. Run the server from this directory, or set BRISVOX_MODELS_DIR.

Linux binaries need glibc 2.39 or newer (Ubuntu 24.04, Debian 13 or newer).
Documentation: https://bilal07karadeniz.github.io/brisvox/
Source and issues: https://github.com/bilal07karadeniz/brisvox
