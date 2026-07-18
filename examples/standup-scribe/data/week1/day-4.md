# Standup — Thursday, week 1

**Priya:** Retry-path fix merged last night — enrichment now runs before any
write, both stores. Replayed the 217 divergent rows; ClickHouse and Postgres
are byte-identical again. Dual-write ramped to 50% at 8am, clean so far.
If tonight holds I ramp to 100% Friday and we do the backfill Sunday night
instead of Saturday — one day late, not two.

**Marcus:** Release branch prep for the SDK v2 beta: cut is tomorrow noon.
I'm auditing the changelog and the migration guide today. One concern: the
v2 auth flow renames `client_secret` to `client_key` and the migration guide
buries it in a footnote — that WILL bite the design partners. Writing a
proper "breaking changes" section this afternoon.

**Jules:** Bootstrap PR merged 🎉. Rate-limiter validation is half done —
I found the config schema allows a `burst` lower than `sustained`, which the
runtime then silently "fixes". I think it should be a validation error
instead. Is that a breaking change? Would love five minutes with someone
after standup.

**Amara:** Metering team picked up METER-291, batch endpoint ETA Monday. In
the meantime I shipped the parallel fan-out behind a flag for the three
loudest EU tenants — their p95 dropped from 4.8s to 1.9s overnight. Not the
real fix, but it buys the SLO room. I'll write the incident doc today even
though we never technically breached.

**Dana:** Jules — yes it's breaking, and yes we should do it; put it behind
the v2 flag with the other breaking changes. Good catch. Everyone: beta
comms go out tomorrow after the branch cut.
