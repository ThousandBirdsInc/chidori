def analyze(path, pattern = ""):
    """Analyze code structure of a file or directory — shows functions, classes, methods, and other symbols without reading the full file contents. Use this to understand what's in a file before deciding what to read in detail. Optionally filter symbols with a pattern."""
    # Try ctags first, fall back to grep-based extraction
    ctags_result = try_call(lambda: _ctags_analyze(path, pattern))
    if ctags_result["error"] == None:
        return ctags_result["value"]
    return _grep_analyze(path, pattern)

def _ctags_analyze(path, pattern):
    args = ["-x", "--output-format=xref", "--sort=no", path]
    result = shell("ctags", args = args, timeout_ms = 10000)
    if result["exit_code"] != 0:
        fail("ctags failed: " + result["stderr"])
    lines = [l for l in result["stdout"].split("\n") if l]
    if pattern:
        lines = [l for l in lines if pattern in l]
    return {"symbols": "\n".join(lines[:200]), "count": len(lines), "method": "ctags"}

def _grep_analyze(path, pattern):
    # Match function/method/class definitions across common languages
    grep_pattern = "^[[:space:]]*\\(def \\|fn \\|func \\|function \\|class \\|struct \\|enum \\|trait \\|impl \\|interface \\|type \\|const \\|pub \\|export \\|async \\|module \\)"
    args = ["-rn", grep_pattern, path]
    result = shell("grep", args = args, timeout_ms = 10000)

    if result["exit_code"] == 1:
        return {"symbols": "(no symbols found)", "count": 0, "method": "grep"}
    if result["exit_code"] != 0:
        return {"error": result["stderr"], "method": "grep"}

    lines = [l for l in result["stdout"].split("\n") if l]
    if pattern:
        lines = [l for l in lines if pattern in l]
    return {"symbols": "\n".join(lines[:200]), "count": len(lines), "method": "grep"}
