
https://github.com/ThousandBirdsInc/chidori/assets/515757/6b088f7d-d8f7-4c7e-9006-4360ae40d1de

<div align="center">

# &nbsp; Chidori &nbsp;

**A reactive runtime for building durable AI agents**

<p>
<a href="https://github.com/ThousandBirdsInc/chidori/commits"><img alt="Current Build Status" src="https://img.shields.io/github/actions/workflow/status/ThousandBirdsInc/chidori/push.yml" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/commits"><img alt="GitHub Last Commit" src="https://img.shields.io/github/last-commit/ThousandBirdsInc/chidori" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/commits"><img alt="Cargo.io download" src="https://img.shields.io/crates/dv/chidori/0.1.1" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/commits"><img alt="Cargo.io download" src="https://img.shields.io/pypi/v/chidori" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/commits"><img alt="Cargo.io download" src="https://img.shields.io/npm/v/@1kbirds/chidori" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/blob/main/LICENSE"><img alt="Github License" src="https://img.shields.io/badge/License-MIT-green.svg" /></a>
</p>

<br />





</div>

Star us on Github! Join us on [Discord](https://discord.gg/CJwKsPSgew).

Check out [high level docs ](https://docs.thousandbirds.ai/3fe20a82965148c7a0b480f7daf0aff6)

## Contents
- [  Chidori  ](#-chidori-)
  - [Contents](#contents)
  - [📖 Chidori](#-chidori)
  - [⚡️ Getting Started](#️-getting-started)
    - [Installation](#installation)
    - [Environment Variables](#environment-variables)
    - [Example](#example)
  - [🤔 About](#-about)
    - [Reactive Runtime](#reactive-runtime)
    - [Monitoring and Observability](#monitoring-and-observability)
    - [Branching and Time-Travel](#branching-and-time-travel)
    - [Code Interpreter Environments](#code-interpreter-environments)
  - [🛣️ Roadmap](#️-roadmap)
    - [Short term](#short-term)
    - [Med term](#med-term)
  - [Contributing](#contributing)
  - [FAQ](#faq)
    - [Why Another AI Framework?](#why-another-ai-framework)
    - [Why Chidori?](#why-chidori)
    - [Well then why Thousand Birds?](#well-then-why-thousand-birds)
    - [Why Rust?](#why-rust)
  - [Inspiration](#inspiration)
  - [License](#license)
  - [Help us out!](#help-us-out)


## 📖 Chidori
Chidori is a reactive runtime for building AI agents. It provides a framework for building AI agents that are reactive, observable, and robust. It supports building agents with Node.js, Python, and Rust. 

It is currently in alpha, and is not yet ready for production use. We are continuing to make significant changes in response to feedback.

- Built from the ground up for constructing agents
- Runtime written in Rust supporting Python and Node.js out of the box
- Build agents that actually work :emoji:
- LLM caching to minimize cost during development
- Optimized for long-running AI workflows
- Embedded code interpreter
- Time travel debugging

## ⚡️ Getting Started

### Installation
You can use Chidori from Node.js, Python or Rust.

<table>
<tr>
<th width="450px"><b>Node.js</b></th>
<th width="450px"><b>Python</b></th>
<th width="450px"><b>Rust</b></th>
</tr>
<tr>
<td>

```bash
npm i @1kbirds/chidori
```

</td>
<td>

```bash
pip install chidori
```

</td>
<td>

```bash
cargo install chidori
```

</td>
</tr>
</table>



### Environment Variables
You will need to set the following environment variables if you depend on nodes that
require them.
```bash
OPENAI_API_KEY=...
```

### Example

The following example shows how to build a simple agent that fetches the top stories from Hacker News and calls
the OpenAI API to filter to AI related launches and then formats that data into markdown. Results from the example
are pushed into the Chidori database and can be visualized using the prompt-graph-ui project. We'll update this example
with a pattern that makes those results more accessible soon.

<table>
<tr>
<th width="450px"><b>Python</b></th>
<th width="450px"><b>Node.js</b></th>
<th width="450px"><b>Rust</b></th>
</tr>
<tr>
<td>

```python
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
            for k in ('title', 'url', 'score'):
                stories_out.append(Story(**dict((k, story.get(k, None)))))

        return stories_out


# ^^^^^^^^^^^^^^^^^^^^^^^^^^^
# Methods for fetching hacker news posts via api

class ChidoriWorker:
    def __init__(self):
        self.c = Chidori("0", "http://localhost:9800")
        self.staged_custom_nodes = []

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
```

</td>
<td>

```javascript
const axios = require('axios');
const {Chidori, GraphBuilder} = require("@1kbirds/chidori");

class Story {
    constructor(title, url, score) {
        this.title = title;
        this.url = url;
        this.score = score;
    }
}

const HN_URL_TOP_STORIES = "https://hacker-news.firebaseio.com/v0/topstories.json?print=pretty";

function fetchStory(id) {
    return axios.get(`https://hacker-news.firebaseio.com/v0/item/${id}.json?print=pretty`)
        .then(response => response.data);
}

function fetchHN() {
    return axios.get(HN_URL_TOP_STORIES)
        .then(response => {
            const storyIds = response.data;
            const tasks = storyIds.slice(0, 30).map(id => fetchStory(id));  // Limit to 30 stories
            return Promise.all(tasks)
                .then(stories => {
                    return stories.map(story => {
                        const { title, url, score } = story;
                        return new Story(title, url, score);
                    });
                });
        });
}

class ChidoriWorker {
    constructor() {
        this.c = new Chidori("0", "http://localhost:9800");  // Assuming this is a connection object, replaced with an empty object for now
    }

    async buildGraph() {
        const g = new GraphBuilder();

        const h = g.customNode({
            name: "FetchTopHN",
            nodeTypeName: "FetchTopHN",
            output: "type FetchTopHN { output: String }"
        });

        const hInterpret = g.promptNode({
            name: "InterpretTheGroup",
            template: `
                Based on the following list of HackerNews threads,
                filter this list to only launches of new AI projects: {{FetchTopHN.output}}
            `
        });
        hInterpret.runWhen(g, h);

        const hFormatAndRank = g.promptNode({
            name: "FormatAndRank",
            template: `
                Format this list of new AI projects in markdown, ranking the most 
                interesting projects from most interesting to least. 
                
                {{InterpretTheGroup.promptResult}}
            `
        });
        hFormatAndRank.runWhen(g, hInterpret);

        await g.commit(this.c, 0)
    }

    async run() {
        // Construct the agent graph
        await this.buildGraph();

        // Start graph execution from the root
        // Implement the functionality of the play function
        await this.c.play(0, 0);

        // Run the node execution loop
        // Implement the functionality of the run_custom_node_loop function
        await this.c.runCustomNodeLoop()
    }
}


async function handleFetchHN(nodeWillExec, cb) {
    const stories = await fetchHN();
    // return JSON.stringify(stories);
    return cb({ "output": JSON.stringify(stories) });
    // return ;
}

async function main() {
    let w = new ChidoriWorker();
    await w.c.startServer(":memory:")
    await w.c.registerCustomNodeHandle("FetchTopHN", handleFetchHN);
    await w.run()
}


main();

```

</td>
<td>

```rust
extern crate chidori;
use std::collections::HashMap;
use std::env;
use std::net::ToSocketAddrs;
use anyhow;
use futures::stream::{self, StreamExt, TryStreamExt};
use reqwest;
use serde::{Deserialize, Serialize};
use serde_json::json;
use chidori::{create_change_value, NodeWillExecuteOnBranch};
use chidori::register_node_handle;
use chidori::translations::rust::{Chidori, CustomNodeCreateOpts, DenoCodeNodeCreateOpts, GraphBuilder, Handler, PromptNodeCreateOpts, serialized_value_to_string};

#[derive(Debug, Deserialize, Serialize)]
struct Story {
    title: String,
    url: Option<String>,
    score: Option<f32>,
}

const HN_URL_TOP_STORIES: &'static str = "https://hacker-news.firebaseio.com/v0/topstories.json?print=pretty";

async fn fetch_hn() -> anyhow::Result<Vec<Story>> {
    let client = reqwest::Client::new();
    // Fetch the top 60 story ids
    let story_ids: Vec<u32> = client.get(HN_URL_TOP_STORIES).send().await?.json().await?;

    // Fetch details for each story
    let stories: anyhow::Result<Vec<Story>> = stream::iter(story_ids.into_iter().take(30))
        .map(|id| {
            let client = &client;
            async move {
                let resource = format!("https://hacker-news.firebaseio.com/v0/item/{}.json?print=pretty", id);
                let mut story: Story = client.get(&resource).send().await?.json().await?;
                Ok(story)
            }
        })
        .buffer_unordered(10)  // Fetch up to 10 stories concurrently
        .try_collect()
        .await;
    stories
}

async fn handle_fetch_hn(_node_will_exec: NodeWillExecuteOnBranch) -> anyhow::Result<serde_json::Value> {
    let stories = fetch_hn().await.unwrap();
    let mut result = HashMap::new();
    result.insert("output", format!("{:?}", stories));
    Ok(serde_json::to_value(result).unwrap())
}

/// Maintain a list summarizing recent AI launches across the week
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut c = Chidori::new(String::from("0"), String::from("http://localhost:9800"));
    c.start_server(Some(":memory:".to_string())).await?;

    let mut g = GraphBuilder::new();

    let h = g.custom_node(CustomNodeCreateOpts {
        name: "FetchTopHN".to_string(),
        node_type_name: "FetchTopHN".to_string(),
        output: Some("type O { output: String }".to_string()),
        ..CustomNodeCreateOpts::default()
    })?;

    let mut h_interpret = g.prompt_node(PromptNodeCreateOpts {
        name: "InterpretTheGroup".to_string(),
        template: "Based on the following list of HackerNews threads, filter this list to only launches of new AI projects: {{FetchTopHN.output}}".to_string(),
        ..PromptNodeCreateOpts::default()
    })?;
    h_interpret.run_when(&mut g, &h)?;

    let mut h_format_and_rank = g.prompt_node(PromptNodeCreateOpts {
        name: "FormatAndRank".to_string(),
        template: "Format this list of new AI projects in markdown, ranking the most interesting projects from most interesting to least. {{InterpretTheGroup.promptResult}}".to_string(),
        ..PromptNodeCreateOpts::default()
    })?;
    h_format_and_rank.run_when(&mut g, &h_interpret)?;

    // Commit the graph
    g.commit(&c, 0).await?;

    // Start graph execution from the root
    c.play(0, 0).await?;

    // Register the handler for our custom node
    register_node_handle!(c, "FetchTopHN", handle_fetch_hn);

    // Run the node execution loop
    if let Err(x) = c.run_custom_node_loop().await {
        eprintln!("Custom Node Loop Failed On - {:?}", x);
    };
    Ok(())
}
```

</td>
</tr>
</table>

## 🤔 About

### Reactive Runtime
At its core, Chidori brings a reactive runtime that orchestrates interactions between different agents and their components. The runtime is comprised of "nodes", which react to system changes they subscribe to, providing dynamic and responsive behavior in your AI systems.
Nodes can encompass code, prompts, vector databases, custom code, services, or even complete systems. 

### Monitoring and Observability
Chidori ensures comprehensive monitoring and observability of your agents. We record all the inputs and outputs emitted by nodes, enabling us to explain precisely what led to what, enhancing your debugging experience and understanding of the system’s production behavior.

### Branching and Time-Travel
With Chidori, you can take snapshots of your system and explore different possible outcomes from that point (branching), or rewind the system to a previous state (time-travel). This functionality improves error handling, debugging, and system robustness by offering alternative pathways and do-overs.

### Code Interpreter Environments
Chidori comes with first-class support for code interpreter environments like [Deno](https://deno.land/) or [Starlark](https://github.com/bazelbuild/starlark/blob/master/spec.md). You can execute code directly within your system, providing quick startup, ease of use, and secure execution. We're continually working on additional safeguards against running untrusted code, with containerized nodes support coming soon.

## 🛣️ Roadmap

### Short term
* [x] Reactive subscriptions between nodes
* [x] Branching and time travel debugging, reverting execution of a graph
* [x] Node.js, Python, and Rust support for building and executing graphs
* [ ] Simple local vector db for development
* [ ] Adding support for containerized nodes
* [ ] Allowing filtering in node queries

### Medium term
* [ ] Analysis tools for comparing executions
* [ ] Agent re-evaluation with feedback
* [ ] Definitive patterns for human in the loop agents
* [ ] Adding support for more vector databases 
* [ ] Adding support for other LLM sources
* [ ] Adding support for more code interpreter environments


## Contributing
This is an early open source release and we're looking for collaborators from the community. 
A good place to start would be to join our [discord](https://discord.gg/CJwKsPSgew)!.

## FAQ

### Why Another AI Framework?
Chidori focuses more on the specifics of how LLM+code execution operates, rather than providing specific compositions of prompts.
We haven't really seen any other frameworks that focus on this space, and we think it's a really important one.
Our effort is to resolve as much of the accidental complexity of building systems in the category of long running agents as possible, helping the broader developer community build successful systems.

### Why Chidori?
Chidori is the name of the lightning blade technique used by Kakashi in the Naruto anime series.
It also happens to [mean Thousand Birds in Japanese](https://en.wikipedia.org/wiki/Chidori), which is a nice coincidence.

### Well then why Thousand Birds?
Thousand Birds is a reference to flocks of birds (or a murmuration) and the emergent behavior that arises from their interactions.
We think this is a good metaphor for the behavior of long running agents, the internal units of LLM execution within them, and the emergent behavior that arises from their interactions.

### Why Rust?
Rust is a great language for building systems, we like the type system and the guarantees provided by it.
We also like the performance characteristics of Rust, and the ability to build a single binary that can be deployed anywhere.
The Rust ecosystem makes it fairly easy to provide bindings to other languages, which is important for us to provide a good developer experience.


## Inspiration
Our framework is inspired by the work of many others, including:
* [Temporal.io](https://temporal.io) - providing reliability and durability to workflows
* [Eve](http://witheve.com) - developing patterns for building reactive systems and reducing accidental complexity
* [Timely Dataflow](https://timelydataflow.github.io/timely-dataflow) - efficiently streaming changes
* [Langchain](https://www.langchain.com) - developing tools and patterns for building with LLMs

## License
Thousand Birds is under the MIT license. See the [LICENSE](LICENSE) for more information.

## Help us out!
Please star the github repo and give us feedback in [discord](https://discord.gg/CJwKsPSgew)!
