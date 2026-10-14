#!/usr/bin/env bash
# Publish the brisvox crates to crates.io in dependency order (docs/RELEASING.md).
#
#   scripts/publish_crates.sh --dry-run          # package and verify everything, upload nothing
#   scripts/publish_crates.sh                    # publish all crates (needs CARGO_REGISTRY_TOKEN)
#   scripts/publish_crates.sh brisvox-core ...   # publish just these crates, in the order given
#
# Safe to run again after a failure: a crate version that is already on crates.io is skipped, and
# crates.io's rate limit on NEW crates (a burst of 5, then one every 10 minutes) is waited out:
# the very first release of a crate name needs about an hour for eleven crates. Version updates of
# existing crates are limited to a burst of 30 and one per minute and never wait here.
set -euo pipefail
cd "$(dirname "$0")/.."
PY=$(command -v python3 || command -v python)

if [ "${1:-}" = "--dry-run" ]; then
  # Cargo packages the publishable crates in dependency order and checks each against the ones
  # before it without a token or an upload; this is also the release workflow's gate.
  exec cargo publish --workspace --dry-run --locked
fi

if [ "$#" -gt 0 ]; then
  CRATES=("$@")
else
  mapfile -t CRATES < <("$PY" scripts/check_release.py --order | tr -d '\r')
fi
: "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN is not set (a token, or the output of rust-lang/crates-io-auth-action)}"

MAX_WAITS="${PUBLISH_MAX_WAITS:-12}"     # x 10 minutes of waiting per crate at most
LOG=$(mktemp)
trap 'rm -f "$LOG"' EXIT

for crate in "${CRATES[@]}"; do
  waits=0
  while true; do
    echo "== publish $crate"
    if cargo publish -p "$crate" --locked 2>&1 | tee "$LOG"; then
      break
    fi
    if grep -q "already uploaded\|already exists" "$LOG"; then
      echo "== $crate: this version is already on crates.io, skipping"
      break
    fi
    if grep -q -i "429\|too many\|rate limit" "$LOG" && [ "$waits" -lt "$MAX_WAITS" ]; then
      waits=$((waits + 1))
      echo "== $crate: crates.io rate limit, waiting 10 minutes ($waits/$MAX_WAITS)"
      sleep 600
      continue
    fi
    echo "== $crate: publish failed" >&2
    exit 1
  done
done
echo "== done"
