# shell() demo — run whitelisted OS commands from inside an agent.
# Set CHIDORI_SHELL_ALLOW before running, e.g.
#   CHIDORI_SHELL_ALLOW=echo,ls ./target/debug/chidori run examples/agents/shell_demo.star

def agent(greeting):
    hello = shell("echo", args = [greeting])

    listing = shell("ls", args = ["-1", "."], timeout_ms = 2000)
    file_count = len([l for l in listing["stdout"].split("\n") if l])

    # Error path: try a command that isn't on the allow list; capture the
    # failure with try_call so the agent can respond instead of crashing.
    disallowed = try_call(lambda: shell("rm", args = ["-rf", "/"]))

    # Timeout path: sleep longer than the timeout.
    slow = shell("sleep", args = ["5"], timeout_ms = 200)

    return {
        "echoed": hello["stdout"].strip(),
        "file_count": file_count,
        "rm_blocked": disallowed["error"] != None,
        "timeout_triggered": slow["timed_out"],
    }
