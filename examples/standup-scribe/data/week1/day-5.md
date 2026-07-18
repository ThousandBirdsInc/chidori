# Standup — Friday, week 1

**Priya:** Dual-write at 100% since 7am, zero divergence in 24 hours at 50%.
Backfill scripts are rehearsed against staging; Sunday 11pm is the window,
Marcus is secondary. After backfill completes we soak for two days, then
flip reads Wednesday. Migration thread is officially back on track.

**Marcus:** Release branch cut at noon as planned. The breaking-changes
section is in the migration guide, `client_key` rename top of the list.
Changelog is clean. I'm on the Sunday backfill as secondary and I set up the
dashboard for it. Also: CI has now gone a full week without an OOM.

**Jules:** Rate-limiter validation PR is up, behind the v2 flag as Dana
suggested — invalid `burst < sustained` configs now fail fast with a clear
error. Wrote tests for the three edge cases Marcus suggested. Next week I'd
like to pick up something in the ingest path to learn that area.

**Amara:** Incident doc for the EU latency is posted (INC-77, "the 40 fast
things"). Parallel fan-out flag is now on for all EU tenants — worst p95
yesterday was 2.1s. Monday I integrate the metering batch endpoint when it
lands and rip the fallback out. Also queued a follow-up to add a per-region
p95 panel to the on-call dashboard so this class of thing surfaces itself.

**Dana:** Beta comms went out; 9 design partners have the migration guide.
Good week — the migration wobbled and recovered, latency contained, and
Jules shipped twice in week one. Sunday backfill crew: get some sleep.
