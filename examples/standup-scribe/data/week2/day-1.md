# Standup — Monday, week 2

**Priya:** Backfill ran Sunday 11pm–3:40am, 4.2B rows, zero errors after one
restart (a chunk hit the CH insert timeout at hour two; the script resumed
from its checkpoint like it was supposed to). We're in the two-day soak now —
row-count and checksum jobs run hourly. Read flip is Wednesday morning if the
soak stays clean.

**Marcus:** Secondary duty was uneventful, dashboard held up. Today I'm on
beta support rotation — two design partners already hit the `client_key`
rename, but both said the breaking-changes section got them through it, so
the guide is doing its job. One partner reports the Python SDK retries on
401, which it shouldn't; filing it.

**Jules:** Pairing with Priya on the ingest path this week as requested.
First task is adding the enrichment-ordering invariant from last week's
divergence bug as an actual test so it can't regress silently.

**Amara:** Metering batch endpoint landed this morning — integrating today.
Plan: switch `/api/usage/summary` to one batched call, keep the parallel
fan-out as an explicit fallback for a week, then delete it. Also the
per-region p95 panel shipped to the on-call dashboard.

**Dana:** Security review for the v2 auth flow is scheduled Thursday with
the external firm — Marcus, you're point. Reminder: on-call rotation
switches to the new schedule next Monday.
