from chidori.core import ch


@ch.on_event("new_file")
def dispatch_agent(ev):
    ch.set("file_path", ev.file_path)


@ch.on_http("new_file")
@ch.session()
def dispatch_agent(ev):
    ch.set("file_path", ev.file_path)
