# Standup — Thursday, week 2

**Priya:** Direct-connection audit found one more offender — a cron job in
the billing repo that reads events nightly. Migrated it to the API this
morning. Postgres event reads are now zero for 24 hours straight. Starting
the deprecation doc for the old table; write-primary handoff is still on
schedule for next week.

**Marcus:** In the security review all day. Morning session already surfaced
one finding: the v2 token refresh endpoint doesn't rate-limit by tenant,
only by IP — a NATed customer could get their whole org throttled, or a
noisy tenant could burn the shared bucket. Firm rates it medium. Fix is
straightforward (per-tenant buckets on refresh); I'll have a PR tomorrow.

**Jules:** Enrichment lint rule landed and it's already annotated two PRs
(one was a false positive, which I fixed by narrowing the rule to store
writes only). Also updated the onboarding doc with everything from my first
two weeks — bootstrap, the flag system, how to read the ingest dashboards.

**Amara:** Newsletter writeup drafted — "One bug, three nets: how a data
divergence became a test, a lint rule, and a faster dashboard." Review
appreciated. Fallback code deletion PR is ready for Monday, tagged
do-not-merge until then.

**Dana:** Review wraps at 4; preliminary read is "no highs, one medium, three
lows" which is a good outcome for a new auth flow. Beta metrics day 7: 8 of
9 partners integrated, median time-to-first-call 40 minutes.
