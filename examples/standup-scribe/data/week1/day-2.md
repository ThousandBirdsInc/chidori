# Standup — Tuesday, week 1

**Priya:** Schema PR merged. Dual-write is ON for 5% of ingest traffic as of
9am — row counts match between Postgres and ClickHouse so far. Ramping to 50%
tomorrow if tonight's soak looks clean. Marcus landed the memory bump, thanks.

**Marcus:** Sharded the browser-matrix job into 4; no OOMs overnight, but
build time went from 14 to 19 minutes because the shards don't reuse the
compile cache. Looking at a shared cache volume today. Onboarding Jules this
afternoon — laptop, repo, first good-first-issue.

**Jules:** Hi everyone! Environment mostly set up. Docs say to run
`make bootstrap` but it fails on Apple Silicon at the protobuf step — Marcus
gave me a workaround, I'll send a docs PR once I understand what it did.

**Amara:** Invoice export shipped behind a flag. The EU slowness is real, not
CDN: p95 on `/api/usage/summary` is 4.1s from Frankfurt vs 800ms from
us-east. It's not the database — query time is flat. Digging into it
tomorrow; treating it as my top priority since two more tickets came in.

**Dana:** Design partners confirmed for the SDK v2 beta — 9 teams. Amara,
flag the latency issue in #incidents if p95 crosses 5s, per the SLO doc.
