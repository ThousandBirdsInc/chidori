def agent():
    results = parallel([
        lambda: 1 + 1,
        lambda: "hello".upper(),
        lambda: [x * x for x in range(4)],
    ])
    return {"results": results}
