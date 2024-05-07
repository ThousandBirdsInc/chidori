
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


## 📖 Chidori V2
Chidori is an open-source environment for building AI agents.
You author code like you typically would with python or javascript, and we provide a layer for interfacing
with the complexities of AI models in long-running workflows.

It is currently in alpha, and is not yet ready for production use. We are continuing to make significant changes in response to feedback.

- Built from the ground up for constructing agents
- Runtime written in Rust supporting Python and Node.js out of the box
- Build agents that actually work
- Cache behaviors and resume from partially executed agents
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

## Examples

The following examples show how to build a simple agent that fetches the top stories from Hacker News and call the OpenAI API to filter to AI related launches and then format that data into markdown.

------

### Beginning here is an example executable Chidori agent:

Chidori agents can be a single file, or a collection of files structured as a typical Typescript or Python project. 
The following example is a single file agent.

```javascript (load_hacker_news)
const axios = require('https://deno.land/x/axiod/mod.ts');

class Story {
    constructor(title, url, score) {
        this.title = title;
        this.url = url;
        this.score = score;
    }
}

const HN_URL_TOP_STORIES = "https://hacker-news.firebaseio.com/v0/topstories.json";

function fetchStory(id) {
    return axios.get(`https://hacker-news.firebaseio.com/v0/item/${id}.json?print=pretty`)
        .then(response => response.data);
}

async function fetchHN() {
    const stories = await axios.get(HN_URL_TOP_STORIES);
    const storyIds = stories.data;
    // only the first 30 
    const tasks = storyIds.slice(0, 30).map(id => fetchStory(id));
    return Promise.all(tasks)
      .then(stories => {
        return stories.map(story => {
          const { title, url, score } = story;
          return new Story(title, url, score);
        });
      });
}
```

Prompt "interpret_the_group"
```prompt (interpret_the_group)
  Based on the following list of HackerNews threads,
  filter this list to only launches of 
  new AI projects: {{fetched_articles}}
```

Prompt "format_and_rank"
```prompt (format_and_rank)
Format this list of new AI projects in markdown, ranking the most 
interesting projects from most interesting to least. 
{{interpret_the_group}}
```

Using a python cell as our entrypoint, demonstrating inter-language execution:
```python
articles = await fetchHN()
format_and_rank(articles=articles)
```
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
* [x] Simple local vector db for development
* [ ] Adding support for containerized nodes
* [ ] Allowing filtering in node queries

### Medium term
* [x] Analysis tools for comparing executions
* [x] Adding support for more vector databases
* [x] Adding support for other LLM sources
* [x] Adding support for more code interpreter environments
* [ ] Agent re-evaluation with feedback
* [ ] Definitive patterns for human in the loop agents


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
