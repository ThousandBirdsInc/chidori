# Language

## Goals
The intention here is to be able to construct reactive
graphs of computations to be executed by our runtime.
Specifically we want to provide reactive semantics to build a graph of computations.
This graph of computations is something that we can more cleanly visualize and reason about.
The reactivity of these computations is the boundary that we want to create between the
definition of a given program and the execution of that program.

## Taking control of execution

We want to be able to take control of the execution of a program in order to give
improved durability guarantees. This means that we want to be able to control the
execution of a program in order to be able to checkpoint the state of the program.

In instances where there are calls to our std api functions, we want to be able to delegate back our our
execution runtime. In order to do this and to instrument internal functions of the user's definitions - we mutate their
AST with a special "checkpoint" function call. This function call is then intercepted by our runtime and we can pause execution,
run our code, and then resume.


## Intermediate experiments

* Implementing our own language to define these graphs
* Providing an API for users to explicitly define these relationships
* Adopting another existing scripting language and manipulating its stack and heap
* Adopting WASM as our execution runtime and implementing our time travel features on top of it

From these experiments it feels clear that the best way to provide this functionality
is to make discovering these relationships implicit. 

These relationships also need to be identified at "compile" time, prior to the program running.
Otherwise the author needs to first think about how they'll structure their computation in two passes with
different entrypoints. This is a bad developer experience.

## The solution

To provide the best user experience and to provide the most flexibility in the future: we're simply implementing our own
initial pass over javascript and python ASTs. We manipulate the AST representation of the code base in order to identify relationships
across function blocks. We construct the graph of computations and then we execute the program ourselves.

This is signficiantly more simple than the other explored alternatives. We implement our own patterns for capturing values and constructing
snapshots of execution in the given languages.



## Unique features

This intersection allows us to also bridge multiple runtimes. We can declare a python function that depends on the execution
of a javascript function. We can declare an LLM prompt that depends on the execution of a python function.