use crate::execution::execution::execution_graph::ExecutionGraph;
use crate::time_travel::global::HistoricalData;
use im::HashMap as ImmutableHashMap;
/// This describes an API for a reactive system with a function "make_triggerable" that wraps
/// a method passing to it a set of triggerable relationships. When those relationships fire,
/// the associated method is invoked.
use std::collections::HashMap;

impl<T: Clone + PartialEq> Subscribable for HistoricalData<T> {
    fn has_changed(&self) -> bool {
        // For demonstration, check if the latest change was processed.
        // You can adjust this logic as per your requirements.
        self.has_processed_change_at(self.history.len().saturating_sub(1))
    }
}
pub trait TriggerContext {}

struct Context<'a> {
    triggered: &'a mut bool,
}

impl<'a> TriggerContext for Context<'a> {}

pub trait Subscribable {
    fn has_changed(&self) -> bool;
}

/// Registers a given function with a reactivity db, based on a pointer to the boxed function.
/// We use this as the identity of that method and return that identity so that we can continue to
/// mutate the composition of the registered function.
pub fn make_triggerable<F>(reactivity_db: &mut ExecutionGraph, args: usize, func: F) -> usize
where
    F: 'static + FnMut(Vec<&Option<Vec<u8>>>) -> Vec<u8>,
{
    let boxed_fn = Box::new(func);
    let box_address = &*boxed_fn as *const _ as usize;
    // reactivity_db.upsert_operation(box_address, args, boxed_fn)
    assert!(false);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_travel::global::HistoricalData;

    #[derive(Debug, Clone)]
    struct Element {
        name: String,
    }

    impl Subscribable for Element {
        fn has_changed(&self) -> bool {
            // A simple example; in reality, you might compare states or versions
            self.name.contains("changed")
        }
    }

    struct HistoryContext<'a> {
        data: &'a mut HistoricalData<ImmutableHashMap<String, String>>,
    }

    impl<'a> TriggerContext for HistoryContext<'a> {}

    #[test]
    fn test_trigger_on_dependency_change() {
        let mut binding = false;
        {
            let mut registry = ExecutionGraph::new();
            let mut context = Context {
                triggered: &mut binding,
            };

            make_triggerable(&mut registry, 2, |ctx: Vec<&Option<Vec<u8>>>| vec![1, 2]);

            assert_eq!(*context.triggered, true);
        }
    }

    #[test]
    fn test_trigger_on_dependency_change_with_historical_data() {
        let initial_data_a = ImmutableHashMap::unit(String::from("status"), String::from("A"));
        let mut historical_data_a = HistoricalData::new(initial_data_a);

        {
            let mut registry = ExecutionGraph::new();
            let mut context = HistoryContext {
                data: &mut historical_data_a,
            };

            make_triggerable(&mut registry, 2, |ctx: Vec<&Option<Vec<u8>>>| vec![1, 2]);
        }
    }
}
