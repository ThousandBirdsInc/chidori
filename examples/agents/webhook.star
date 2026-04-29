# Webhook event handler agent.
#
# Listens for incoming HTTP events and processes them with an LLM.
# Run with: app-agent serve examples/agents/webhook.star --port 8080
#
# The agent receives every HTTP request as an `event` dict:
#   {
#     "method": "POST",
#     "path": "/github",
#     "headers": {...},
#     "query": {...},
#     "body": {...}   (parsed as JSON if possible, otherwise string)
#   }
#
# Test it:
#   curl -X POST http://localhost:8080/github \
#     -H "Content-Type: application/json" \
#     -d '{"action": "opened", "pull_request": {"title": "Add login page", "body": "Implements OAuth flow", "user": {"login": "alice"}}}'
#
#   curl -X POST http://localhost:8080/alert \
#     -H "Content-Type: application/json" \
#     -d '{"severity": "high", "service": "payments", "message": "Latency spike detected: p99 > 2s for 5 minutes"}'
#
#   curl http://localhost:8080/ping

config(model = "claude-sonnet")

def dict_get(d, key, default = None):
    """Safe dict access — Starlark dicts don't have .get()."""
    if key in d:
        return d[key]
    return default

def agent(event):
    path = event["path"]

    if path == "/ping":
        return {"status": 200, "body": {"pong": True}}

    if path == "/github":
        return handle_github(event)

    if path == "/alert":
        return handle_alert(event)

    return {
        "status": 404,
        "body": {"error": "Unknown path: " + path},
    }

def handle_github(event):
    body = event["body"]
    action = dict_get(body, "action", "unknown")

    summary = prompt(
        "You are a concise code review assistant. "
        + "Summarize this GitHub webhook event in 1-2 sentences "
        + "and suggest any immediate actions the team should take.\n\n"
        + "Event:\n" + repr(body),
        max_tokens = 200,
    )

    log("GitHub event processed", action = action)

    return {
        "status": 200,
        "body": {
            "action": action,
            "summary": summary,
        },
    }

def handle_alert(event):
    body = event["body"]
    severity = dict_get(body, "severity", "unknown")

    diagnosis = prompt(
        "You are an on-call SRE assistant. Given the following alert, "
        + "provide: 1) likely root cause, 2) immediate steps to mitigate, "
        + "3) who to escalate to.\n\n"
        + "Alert:\n" + repr(body),
        max_tokens = 300,
    )

    log("Alert processed", severity = severity)

    return {
        "status": 200,
        "body": {
            "severity": severity,
            "diagnosis": diagnosis,
        },
    }
