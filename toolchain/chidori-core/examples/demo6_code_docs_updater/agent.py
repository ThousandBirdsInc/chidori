from chidori.core import ch

ch.prompt.configure("default", ch.llm(model="openai"))

def create_dockerfile():
    return ch.prompt("prompts/create_dockerfile")

def migration_agent():
    ch.set("bar", 1)

@ch.on_event("new_file")
def dispatch_agent(ev):
    ch.set("file_path", ev.file_path)

def evaluate_agent(ev):
    ch.set("file_path", ev.file_path)

@ch.generate("")
def wizard():
    pass

@ch.p(create_dockerfile)
def setup_pipeline(x):
    return x

def main():
    bar() | foo() | baz()
