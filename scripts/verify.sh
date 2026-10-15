#!/usr/bin/env bash
# Quality gate. Must be green before any task is marked [x].
set -euo pipefail
cd "$(dirname "$0")/.."
if [ ! -f Cargo.toml ]; then
  echo "verify: no Cargo workspace yet — skipping"; exit 0
fi
# Disk guard: the shared target/ grows by tens of GB per day of parallel worktree builds (it
# reached 337 GB once and filled the owner's disk). Below BRISVOX_MIN_FREE_GB (default 40) free on
# the target's drive, the debug build output (regenerable) is removed before building.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
MIN_FREE_GB="${BRISVOX_MIN_FREE_GB:-40}"
mkdir -p "$TARGET_DIR"
FREE_GB=$(df -Pk "$TARGET_DIR" | awk 'NR==2 {print int($4/1024/1024)}')
if [ -n "$FREE_GB" ] && [ "$FREE_GB" -lt "$MIN_FREE_GB" ]; then
  echo "verify: only ${FREE_GB} GB free on the target drive (< ${MIN_FREE_GB} GB): removing the debug build output"
  cargo clean --profile dev
fi
echo "== toolchain"
# --all-features turns on the brisvox-transport `webrtc` feature, which compiles C: libopus
# (opusic-sys, via the cmake crate) and aws-lc-sys (str0m's DTLS crypto). That needs a C compiler
# and cmake (docs/TESTING.md, "Toolchain for --all-features"). The default build needs neither.
if ! command -v cmake >/dev/null 2>&1 && [ -z "${CMAKE:-}" ]; then
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*)
      echo "verify: WARNING cmake is not on PATH; the build only works if the cmake crate finds the one bundled with Visual Studio (install cmake otherwise)" ;;
    *)
      echo "verify: ERROR cmake not found, but --all-features builds libopus and aws-lc-sys with it." >&2
      echo "        Install cmake and a C compiler (apt install cmake build-essential), see docs/TESTING.md." >&2
      exit 1 ;;
  esac
fi
echo "== fmt";    cargo fmt --all -- --check
echo "== clippy"; cargo clippy --workspace --all-targets --all-features -- -D warnings
echo "== doc"
# Broken or redundant intra-doc links fail the gate.
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
echo "== models"
# On-device models used by feature-gated tests (pinned SHA-256; downloads only if missing).
PY=$(command -v python3 || command -v python)
"$PY" scripts/models/fetch_models.py
echo "== python sdk"
# sdk/python needs Python 3.11+ with the pinned `websockets` (sdk/python/pyproject.toml).
# The first interpreter that has it runs the SDK's own tests here and is handed to the Rust test
# crates/brisvox-agent-link/tests/python_sdk.rs (via BRISVOX_PYTHON_SDK) in the tests stage below.
# Without one the stage is skipped with a warning, unless BRISVOX_REQUIRE_PYTHON_SDK=1 (CI sets it).
PYSDK=""
for cand in "${BRISVOX_PYTHON_SDK:-}" python3.13 python3.12 python3.11 python3 python; do
  [ -n "$cand" ] || continue
  if "$cand" -c 'import sys, websockets; sys.exit(0 if sys.version_info >= (3, 11) else 1)' >/dev/null 2>&1; then
    PYSDK="$cand"; break
  fi
done
if [ -n "$PYSDK" ]; then
  # Hand the Rust tests the interpreter's real path: a bare name such as `python3.13` is not
  # found by std::process::Command on Windows (".13" reads as an extension, so no ".exe" is tried).
  PYSDK="$("$PYSDK" -c 'import sys; print(sys.executable)')"
  export BRISVOX_PYTHON_SDK="$PYSDK"
  (cd sdk/python && PYTHONPATH="$PWD" "$PYSDK" -m unittest discover -s tests -t tests)
  # The umbrella package `brisvox` (pip install brisvox) re-exports the SDK; its tests run against
  # the checkout of both.
  (cd sdk/python-umbrella && PYTHONPATH="$PWD:$PWD/../python" "$PYSDK" -m unittest discover -s tests -t tests)
elif [ -n "${BRISVOX_REQUIRE_PYTHON_SDK:-}" ] && [ "${BRISVOX_REQUIRE_PYTHON_SDK}" != "0" ]; then
  echo "verify: ERROR BRISVOX_REQUIRE_PYTHON_SDK is set but no Python 3.11+ with websockets was found." >&2
  echo "        pip install 'websockets>=15,<16' (see sdk/python/pyproject.toml)." >&2
  exit 1
else
  echo "verify: WARNING no Python 3.11+ with websockets found; sdk/python tests and python_sdk.rs are NOT run"
fi
echo "== typescript sdk"
# sdk/typescript needs Node 20+ and npm (dependencies pinned in sdk/typescript/package-lock.json).
# The first Node that qualifies installs the SDK's dependencies, builds it, runs its own tests here
# and is handed to the Rust test crates/brisvox-agent-link/tests/ts_sdk.rs (via BRISVOX_TS_SDK) in the
# tests stage below. Without one the stage is skipped with a warning, unless BRISVOX_REQUIRE_TS_SDK=1
# (CI sets it).
TSNODE=""
if command -v npm >/dev/null 2>&1; then
  for cand in "${BRISVOX_TS_SDK:-}" node; do
    [ -n "$cand" ] || continue
    if "$cand" -e 'process.exit(Number(process.versions.node.split(".")[0]) >= 20 ? 0 : 1)' >/dev/null 2>&1; then
      TSNODE="$cand"; break
    fi
  done
fi
if [ -n "$TSNODE" ]; then
  export BRISVOX_TS_SDK="$TSNODE"
  (
    cd sdk/typescript
    if [ ! -d node_modules ] || [ package-lock.json -nt node_modules/.package-lock.json ]; then
      npm ci --no-audit --no-fund
    fi
    npm run build
    "$TSNODE" scripts/run-tests.mjs
  )
elif [ -n "${BRISVOX_REQUIRE_TS_SDK:-}" ] && [ "${BRISVOX_REQUIRE_TS_SDK}" != "0" ]; then
  echo "verify: ERROR BRISVOX_REQUIRE_TS_SDK is set but no Node 20+ with npm was found." >&2
  exit 1
else
  echo "verify: WARNING no Node 20+ with npm found; sdk/typescript tests and ts_sdk.rs are NOT run"
fi
echo "== web sdk"
# sdk/web: the browser client module, no dependencies and no build; its tests run under
# node's test runner against fake WebSocket / RTCPeerConnection / AudioContext / getUserMedia. The
# crates/brisvox-transport demo page serves the same file (include_str!), so the Rust tests of the
# tests stage below cover that side. Needs only Node 20+ (the TypeScript stage's Node when it found one).
WEBNODE=""
for cand in "${TSNODE:-}" "${BRISVOX_TS_SDK:-}" node; do
  [ -n "$cand" ] || continue
  if "$cand" -e 'process.exit(Number(process.versions.node.split(".")[0]) >= 20 ? 0 : 1)' >/dev/null 2>&1; then
    WEBNODE="$cand"; break
  fi
done
if [ -n "$WEBNODE" ]; then
  (cd sdk/web && "$WEBNODE" scripts/run-tests.mjs)
  # The d.ts check: test/types-check.ts is the documented usage, compiled with the TypeScript
  # compiler the typescript sdk stage installed (sdk/typescript/node_modules). Without it the check
  # is skipped with a warning (the compiler is a dev dependency of another package, not of sdk/web).
  WEBTSC="sdk/typescript/node_modules/typescript/bin/tsc"
  if [ -f "$WEBTSC" ]; then
    (cd sdk/web && "$WEBNODE" ../typescript/node_modules/typescript/bin/tsc --noEmit --strict       --module nodenext --moduleResolution nodenext --target es2022 --lib es2022,dom --skipLibCheck test/types-check.ts)
  else
    echo "verify: WARNING no TypeScript compiler (sdk/typescript/node_modules); sdk/web/brisvox-call.d.ts is NOT type-checked"
  fi
elif [ -n "${BRISVOX_REQUIRE_TS_SDK:-}" ] && [ "${BRISVOX_REQUIRE_TS_SDK}" != "0" ]; then
  echo "verify: ERROR BRISVOX_REQUIRE_TS_SDK is set but no Node 20+ was found for sdk/web." >&2
  exit 1
else
  echo "verify: WARNING no Node 20+ found; sdk/web tests are NOT run"
fi
echo "== npm umbrella"
# sdk/npm-umbrella: the package `brisvox` (npm i brisvox) has no code of its own. Static checks
# (pins, exports map, plain re-exports) always; then the way a user gets it: the three tarballs
# (`brisvox`, `@brisvox/agent`, `@brisvox/web`) installed into a fresh project (the registry serves
# only the agent SDK's `ws`), `brisvox` and `brisvox/web` imported, the types resolved.
if [ -n "$WEBNODE" ]; then
  (cd sdk/npm-umbrella && "$WEBNODE" scripts/smoke.mjs)
  if [ -n "$TSNODE" ]; then
    UMBDIR="$(mktemp -d)"
    (cd sdk/typescript && npm pack --silent --pack-destination "$UMBDIR" >/dev/null)
    (cd sdk/web && npm pack --silent --pack-destination "$UMBDIR" >/dev/null)
    (cd sdk/npm-umbrella && npm pack --silent --pack-destination "$UMBDIR" >/dev/null)
    (cd sdk/npm-umbrella && BRISVOX_TSC="$PWD/../typescript/node_modules/typescript/bin/tsc" "$WEBNODE" scripts/smoke.mjs --install \
      "$UMBDIR"/brisvox-[0-9]*.tgz "$UMBDIR"/brisvox-agent-*.tgz "$UMBDIR"/brisvox-web-*.tgz)
    rm -rf "$UMBDIR"
  fi
else
  echo "verify: WARNING no Node 20+ found; sdk/npm-umbrella is NOT checked"
fi
echo "== examples"
# examples/: the Python halves of the runnable examples, offline and without keys. The
# phone receptionist's webhook scripts and the web page's static checks need only the standard
# library; the booking agent needs the Python SDK stage's interpreter (3.11+ with websockets).
# The Rust half (every example's agent file through the server's config loader, the env
# templates through its settings parsers) is crates/brisvox-server/tests/examples.rs, run in the
# tests stage below. Without the SDK's interpreter the booking tests are skipped with a warning,
# unless BRISVOX_REQUIRE_EXAMPLES=1 (CI sets it).
EXPY="${PYSDK:-$PY}"
"$EXPY" -m unittest discover -s examples/phone-receptionist -p 'test_*.py'
"$EXPY" -m unittest discover -s examples/web-assistant -p 'test_*.py'
if [ -n "$PYSDK" ]; then
  "$PYSDK" -m unittest discover -s examples/booking-agent -p 'test_*.py'
elif [ -n "${BRISVOX_REQUIRE_EXAMPLES:-}" ] && [ "${BRISVOX_REQUIRE_EXAMPLES}" != "0" ]; then
  echo "verify: ERROR BRISVOX_REQUIRE_EXAMPLES is set but no Python 3.11+ with websockets was found." >&2
  exit 1
else
  echo "verify: WARNING no Python 3.11+ with websockets found; examples/booking-agent tests are NOT run"
fi
echo "== book"
# book/: the mdBook that organises docs/*.md. Cheap structure check, standard library only:
# SUMMARY.md links, {{#include}} paths, relative links, every doc included, README links.
# When mdbook is installed it also builds the book (into target/book); without it only the check runs.
"$PY" scripts/test_check_book.py
"$PY" scripts/test_bench_live_report.py
"$PY" scripts/check_book.py
# llms.txt / llms-full.txt (the entry points for AI coding agents) are generated from the docs.
"$PY" scripts/gen_llms.py --check
# The README / book charts (assets/charts/*.svg) are generated from bench/results and checked for staleness.
"$PY" scripts/gen_charts.py --check
if command -v mdbook >/dev/null 2>&1; then
  mdbook build book
else
  echo "verify: mdbook not installed; the book structure was checked but 'mdbook build book' was NOT run"
fi
echo "== release"
# Release infrastructure (docs/RELEASING.md): one version everywhere, crates.io metadata, the
# publish order, license files in every package, the embedded browser module equals sdk/web, and
# the workflows parse with every third-party action pinned to a commit SHA.
"$PY" scripts/check_release.py
echo "== tests"
# CI (GitHub Actions sets CI=true) uses the nextest `ci` profile: the real-time capacity tests are
# retried twice there (.config/nextest.toml); locally they are strict.
NEXTEST_PROFILE=default
if [ "${CI:-}" = "true" ]; then NEXTEST_PROFILE=ci; fi
if command -v cargo-nextest >/dev/null 2>&1; then
  cargo nextest run --profile "$NEXTEST_PROFILE" --workspace --all-features
  cargo test --workspace --doc --all-features
else
  cargo test --workspace --all-features
fi
echo "== server without optional features"
# --all-features above also turns on brisvox-server's `recording-encryption`; this builds the
# crate's default-feature code paths too (the stubs of the optional features) and runs the
# recording tests that cover them (the variable without the feature is a startup error).
cargo clippy -p brisvox-server --all-targets -- -D warnings
if command -v cargo-nextest >/dev/null 2>&1; then
  cargo nextest run --profile "$NEXTEST_PROFILE" -p brisvox-server -E 'test(/privacy|recording/) | binary(cli)'
else
  cargo test -p brisvox-server privacy
  cargo test -p brisvox-server recording
  cargo test -p brisvox-server --test cli
fi
if ! command -v cargo-deny >/dev/null 2>&1; then
  echo "== deny: WARNING cargo-deny not installed — license/advisory policy NOT checked (cargo install --locked cargo-deny)"
elif [ -f deny.toml ]; then
  echo "== deny"; cargo deny check
fi
echo "verify: OK"
