config(model = "claude-sonnet-4-6")

def agent(name):
    reply = prompt(
        "Use the greet tool to greet " + name + " warmly, then tell me what it returned.",
        tools = ["greet"],
        max_turns = 4,
    )
    return {"reply": reply}
