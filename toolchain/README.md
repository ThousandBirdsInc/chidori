This folder contains the main suite of packages that comprise Chidori

# Chidori
This is the main package provided to users of Chidori. It contains bindings for multiple
languages to the runtime provided by `prompt-graph-exec`. 

# Prompt Graph Core
This package contains the core data structures and algorithms used by Chidori. It includes a few categories of functionality:

### Reactivity
Reactivity describes the event system used by Chidori. Not all applications built with Chidori need to leverage reactivity 
but we find it to be a useful abstraction for many applications.

### Time Travel
Time-travel in our case relates to the ability to revert and replay the state of an applications. We provide an extensible
framework for leveraging this by re-packaging some specific patterns around persistent data structures and event sourcing.

### Prompt Composition
Prompt composition is core to any application built to leverage LLMs. Whether using automatic composition or manual composition.
This package provides the necessary abstractions for composing prompts in a way that can be monitored and controlled by the runtime.

# Prompt Graph Exec
This package includes the implementation of the Chidori runtime and the Tonic GPRC server. 

# Prompt Graph UI
`prompt-graph-ui` is a free prototype implementation of a UI for Chidori. It is not intended to be used in production, but rather as a reference implementation.


Referenced during development:
* https://github.com/Adapton/adapton.rust/tree/master
* https://github.com/salsa-rs/salsa