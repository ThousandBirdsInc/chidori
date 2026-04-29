def agent(url):
    page = tool("fetch_url", url = url, max_chars = 400)
    return {
        "status": page["status"],
        "title": page["title"],
        "text_preview": page["text"],
        "truncated": page["truncated"],
    }
