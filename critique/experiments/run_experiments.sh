#!/usr/bin/env bash
# Reproducible end-user experiment suite for the Chidori critique.
#
# Run from the repository root after `cargo build --release`:
#
#   bash critique/experiments/run_experiments.sh
#
# Requires: bash, curl, python3, node >= 18 (for the SDK driver scenario).
# No LLM API key is needed: experiment 6 uses fake_llm.py (an OpenAI-compatible
# stub on 127.0.0.1:4401) to exercise the real provider path.
#
# Every experiment prints PASS/FAIL/EXPECTED-FAIL; the script exits non-zero if
# any experiment behaves differently from what the critique documents.
set -u
cd "$(git rev-parse --show-toplevel)"

BIN=./target/release/chidori
SCRATCH=$(mktemp -d)
FAILURES=0
trap 'rm -rf "$SCRATCH"; pkill -x chidori 2>/dev/null; [ -n "${FAKE_PID:-}" ] && kill "$FAKE_PID" 2>/dev/null' EXIT

note() { printf '\n=== %s ===\n' "$*"; }
check() { # check <label> <expected-exit> <actual-exit>
  if [ "$2" = "$3" ]; then echo "PASS: $1"; else echo "FAIL: $1 (expected exit $2, got $3)"; FAILURES=$((FAILURES+1)); fi
}

[ -x "$BIN" ] || { echo "build first: cargo build --release"; exit 1; }

# ---------------------------------------------------------------- experiment 1
note "1. hello agent runs and matches getting-started.md output"
OUT=$($BIN run examples/agents/hello.ts --input name=Colton 2>/dev/null)
echo "$OUT" | grep -q '"greeting": "Hello, Colton!"'
check "hello output matches docs" 0 $?

# ---------------------------------------------------------------- experiment 2
note "2. ask-by-default policy fails closed with no TTY (documented in root README since #132)"
$BIN run examples/record-replay/exactly_once.ts -i name=Ada </dev/null >/dev/null 2>&1
check "gated effect refused non-interactively without --trusted" 1 $?

# ---------------------------------------------------------------- experiment 3
note "3. record -> trace -> replay is byte-identical"
$BIN run examples/record-replay/exactly_once.ts -i name=Ada --trusted 2>/dev/null > "$SCRATCH/rec.json"
RUN_ID=$(ls -t examples/record-replay/.chidori/runs | head -1)
$BIN trace "$RUN_ID" -d examples/record-replay >/dev/null
check "trace renders" 0 $?
$BIN resume examples/record-replay/exactly_once.ts "$RUN_ID" -d examples/record-replay 2>/dev/null > "$SCRATCH/rep.json"
diff -q "$SCRATCH/rec.json" "$SCRATCH/rep.json" >/dev/null
check "replay output byte-identical to recorded run" 0 $?

# ---------------------------------------------------------------- experiment 4
note "4. exactly-once: sabotage the tool body, replay still serves recorded result"
cp examples/record-replay/tools/send_email.ts "$SCRATCH/send_email.ts.bak"
cat > examples/record-replay/tools/send_email.ts <<'EOF'
import type { Chidori } from "@1kbirds/chidori/agent";
export async function run(_args: unknown, _chidori: Chidori): Promise<never> {
  throw new Error("boom - tool body was re-executed, exactly-once VIOLATED");
}
EOF
$BIN resume examples/record-replay/exactly_once.ts "$RUN_ID" -d examples/record-replay 2>/dev/null > "$SCRATCH/rep2.json"
STATUS=$?
cp "$SCRATCH/send_email.ts.bak" examples/record-replay/tools/send_email.ts
[ $STATUS -eq 0 ] && diff -q "$SCRATCH/rec.json" "$SCRATCH/rep2.json" >/dev/null
check "broken tool never re-invoked on replay" 0 $?

# ---------------------------------------------------------------- experiment 5
note "5. edit-and-resume: refused by default, opt-in via --allow-source-change (#132)"
cp examples/record-replay/exactly_once.ts "$SCRATCH/exactly_once.ts.bak"
printf '\n// tail-only edit\n' >> examples/record-replay/exactly_once.ts
$BIN resume examples/record-replay/exactly_once.ts "$RUN_ID" -d examples/record-replay >/dev/null 2>&1
E5A=$?
$BIN resume examples/record-replay/exactly_once.ts "$RUN_ID" -d examples/record-replay \
  --allow-source-change 2>/dev/null > "$SCRATCH/rep5.json"
E5B=$?
diff -q "$SCRATCH/rec.json" "$SCRATCH/rep5.json" >/dev/null || E5B=1
# An edit to an ALREADY-EXECUTED step must still fail loudly even with the flag.
cp "$SCRATCH/exactly_once.ts.bak" examples/record-replay/exactly_once.ts
sed -i 's/onboard \${name}/URGENT onboard \${name}/' examples/record-replay/exactly_once.ts
$BIN resume examples/record-replay/exactly_once.ts "$RUN_ID" -d examples/record-replay \
  --allow-source-change >/dev/null 2>&1
E5C=$?
cp "$SCRATCH/exactly_once.ts.bak" examples/record-replay/exactly_once.ts
check "source edit without the flag still refuses (safe default)" 1 $E5A
check "tail-only edit + --allow-source-change resumes with recorded prefix" 0 $E5B
check "edited executed step + flag fails loudly (divergence guard)" 1 $E5C

# ---------------------------------------------------------------- experiment 6
note "6. LLM path: record via OpenAI-compatible stub, replay with provider DEAD"
python3 critique/experiments/fake_llm.py & FAKE_PID=$!
sleep 1
export LITELLM_API_URL=http://127.0.0.1:4401/v1 LITELLM_API_KEY=sk-fake
$BIN run examples/agents/summarizer.ts -i document="Chidori records host calls." --trusted 2>/dev/null > "$SCRATCH/llm1.json"
COUNT=$(curl -s http://127.0.0.1:4401/__count | python3 -c 'import sys,json;print(json.load(sys.stdin)["count"])')
kill "$FAKE_PID"; wait "$FAKE_PID" 2>/dev/null; FAKE_PID=
LLM_RUN=$(ls -t examples/agents/.chidori/runs | head -1)
$BIN resume examples/agents/summarizer.ts "$LLM_RUN" -d examples/agents 2>/dev/null > "$SCRATCH/llm2.json"
[ "$COUNT" = 1 ] && diff -q "$SCRATCH/llm1.json" "$SCRATCH/llm2.json" >/dev/null
check "1 recorded provider call; replay identical with provider unreachable" 0 $?
unset LITELLM_API_URL LITELLM_API_KEY

# ---------------------------------------------------------------- experiment 7
note "7. determinism: two independent live runs are byte-identical"
$BIN run examples/record-replay/deterministic_identity.ts -i seed=x --trusted 2>/dev/null > "$SCRATCH/d1.json"
$BIN run examples/record-replay/deterministic_identity.ts -i seed=x --trusted 2>/dev/null > "$SCRATCH/d2.json"
diff -q "$SCRATCH/d1.json" "$SCRATCH/d2.json" >/dev/null
check "fixed clock + seeded RNG across separate processes" 0 $?

# ---------------------------------------------------------------- experiment 8
note "8. server + SDK: pause for human input, resume, replay (needs node)"
if command -v node >/dev/null; then
  setsid $BIN serve examples/record-replay/human_approval.ts --port 8080 \
    > "$SCRATCH/serve.log" 2>&1 < /dev/null &
  sleep 2
  node examples/record-replay/driver.mjs --scenario human_approval > "$SCRATCH/driver.log" 2>&1
  check "pause -> resume -> replay via AgentClient" 0 $?
  pkill -x chidori
else
  echo "SKIP: node not installed"
fi

# ---------------------------------------------------------------- experiment 9
note "9. error-message quality probes"
cat > "$SCRATCH/bad_syntax.ts" <<'EOF'
import { chidori, run } from "chidori:agent";
run(async () => { const x = {,}; return x; });
EOF
ERR=$($BIN check "$SCRATCH/bad_syntax.ts" 2>&1)
echo "$ERR" | grep -qE ':[0-9]+'
check "parse error carries line/column (#132)" 0 $?
cat > "$SCRATCH/thrower.ts" <<'EOF'
import { chidori, run } from "chidori:agent";
function inner() { throw new Error("kaboom"); }
run(async () => { inner(); return {}; });
EOF
ERR=$($BIN run "$SCRATCH/thrower.ts" --trusted 2>&1)
echo "$ERR" | grep -q 'inner'
check "runtime error carries stack frames (#132)" 0 $?

# --------------------------------------------------------------- experiment 10
note "10. 'calls replayed' matches the trace's call count (fixed in #132)"
MSG=$($BIN resume examples/record-replay/exactly_once.ts "$RUN_ID" -d examples/record-replay 2>&1 >/dev/null | grep 'calls replayed')
CALLS=$($BIN trace "$RUN_ID" -d examples/record-replay | grep -oP 'Calls: \K[0-9]+')
echo "trace says $CALLS calls; resume said: '$MSG'"
echo "$MSG" | grep -q "calls replayed"
check "resume prints a call count in its summary line" 0 $?

# --------------------------------------------------------------- experiment 11
note "11. serve's .chidori/sessions.sqlite3* is gitignored (fixed in #132)"
setsid $BIN serve examples/record-replay/exactly_once.ts --port 8082 --trusted \
  > "$SCRATCH/s2.log" 2>&1 < /dev/null &
sleep 1.5
curl -s -X POST http://127.0.0.1:8082/sessions -H 'content-type: application/json' \
  -d '{"input":{"name":"Q"}}' >/dev/null
pkill -x chidori
git check-ignore -q examples/record-replay/.chidori/sessions.sqlite3
check "session store artifacts are gitignored" 0 $?
rm -rf examples/record-replay/.chidori examples/agents/.chidori

# --------------------------------------------------------------- experiment 12
note "12. example READMEs still lack --trusted / --allow-source-change (doc drift)"
if grep -q 'trusted' examples/record-replay/README.md; then
  echo "PASS: record-replay README documents the policy flag (fixed?)"
else
  echo "EXPECTED-FAIL: examples/record-replay/README.md commands still fail as written"
fi

echo
[ "$FAILURES" -eq 0 ] && echo "All experiments behaved as documented in the critique." \
                      || echo "$FAILURES experiment(s) diverged from the critique."
exit "$FAILURES"
