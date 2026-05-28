config(model = "claude-sonnet")

def agent(document):
    summary = prompt(
        "Summarize the following document in 3 bullet points:\n" + document,
    )

    action_items = prompt(
        "Given this summary, extract any action items:\n" + summary,
    )

    return {"summary": summary, "action_items": action_items}
