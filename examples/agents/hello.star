# Simplest possible agent — just returns a greeting.
# No LLM calls, useful for testing the runtime.

def agent(name = "world"):
    return {"greeting": "Hello, " + name + "!"}
