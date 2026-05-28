def agent(user, theme):
    memory("set", key = "pref:" + user, value = {"theme": theme})
    loaded = memory("get", key = "pref:" + user)
    all_prefs = memory("list", prefix = "pref:")
    return {
        "loaded": loaded,
        "count": len(all_prefs),
        "all": all_prefs,
    }
