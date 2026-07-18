# Standup — Monday, week 1

**Priya:** Kicking off the ClickHouse migration for the events table this week.
Plan is dual-write from the ingest workers starting tomorrow, backfill the last
90 days over the weekend, then flip reads next week. Schema PR is up (#412).
No blockers yet, but I'll need Marcus to bump the ingest worker memory limits
before dual-write goes on.

**Marcus:** CI is on fire again — the e2e runner OOM-killed four builds on
Friday and two this morning. I suspect the new browser-matrix job; going to
split it into shards today. Also on my list: Priya's memory-limit bump, should
be a one-line Terraform change. Deploy pipeline is otherwise green.

**Amara:** Finishing the invoice-export endpoint today, then I'm picking up
the query planner work for the usage dashboard. Heads-up: support forwarded
two reports of the dashboard feeling slow in EU — I haven't reproduced it yet,
might just be the CDN, keeping an eye on it.

**Dana (EM):** Reminder that the SDK v2 beta goes to the design-partner list
a week from Thursday. Jules starts tomorrow — Marcus, you're onboarding buddy.
Please keep PRs small this week; release branch cuts Friday.
