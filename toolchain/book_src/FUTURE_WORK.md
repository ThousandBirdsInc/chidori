# Future Work

## Sandboxing
* Use libkrun in order to sandbox code execution for safe iteration (https://github.com/containers/libkrun/tree/main) (https://github.com/containers/crun)

## Evaluation
* Include an easy way to run WebArena and evaluate agents against realistic websites in a sandboxed way (https://github.com/web-arena-x/webarena/tree/main/environment_docker)

## Dependency management
* Deno makes dependencies cleaner, but in order to have a story for Python and to simplify installation we can consider depending on Rye (https://github.com/astral-sh/rye/blob/main/rye/src/pyproject.rs)