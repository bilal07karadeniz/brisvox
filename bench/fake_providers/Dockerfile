# syntax=docker/dockerfile:1.7
#
# Image of the fake providers (bench/fake_providers/README.md). Build from the repository root:
#
#   docker build -f bench/fake_providers/Dockerfile -t brisvox/fake-providers .
#   docker run --rm -p 8443:8443 -p 8080:8080 brisvox/fake-providers
#
# Two ports: 8443 serves wss:// and https:// (a certificate signed by a throwaway CA generated at
# start), 8080 serves ws:// and http:// plus /metrics, /healthz and /ca.pem (the CA certificate the
# clients are told to trust). $PORT, when set (Railway), replaces 8080. Arguments after the image
# name are the server's flags (`--help`).

ARG RUST_VERSION=1.99.0
ARG DEBIAN_RELEASE=trixie

FROM rust:${RUST_VERSION}-slim-${DEBIAN_RELEASE} AS builder
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION} \
    CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0
# gcc/make: libmimalloc and ring compile C. Not version-pinned: the base tracks Debian security
# updates.
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential pkg-config ca-certificates \
 && rm -rf /var/lib/apt/lists/*
ARG BUILD_JOBS=
WORKDIR /src
# The workspace manifest lists every member, so the whole tree (minus .dockerignore) is the
# context; only this package and its small dependency set are compiled.
COPY . .
RUN CARGO_BUILD_JOBS="${BUILD_JOBS}" cargo build --release --locked -p brisvox-fake-providers \
 && strip target/release/brisvox-fake-providers \
 && install -D target/release/brisvox-fake-providers /out/brisvox-fake-providers

FROM debian:${DEBIAN_RELEASE}-slim AS runtime
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /app --shell /usr/sbin/nologin fake
WORKDIR /app
COPY --from=builder /out/brisvox-fake-providers /usr/local/bin/brisvox-fake-providers
# The fixture sidecars (the ground truth of what is said); the audio itself stays with the client.
COPY fixtures/en_two_utterances.json fixtures/tr_two_utterances.json fixtures/en_tr_mixed.json /app/fixtures/
USER fake
EXPOSE 8443 8080
ENTRYPOINT ["/usr/local/bin/brisvox-fake-providers"]
CMD ["--fixtures-dir", "/app/fixtures"]
