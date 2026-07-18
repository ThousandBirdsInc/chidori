# Standup — Friday, week 2

**Priya:** Deprecation doc for the Postgres events table is out for review;
removal target is end of month. Next week: hand write-primary to ClickHouse
Tuesday, then the ingest workers lose their dual-write path entirely. Jules
is writing the runbook for the handoff as an exercise, I'm reviewing.

**Marcus:** Per-tenant rate limiting on token refresh is merged and deployed
— the medium finding is closed within 24 hours, which the firm noted. The
three lows are all doc/logging nits; I've filed them for next sprint. Formal
report lands Monday, and I'll circulate a one-page summary.

**Jules:** Runbook draft is half done. Also my lint rule caught its first
real bug in a teammate's PR (a metrics write on an early-return path before
enrichment). Two weeks in: two merged fixes, a test, a lint rule, and a
runbook in flight. This place is fun.

**Amara:** Newsletter piece shipped internally, three teams replied asking
for the lint rule pattern. EU p95 held under 750ms all week; the fallback
deletion merges Monday and closes INC-77 for good. Next week I start on the
usage-dashboard query planner work I postponed in week one.

**Dana:** Strong week. Monday: new on-call schedule starts, security report
lands, fallback deletion merges, and we plan the write-primary handoff.
Have a good weekend, everyone — no Sunday backfills this time.
