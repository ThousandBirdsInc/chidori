import aiohttp
import asyncio
from typing import List, Optional
import json
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

        stories_out = []
        for story in stories:
            story_dict = {k: story.get(k, None) for k in ('title', 'url', 'score')}
            stories_out.append(Story(**story_dict))
        return stories_out


# ^^^^^^^^^^^^^^^^^^^^^^^^^^^
# Methods for fetching hacker news posts via api

class ChidoriWorker:
    def __init__(self):
        self.c = Chidori("0", "http://localhost:9800")

    async def build_graph(self):
        g = GraphBuilder()

        # Create a custom node, we will implement our
        # own handler for this node type
        h = await g.custom_node(
            name="FetchTopHN",
            node_type_name="FetchTopHN",
            output="type O { output: String }"
        )

        # A prompt node, pulling in the value of the output from FetchTopHN
        # and templating that into the prompt for GPT3.5
        h_interpret = await g.prompt_node(
            name="InterpretTheGroup",
            template="""
                Based on the following list of HackerNews threads, 
                filter this list to only launches of new AI projects: {{FetchTopHN.output}}
            """
        )
        await h_interpret.run_when(g, h)

        h_format_and_rank = await g.prompt_node(
            name="FormatAndRank",
            template="""
                Format this list of new AI projects in markdown, ranking the most 
                interesting projects from most interesting to least. 
                
                {{InterpretTheGroup.promptResult}}
            """
        )
        await h_format_and_rank.run_when(g, h_interpret)

        # Commit the graph, this pushes the configured graph
        # to our durable execution runtime.
        await g.commit(self.c, 0)

    async def run(self):
        # Construct the agent graph
        await self.build_graph()

        # Start graph execution from the root
        await self.c.play(0, 0)

        # Run the node execution loop
        await self.c.run_custom_node_loop()


async def handle_fetch_hn(node_will_exec):
    stories = await fetch_hn()
    result = {"output": json.dumps([story.__dict__ for story in stories])}
    return result


async def main():
    w = ChidoriWorker()
    await w.c.start_server(":memory:")
    await w.c.register_custom_node_handle("FetchTopHN", handle_fetch_hn)
    await w.run()


if __name__ == "__main__":
    asyncio.run(main())
