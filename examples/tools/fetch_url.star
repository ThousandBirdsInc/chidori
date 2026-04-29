# fetch_url — retrieve a web page and return its visible text.
#
# Wraps the built-in `http()` host function and does a cheap HTML-to-text
# pass: strips <script> / <style> blocks, removes tags, and collapses
# whitespace. Not a real readability parser — good enough for feeding page
# content into an LLM prompt. Starlark has no `while`, so all loops below
# are bounded via `range()` for termination.

_BLOCK_TAGS = ["script", "style", "noscript", "template"]
_MAX_BLOCKS = 256
_MAX_TAGS = 50000

def _strip_blocks(html, tag):
    open_tag = "<" + tag
    close_tag = "</" + tag + ">"
    out = ""
    i = 0
    for _ in range(_MAX_BLOCKS):
        j = html.find(open_tag, i)
        if j == -1:
            out += html[i:]
            return out
        out += html[i:j]
        k = html.find(close_tag, j)
        if k == -1:
            return out
        i = k + len(close_tag)
    out += html[i:]
    return out

def _strip_tags(html):
    out = ""
    in_tag = False
    for ch in html.elems():
        if ch == "<":
            in_tag = True
        elif ch == ">":
            in_tag = False
        elif not in_tag:
            out += ch
    return out

def _collapse_ws(text):
    # Replace runs of whitespace (including newlines) with a single space,
    # then re-introduce paragraph breaks where the original text had them.
    lines = []
    for raw in text.split("\n"):
        stripped = raw.strip()
        if stripped:
            pieces = [p for p in stripped.split(" ") if p]
            lines.append(" ".join(pieces))
    return "\n".join(lines)

def fetch_url(url, max_chars = 8000):
    """Fetch a URL and return its visible text content."""
    response = http(url, method = "GET", headers = {
        "User-Agent": "app-agent/0.1 fetch_url tool",
        "Accept": "text/html,application/xhtml+xml",
    })

    status = response["status"]
    body = response["body"]

    if type(body) != "string":
        text = str(body)
        raw_html = ""
    else:
        raw_html = body
        stripped = body
        for tag in _BLOCK_TAGS:
            stripped = _strip_blocks(stripped, tag)
        text = _collapse_ws(_strip_tags(stripped))

    # Grab a <title> if one was in the original HTML.
    title = ""
    if raw_html:
        lo = raw_html.lower()
        ts = lo.find("<title")
        if ts != -1:
            te = lo.find("</title>", ts)
            if te != -1:
                open_end = raw_html.find(">", ts)
                if open_end != -1 and open_end < te:
                    title = raw_html[open_end + 1:te].strip()

    truncated = len(text) > max_chars
    if truncated:
        text = text[:max_chars]

    return {
        "url": url,
        "status": status,
        "title": title,
        "text": text,
        "truncated": truncated,
    }
