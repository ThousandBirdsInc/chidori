def agent(action):
    answer = input("Approve `" + action + "`?")
    return {"action": action, "approved": answer.lower().startswith("y")}
