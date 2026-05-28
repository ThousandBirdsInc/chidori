def text_editor(path, old_text, new_text = ""):
    """Edit a file by finding and replacing text. The old_text must match exactly one location in the file. Use empty new_text to delete the matched text. Both old_text and new_text should include enough surrounding context to be unambiguous."""
    content = read_file(path)
    count = content.count(old_text)
    if count == 0:
        return {"error": "old_text not found in " + path}
    if count > 1:
        return {"error": "old_text matches " + str(count) + " locations — include more context to make it unique"}
    updated = content.replace(old_text, new_text, 1)
    write_file(path, updated)
    return {"ok": True, "path": path}
