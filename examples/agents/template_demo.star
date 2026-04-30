# Demonstrates the template() host function for prompt construction.

config(model = "claude-sonnet-4-6")

def agent(items, role = "helpful"):
    rendered = template(
        "prompts/analysis.jinja",
        role = role,
        items = items,
    )

    result = prompt(rendered)
    return {"analysis": result}
