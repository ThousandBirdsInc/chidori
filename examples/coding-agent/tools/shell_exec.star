_MAX_LINES = 2000
_OVERFLOW_DIR = "/tmp/app_agent_overflow"

def shell_exec(command, args = [], cwd = ""):
    """Execute a shell command. Use this for running tests, builds, linters, git operations, and other CLI tools. Returns stdout, stderr, and exit_code. Output longer than 2000 lines is truncated and saved to a temporary file."""
    kwargs = {"timeout_ms": 30000}
    if cwd:
        kwargs["cwd"] = cwd
    result = shell(command, args = args, **kwargs)

    stdout = _truncate(result["stdout"], "stdout")
    stderr = _truncate(result["stderr"], "stderr")

    return {
        "stdout": stdout["text"],
        "stderr": stderr["text"],
        "exit_code": result["exit_code"],
        "timed_out": result["timed_out"],
        "stdout_overflow": stdout["overflow_path"],
        "stderr_overflow": stderr["overflow_path"],
    }

def _truncate(text, label):
    lines = text.split("\n")
    if len(lines) <= _MAX_LINES:
        return {"text": text, "overflow_path": ""}

    # Save full output to temp file
    try_call(lambda: shell("mkdir", args = ["-p", _OVERFLOW_DIR], timeout_ms = 1000))
    ts = shell("date", args = ["+%s%N"], timeout_ms = 1000)["stdout"].strip()
    overflow_path = _OVERFLOW_DIR + "/" + label + "_" + ts + ".txt"
    write_file(overflow_path, text)

    truncated = "\n".join(lines[:_MAX_LINES])
    truncated = truncated + "\n\n... (" + str(len(lines) - _MAX_LINES) + " more lines saved to " + overflow_path + ")"
    return {"text": truncated, "overflow_path": overflow_path}
