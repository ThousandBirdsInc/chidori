# Chidori Core

This implements an interface for constructing prompt graphs.
This can be used to annotate existing implementations with graph definitions as well.

## Features

- [ ] A graph definition language for reactive programs, wrapping other execution runtimes
- [ ] A pattern for annotating existing code to expose it to the graph definition language
- [ ] A scheduler for executing reactive programs
- [ ] Support for branching and merging reactive programs
- [ ] A wrapper around handlebars for rendering templates that supports tracing
- [ ] A standard library of core agent functionality
- [ ] Support for long running durable execution of agents

## Why

### Q: Why extract the execution of code or LLMs from the source itself?
In order to go beyond tracing alone, we want to have control over where and when prompts are executed.

### Q: Why choose to break apart the source code provided into a graph?
Breaking apart the source code into it's own graph allows us to take more ownership over how units of code are executed.
We want to be able to pause execution of a graph, and resume it later.

### Q: Why operate over source code rather than provide an SDK?
Constructing the execution graph is something that can be done at runtime, and we want to be able to do this without requiring a build step.
We also want to be able to annotate existing code with graph definitions, and this is easier to do if we can operate over the source code directly.


## Functionality

### Reactive graphs



