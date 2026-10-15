# The load generator and control page of the listening test (bench/railway/README.md).
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --chmod=0755 brisvox-bench /usr/local/bin/brisvox-bench
COPY fixtures/ /opt/fixtures/
COPY --chmod=0555 loadgen-start.sh /usr/local/bin/loadgen-start.sh
CMD ["/usr/local/bin/loadgen-start.sh"]
