# The image of the LiveKit Agents worker and of the load client (same Python as the Pipecat run).
# Build context: bench/livekit_baseline (the fixtures and the repo are mounted at run time).
FROM python:3.13-slim-bookworm
ENV PIP_NO_CACHE_DIR=1 PYTHONUNBUFFERED=1 HF_HOME=/opt/hf
WORKDIR /app
COPY requirements.lock.txt requirements.txt /app/
RUN python -m pip install -q -r /app/requirements.lock.txt
