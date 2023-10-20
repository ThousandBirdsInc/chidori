pub mod execution_graph;
pub mod execution_state;
pub mod mutate_active_execution;
use crate::execution::integration::triggerable::{Subscribable, TriggerContext};
use crate::execution::primitives::identifiers::{ArgumentIndex, OperationId, TimestampOfWrite};
use crate::execution::primitives::operation::{
    OperationFn, OperationNode, OperationNodeDefinition, Signature,
};
use crate::execution::primitives::serialized_value::deserialize_from_buf;
use crate::execution::primitives::serialized_value::RkyvSerializedValue as RSV;
use crossbeam_utils::sync::Unparker;
pub use execution_state::{DependencyGraphMutation, ExecutionState};
use futures::StreamExt;
use im::HashMap as ImHashMap;
use im::HashSet as ImHashSet;
use indoc::indoc;
use petgraph::algo::toposort;
use petgraph::data::Build;
use petgraph::dot::{Config, Dot};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::graphmap::DiGraphMap;
use petgraph::visit::{Dfs, IntoEdgesDirected, VisitMap, Walker};
use petgraph::Direction;
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::{self, Formatter, Write};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

type OperationValue = Vec<u8>;
type OperationEventHandler = Box<dyn Fn(&OperationValue)>;
type OperationEventHandlers = Rc<RefCell<HashMap<usize, OperationEventHandler>>>;

/// The set of async nodes for which the scheduler has received ready
/// notifications.
#[derive(Clone)]
struct Notifications {
    /// Nodes that received notifications.
    nodes: Arc<Mutex<HashSet<OperationId>>>,

    /// Handle to wake up the scheduler thread when a notification arrives.
    unparker: Unparker,
}

impl Notifications {
    fn new(size: usize, unparker: Unparker) -> Self {
        Self {
            nodes: Arc::new(Mutex::new(HashSet::with_capacity(size))),
            unparker,
        }
    }

    /// Add a new notification.
    fn notify(&self, node_id: OperationId) {
        self.nodes.lock().unwrap().insert(node_id);
        self.unparker.unpark();
    }
}
