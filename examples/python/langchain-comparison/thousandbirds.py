from typing import List, Optional
import aiohttp
import json
import os
import streamlit as st
import asyncio
from chidori import Chidori, GraphBuilder


class ChidoriWorker:
    def __init__(self):
        self.c = Chidori("0", "http://localhost:9800")

    async def build_graph(self):
        g = GraphBuilder()

        initial_context = await g.custom_node(
            name="Initialize",
            node_type_name="Initialize",
            output="""{ 
               objective: String,
 irst_task: String,
               max_iterations: String,
               incomplete_tasks: String
            }"""
        )


        # Whenever we receive a new task in plaintext form, break it down into a list of tasks
        # extracting the top task and producing a new task id
        task_queue = await g.custom_node(
            name="TaskQueue",
            queries=["SELECT promptResult FROM TaskCreation"],
            node_type_name="TaskQueue",
            output="""{ 
                top_task: String,
                next_task_id: Integer,
            }"""
        )

        task_creation = await g.prompt_node(
            name="TaskCreation",
            queries=["SELECT objective, first_task, incomplete_tasks FROM Initialize"],
            template="""
                You are an task creation AI that uses the result of an execution agent
                to create new tasks with the following objective: {{objective}},
                The last completed task has the result: {{result}}.
                This result was based on this task description: {{task_description}}.
                These are incomplete tasks: {{incomplete_tasks}}.
                Based on the result, create new tasks to be completed
                by the AI system that do not overlap with incomplete tasks.
                Return the tasks as an array.
            """
        )

        task_prioritization = await g.prompt_node(
            name="TaskPrioritization",
            queries=["""
               SELECT 
                 TaskQueue.task_names,
                 TaskQueue.next_task_id,
                 Initialize.objective
               FROM 
                 TaskQueue, 
                 Initialize
            """],
            template="""
                You are an task prioritization AI tasked with cleaning the formatting of and re-prioritization of
                the following tasks: {{task_names}}.
                Consider the ultimate objective of your team: {{objective}}.
                Do not remove any tasks. Return the result as a numbered list, like:
                #. First task
                #. Second task
                Start the task list with number {{next_task_id}}.
            """
        )


        fetch_context = await g.vector_memory_node(
            name="MemoryFetchResultOfTasksAsContext",
            queries=[
                """ SELECT TaskPrioritization.promptResult FROM TaskPrioritization """,
            ],
            action="READ",
            collection_name="context",
        )

        task_execution = await g.prompt_node(
            name="TaskExecution",
            queries=[
                """
                SELECT 
                    TaskPrioritization.promptResult, 
                    MemoryFetchResultOfTasksAsContext.result as context 
                FROM 
                    TaskPrioritization, 
                    MemoryFetchResultOfTasksAsContext
                """,
            ],
            template="""
                You are an AI who performs one task based on the following objective: {{objective}}.
                Take into account these previously completed tasks: {{context}}.
                Your task: {{task}}.
                Response:
            """
        )

        store_task_exec = await g.vector_memory_node(
            name="StoreResultOfTasksAsContext",
            queries=["SELECT TaskExecution.result FROM TaskExecution"],
            action="WRITE",
            collection_name="context",
        )

        # Commit the graph, this pushes the configured graph
        # to our execution runtime.
        await g.commit(self.c, 0)

    async def run(self):

        # Start graph execution from the root
        await self.c.play(0, 0)

        # Run the node execution loop
        await self.c.run_custom_node_loop()


async def task_queue(node_will_exec):
    # Handle receiving tasks, breaking them down, and returning the most recent task
    # Receives a plain text task list and returns the top task and the remaining list
    prompt_result = node_will_exec["event"]["TaskCreation"]["promptResult"]
    new_tasks = prompt_result.split("\n")
    prioritized_task_list = []
    for task_string in new_tasks:
        task_parts = task_string.strip().split(".", 1)
        if len(task_parts) == 2:
            task_id = task_parts[0].strip()
            task_name = task_parts[1].strip()
            prioritized_task_list.append(
                {"task_id": task_id, "task_name": task_name}
            )
    try:
        int(prioritized_task_list[0]["task_id"])
    except:
        prioritized_task_list[0]["task_id"] = "1"
    return {
        "top_task": prioritized_task_list[0]["task_name"],
        "next_task_id": int(prioritized_task_list[0]["task_id"]) + 1,
        "remaining": prioritized_task_list
    }


async def initialize(node_will_exec):
    objective = "Learn Python in 3 days",

    first_task = "Make a todo list"
    max_iterations = 3

    result = {
        "objective": objective,
        "first_task": first_task,
        "max_iterations": max_iterations,
        "incomplete_tasks": "",
    }
    return result


async def main():
    st.title("👶🏼 Baby-AGI 🤖 ")
    st.markdown(
        """
            > Powered by : 🦜 [Chidori]() 💜 
        """
    )
    w = ChidoriWorker()
    await w.c.start_server(":memory:")
    await w.c.register_custom_node_handle("Initialize", initialize)
    await w.c.register_custom_node_handle("TaskQueue", task_queue)
    # Construct the agent graph
    await w.build_graph()
    print(await w.c.display_graph_structure())
    await w.run()


if __name__ == "__main__":
    asyncio.run(main())
