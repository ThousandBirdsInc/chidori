def search(pattern, path = ".", glob_filter = ""):
    """Search for a pattern in files using grep. Returns matching lines with file paths and line numbers. Use glob_filter to restrict to specific file types, e.g. '*.rs' or '*.py'."""
    # Try rg first, fall back to grep -rn
    rg = try_call(lambda: _rg_search(pattern, path, glob_filter))
    if rg["error"] == None:
        return rg["value"]
    return _grep_search(pattern, path, glob_filter)

def _rg_search(pattern, path, glob_filter):
    args = ["-n", "--no-heading", pattern, path]
    if glob_filter:
        args = ["-g", glob_filter] + args
    result = shell("rg", args = args, timeout_ms = 10000)
    if result["exit_code"] == 1:
        return {"matches": [], "count": 0}
    if result["exit_code"] != 0:
        fail(result["stderr"])
    lines = [l for l in result["stdout"].split("\n") if l]
    return {"matches": lines[:100], "count": len(lines), "truncated": len(lines) > 100}

def _grep_search(pattern, path, glob_filter):
    args = ["-rn", pattern, path]
    if glob_filter:
        args = ["-rn", "--include", glob_filter, pattern, path]
    result = shell("grep", args = args, timeout_ms = 10000)
    if result["exit_code"] == 1:
        return {"matches": [], "count": 0}
    if result["exit_code"] != 0:
        return {"error": result["stderr"]}
    lines = [l for l in result["stdout"].split("\n") if l]
    return {"matches": lines[:100], "count": len(lines), "truncated": len(lines) > 100}
