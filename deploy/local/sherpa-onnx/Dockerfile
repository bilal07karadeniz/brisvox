# syntax=docker/dockerfile:1
# A streaming speech-to-text server for the keyless local stack (deploy/local/README.md).
#
# sherpa-onnx publishes no server image, so this is the project's own `streaming_server.py`
# (Apache-2.0) on its pip package, with the English streaming zipformer model (Apache-2.0).
# Everything is pinned and checked: the Python image by digest, the pip packages by version, the
# two scripts and the model archive by SHA-256 (the build fails on a mismatch).
# Facts and the verified run: docs/providers/sherpa-onnx.md.
#
# server.py wraps the upstream script to send sentence-case text (the model prints capitals).
#
# websockets is held at 13.x on purpose: streaming_server.py uses the legacy `process_request`
# hook (path, headers), which websockets 14 and later changed.

ARG PYTHON_IMAGE=python:3.12-slim-trixie@sha256:cb33f7f771ecec2ef1225cf39b159591a040391bd73e4c01f00fdfc4c47611d2

FROM ${PYTHON_IMAGE} AS fetch
ARG SHERPA_RELEASE=v1.13.8
ARG MODEL=sherpa-onnx-streaming-zipformer-en-2023-06-26
ARG MODEL_SHA256=639e25b578e9e997131402199419c13a941f8e4e198e2da1ce57dbf5cf401282
ARG STREAMING_SERVER_SHA256=3f74d3c0f57724b1ed5fd7a637b37b31b6b5e396a80d6f7b2ecc8b65b1a65752
ARG HTTP_SERVER_SHA256=a78a7df73d44b7c8a6793a6454c10b948d245460d9f1ec69612eb2785e6901a0
COPY fetch.py /fetch.py
RUN python /fetch.py "${SHERPA_RELEASE}" "${MODEL}" "${MODEL_SHA256}" \
        "${STREAMING_SERVER_SHA256}" "${HTTP_SERVER_SHA256}" /out/app /out/model

FROM ${PYTHON_IMAGE}
ARG SHERPA_ONNX_VERSION=1.13.8
RUN pip install --no-cache-dir \
        "sherpa-onnx==${SHERPA_ONNX_VERSION}" "websockets==13.1" "numpy==2.2.6" \
 && useradd --system --uid 10002 --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin sherpa
COPY --from=fetch /out/app /app
# Runs the upstream script with the text in sentence case instead of capitals (see the file).
COPY server.py /app/server.py
COPY --from=fetch /out/model /opt/model
WORKDIR /app
USER 10002:10002
# The server logs to ./log/ (a writable tmpfs in the compose file) and listens on every
# interface of its network namespace; the compose file keeps that namespace unpublished.
EXPOSE 6007
ENTRYPOINT ["python", "server.py", \
    "--tokens", "/opt/model/tokens.txt", \
    "--encoder", "/opt/model/encoder-epoch-99-avg-1-chunk-16-left-128.int8.onnx", \
    "--decoder", "/opt/model/decoder-epoch-99-avg-1-chunk-16-left-128.onnx", \
    "--joiner", "/opt/model/joiner-epoch-99-avg-1-chunk-16-left-128.int8.onnx", \
    "--doc-root", "/app/web", "--num-threads", "2", "--port", "6007"]
