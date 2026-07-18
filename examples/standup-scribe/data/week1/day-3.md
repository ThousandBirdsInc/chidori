# Standup — Wednesday, week 1

**Priya:** Bad news on the migration: the overnight soak at 5% found a
dual-write divergence — 217 rows in ClickHouse have a null `tenant_id` that
Postgres has populated. Root cause is the ingest worker writing the CH row
*before* the enrichment step runs on the retry path. I'm NOT ramping to 50%;
dual-write stays at 5% until the ordering bug is fixed. Marcus is pairing
with me on the retry-path fix this afternoon. This probably costs us the
weekend backfill slot.

**Marcus:** Shared compile-cache volume works — shards are back to 15-minute
builds with zero OOMs in 36 hours. Calling the CI thread done unless it
regresses. Rest of my day is the retry-path pairing with Priya.

**Jules:** First PR is up! It's the Apple Silicon bootstrap fix — turns out
the workaround pins protobuf 25.x, so I made the Makefile detect the arch.
Small, but it's something. Starting on the good-first-issue (rate-limiter
config validation) next.

**Amara:** Found the EU latency: it's not one slow thing, it's 40 fast
things. `/api/usage/summary` fans out one HTTP call *per project* to the
metering service, serially, and EU tenants average 38 projects. us-east
doesn't notice because the metering service is co-located. Fix is a batch
endpoint on metering plus a parallel fan-out fallback. Batch endpoint needs
the metering team — I've opened METER-291 and pinged their lead. p95 touched
4.8s yesterday, still under the 5s SLO line, but barely.

**Dana:** Priya, thanks for holding the ramp — right call. Amara, if
METER-291 doesn't get picked up by Friday, escalate to me.
