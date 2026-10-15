# The Pipecat container of the listening test (bench/railway/README.md). Context: stage.sh output.
FROM python:3.13-slim-trixie
RUN apt-get update && apt-get install -y --no-install-recommends curl \
 && rm -rf /var/lib/apt/lists/*
COPY app/bench/pipecat_baseline/requirements.lock.txt /tmp/requirements.lock.txt
RUN pip install --no-cache-dir -r /tmp/requirements.lock.txt
COPY app/ /app/
COPY models/ /opt/models/
COPY --chmod=0755 brisvox-bench /usr/local/bin/brisvox-bench
COPY --chmod=0555 pipecat-start.sh /usr/local/bin/pipecat-start.sh
CMD ["/usr/local/bin/pipecat-start.sh"]
