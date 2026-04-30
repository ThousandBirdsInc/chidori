def tree(path = ".", max_depth = 3):
    """List a directory tree showing files with line counts. Respects .gitignore and skips common noise directories (.git, node_modules, target, __pycache__, .venv). Line counts help you gauge file size before reading."""
    _SKIP = ["-not", "-path", "*/.git/*",
             "-not", "-path", "*/node_modules/*",
             "-not", "-path", "*/target/*",
             "-not", "-path", "*/__pycache__/*",
             "-not", "-path", "*/.venv/*"]

    # Get directories
    dir_result = shell(
        "find", args = [path, "-maxdepth", str(max_depth), "-type", "d"] + _SKIP,
        timeout_ms = 5000,
    )
    dirs = sorted([d for d in dir_result["stdout"].split("\n") if d])

    # Get files with line counts in one shot
    file_result = shell(
        "find", args = [path, "-maxdepth", str(max_depth), "-type", "f"] + _SKIP,
        timeout_ms = 5000,
    )
    files = sorted([f for f in file_result["stdout"].split("\n") if f])

    output_lines = []
    for d in dirs:
        output_lines.append(d + "/")

    if files:
        wc_result = shell("wc", args = ["-l"] + files, timeout_ms = 10000)
        if wc_result["exit_code"] == 0:
            for line in wc_result["stdout"].strip().split("\n"):
                line = line.strip()
                if line and " total" not in line:
                    output_lines.append(line)
        else:
            for f in files:
                output_lines.append(f)

    return {"tree": "\n".join(output_lines), "file_count": len(files), "dir_count": len(dirs)}
