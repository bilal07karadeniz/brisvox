#!/usr/bin/env bash
# Container HEALTHCHECK: GET /healthz on BRISVOX_HEALTH_ADDR, exit 0 on a 200.
# Needs bash only (the runtime image has no curl or wget); /healthz is the liveness probe and
# stays 200 while the server drains or is full (docs/OPERATIONS.md, "Health endpoints").
set -u
addr="${BRISVOX_HEALTH_ADDR:-0.0.0.0:8081}"
if [ "$addr" = "off" ]; then
    exit 0 # health listener disabled by the operator: nothing to probe
fi
port="${addr##*:}"
host="${addr%:*}"
# Dial the configured bind host; a wildcard bind (or no host) is reached through loopback.
case "$host" in
    "" | "0.0.0.0" | "[::]" | "::" | "$addr") host="127.0.0.1" ;;
esac
host="${host#[}"
host="${host%]}"
exec 3<>"/dev/tcp/${host}/${port}" || exit 1
printf 'GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n' >&3 || exit 1
IFS= read -r -t 2 status <&3 || exit 1
case "$status" in
    "HTTP/1.1 200"*) exit 0 ;;
    *) exit 1 ;;
esac
