# Standup — Wednesday, week 2

**Priya:** Reads flipped: internal tenants at 9am, external at 12:15. Query
latency on the events table is down 60–85% depending on the query shape, and
the two heaviest dashboard queries went from 2.2s to 300ms. One hiccup: a
legacy admin report still pointed at Postgres directly with a raw connection
string — it broke at flip time and we patched it within the hour. I'm
auditing for other direct connections today; there should be none.

**Marcus:** Security-review prep done — threat model, sequence diagrams, and
a test tenant for the firm to poke at. Also rolled the SDK 401-retry fix
into a v2.0.1 patch release; two of the affected partners confirmed the
rate-limit symptom is gone.

**Jules:** Enrichment fix merged. I'm writing the invariant up as a lint
rule now — the pattern ("no store writes before enrichment completes") is
mechanical enough to catch in CI, and then the test AND the linter guard it.
Should land tomorrow.

**Amara:** Batch endpoint live in production EU since 8am — Frankfurt p95 is
710ms, worst tenant 900ms. The SLO panel has been green all day. Fallback
deletion is scheduled for Monday after a week of quiet. Starting the
newsletter writeup Dana asked for.

**Dana:** The migration flip going this smoothly after last week's wobble is
exactly why we soak. Tomorrow: security review 10am–4pm, Marcus is point,
please keep his calendar clear.
