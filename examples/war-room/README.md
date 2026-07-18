# War Room — a multiplayer incident commander served over HTTP

The demo behind
[consumer usability review, round 5](../../docs/consumer-usability-review-5.md):
an incident-response agent **shipped as a service** that a whole on-call team
interacts with at once.

An alert opens an incident. The commander triages it with the model,
investigates with real tools against an ops API, proposes a mitigation — then
opens the run to the humans: a live dashboard streams every token and tool
probe over SSE, and any responder can push context, demand escalation, or
approve the mitigation as **signals** delivered mid-run. If nobody answers
before the deadline, the commander pages the secondary on-call and keeps
waiting. Every human word and model token lands in one durable, replayable
journal.

## Pieces

| File | Role |
|---|---|
| `commander.ts` | The agent: triage → tool investigation → signal fan-in war room → mitigation → postmortem |
| `ops_api.mjs` | Mock ops API (status, runbooks, paging, mitigation execution) on `127.0.0.1:9911` |
| `dashboard.mjs` | TypeScript-SDK streaming client: opens the incident, renders deltas/calls/pauses live |
| `responder.mjs` | TypeScript-SDK signal sender: `note` / `escalate` / `approve` as different humans |
| `policy.json` | Production-style `CHIDORI_POLICY_FILE`: allow `http` + workspace writes, deny-by-default otherwise |

## Run it

```bash
# 0. deps (SDK client for the two Node scripts)
npm install

# 1. the ops API the commander investigates
node ops_api.mjs &

# 2. the server — production posture, not --trusted
export CHIDORI_OPENAI_COMPAT_URL=https://api.deepseek.com
export CHIDORI_OPENAI_COMPAT_KEY=sk-...
export CHIDORI_MODEL=deepseek-v4-flash
export CHIDORI_POLICY_FILE=policy.json        # http scoped to the ops API via url_prefix
export CHIDORI_HTTP_ALLOW_HOSTS=127.0.0.1     # ops API is loopback; SSRF guard needs the opt-in
export CHIDORI_API_KEY=$(openssl rand -hex 16)  # bearer auth; clients pass it as apiKey
export WARROOM_API_KEY=$CHIDORI_API_KEY         # picked up by dashboard.mjs / responder.mjs
chidori serve commander.ts --port 8787

# 3. the dashboard opens the incident and streams it
node dashboard.mjs

# 4. in other terminals, be the war room
node responder.mjs <sessionId> note "redis maxclients was halved in last week's cost cut" --as dana
node responder.mjs <sessionId> escalate --as marcus
node responder.mjs <sessionId> approve mitigate --as sam
```

Let the first `paused` deadline lapse to watch the commander page the
secondary on-call instead of stalling forever.

Or open the incident from a **webhook** instead of the dashboard — a pausing
event run comes back as `202` with a real session id you can signal:

```bash
curl -s -H "Authorization: Bearer $CHIDORI_API_KEY" -H content-type:application/json \
  -X POST http://127.0.0.1:8787/alerts/pagerduty \
  -d '{"alert":{"id":"INC-7002","service":"checkout-api","summary":"5xx spike"}}'
# → {"id":"<sessionId>","status":"paused","pending_signal_names":["approve","note","escalate"],...}
```

## What this exercises

- `chidori serve` under a real **policy file** (not `--trusted`), with the
  SSRF guard opt-in for loopback tools
- `POST /sessions/stream` — SSE prompt deltas, host-call records, live
  `paused` events — via `AgentClient.stream()`
- signal **fan-in** (`chidori.signal(["approve","note","escalate"])`) with
  `timeoutMs` paging, the durable mailbox, and `delivered_live` in-process
  resumes
- crash recovery: kill the server while the war room waits; restart; queued
  signals and the pause survive
- `chidori resume` / `trace` replay of the whole multiplayer session for $0
