# Standup — Tuesday, week 2

**Priya:** Soak is clean — 20 hourly checksum runs, 20 matches. Tomorrow 9am
I flip reads for internal tenants first, external at noon if internal looks
good. Postgres stays as write-primary for two more weeks as the safety net.
Jules's invariant test caught a real thing already, see below.

**Jules:** The enrichment-ordering test I wrote yesterday failed on main! Not
the old bug — a new code path added Friday (the v2 batch ingest) writes
metrics before enrichment on one branch. It's not user-visible yet because
the path is flagged off, but it's exactly the same class of bug. Fix is a
five-liner, PR up, Priya reviewed. Feeling very good about tests today.

**Marcus:** Filed the Python SDK 401-retry bug (SDK-188) and fixed it — it
was retrying on all 4xx, which also explains a rate-limit complaint from
week one. Prepping materials for Thursday's security review the rest of
today: threat model doc and the auth-flow sequence diagrams.

**Amara:** Batch endpoint integrated in staging — `/api/usage/summary` is one
metering call now. Frankfurt p95 in staging: 640ms. Rolling to production
EU tomorrow morning. The 40-fast-things era is nearly over.

**Dana:** Nice catch Jules — that test paid for itself in one day. I want a
short writeup linking INC-77, the divergence bug, and your test in the
engineering newsletter; Amara can you own that Friday?
