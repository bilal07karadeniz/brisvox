# syntax=docker/dockerfile:1.7
#
# brisvox-server production image (docs/DEPLOY.md).
#
#   docker build -t brisvox/server .
#   docker build -t brisvox/server --build-arg BRISVOX_FEATURES=all-providers,call-control,smart-turn-ort,sip .
#
# Stages: chef (toolchain + cargo-chef) -> planner (dependency recipe) -> builder (cached
# dependency build, then the binary) | models (checksum-verified Smart Turn model) -> runtime.
# The runtime is debian-slim with only ca-certificates and libstdc++6 (the ONNX Runtime that
# `smart-turn-ort` links statically needs it), a non-root user, the stripped binary and the
# model. The Silero VAD is compiled into the binary (feature `silero-tract`).

ARG RUST_VERSION=1.99.0
# trixie, not bookworm: the prebuilt static ONNX Runtime of `ort` needs glibc >= 2.38 and a GCC 13+
# libstdc++ (bookworm: undefined __isoc23_strtol and std::__cxx11::basic_string::_M_replace_cold).
ARG DEBIAN_RELEASE=trixie
ARG CARGO_CHEF_VERSION=0.1.78
ARG PYTHON_VERSION=3.12

# ---------------------------------------------------------------------------------------------
FROM rust:${RUST_VERSION}-slim-${DEBIAN_RELEASE} AS chef
ARG RUST_VERSION
ARG CARGO_CHEF_VERSION
# The image's toolchain must be the workspace's (rust-toolchain.toml); RUSTUP_TOOLCHAIN also
# stops rustup from installing the file's rustfmt/clippy components during the build.
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION} \
    CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0
# gcc/g++/cmake/make: libmimalloc, aws-lc, opus and the static ONNX Runtime link need them.
# Packages are deliberately not version-pinned: the base image tracks Debian security updates.
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential cmake pkg-config ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && cargo install cargo-chef --locked --version "${CARGO_CHEF_VERSION}"
WORKDIR /src

# ---------------------------------------------------------------------------------------------
FROM chef AS planner
COPY . .
# Fails the build when rust-toolchain.toml and RUST_VERSION drift apart.
RUN grep -q "^channel = \"${RUSTUP_TOOLCHAIN}\"" rust-toolchain.toml \
 || { echo "rust-toolchain.toml does not pin ${RUSTUP_TOOLCHAIN}: update ARG RUST_VERSION" >&2; exit 1; }
RUN cargo chef prepare --recipe-path recipe.json

# ---------------------------------------------------------------------------------------------
FROM chef AS builder
# What the production agent needs: every provider adapter, the carriers' call control for
# transfer_call/send_dtmf/end_call and Smart Turn on ONNX Runtime. The server's default
# features (mimalloc, yaml, silero-tract) stay on. Add `sip` for the SIP trunk listener.
ARG BRISVOX_FEATURES=all-providers,call-control,smart-turn-ort
# 1 strips the binary (smaller; panic messages stay, backtrace line tables go); 0 keeps them.
ARG STRIP_BINARY=1
# Optional cap on cargo's parallel rustc jobs (empty: one per core). Set it on small build
# hosts: `--build-arg BUILD_JOBS=4`. A full build peaks near 1 GB of RAM per job.
ARG BUILD_JOBS=
COPY --from=planner /src/recipe.json recipe.json
# Dependencies only: this layer is rebuilt when a Cargo.toml/Cargo.lock changes, not for source
# edits. The ONNX Runtime binaries are downloaded here (build time network).
RUN if [ -n "${BUILD_JOBS}" ]; then export CARGO_BUILD_JOBS="${BUILD_JOBS}"; fi  && cargo chef cook --release --locked -p brisvox-server --features "${BRISVOX_FEATURES}" \
    --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked -p brisvox-server --features "${BRISVOX_FEATURES}" \
 && if [ "${STRIP_BINARY}" = "1" ]; then strip target/release/brisvox-server; fi \
 && install -D -m 0755 target/release/brisvox-server /out/brisvox-server

# ---------------------------------------------------------------------------------------------
FROM python:${PYTHON_VERSION}-slim-${DEBIAN_RELEASE} AS models
# The pinned, SHA-256-verified downloads of scripts/models/fetch_models.py (the build fails on
# a mismatch). Only the Smart Turn model is shipped: Silero is inside the binary.
WORKDIR /src
COPY scripts/models/fetch_models.py scripts/models/fetch_models.py
RUN python3 -I scripts/models/fetch_models.py \
 && install -D -m 0444 models/smart-turn-v3.2-cpu.onnx /out/smart-turn-v3.2-cpu.onnx

# ---------------------------------------------------------------------------------------------
FROM debian:${DEBIAN_RELEASE}-slim AS runtime
# ca-certificates: TLS to the STT/LLM/TTS providers and the carriers. libstdc++6: the
# statically linked ONNX Runtime. bash/grep/coreutils (already in the base) run the HEALTHCHECK.
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libstdc++6 \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 brisvox \
 && useradd --system --uid 10001 --gid brisvox --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin brisvox

COPY --from=builder /out/brisvox-server /usr/local/bin/brisvox-server
COPY --from=models /out/smart-turn-v3.2-cpu.onnx /opt/brisvox/models/smart-turn-v3.2-cpu.onnx
# The notices that go with the two models (Smart Turn here, Silero inside the binary).
COPY deploy/models-notice.txt /opt/brisvox/models/NOTICE
COPY --chmod=0555 deploy/healthcheck.sh /usr/local/bin/brisvox-healthcheck
# Reference definitions to start from (docs/CONFIG.md). A real deployment mounts its own file at
# /etc/brisvox/agent.toml (deploy/compose.yaml), sets BRISVOX_AGENT_FILE, or passes the text in
# BRISVOX_AGENT_DEFINITION (a platform that cannot mount a file).
COPY deploy/agents/ /etc/brisvox/agents/

# Defaults only: the listeners are chosen per deployment (docs/OPERATIONS.md). No call listener
# is enabled here on purpose: the generic websocket listener is unauthenticated.
# BRISVOX_DEFAULT_AGENT_FILE, not BRISVOX_AGENT_FILE: it applies only when neither
# BRISVOX_AGENT_FILE nor BRISVOX_AGENT_DEFINITION is set, so an inline definition alone works.
ENV BRISVOX_DEFAULT_AGENT_FILE=/etc/brisvox/agent.toml \
    BRISVOX_MODELS_DIR=/opt/brisvox/models \
    BRISVOX_HEALTH_ADDR=0.0.0.0:8081 \
    BRISVOX_LOG_FORMAT=json \
    RUST_LOG=info

USER 10001:10001
EXPOSE 8081
# /healthz is liveness: it stays 200 while draining or full, so a drain never restarts the
# container. Orchestrators that route traffic should watch /readyz (docs/OPERATIONS.md).
HEALTHCHECK --interval=10s --timeout=4s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/brisvox-healthcheck"]
# The server drains on SIGTERM (BRISVOX_DRAIN_TIMEOUT 30s + cancel grace 5s + abort 1s + ~7s):
# stop the container with a grace period above 43s (`docker stop --time 45`, compose
# `stop_grace_period: 45s`). Exec form: the binary is PID 1 and receives the signal itself.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/brisvox-server"]
