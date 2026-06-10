#!/usr/bin/env bash
#
# Run the interactive pipeline agent on the pure-Rust JS runtime, streaming OTEL
# spans to tael when tael is listening on its default OTLP gRPC port
# (127.0.0.1:4317). The agent pauses at each stage for input you type here.
#
#   ./run.sh                                  # defaults: 5 stages, 4 items each
#   INPUT='{"pipeline":"demo","stages":8,"itemsPerStage":6}' ./run.sh
#
# The agent uses the `run(handler)` entrypoint + `import { chidori } from
# "chidori"`, which runs on the pure-Rust engine (CHIDORI_JS_ENGINE=rust, built
# with `--features rust-engine`). Requires a bash with /dev/tcp for the tael
# port probe.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"
AGENT="$SCRIPT_DIR/interactive_pipeline.ts"
DEFAULT_INPUT='{"pipeline":"triage","stages":5,"itemsPerStage":4}'
INPUT="${INPUT:-$DEFAULT_INPUT}"

# Select the pure-Rust JS engine (the one that supports the run() entrypoint).
export CHIDORI_JS_ENGINE=rust

# Stream to tael only if something is actually listening on its default port —
# that's the "emit to tael if it's running" part. (An unreachable OTLP endpoint
# is silently dropped, so this probe just avoids the wasted work.)
if (exec 3<>/dev/tcp/127.0.0.1/4317) 2>/dev/null; then
  export OTEL_EXPORTER_OTLP_ENDPOINT="http://127.0.0.1:4317"
  export OTEL_SERVICE_NAME="${OTEL_SERVICE_NAME:-interactive-pipeline}"
  echo "tael detected on :4317 — streaming spans (service.name=$OTEL_SERVICE_NAME)" >&2
else
  echo "tael not detected on :4317 — running without OTEL export" >&2
  echo "  (start tael, then re-run; or set OTEL_EXPORTER_OTLP_ENDPOINT yourself)" >&2
fi

exec cargo run --quiet --features rust-engine --manifest-path "$REPO/Cargo.toml" \
  -- run "$AGENT" -i "$INPUT"
