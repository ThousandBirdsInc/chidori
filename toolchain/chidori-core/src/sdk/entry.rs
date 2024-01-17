use crate::execution::primitives::serialized_value::RkyvSerializedValue as RKV;
use std::collections::HashMap;

/// This is an SDK for building execution graphs. It is designed to be used iteratively.

type Func = fn(RKV) -> RKV;

struct Environment {
    default_imports: HashMap<String, Func>,
}

impl Environment {
    fn new() -> Self {
        let mut default_imports = HashMap::new();
        default_imports.insert("ai/audio", "");
        default_imports.insert("ai/vision", "");
        default_imports.insert("ai/llm", "");
        default_imports.insert("ai/memory", "");
        default_imports.insert("code/deno", "");
        default_imports.insert("code/python", "");
        default_imports.insert("code/starlark", "");
        default_imports.insert("io", "");
        default_imports.insert("schedule", "");
        default_imports.insert("templating", "");
        Environment { default_imports }
    }

    /// Execute a group of nodes, invoking them as they continue to be triggered
    fn execute_module() {
        // TODO: - load each file in the directory
        // TODO: - locate entry point
    }

    /// Execute an individual function in isolation
    fn execute_block() {}
}
