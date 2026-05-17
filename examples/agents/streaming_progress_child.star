def agent(topic):
    note = prompt(
        "As a nested sub-agent, summarize why labelled prompt streams matter for: " + topic,
        type = "subagent",
        max_tokens = 120,
    )
    return {"note": note}
