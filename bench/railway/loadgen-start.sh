#!/bin/sh
# The load generator and control page of the listening test (bench/railway/README.md).
# BRISVOX_PUBLIC / PIPECAT_PUBLIC: the public domains of the two servers (Railway reference
# variables, e.g. ${{brisvox.RAILWAY_PUBLIC_DOMAIN}}). LOAD_TOKEN, CALL_TOKEN, HOLD_ACCESS_KEY:
# shared variables.
set -eu
B=${BRISVOX_HOST:-brisvox.railway.internal}
P=${PIPECAT_HOST:-pipecat.railway.internal}
exec brisvox-bench hold --bind "[::]:${PORT:-8080}" --fixtures-dir /opt/fixtures \
  --target "brisvox;brisvox;ws://$B:9000/?token=$LOAD_TOKEN;http://$B:9100/stats;https://$BRISVOX_PUBLIC;CALL_TOKEN" \
  --target "pipecat;Pipecat;ws://$P:9001/;http://$P:9100/stats;https://$PIPECAT_PUBLIC;CALL_TOKEN"
