import aiohttp
import asyncio
from typing import List, Optional
import json
from collections import defaultdict
from chidori import Chidori, GraphBuilder


class Story:
    def __init__(self, title: str, url: Optional[str], score: Optional[float]):
        self.title = title
        self.url = url
        self.score = score

HN_URL_TOP_STORIES = "https://hacker-news.firebaseio.com/v0/topstories.json?print=pretty"

async def fetch_story(session, id):
    async with session.get(f"https://hacker-news.firebaseio.com/v0/item/{id}.json?print=pretty") as response:
        return await response.json()

async def fetch_hn() -> List[Story]:
    async with aiohttp.ClientSession() as session:
        async with session.get(HN_URL_TOP_STORIES) as response:
            story_ids = await response.json()

        tasks = []
        for id in story_ids[:30]:  # Limit to 30 stories
            tasks.append(fetch_story(session, id))

        stories = await asyncio.gather(*tasks)

        return [Story(**dict((k, story.get(k, None)) for k in ('title', 'url', 'score'))) for story in stories]

class ChidoriWorker:
    def __init__(self):
        self.c = Chidori("0", "http://localhost:9800")
        self.staged_custom_nodes = []

    async def start(self):
        await self.c.start_server(":memory:")

    async def build_graph(self):
        g = GraphBuilder()

        h = await g.custom_node(
            name="FetchTopHN",
            node_type_name="FetchTopHN",
            output="type O { output: String }"
        )

        h_interpret = await g.prompt_node(
            name="InterpretTheGroup",
            template="Based on the following list of HackerNews threads, filter this list to only launches of new AI projects: {{FetchTopHN.output}}"
        )
        await h_interpret.run_when(g, h)

        h_format_and_rank = await g.prompt_node(
            name="FormatAndRank",
            template="Format this list of new AI projects in markdown, ranking the most interesting projects from most interesting to least. {{InterpretTheGroup.promptResult}}"
        )
        await h_format_and_rank.run_when(g, h_interpret)

        generate_email = await g.prompt_node(
            name="GenerateEmailFn",
            template="Write the body of a javascript function that returns {'subject': string, 'body': string} and populate the body with {{FormatAndRank.promptResult}} put any commentary in comments."
        )
        await generate_email.run_when(g, h_format_and_rank)

        # Commit the graph
        await g.commit(self.c, 0)

    def node(self, node_type):
        def decorator(f):
            self.staged_custom_nodes.append((node_type, f))
            return f
        return decorator

    async def run(self):
        await self.build_graph()
        print(await self.c.display_graph_structure())
        for node_type, f in self.staged_custom_nodes:
            await self.c.register_custom_node_handle(node_type, f)
        c = self.c
        # Start graph execution from the root
        await c.play(0, 0)

        # Run the node execution loop
        try:
            await c.run_custom_node_loop()
        except Exception as e:
            print(f"Custom Node Loop Failed On - {e}")


async def main():
    w = ChidoriWorker()
    await w.start()

    @w.node("FetchTopHN")
    async def handle_fetch_hn(node_will_exec):
        stories = await fetch_hn()
        result = {"output": json.dumps([story.__dict__ for story in stories])}
        return result

    await w.run()

if __name__ == "__main__":
    asyncio.run(main())
