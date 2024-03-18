# Services to schedule tasks

## Local

Local scheduler events are managed by the `chidori-core` library and are executed in-process.

### Tokio Cron Scheduler

This runs a cron scheduler on a background thread, triggering execution when it is time to do so.


## Remote

Remote  scheduler events leverage remote services in order to facilitate the management of scheduled events.

### Redis Cron Scheduler

This uses redis as the source for cron events, triggered on push events from the redis instance.
