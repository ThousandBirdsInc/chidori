# Architecture

## Overview

Chidori consists of the following crates:

- `chidori-core` contains our orchestrator and runtime.
- `chidori-debugger` contains a UI for visualizing and debugging Chidori executed programs.
- `chidori-im-rs` contains a fork of im-rs to add support for persisting these data structures to disk.
- `chidori-prompt-format` implements handlebars-like templating with support for tracing composition
- `chidori-static-analysis` implements our parsing and extraction of control-flow from Python and TypeScript source code
- `chidori-optimizer-dsp` (IGNORE) - not yet implemented, in the future we'd like to support DSPy-like optimization
- `chidori-tsne` (IGNORE) - not yet implemented, in the future we'd like to add support for visualizing embeddings within our debugger


### Chidori Core

Chidori Core is constructed mainly around our *execution graph*. The execution graph is a graph of all
state transitions that take place between function invocations in the code we run. Each state is represented by
an *execution state*.

When code is loaded by Chidori it can go through two modes of operation: notebooks or complete files.
Notebooks are broken down into their component cells. These cells are treated as operations in a dependency graph,
there is no explicit entrypoint to the notebook. Cells execute in their topologically sorted and declared order.
Complete files are handled equivalently to single-cell notebooks.






