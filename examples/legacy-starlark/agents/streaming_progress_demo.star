# Demonstrates labelled prompt streams for user-facing progress UIs.
#
# Run with:
#   chidori run examples/legacy-starlark/agents/streaming_progress_demo.star --stream \
#     --input '{"topic":"parallel agent progress"}'
#
# Clients can listen for prompt_delta events and filter on prompt_type:
#   - "progress" for incremental status text
#   - "draft" for parallel worker output
#   - "subagent" for nested agent output
#   - "final" for the user-visible answer

def agent(topic = "runtime streaming"):
    progress = prompt(
        "In one short sentence, say what work is starting for: " + topic,
        type = "progress",
        max_tokens = 80,
    )

    drafts = parallel([
        lambda: prompt(
            "Draft two terse bullet points about implementation risks for: " + topic,
            type = "draft",
            max_tokens = 120,
        ),
        lambda: prompt(
            "Draft two terse bullet points about user-facing progress updates for: " + topic,
            type = "draft",
            max_tokens = 120,
        ),
    ])

    sub = call_agent(
        "examples/legacy-starlark/agents/streaming_progress_child.star",
        topic = topic,
    )

    final = prompt(
        "Write a concise final answer for a product user.\n\n"
        + "Topic: " + topic + "\n"
        + "Progress note: " + progress + "\n"
        + "Drafts: " + repr(drafts) + "\n"
        + "Sub-agent note: " + sub["note"],
        type = "final",
        max_tokens = 220,
    )

    return {
        "progress": progress,
        "drafts": drafts,
        "subagent": sub,
        "final": final,
    }
