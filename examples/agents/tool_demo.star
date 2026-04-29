def agent(name = "world"):
    msg = tool("greet", name = name, greeting = "Hi")
    log("greeted", name = name)
    return {"message": msg}
