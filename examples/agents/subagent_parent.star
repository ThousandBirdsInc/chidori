def agent(a, b):
    child = call_agent("examples/agents/subagent_child.star", x = a, y = b)
    return {
        "from_child": child,
        "doubled_sum": child["sum"] * 2,
    }
