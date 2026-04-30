def agent(action):
    answer = input("Approve action '" + action + "'? [y/n]")
    approved = answer.lower().startswith("y")
    return {
        "action": action,
        "approved": approved,
        "answer": answer,
    }
