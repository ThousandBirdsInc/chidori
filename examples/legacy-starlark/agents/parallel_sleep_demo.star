def agent():
    results = parallel([
        lambda: shell("sleep", args = ["1"], timeout_ms = 5000),
        lambda: shell("sleep", args = ["1"], timeout_ms = 5000),
        lambda: shell("sleep", args = ["1"], timeout_ms = 5000),
    ])
    return {
        "exit_codes": [r["exit_code"] for r in results],
        "timed_out": [r["timed_out"] for r in results],
    }
