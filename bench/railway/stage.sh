#!/bin/bash
# Lays out the four Docker build contexts of the listening test (README.md here) from a checkout
# and Linux release binaries, one directory per Railway service, ready for `railway up DIR`.
#
#   BIN=target/release OUT=/tmp/railway-stage bench/railway/stage.sh
#
# BIN must hold brisvox-server (features all-providers,smart-turn-ort), brisvox-bench and
# brisvox-fake-providers built for x86_64 Linux (glibc of Debian trixie or older); models/ must
# hold silero_vad.onnx and smart-turn-v3.2-cpu.onnx (scripts/models/fetch_models.py).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${BIN:-$ROOT/target/release}
OUT=${OUT:-/tmp/railway-stage}
HERE=$ROOT/bench/railway
rm -rf "$OUT"
mkdir -p "$OUT"/{fakes/fixtures,brisvox/models,pipecat/app/bench/fake_providers,pipecat/models,loadgen/fixtures}
bin() { strip -o "$2/$1" "$BIN/$1" 2>/dev/null || cp "$BIN/$1" "$2/$1"; }

cp "$HERE/fakes.Dockerfile" "$OUT/fakes/Dockerfile"
bin brisvox-fake-providers "$OUT/fakes"
cp "$ROOT"/fixtures/{en_two_utterances,tr_two_utterances,en_tr_mixed}.json "$OUT/fakes/fixtures/"

cp "$HERE/brisvox.Dockerfile" "$OUT/brisvox/Dockerfile"
bin brisvox-server "$OUT/brisvox"
bin brisvox-bench "$OUT/brisvox"
cp "$ROOT/models/smart-turn-v3.2-cpu.onnx" "$OUT/brisvox/models/"
cp "$ROOT/bench/pipecat_baseline/agent_live_en_aura.toml" "$OUT/brisvox/agent.real.toml"
cp "$ROOT/bench/fake_providers/agent.fakes.toml" "$OUT/brisvox/agent.fakes.toml"
cp "$HERE/brisvox-start.sh" "$OUT/brisvox/"

cp "$HERE/pipecat.Dockerfile" "$OUT/pipecat/Dockerfile"
bin brisvox-bench "$OUT/pipecat"
cp -r "$ROOT/bench/pipecat_baseline" "$OUT/pipecat/app/bench/"
rm -rf "$OUT/pipecat/app/bench/pipecat_baseline/"{__pycache__,tests,.venv}
cp "$ROOT/bench/fake_providers/agent.fakes.toml" "$OUT/pipecat/app/bench/fake_providers/"
cp "$ROOT/models/silero_vad.onnx" "$OUT/pipecat/models/"
cp "$HERE/pipecat-start.sh" "$OUT/pipecat/"

cp "$HERE/loadgen.Dockerfile" "$OUT/loadgen/Dockerfile"
bin brisvox-bench "$OUT/loadgen"
cp "$ROOT"/fixtures/en_two_utterances.{wav,json} "$OUT/loadgen/fixtures/"
cp "$HERE/loadgen-start.sh" "$OUT/loadgen/"

du -sh "$OUT"/*
