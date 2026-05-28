config(model = "claude-sonnet-4-6")

_TOOLS = ["text_editor", "tree", "search", "shell_exec", "create_file", "read", "todo", "analyze"]

def agent(task, working_dir = "."):
    # Gather context to inject into the system prompt (MOIM pattern)
    git_branch = _get_git_branch(working_dir)
    timestamp = _get_timestamp()
    top_level = _get_top_level_listing(working_dir)

    system = template(
        "prompts/system.jinja",
        working_dir = working_dir,
        git_branch = git_branch,
        timestamp = timestamp,
        top_level_listing = top_level,
        context = "",
    )

    result = prompt(
        task,
        system = system,
        tools = _TOOLS,
        max_turns = 50,
    )

    todo_state = try_call(lambda: memory("get", key = "_todo_items"))
    todos = todo_state["value"] if todo_state["error"] == None else []

    return {
        "result": result,
        "todos": todos,
    }

def _get_git_branch(dir):
    r = try_call(lambda: shell("git", args = ["branch", "--show-current"], cwd = dir, timeout_ms = 2000))
    if r["error"] == None and r["value"]["exit_code"] == 0:
        return r["value"]["stdout"].strip()
    return ""

def _get_timestamp():
    r = try_call(lambda: shell("date", args = ["+%Y-%m-%d %H:%M"], timeout_ms = 1000))
    if r["error"] == None:
        return r["value"]["stdout"].strip()
    return ""

def _get_top_level_listing(dir):
    r = try_call(lambda: shell("ls", args = ["-1", dir], timeout_ms = 2000))
    if r["error"] == None and r["value"]["exit_code"] == 0:
        return r["value"]["stdout"].strip()
    return ""
