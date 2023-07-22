<div align="center">

# &nbsp; Chidori &nbsp;

**A Reactive Runtime for AI Systems.**

<p>
<a href="https://github.com/ThousandBirdsInc/chidori/commits"><img alt="GitHub Last Commit" src="https://img.shields.io/github/last-commit/ThousandBirdsInc/chidori" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/issues"><img alt="GitHub Issues" src="https://img.shields.io/github/issues/ThousandBirdsInc/chidori" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/pulls"><img alt="GitHub Pull Requests" src="https://img.shields.io/github/issues-pr/ThousandBirdsInc/chidori" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori/blob/main/LICENSE"><img alt="Github License" src="https://img.shields.io/badge/License-MIT-green.svg" /></a>
<a href="https://github.com/ThousandBirdsInc/chidori"><img alt="GitHub Repo stars" src="https://img.shields.io/github/stars/ThousandBirdsInc/chidori?style=social" /></a>
</p>

<br />

</div>


### Quick Links
- [Getting Started](https://github.com/ThousandBirdsInc/chidori/tree/main#-getting-started)
- [Documentation](https://www.notion.so/Documentation-3fe20a82965148c7a0b480f7daf0aff6?pvs=21)
- [About](https://github.com/ThousandBirdsInc/chidori/tree/main#-about)
- [Roadmap](https://github.com/ThousandBirdsInc/chidori/tree/main#-roadmap)

## ⚡️ Getting Started


### Installation
```bash
npm i @1kbirds/chidori
yarn add @1kbirds/chidori
```
```bash
pip install chidori
```
```bash
cargo install chidori
```

### Environment Variables
```bash
OPENAI_API_KEY=...
```

### Example
```python
chidori = Chidori("100", "http://localhost:9800")

# Generate an inspirational quote
iq = await self.client.prompt_node(
    name="InspirationalQuote",
    template="""Come up with a novel and interesting quote. Something that will make them
    want to seize the day. Do not wrap the quote in quotes.
    """
)

# Get the current date
await self.client.deno_code_node(
    name="CurrentDate",
    code=""" return {"output": "" + new Date() } """,
)


# Format the date in a fun way
await self.client.prompt_node(
    name="FunFormat",
    queries=[""" query Q { CurrentDate { output } } """],
    template="""Format the following in a fun and more informal way: {{CodeNode.output}} """
)

# Return the quote with the date
await self.client.deno_code_node(
    name="ResultingQuote",
    queries=[""" query Q { FunFormat { promptResult } InspirationalQuote { promptResult } } """],
    code=""" return {"output": `{{FunFormat.promptResult}}: \n {{InspirationalQuote.promptResult}}` } """
)
```

## 🤔 About

### Reactive Runtime
At its core, Thousand Birds brings a reactive runtime that orchestrates interactions between different agents and their components. The runtime is comprised of "nodes", which react to system changes they subscribe to, providing dynamic and responsive behavior in your AI systems.
Nodes can encompass code, prompts, vector databases, custom code, services, or even complete systems. 

### Monitoring and Observability
Thousand Birds ensures comprehensive monitoring and observability of your agents. We record all the inputs and outputs emitted by nodes, enabling us to explain precisely what led to what, enhancing your debugging experience and understanding of the system’s production behavior.

### Branching and Time-Travel
With Thousand Birds, you can take snapshots of your system and explore different possible outcomes from that point (branching), or rewind the system to a previous state (time-travel). This functionality improves error handling, debugging, and system robustness by offering alternative pathways and do-overs.

### Memory via Vector Databases
Vector databases, akin to an AI’s brain, help your AI remember and understand information. Thousand Birds comes with a built-in minimal vector database. If you prefer, you can integrate your own or choose from a selection of other options we support.

### Code Interpreter Environments
Thousand Birds comes with first-class support for code interpreter environments like [Deno](https://deno.land/) or [Starlark](https://github.com/bazelbuild/starlark/blob/master/spec.md). You can execute code directly within your system, providing quick startup, ease of use, and secure execution. We're continually working on additional safeguards against running untrusted code, with containerized nodes support coming soon.

## 🛣️ Roadmap

### Short term
* [ ] Improving the ergonomics of time travel and branching
* [ ] Improving the ergonomics of subscribing nodes and constructing graphs
* [ ] Adding support for containerized nodes
* [ ] Allowing filtering in node queries

### Med term
* [ ] Analysis tools for comparing executions
* [ ] Agent re-evaluation with feedback
* [ ] Definitive patterns for human in the loop agents
* [ ] Adding support for more vector databases 
* [ ] Adding support for other LLM sources
* [ ] Adding support for more code interpreter environments


## Contributing
We look forward to future contributions from the community. For now it will be difficult to contribute, as we are still in the process of setting up our development environment. We will update this section as soon as we have a more stable development environment.
If you have feedback or would like to chat with us, please add to the discussion on our Github issues!


### Why Another AI Framework?
Thousand Birds pushes to be more than a simple wrapper around LLMs. Our effort is to resolve as much of the accidental complexity of building systems in the category of long running agents as possible, helping the broader developer community build successful systems.

## Inspiration
Our framework is inspired by the work of many others, including:
* https://temporal.io/ - providing reliability and durability to workflows
* http://witheve.com/ - developing patterns for building reactive systems and reducing accidental complexity
* https://timelydataflow.github.io/timely-dataflow/ - efficiently streaming changes
* https://www.langchain.com/ - developing tools and patterns for building with LLMs
* (many more we'll follow up on listing later)

## License
Thousand Birds is under the MIT license. See the [LICENSE](LICENSE.md) for more information.
