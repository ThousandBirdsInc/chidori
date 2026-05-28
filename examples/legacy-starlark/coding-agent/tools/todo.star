def todo(action, item = "", index = -1):
    """Manage a task checklist to track progress on multi-step work. Actions: 'add' to add an item, 'done' to mark item at index as complete, 'list' to show all items, 'clear' to reset."""
    items = memory("get", key = "_todo_items")
    if items == None:
        items = []

    if action == "add":
        items.append({"text": item, "done": False})
        memory("set", key = "_todo_items", value = items)
        return {"items": items, "added": item}
    elif action == "done":
        if index < 0 or index >= len(items):
            return {"error": "invalid index " + str(index)}
        items[index]["done"] = True
        memory("set", key = "_todo_items", value = items)
        return {"items": items, "completed": items[index]["text"]}
    elif action == "list":
        return {"items": items}
    elif action == "clear":
        memory("set", key = "_todo_items", value = [])
        return {"items": [], "cleared": True}
    else:
        return {"error": "unknown action: " + action + ". Use add, done, list, or clear."}
