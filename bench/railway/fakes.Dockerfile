# The fake providers of the listening test (bench/railway/README.md). Context: stage.sh output.
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /app --shell /usr/sbin/nologin fake
COPY --chmod=0755 brisvox-fake-providers /usr/local/bin/brisvox-fake-providers
COPY fixtures/ /app/fixtures/
USER fake
WORKDIR /app
CMD ["/usr/local/bin/brisvox-fake-providers", "--fixtures-dir", "/app/fixtures", "--san", "fakes.railway.internal", "--tls-addr", "[::]:8443", "--plain-addr", "[::]:8080"]
