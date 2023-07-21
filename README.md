# Thousand Birds - Chidori: A Reactive Runtime for AI Systems

Thousand Birds offers a reactive environment to build AI systems seamlessly. Inspired by the notion of ["agents"](https://en.wikipedia.org/wiki/Intelligent_agent), Thousand Birds combines the strength of AI with the regularity of software development. Our goal is to create an environment where AI can operate smoothly and humans can interact efficiently with AI during its creation and maintenance.

### Important

This is alpha software under active development. Breaking changes can happen anytime, without prior notice. You are welcome to experiment with the software, but please understand that we can't provide support at this time.

For questions, issues and bug reports, please open an issue on Github.

### Why Another AI Framework?

Thousand Birds pushes to be more than a simple wrapper around LLMs. Our effort is to resolve as much of the accidental complexity of building systems in the category of long running agents as possible, helping the broader developer community build successful systems.

### Quick Links
- [Example Agents](https://www.notion.so/Example-Agents-d2c4164cb0f64f7ab6716a6f6e55577d?pvs=21)
- [Documentation](https://www.notion.so/Documentation-3fe20a82965148c7a0b480f7daf0aff6?pvs=21)
- [Roadmap](https://www.notion.so/Roadmap-98b73a3aab9e48dcb2e51f87d9752c1c?pvs=21)
- [About](https://www.notion.so/About-da11db5a115444f68c5d912dc077daee?pvs=21)
- [Blog](https://www.notion.so/Blog-4a284fcb736d4e5e8ce2309303c272a2?pvs=21)

### Reactive Runtime
At its core, Thousand Birds brings a reactive runtime that orchestrates interactions between different agents and their components. The runtime comprises of "nodes", which react to system changes they subscribe to, providing dynamic and responsive behavior to your AI systems.

Nodes can encompass code, prompts, vector databases, custom code, services, or even complete systems. The runtime was designed around the principles discussed in the paper ["Out of the Tar Pit"](https://github.com/papers-we-love/papers-we-love/blob/master/design/out-of-the-tar-pit.pdf), leveraging its concepts to help you create more dynamic, robust, and controlled agents.

### Monitoring and Observability
Thousand Birds ensures comprehensive monitoring and observability of your agents. We record all the changes emitted by nodes, enabling us to explain precisely what led to what, enhancing your debugging experience and understanding of the system’s production behavior.

### Branching and Time-Travel
With Thousand Birds, you can take snapshots of your system and explore different possible outcomes from that point (branching), or rewind the system to a previous state (time-travel). This functionality improves error handling, debugging, and system robustness by offering alternative pathways and do-overs.

### Memory via Vector Databases
Vector databases, akin to an AI’s brain, help your AI remember and understand information. Thousand Birds comes with a built-in minimal vector database. If you prefer, you can integrate your own or choose from a selection of other options we support.

### Code Interpreter Environments
Thousand Birds comes with first-class support for code interpreter environments like [Deno](https://deno.land/) or [Starlark](https://github.com/bazelbuild/starlark/blob/master/spec.md). You can execute code directly within your system, providing quick startup, ease of use, and secure execution. We're continually working on additional safeguards against running untrusted code, with containerized nodes support coming soon.

## Contributing
We look forward to future contributions from the community. For now it will be difficult to contribute, as we are still in the process of setting up our development environment. We will update this section as soon as we have a more stable development environment.
If you have feedback or would like to chat with us, please add to the discussion on our Github issues!

## Inspiration
Our framework is inspired by the work of many others, including:
* https://temporal.io/ - providing reliability and durability to workflows
* http://witheve.com/ - developing patterns for building reactive systems and reducing accidental complexity
* https://timelydataflow.github.io/timely-dataflow/ - efficiently streaming changes
* https://www.langchain.com/ - developing tools and patterns for building with LLMs
* (many more we'll follow up on listing later)

## License
Thousand Birds is under the MIT license. See the [LICENSE](LICENSE.md) for more information.
