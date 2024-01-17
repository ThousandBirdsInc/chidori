use crate::execution::execution::execution_graph::ExecutionGraph;



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
pub fn make_triggerable<F>(_reactivity_db: &mut ExecutionGraph, _args: usize, func: F) -> usize
where
    F: 'static + FnMut(Vec<&Option<Vec<u8>>>) -> Vec<u8>,
{
    let boxed_fn = Box::new(func);
    let _box_address = &*boxed_fn as *const _ as usize;
    // reactivity_db.upsert_operation(box_address, args, boxed_fn)
    assert!(false);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
