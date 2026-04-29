# Takes ~1 second per call so we can saturate the concurrency semaphore in
# a deterministic way. Requires APP_AGENT_SHELL_ALLOW=sleep.
def agent(label):
    shell("sleep", args = ["1"], timeout_ms = 5000)
    return {"label": label}
