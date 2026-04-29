def agent():
    # Success path: try_call returns {"value": ..., "error": None}.
    ok = try_call(lambda: 21 * 2)

    # Failure path: the inner http() call hits an unreachable port.
    failure = try_call(lambda: http("http://127.0.0.1:1/unreachable"))

    # retry: each attempt fails, final error propagates; wrap in try_call.
    retry_outcome = try_call(
        lambda: retry(
            lambda: http("http://127.0.0.1:1/unreachable"),
            max_attempts = 2,
            backoff = "constant",
            initial_delay_ms = 10,
        ),
    )

    return {
        "ok_value": ok["value"],
        "ok_error": ok["error"],
        "captured_error": failure["error"] != None,
        "retry_final_error": retry_outcome["error"] != None,
    }
