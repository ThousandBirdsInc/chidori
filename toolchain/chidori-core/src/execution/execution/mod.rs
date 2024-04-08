pub mod execution_graph;
pub mod execution_state;


use crate::execution::primitives::identifiers::{OperationId};



use crossbeam_utils::sync::Unparker;
pub use execution_state::{DependencyGraphMutation, ExecutionState};











use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;

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

trait ExecutionStateInstance {}
