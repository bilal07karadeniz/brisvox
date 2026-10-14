# The booking agent as a container: the SDK agent of "Service 2" in docs/DEPLOY.md.
#
# The Python SDK is not on PyPI, so the image vendors it: the build context is the REPOSITORY
# ROOT (not this directory), and `pip install` runs on the copied `sdk/python`. Build and run:
#
#   docker build -f examples/booking-agent/Dockerfile -t booking-agent .
#   docker run --rm -p 8765:8765 -e BRISVOX_AGENT_TOKEN=change-me -e SALON_TZ=Australia/Sydney \
#       booking-agent
#
# For your own agent copy this file next to your code, keep the `COPY sdk/python` line (a clone or
# a git submodule of this repository inside your build context) and replace the two `COPY
# examples/booking-agent/...` lines with yours.
#
# The agent listens on all interfaces (the SDK default, 127.0.0.1, is unreachable from another
# container). On Railway with an environment older than 2025-10-16 whose private network is IPv6
# only, override the command with `--host ::` (docs/DEPLOY.md, "Service 2"). The server reaches
# this container with BRISVOX_AGENT_URL=ws://<this container's name>:8765 and the same
# BRISVOX_AGENT_TOKEN. The SDK agent answers a plain HTTP request with an error status, so give the
# service no HTTP health check.

ARG PYTHON_VERSION=3.12
ARG DEBIAN_RELEASE=trixie
FROM python:${PYTHON_VERSION}-slim-${DEBIAN_RELEASE}

ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1 \
    PIP_NO_CACHE_DIR=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1

# The vendored SDK (one dependency, websockets). `tzdata` gives SALON_TZ a time zone database in
# this slim image.
COPY sdk/python /tmp/brisvox-sdk
RUN pip install /tmp/brisvox-sdk tzdata && rm -rf /tmp/brisvox-sdk

WORKDIR /app
COPY examples/booking-agent/booking_agent.py examples/booking-agent/booking_store.py ./

RUN useradd --system --uid 10002 --no-create-home --home-dir /nonexistent agent
USER 10002

EXPOSE 8765
CMD ["python", "booking_agent.py", "--host", "0.0.0.0", "--port", "8765"]
