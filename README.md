
https://github.com/ThousandBirdsInc/chidori/assets/515757/6b088f7d-d8f7-4c7e-9006-4360ae40d1de

<div align="center">

# &nbsp; Chidori (v2) &nbsp;

**A reactive runtime for building durable AI agents**

<p>
<a href="https://github.com/ThousandBirdsInc/chidori/commits"><img alt="Current Build Status" src="https://img.shields.io/github/actions/workflow/status/ThousandBirdsInc/chidori/push.yml" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/commits"><img alt="GitHub Last Commit" src="https://img.shields.io/github/last-commit/ThousandBirdsInc/chidori" /></a>
<a href="https://crates.io/crates/chidori"><img alt="Cargo.io download" src="https://img.shields.io/crates/v/chidori" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/blob/main/LICENSE"><img alt="Github License" src="https://img.shields.io/badge/License-MIT-green.svg" /></a>
</p>

<br />





</div>

Star us on Github! Join us on [Discord](https://discord.gg/CJwKsPSgew).

Check out [high level docs ](https://docs.thousandbirds.ai/3fe20a82965148c7a0b480f7daf0aff6)

## Contents
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
Chidori is an open-source environment for building AI agents with simple and straight forward code.
You author code like you typically would with python or javascript, and we provide a layer for interfacing
with the complexities of AI models in long running workflows.

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
Chidori is available on [crates.io](https://crates.io/crates/chidori) and can be installed using cargo.

```bash
cargo install chidori-core
```


### Environment Variables
You will need to set the following environment variables if you depend on nodes that
require them.
```bash
OPENAI_API_KEY=...
```

### Examples

In the table below are examples for Node.js, Python and Rust. You'll need to scroll horizontally to view each.

The following examples show how to build a simple agent that fetches the top stories from Hacker News and call
the OpenAI API to filter to AI related launches and then format that data into markdown. Results from the example
are pushed into the Chidori database and can be visualized using the prompt-graph-ui project. We'll update this example
with a pattern that makes those results more accessible soon.


Beginning here is an executable Chidori agent:
------
<pre>
Chidori agents can be a single file, or a collection of files structured as a typical Typescript or Python project. 
The following example is a single file agent.

```js
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
            output: "{ output: String }"
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
</pre>

------


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
A good place to start would be to join our [discord](https://discord.gg/CJwKsPSgew)!

## FAQ

### Why Another AI Framework?
Chidori focuses on the specifics of how LLM+code execution operates rather than providing specific compositions of prompts. Other frameworks haven’t focused on this space, and it's an important one. We reduce accidental complexity in building systems for long-running agents; this helps developers build successful systems.

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
