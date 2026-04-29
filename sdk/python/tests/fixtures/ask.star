config(model = "mock-model")

def agent(question):
    answer = prompt("Q: " + question)
    return {"question": question, "answer": answer}
