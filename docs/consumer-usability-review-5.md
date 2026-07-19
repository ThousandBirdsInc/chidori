---
title: "Round 5: Shipping to Users"
description: "Round 5: shipping to users \u2014 serve in production posture, SSE streaming, multiplayer signals under crashes, SDK-as-client, webhooks."
---

# Consumer usability review, round 5: shipping an agent to users

**Date:** 2026-07-18 · **Chidori:** 3.6.1, built from source at `15d69ec` ·
**Perspective:** the same consumer as rounds
[1](./consumer-usability-review.md) · [2](./consumer-usability-review-2.md) ·
[3](./consumer-usability-review-3.md) · [4](./consumer-usability-review-4.md)
— a developer whose provider is DeepSeek — who has now built an agent they
like and wants to **put it in front of other people**. Rounds 1–4 lived on
the developer's own terminal. This round lives on the wire: `chidori serve`
in the production posture the deployment docs prescribe (policy file, API
key, no `--trusted`), the SSE streaming surface, multiplayer signals under
real humans and real crashes, the TypeScript SDK as the client a product
team would actually write, the event-driven webhook surface, and the
[deployment checklist](./deployment.md#production-checklist) item by item.

## What was built

A **War Room** ([`examples/war-room/`](../examples/war-room/)): an incident
commander served over HTTP that a whole on-call team talks to at once, live
on `deepseek-v4-flash`:

1. an alert opens an incident; the commander **triages** it (structured
   JSON prompt) and **investigates** with two `defineTool` tools probing a
   local ops API (service status, runbooks) via the captured `fetch`;
2. it proposes a mitigation and opens the **war room**: a fan-in listen
   point — `chidori.signal(["approve", "note", "escalate"], { timeoutMs })`
   — where any responder can push context, demand escalation, or approve;
3. nobody answers before the deadline → it **pages the secondary on-call**
   through the ops API and keeps waiting; twice unanswered → stands down;
4. approval on the record → it executes the mitigation (a real POST) and
   publishes a postmortem with `workspace.write`.

Around it, the two client programs a product team would write with the
TypeScript SDK: a **dashboard** (`AgentClient.stream()` — renders prompt
deltas, tool probes, and war-room pauses as they happen) and a **responder**
CLI (`AgentClient.signal()` as different named humans). The server ran under
a hand-written `CHIDORI_POLICY_FILE` — not `--trusted` — with
`CHIDORI_HTTP_ALLOW_HOSTS=127.0.0.1` for the loopback ops API, and later
under `CHIDORI_API_KEY` bearer auth.

## The numbers first

| Scenario | Result |
|---|---|
| Full multiplayer incident, first attempt: stream → context note from "dana" (`delivered_live`, in-process resume on the same SSE stream) → plan revised → approve from "sam" → mitigation POST → postmortem published | **Completed first try, 42s**, 17 recorded calls, $0.0013 |
| Unattended incident (15s war-room deadline, nobody answers) | **Exactly as designed**: three server-side deadlines fired on one live stream, two real pages hit the ops API, stand-down postmortem written; 68s |
| `kill -9` the server while the war room is paused; restart | **Pause survived perfectly**: status `paused`, fan-in names and deadline intact; approve resolved the durable pause and the rest of the run (mitigation, postmortem, publish) completed inside that one request — with the pre-crash context note in the output |
| …but the pause's **expired timeout** after that restart | ~~Never fired~~ — **finding retracted**: the deadline was still 3 minutes in the future during the poll, and the manual approve preempted the correctly re-armed timer. Deterministic repro shows boot re-arm works in every shape (Finding 3, [retracted](#follow-up-2026-07-18-fixes-landed-and-one-finding-retracted)) |
| `chidori verify` on the crash-spanning multiplayer run | **exit 0 in 30ms, $0**, output identical — a multiplayer run that survived SIGKILL is a valid CI fixture |
| Replay of a completed run (invalid provider key) | 33ms, $0, correct output — reported as "16 replayed, **1 executed live**", which is actually a workspace re-materialization that rewrites the published file's mtime on every replay (Finding 7) |
| Server under `CHIDORI_API_KEY` (the production checklist's posture) vs the official SDKs | **Both SDKs locked out — neither can send a bearer token at all** (Finding 1) |
| Webhook (`ANY /*`) opening an incident that pauses | **Run orphaned**: 3 prompts billed, durable pause persisted — HTTP response is `null`, no session id, no endpoint can ever deliver its signals (Finding 2) |
| Policy file scoping `http` to the ops API | **Inexpressible**: `match_args` is exact-subset only — one exact URL allowed, the very next runbook URL denied; no prefix/host matching (Finding 6) |
| SSRF-guard and policy denials during a run | Fired correctly with **excellent message text** — visible only in the journal; the tool loop swallowed them and the model confidently planned without data (Finding 5) |
| Whole campaign: 9 live runs, 26 prompt calls, every scenario above | **$0.006 by chidori's meter**; DeepSeek balance unmoved at display precision (19.89 → 19.89) |

Same shape as every prior round, sharper than ever: **the engine went nine
for nine** — nothing in the recording, replay, pause, mailbox, or
crash-recovery machinery so much as flinched, under SIGKILL, with humans
racing the process. (On re-verification it went ten for ten: the one
engine-side finding, the restart timer, was my own test error — see the
follow-up.) What failed is everything *around* the engine that a team
shipping to users touches first: the client SDK can't authenticate, the
webhook surface strands runs, and the operator can't see denials
happening.

## The verdict up front

The war-room demo is the best case I can make for this framework, and it
made itself: a **multiplayer, streaming, crash-surviving, auditable
incident session** — humans pushing context into a live model loop with
full attribution, a deadline that pages when ignored, and the entire
collaboration replayable byte-for-byte for $0 in 30ms — was ~250 lines of
plain TypeScript and worked **on the first attempt**. No other framework I
know gives you `signal(["approve","note","escalate"], {timeoutMs})` as one
awaited line whose consumption is a durable, attributed, replayable record.

But this round's premise was "now ship it," and on that premise the sharp
edges are not cosmetic. Following the project's own production checklist
locks the project's own SDKs out of the server (Finding 1). Pointing a
webhook at the flagship event surface silently strands paid-for runs
(Finding 2). (A third apparent contradiction — the restart dropping the
paging deadline, Finding 3 — turned out on re-verification to be my own
test error; retracted in the follow-up.) None of these are deep flaws —
every one looks like a week of work — but all of them sit on the exact
path from "demo that thrilled me" to "service my team uses," and two
directly contradict a documented promise.

---

## Finding 1: the production checklist locks the official SDKs out of the server

**Severity: blocker for any authed deployment · Docs contradiction**

The deployment docs are unambiguous: set `CHIDORI_API_KEY`
(checklist item 1); a non-loopback bind refuses to start without it. The
server enforces it well — constant-time comparison, comma-separated
rotation, `/health` exempt, a clear 401 (`missing or invalid bearer
token`), and an honest startup banner (`Auth: REQUIRED`).

Neither SDK can satisfy it. `AgentClientOptions` (TypeScript) is
`{timeoutMs, retries, retryDelayMs}` — no `apiKey`, no `headers`, no
request hook; the Python client is the same. Every documented client call
against a checklist-compliant server fails:

```
HttpError: POST /sessions/.../signal failed: HTTP 401: missing or invalid bearer token
[dash +0.1s] stream failed: POST /sessions/stream failed: HTTP 401: missing or invalid bearer token
```

The SDK README's own worked examples cannot run against the deployment
docs' own configuration. The workaround I shipped the dashboard with is a
global-fetch monkey-patch:

```js
const of = globalThis.fetch;
globalThis.fetch = (u, i = {}) =>
  of(u, { ...i, headers: { ...(i.headers ?? {}), authorization: `Bearer ${KEY}` } });
```

— which works (and is itself evidence the fix is trivial), but no team
should ship auth via global mutation, and in a browser (the SDK advertises
browser support, and `CHIDORI_CORS_ORIGINS` exists for exactly that) the
patch pollutes every other request on the page.

**Fix:** `new AgentClient(url, { apiKey })` (+ `headers` escape hatch) in
both SDKs, applied in `request()` *and* the hand-rolled `stream()` fetch;
same for Python. One option, five call sites, closes the gap between the
two halves of the project's own documentation.

## Finding 2: the event-driven surface strands any run that pauses — after billing for it

**Severity: broken product path · silent money loss**

"React to webhooks" is a README bullet, and `ANY /* → agent(event)` is on
the startup banner. So I pointed the alert webhook at the commander —
the obvious product shape: *PagerDuty opens the war room.*

What actually happens when the agent reaches its listen point:

- the webhook response is the four bytes `null` (HTTP 200, ~12s later);
- **no session exists** — `GET /sessions` doesn't list it, so
  `POST /sessions/{id}/signal` has no id to target, ever;
- the run *did* execute — 10 journaled calls, **3 billed DeepSeek prompts**
  — and *did* persist a durable `signal_any` pause (`pending.json`, mailbox
  armed). The engine did everything right; the surface then dropped the
  handle on the floor.

The stranded run isn't even rescuable from the CLI in any useful way:
`chidori resume commander.ts <run_id>` replays the 10 calls, hits the
pause, prints `null` and `(10 recorded calls replayed, 0 executed live)`,
and **exits looking like success** — no mention that the run is waiting on
`approve`/`note`/`escalate`, and the recorded `timeoutMs` is not honored
outside the server (no page, no sentinel — the deadline machinery is
server-only, which nothing documents).

Two adjacent surprises while probing this surface:

- **Every stray request runs the whole agent.** `GET /favicon.ico` (or any
  scanner probe) executes `agent(event)` end to end. My agent had a cheap
  "no alert → 400" guard; the `init`-template agents all reach the model,
  so on a loopback-default server (auth off) *every drive-by request costs
  tokens*.
- **The event dict's shape is documented nowhere.** `{event: {method,
  path, headers, query, body}}` had to be read out of
  `server/events.rs`, and a typed session-shaped agent greets its first
  real webhook with `TypeError: Cannot read properties of undefined` (the
  error frame at least names the agent line — round 1's fix paying off).

**Fix:** event runs should create real sessions — return
`{id, status, pending_signal_names}` on pause instead of `null` — or, if
the surface is meant to stay synchronous, refuse pausing host calls on it
with an error naming the session API. Document the event shape in
running-modes.md, and say out loud that signal deadlines are enforced only
by a live server.

## Finding 3: signal deadlines do not re-arm after a restart

> **RETRACTED (same day).** This finding was a test error on my side, not a
> framework bug — see the [follow-up](#follow-up-2026-07-18-fixes-landed-and-one-finding-retracted)
> for the disproof and what actually happened. The original text is kept
> below unedited, as a record of how convincing a false negative can look.

**Severity: durability promise broken · direct docs contradiction**

[signals.md](./signals.md) on `timeoutMs`: *"an in-process timer armed
against a persisted `pending_signal_deadline` on the session,* ***re-armed
for every paused session at server startup***.*"* And the deployment docs'
rule 3 says auto-restart *is* the recovery mechanism.

Reality: SIGKILL the server while the war room is paused with a live
deadline, restart it, and the session comes back `paused` with
`pending_signal_deadline` intact — **in the past** — and nothing happens.
I polled for over a minute past the persisted deadline: no timeout
sentinel, no page to the secondary on-call, no state change. The timer is
armed only on the live-worker path; the boot path forgets it.

For this agent that's the difference between "the secondary got paged at
09:43" and "the incident sat silent until a human happened to look." The
worst part is the failure's shape: everything *visible* recovered
perfectly (status, names, deadline all correct in the session JSON), so an
operator has every reason to believe the deadline is live. A dead timer
that displays its deadline is worse than an honest error.

(The rest of crash recovery was flawless — see the credits.)

**Fix:** on startup, scan paused sessions for `pending_signal_deadline`
and arm timers, firing immediately for already-expired ones — the exact
promise signals.md already makes. The detached-agent fleet re-arms alarms
at boot, so the pattern exists in-tree.

## Finding 4: the multiplayer audit trail is invisible everywhere except the raw journal

**Severity: observability gap · docs show a UI that doesn't exist**

The signals pitch is *attribution* — "the trace records who steered the
run." The data is there: `records.jsonl` faithfully carries
`{from: {kind:"human", id:"dana"}, payload: {...}}` for every consumed
signal. But no surface shows it:

- **The live stream omits signal records entirely.** The dashboard renders
  `call` events for every host call — and the journal's `signal_any`
  records (#11, #13) simply never arrive as SSE events. My dashboard
  numbering jumps #10 → #12. A war-room UI cannot show "note from dana"
  *live* — during the one session where attribution matters most — without
  re-fetching the checkpoint after each pause.
- **`chidori trace` prints signal args but not results.** You see
  `signal_any {"names":["approve","note","escalate"]}` — the listen point
  — but not who answered or what they said; `from`/`payload` live in the
  result, which trace doesn't render for any call. The worked example in
  signals.md shows a trace with `from=agent:compliance-bot
  decision=changes` inline; the real CLI has no such rendering (and no
  verbose flag to ask for it).

So the round-1 promise "trace gives you who-said-what" is, in round 5
practice, `python -c 'json.loads...'` over `records.jsonl`.

**Fix:** emit consumed-signal `CallRecord`s as SSE `call` events (they are
host calls; the omission looks like an oversight), and teach `trace` to
render `signal*` results (`← note from human:dana: "..."`) — or add
`trace --results`.

## Finding 5: policy and SSRF denials are silent where the operator sits

**Severity: silent degradation**

The denial messages themselves are the best I've seen in this series —
self-remedying, e.g.:

> `SSRF protection: '127.0.0.1' resolves to non-public address 127.0.0.1,
> which the http effect refuses to reach; set CHIDORI_HTTP_ALLOW_HOSTS
> (comma-separated hostnames, IPs, or CIDRs; '*' disables the guard) to
> allow it`

But run the commander without the allowlist and *watch the console*:
nothing. The tool's `fetch` throws inside the provider tool loop, the
model receives the error as a tool result, shrugs, and **writes a
confident mitigation plan with zero real data**. The run "succeeds." The
only place the denial exists is `ERROR:` annotations on http records in
the journal — which you will read only if you already suspect something.
Same story for policy denials (`policy: http denied (war-room: only the
exact ops status URL is allowed)`). A misconfigured production server
doesn't fail; it quietly produces plausible garbage — for an *incident
response* agent, radioactive.

Two adjacent policy-consumer facts from this round:

- **The policy-file schema is documented nowhere.** The deployment
  checklist requires `CHIDORI_POLICY_FILE`; sandbox-model.md says "copy
  the profile's shape" without showing it. I wrote mine by reading
  `src/policy.rs` (`{rules: [{target, decision, match_args?, reason?}],
  default, default_reason}`, snake_case decisions). It then worked
  exactly as the source says — including layering — but source-diving is
  the only path.
- **`match_args` cannot scope `http` by host or prefix** (exact JSON
  subset only): allowing my ops API's exact status URL denies its runbook
  URL one call later. Real policies therefore collapse to `http:
  always_allow` + `CHIDORI_HTTP_ALLOW_HOSTS` as the only real scoping —
  workable, but it means the *policy* file can't express the most common
  production rule ("this agent talks to these hosts only").

**Fix:** a stderr warning per denied gated call under `serve`/`run` (one
line, seq + target + reason — the text already exists); a policy-file
section with the JSON shape in sandbox-model.md; host/prefix matching for
`http` targets (even just `{"url_prefix": ...}`) in `match_args`.

## Finding 6: the published SDK is a version behind the runtime it must match

**Severity: first-hour friction for every new TypeScript consumer**

The SDK README warns: *"install the SDK version matching your chidori
binary."* You can't. Runtime built from `main` is 3.6.1-era; npm's latest
`@1kbirds/chidori` is 3.6.0, and against it:

- **`defineTool` doesn't exist in the published types** — the README's and
  llm.txt's flagship authoring idiom fails typecheck on
  `import { defineTool }`;
- the **fan-in `signal(string[])` overload is missing** — this round's
  centerpiece listen point is a type error;
- `Session.runId` is missing, so an SDK client can't learn the id that
  `chidori trace`/`resume`/`verify` take (it's `run_id` in the JSON; the
  published `sessionFrom` drops it) — the exact CLI-correlation gap
  round 3 got fixed, un-fixed by version lag; `pendingDetails` likewise.

Workaround: `npm install -D ../../sdk/typescript` from a source checkout —
fine for me, not for a consumer who installed the runtime via the curl
one-liner and has no checkout. Smaller true-SDK gaps met on the way, all
survivable but each a paper cut: `prompt(..., {format: "json"})` is still
typed `string` (every structured triage needs `as unknown as T`);
`stream()` accepts no `policyProfile` even though `POST /sessions/stream`
does; and once an SSE stream drops there is **no way to re-attach** — no
`GET /sessions/{id}/stream` — so a dashboard that hiccups is reduced to
polling `getSession` (my dashboard's post-crash experience: 13 s of
silence, then `stream failed: terminated`).

**Fix:** publish SDKs in lockstep with the runtime (the version-match
warning implies the policy; CI can enforce it), type `format:"json"`
prompts as `AgentJson`, accept `policyProfile` in `stream()`, and add a
stream-re-attach endpoint (last-N-events replay would make dashboards
trivial to write correctly).

## Finding 7: "replay" quietly re-executes workspace writes, and the CLIs describe one mechanism three ways

**Severity: trust erosion at the feature you're told to trust most**

Replaying my *completed* multiplayer run (`chidori resume`, provider key
deliberately invalid) printed:

> `16 recorded calls replayed, 1 executed live`

"Executed live" — during what the README calls a byte-identical, zero-cost
replay — sent me straight to the ops API's state to check whether the
mitigation POST had re-fired. It hadn't; the "live" call is the final
`workspace.write`, re-applied on every replay (the published postmortem's
mtime changes each time). `chidori verify` names the same mechanism
honestly — "16 calls replayed, **1 workspace re-materialization(s)**" —
while llm.txt describes verify as running with "no writes," and `verify`
also rewrites the file's mtime. So one deliberate mechanism
(re-materializing derived workspace artifacts) surfaces as: a scary
mislabel on `resume`, an accurate label on `verify`, and a contradicted
promise in llm.txt.

Consistency nits in the same family: `chidori run` reaching a signal pause
prints an exemplary message (names + the exact `POST .../signal` line to
deliver); `chidori resume` reaching the *same* pause prints `null` and
exits 0 (see Finding 2); `chidori stats` reports `Tool calls: 0` for a
campaign with 28 `defineTool` invocations (in-VM tools journal as `mark`,
which stats doesn't count); and `CHIDORI_PRICING` must ride along in every
shell's env — cost display is `unknown` in any terminal without it, since
pricing isn't journaled with the run.

**Fix:** label re-materialization as itself on `resume`; correct llm.txt's
"no writes"; make `resume` print the same pause guidance as `run`; count
`mark`-journaled tool invocations in `stats`; journal the pricing table
with the run.

---

## What worked — and deserves to be said just as loudly

- **The multiplayer engine is real, and it is the product.** Fan-in
  signals, the durable mailbox, `delivered_live` in-process resumes on an
  open SSE stream, server-side deadlines firing repeatedly on one live
  run, attributed consumption records — every mechanism signals.md
  describes behaved exactly as specified, first try, with real humans
  racing a real model loop.
- **Crash recovery of a live multiplayer session is genuinely flawless**
  (timer aside): SIGKILL between listen points, restart, and the paused
  war room is standing there with its names and deadline; one `signal`
  call resolved the durable pause and drove the run — mitigation POST,
  postmortem, publish — to completion, with pre-crash context intact. Then
  `chidori verify` certified the whole crash-spanning, multi-human session
  in 30ms for $0. That last sentence is science fiction in any other
  agent framework I've used.
- **The security posture defaults are right**: loopback bind unless
  auth is set, deny-by-default policy on `serve`, ask-by-default on `run`,
  SSRF guard on by default *even under `--trusted`*, per-session profiles
  that can only tighten. The startup banner states the active posture
  (`Auth: REQUIRED`, `Policy: from CHIDORI_POLICY* configuration`,
  `Isolation: on — process-per-run worker`) — an operator can read their
  security position off one screen.
- **Error text quality is now consistently excellent** — SSRF and policy
  denials name their own remedy; the 401 is crisp; agent exceptions carry
  the right source frame. (The remaining problem is *where* they surface —
  Finding 5 — not what they say.)
- **DeepSeek onboarding has gone from round 1's wall to genuinely zero
  friction**: two env vars and the README's own Quick Start ran unmodified
  on the first attempt. Even the README's stale `deepseek-chat` example
  still works (DeepSeek aliases it server-side; still worth updating to a
  listed model).
- **Serve hot-reloads agent source per run** — edit `commander.ts`, next
  session runs the new code, no restart. Lovely for development (and worth
  a deployment-docs note on pinning, since it also means a prod server
  runs whatever is on disk *now*).
- **Cheap beyond argument**: the entire round — nine live runs, streaming,
  crashes, replays, verifies — cost $0.006.

## Where that leaves a round-5 consumer

Rounds 1–4 asked whether the engine can be trusted; they answered yes, and
round 5 re-confirmed it under the most adversarial conditions yet. Round
5's question was whether a team can *put this in front of users*, and the
answer at review time was: **not on the documented path, today** — not
because anything is architecturally wrong, but because the last mile was
unfinished in specific, small places: an SDK auth option (Finding 1), a
session handle for event runs (Finding 2), signal events on the stream
(Finding 4), a stderr line per denial (Finding 5). (The boot-time timer
scan this list originally included was Finding 3, since retracted — it
already worked.) None of these looked like more than days of work, and the
[follow-up](#follow-up-2026-07-18-fixes-landed-and-one-finding-retracted)
below confirms it: all of them landed the next day. The engine underneath
was already there.

## Follow-up (2026-07-18): fixes landed, and one finding retracted

All findings were re-worked the day after the review, with regression tests;
each item below names what shipped. One finding did not survive its own
re-verification and is retracted.

**Finding 3 — RETRACTED (my test error, not a framework bug).** Rebuilding
the crash timeline from the artifacts showed the "expired" deadline
(`00:10:47`) was still **three minutes in the future** during my 60-second
poll — the 300s `timeoutMs` I had set for the crash scenario put it well
past the restart, I misread it as already-past, and my manual `approve`
then resolved the pause before the (correctly re-armed) timer could ever
fire. A deterministic mock-provider reproduction of all three shapes —
deadline already expired at boot, deadline still in the future at boot, and
the exact war-room shape (an in-process live resume before the crash) —
shows the boot re-arm firing correctly every time, including an immediate
fire for an already-expired deadline. The behavior signals.md promises is
real; a regression test now pins it
(`signal_timeout_rearm_fires_for_deadline_persisted_by_a_dead_server`).
Left as a lesson: the failure looked exactly like rounds 1–4's real
failure-path bugs, and I stopped verifying one step too early.

**Finding 1 — fixed.** Both SDKs authenticate: `new AgentClient(url,
{ apiKey })` (TypeScript) / `AgentClient(url, api_key=...)` (Python) send
the bearer token on every request including the SSE stream, with a
`headers` escape hatch (an explicit `Authorization` wins). Covered by SDK
tests; the deployment docs and both SDK READMEs now show it, and the
war-room dashboard/responder use it via `WARROOM_API_KEY`.

**Finding 2 — fixed.** An event-driven run that pauses is now persisted as
a **real session** and answered `202 Accepted` with the session view (id,
status, pending signal names), so a webhook can open a war room and hand
its caller the id to drive it; completed event runs stay stateless (no
session row per scanner probe), and event runs now respect the same
concurrency semaphore as sessions. The event dict shape and both behaviors
are documented in running-modes.md and llm.txt. Regression tests:
`event_run_that_pauses_becomes_a_deliverable_session`,
`event_run_that_completes_stays_stateless`. The CLI half also landed:
`chidori resume` reaching a signal pause now prints the same
deliver-instructions message `chidori run` does (naming the server-side
nature of delivery and deadlines) instead of a bare `null`.

**Finding 4 — fixed.** The consumed signal record now rides the live SSE
stream as a `call` event (it was the one host call the stream omitted), so
a dashboard can render "note from human:dana" the moment it lands —
asserted in `stream_session_resolves_signal_pause_in_process`. And
`chidori trace` renders signal results inline
(`signal_any {...} ← note from human:dana: {"text":"..."}`, with timeout
and empty-poll variants), so the trace finally is the multiplayer audit
trail signals.md advertises.

**Finding 5 — fixed.** Policy and SSRF denials now print one line to the
server/CLI **stderr** at the moment they fire (the full self-remedying
message), in addition to the journal — a tool loop can still swallow the
error, but the operator hears it. The policy-file schema is documented in
sandbox-model.md (shape, targets, decisions, `match_args` semantics, the
fail-closed guarantee). And scoping `http` by host is now expressible *and
safe*: the discovery here was that string `match_args` had always
substring-matched (undocumented — and unsafe as a boundary, since
`{"url": "http://ops:9911/"}` would also match that text inside a hostile
URL's query string); the new reserved `url_prefix` key **anchors** at the
start of the URL (`url_prefix_match_args_anchor_at_the_start` pins the
bypass case). The war-room policy.json now uses it.

**Finding 6 — partly fixed, partly a release step.** In the SDK source:
`prompt(text, { format: "json" })` now types as `Promise<AgentJson>` (no
more double-cast), and `stream()` accepts `{ policyProfile }` like `run()`.
`Session.runId`/`pendingDetails` and the fan-in overload were already in
the repo SDK — the remaining gap is *publishing*: the npm/PyPI packages
must ship in lockstep with the runtime (both now sit at 3.6.1 in-tree;
cutting the release is a maintainer step this branch can't perform). A
stream re-attach endpoint remains future work.

**Finding 7 — fixed.** `chidori resume` reports the honest split —
`N recorded calls replayed, M workspace re-materialization(s), K executed
live` — instead of folding re-materializations into "executed live";
llm.txt's `verify` description now says workspace writes re-materialize
(same bytes, fresh mtime) rather than "no writes"; and `chidori stats`
counts in-VM `defineTool` invocations (journaled as `mark("tool:...")`)
instead of reporting `Tool calls: 0` for the most common agent shape.

**Live re-verification.** Every fix was then exercised end-to-end against
live DeepSeek with the same war-room demo, in the full production posture
(`CHIDORI_API_KEY` + `CHIDORI_POLICY_FILE` with `url_prefix` + SSRF
allowlist), on the rebuilt binary:

| Fix | Live result |
|---|---|
| SDK auth (F1) | Dashboard streamed and responders signaled through bearer auth, first try — full multiplayer incident (note → revise → approve → mitigate → postmortem) in 54s |
| Webhook sessions (F2) | `POST /alerts/pagerduty` with a pausing incident → `202` with `{id, status: "paused", pending_signal_names, run_id}`; one `responder.mjs approve` completed it. Stray `GET /favicon.ico` → 400, zero session rows added |
| Signal records on the stream (F4) | `call #11 signal_any` arrived on the SSE stream at delivery; `chidori trace` renders `← note from human:dana: {"text":"..."}` and `← approve from human:sam: {"decision":"mitigate"}` |
| Audible denials + `url_prefix` (F5) | The same policy that allowed every `127.0.0.1:9911` call denied a `:9912` probe — with `chidori: policy: \`http\` denied (war-room server allows only the ops API...)` on the server console at fire time |
| Replay labels + pricing (F7) | `resume` of the completed run: "16 recorded calls replayed, **1 workspace re-materialization(s), 0 executed live**"; `resume` of a signal-paused run prints delivery guidance instead of `null`; `stats` reports `Tool calls: 4`; `trace` priced the run ($0.001359) in a shell with **no** `CHIDORI_PRICING` set, from the manifest-journaled table |

Test additions: 5 server/policy regression tests (boot re-arm, event-pause
session, event statelessness, SSE signal record, `url_prefix` anchoring)
and 2 SDK auth tests — 426/426 lib tests and 36/36 SDK tests pass.
Verification spend: ~$0.01 of DeepSeek.

With these landed, the review's bottom line moves: the "one focused
sprint" items are done (in this branch; publishing remains), and the
sharpest sentence in the verdict — *"not on the documented path, today"* —
no longer holds for auth, webhooks, or observability. What remains open:
SDK package publishing, and a stream re-attach endpoint for dashboards
that lose their SSE connection.

## Appendix: what was run

All sources in [`examples/war-room/`](../examples/war-room/): the
commander agent (~250 lines), the mock ops API, the SDK dashboard and
responder clients, and the policy file. The campaign, in order: the
first-try multiplayer session (note → revision → approval → mitigation →
postmortem); the unattended paging/stand-down session; the SIGKILL +
restart + durable-approve session and its 30ms `verify`; replay/verify
re-materialization checks; the auth matrix (curl vs both SDK clients vs
monkey-patch); the webhook-orphan probe and its CLI rescue attempt; the
SSRF-silence run; and the exact-URL policy-scoping run. 9 runs, 26 prompt
calls, 6.0k in / 9.5k out tokens plus 6.5k cache reads, $0.006 total.
