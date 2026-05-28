def agent(url = "https://httpbin.org/get"):
    resp = http(url, headers = {"User-Agent": "chidori/0.1"})
    return {
        "status": resp["status"],
        "url_echo": resp["body"].get("url") if type(resp["body"]) == "dict" else None,
    }
