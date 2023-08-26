import asyncio
from chidori import Chidori, prompt_node, graph_builder


async def build_and_run_graph():
    c = Chidori("0", "http://localhost:9800")
    await c.start_server(":memory:")

    @prompt_node
    def apa_yipyip():
        return {"template": "Pretend you're Aang talking to Aapa."}

    @prompt_node
    def shrek():
        return {"template": "Say something like you're shrek talking to Aapa."}

    print("lmaooo")
    print(await apa_yipyip)

    (await apa_yipyip).run_when(graph_builder, other_node_handle=await shrek)

    await graph_builder.commit(c, 0)
    await c.play()
    await c.run_custom_node_loop()


if __name__ == "__main__":
    asyncio.run(build_and_run_graph())
