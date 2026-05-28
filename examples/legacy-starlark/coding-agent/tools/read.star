def read(path, start_line = 0, end_line = 0):
    """Read the contents of a file with line numbers. For large files, use start_line and end_line to read a specific range (1-indexed, inclusive). Omit both to read the entire file."""
    content = read_file(path)
    lines = content.split("\n")
    total = len(lines)

    if start_line > 0:
        s = start_line - 1
        e = end_line if end_line > 0 else total
        lines = lines[s:e]
        offset = s
    else:
        offset = 0

    numbered = [str(offset + i + 1) + "\t" + line for i, line in enumerate(lines)]
    header = "# " + path + " (" + str(total) + " lines total)"
    if start_line > 0:
        header = header + " — showing lines " + str(start_line) + "-" + str(min(end_line if end_line > 0 else total, total))
    return header + "\n" + "\n".join(numbered)
