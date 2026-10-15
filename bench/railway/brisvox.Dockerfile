# The brisvox container of the listening test (bench/railway/README.md). Context: stage.sh output.
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin brisvox
COPY --chmod=0755 brisvox-server brisvox-bench /usr/local/bin/
COPY models/ /opt/brisvox/models/
COPY agent.real.toml agent.fakes.toml /etc/bench/
COPY --chmod=0555 brisvox-start.sh /usr/local/bin/brisvox-start.sh
USER brisvox
CMD ["/usr/local/bin/brisvox-start.sh"]
