
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

## Contents
- [📖 Chidori V2](#-chidori-v2)
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
  - [Medium term](#med-term)
- [Contributing](#contributing)
- [Inspiration](#inspiration)
- [License](#license)
- [Help us out!](#help-us-out)


## 📖 Chidori V2
Chidori is an open-source orchestrator, runtime, and IDE for building software in symbiosis with modern AI tools.
It is especially catered towards building AI agents by providing solutions to the following problems:

- How do we understand what an agent is doing and how it got into a given state?
- How can we pause execution and then resume after interaction with a human?
- How do we handle the accidental complexity of state-space exploration, evaluating and reverting execution throughout our software?

When using Chidori, you author code with python or javascript, we provide a layer for interfacing
with the complexities of AI models in long-running workflows. We have avoided the need for declaring a new language 
or SDK in order to provide these capabilities so that you can leverage software patterns that you are already familiar with.

Features:

- Runtime written in Rust, supporting Python and JavaScript code execution
- The ability to cache behaviors and resume from partially executed agents
- Time travel debugging, execution of the program can be reverted to prior states
- Visual debugging environment, visualize and manipulate the graph of states your code has executed through.
- Create and navigate tree-searching code execution workflows

We are continuing to make significant changes in response to feedback and iterating on different features.
Feedback is greatly appreciated! Please add to our issue tracker.


## ⚡️ Getting Started

### Installation
Chidori is available on [crates.io](https://crates.io/crates/chidori) and can be installed using cargo. Our expected entrypoint for
prototype development is `chidori-debugger` which wraps our runtime in a useful visual interface.

```bash
xcode-select --install

# These dependencies are necessary for a successful build
brew install \ 
  cmake \
# Protobuf is depended upon by denokv, which we in turn currently depend on
  protobuf \
# We are investigating if this is necessary or can be removed
  libiconv \
  python@3.12 \
# Chidori uses uv for handling python dependencies 
  uv

cargo install chidori-debugger
```

If you prefer to use a different python interpreter you can set PYO3_PYTHON=python3.12 (or whichever version > 3.7) during
your installation to change which is linked against.


### Setting Up The Runtime Environment
Chidori's interactions with LLMs default to http://localhost:4000 to hook into LiteLLM's proxy.
If you'd like to leverage gpt-3.5-turbo the included config file will support that.
You will need to install `pip install litellm[proxy]` in order to run the below:
```bash
uv sync
export OPENAI_API_KEY=...
uv run litellm --config ./litellm_config.yaml
```

## Examples

The following example shows how to build a simple agent that fetches the top stories from Hacker News and call the OpenAI API to filter to AI related launches and then format that data into markdown.

------

### Beginning here is an example executable Chidori agent:

Chidori agents can be a single file, or a collection of files structured as a typical Typescript or Python project. 
The following example is a single file agent. Consider this similar to something like a jupyter/iPython notebook 
represented as a markdown file.

<pre>

```javascript (load_hacker_news)
const axios = require('https://deno.land/x/axiod/mod.ts');

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
          return {title, url, score};
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
</pre>
------


## About

### Reactive Runtime
At its core, Chidori brings a reactive runtime that orchestrates interactions between different agents and their components. 
Chidori accepts arbitrary Python or JavaScript code, taking over brokering and execution of it to allow for interruptions and reactivity.
This allows you to get the benefits of these runtime behaviors while leveraging the patterns you're already familiar with.

### Monitoring and Observability
Chidori ensures comprehensive monitoring and observability of your agents. We record all the inputs and outputs emitted by functions throughout the execution of your agent, enabling us to explain precisely what led to what, enhancing your debugging experience and understanding of the system’s production behavior.

### Branching and Time-Travel
With Chidori, you can take snapshots of your system and explore different possible outcomes from that point (branching), or rewind the system to a previous state (time-travel). This functionality improves error handling, debugging, and system robustness by offering alternative pathways and do-overs.

### Code Interpreter Environments
Chidori comes with first-class support for code interpretation for both Python and JavaScript. You can execute code directly within your system, providing quick startup, ease of use, and secure execution. We're continually working on additional safeguards against running untrusted code, with containerized environment support coming soon.

### Code Generation During Evaluation
With our execution graph, preservation of state, and tools for debugging - Chidori is an exceptional environment for generating code during the evaluation of your agent.
You can use this to leverage LLMs to achieve more generalized behavior and to evolve your agents over time.




## 🛣️ Roadmap

### Short term
* [x] Reactive subscriptions between nodes
* [x] Branching and time travel debugging, reverting execution of a graph
* [x] Node.js, Python, and Rust support for building and executing graphs
* [x] Simple local vector db for development
* [ ] Adding support for containerized nodes

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

## Inspiration
Our framework is inspired by the work of many others, including:
* [Temporal.io](https://temporal.io) - providing reliability and durability to workflows
* [Eve](http://witheve.com) - developing patterns for building reactive systems and reducing accidental complexity
* [Timely Dataflow](https://timelydataflow.github.io/timely-dataflow) - efficiently streaming changes
* [Langchain](https://www.langchain.com) - developing tools and patterns for building with LLMs

## License
Chidori is under the MIT license. See the [LICENSE](LICENSE) for more information.

## Help us out!
Please star the GitHub repo and give us feedback in [discord](https://discord.gg/CJwKsPSgew)!