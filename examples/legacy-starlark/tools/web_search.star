# web_search — query the web via Tavily's search API.
#
# Wraps the built-in `http()` host function. Pure Starlark; no Rust deps.
# Requires a Tavily API key from https://tavily.com/. The tool reads it
# from the `TAVILY_API_KEY` env var and errors out if it's missing so the
# agent gets a clear signal rather than a silent stub.
#
# Usage from an agent:
#   config(model = "claude-sonnet-4-6")
#   def agent(question):
#       hits = tool("web_search", query = question, max_results = 5)
#       context = "\n\n".join([h["title"] + ": " + h["snippet"] for h in hits["results"]])
#       return prompt("Answer using these results:\n" + context + "\n\nQ: " + question)

_TAVILY_URL = "https://api.tavily.com/search"

def web_search(query, max_results = 5, include_answer = True):
    """Search the web with Tavily and return title/url/snippet triples."""
    api_key = env("TAVILY_API_KEY")
    if api_key == None:
        fail("web_search: TAVILY_API_KEY is not set")

    response = http(
        _TAVILY_URL,
        method = "POST",
        headers = {"Content-Type": "application/json"},
        body = {
            "api_key": api_key,
            "query": query,
            "max_results": max_results,
            "include_answer": include_answer,
            "search_depth": "basic",
        },
    )

    if response["status"] != 200:
        fail("web_search: Tavily returned status " + str(response["status"]))

    body = response["body"]
    raw_results = body["results"] if "results" in body else []
    results = [
        {
            "title": r.get("title", ""),
            "url": r.get("url", ""),
            "snippet": r.get("content", ""),
            "score": r.get("score", 0),
        }
        for r in raw_results
    ]

    return {
        "query": query,
        "answer": body.get("answer", "") if include_answer else "",
        "results": results,
    }
