// This language exists to be able to author lazily evaluated functions.
// It's possible to do this in Rust, but it's not ergonomic.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use petgraph::graph::DiGraph;
use petgraph::graphmap::DiGraphMap;
use petgraph::visit::EdgeRef;
use thiserror::Error;

pub mod typechecker;
pub mod javascript;
pub mod python;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq, Hash)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

// TODO: implement a function that infers the language from the source code successfully parsing

// TODO: it would be helpful if reports noted if a value is a global, an arg, or a kwarg
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportItem {
    // pub context_path: Vec<ContextPath>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportTriggerableFunctions {
    pub arguments: Vec<String>,
    // pub context_path: Vec<ContextPath>,
    // TODO: these need their own set of depended values
    // TODO: we need to extract signatures for triggerable functions
    pub emit_event: Vec<String>,
    pub trigger_on: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct InternalCallGraph {
    graph: DiGraph<String, ()>
}

impl Serialize for InternalCallGraph {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
    {
        let mut adjacency_list = HashMap::new();
        for node in self.graph.node_indices() {
            let edges = self.graph.edges(node)
                .map(|edge| edge.target().index())
                .collect::<Vec<_>>();
            adjacency_list.insert(self.graph[node].clone(), edges);
        }
        adjacency_list.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InternalCallGraph {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
    {
        let adjacency_list: HashMap<String, Vec<usize>> = HashMap::deserialize(deserializer)?;
        let mut graph = DiGraph::new();
        let mut node_indices = HashMap::new();

        // Add nodes
        for node in adjacency_list.keys() {
            let index = graph.add_node(node.clone());
            node_indices.insert(node.clone(), index);
        }

        // Add edges
        for (node, edges) in adjacency_list {
            let source_index = node_indices[&node];
            for target_index in edges {
                if let Some(&target_node_index) = node_indices.values().find(|&&idx| idx.index() == target_index) {
                    graph.add_edge(source_index, target_node_index, ());
                }
            }
        }

        Ok(InternalCallGraph { graph })
    }
}

impl PartialEq for InternalCallGraph {
    fn eq(&self, other: &Self) -> bool {
        // Compare the graphs for equality
        self.graph.node_indices().all(|n| {
            self.graph[n] == other.graph[n]
        }) && self.graph.edge_indices().all(|e| {
            self.graph.edge_endpoints(e) == other.graph.edge_endpoints(e)
        })
    }
}

impl Eq for InternalCallGraph {}

impl Hash for InternalCallGraph {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hash the nodes and edges of the graph
        for node in self.graph.node_indices() {
            self.graph[node].hash(state);
        }
        for edge in self.graph.edge_indices() {
            self.graph.edge_endpoints(edge).hash(state);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub internal_call_graph: InternalCallGraph,
    pub cell_exposed_values: HashMap<String, ReportItem>,
    pub cell_depended_values: HashMap<String, ReportItem>,
    pub triggerable_functions: HashMap<String, ReportTriggerableFunctions>,
}


#[derive(Debug, Clone, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum ContextPath {
    Initialized,
    InFunction(String, TextRange),
    InAnonFunction,
    FunctionArguments,
    FunctionArgument(String),
    InClass(String),
    InFunctionDecorator(usize),
    InCallExpression,
    ChName,
    AssignmentToStatement,
    AssignmentFromStatement,
    // bool = true (is locally defined)
    IdentifierReferredTo {
        name: String,
        in_scope: bool,
        exposed: bool
    },
    Attribute(String),
    Constant(String),
}


#[derive(Error, Debug, Serialize, Deserialize)]
pub enum ChidoriStaticAnalysisError {
    #[error("Unknown chidori analysis error")]
    Unknown,
    #[error("Parse error at offset {offset} in {source_path}: {msg}. Source: {source_code}")]
    ParseError {
        msg: String,
        offset: u32,
        source_path: String,
        source_code: String
    },
}



