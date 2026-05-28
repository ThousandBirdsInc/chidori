def create_file(path, content):
    """Create a new file with the given content. Parent directories are created automatically. Use this for new files only — for modifying existing files, use text_editor instead."""
    write_file(path, content)
    return {"ok": True, "path": path}
