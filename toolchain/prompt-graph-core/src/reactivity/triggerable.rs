/// This describes an API for a reactive system with a function "make_triggerable" that wraps
/// a method passing to it a set of triggerable relationships. When those relationships fire,
/// the associated method is invoked.

use std::collections::HashMap;
use im::HashMap as ImmutableHashMap;
use crate::reactivity::database::ReactivityDatabase;
use crate::time_travel::global::HistoricalData;

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

pub fn make_triggerable<F>(
    reactivity_db: &mut ReactivityDatabase,
    func: F,
)
    where
        T: TriggerContext,
        S: Subscribable,
        F: 'static + FnMut(&mut T),
{
    let boxed_fn = Box::new(func);
    let box_address = &*boxed_fn as *const _ as usize;
    reactivity_db.add_operation(box_address, boxed_fn);
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
    fn test_has_changed() {
        let element = Element { name: "unchanged".into() };
        assert_eq!(element.has_changed(), false);

        let changed_element = Element { name: "name changed".into() };
        assert_eq!(changed_element.has_changed(), true);
    }

    #[test]
    fn test_historical_data() {
        let initial_data = ImmutableHashMap::unit(String::from("status"), String::from("initial"));
        let updated_data = ImmutableHashMap::unit(String::from("status"), String::from("updated"));

        let mut historical_data = HistoricalData::new(initial_data);
        assert_eq!(historical_data.has_changed(), false);

        historical_data.update(updated_data);
        assert_eq!(historical_data.has_changed(), true);
    }


    #[test]
    fn test_trigger_on_dependency_change() {
        let mut binding = false;
        {
            let mut registry = ReactivityDatabase::new();
            let mut context = Context {
                triggered: &mut binding,
            };

            make_triggerable( &mut registry, |ctx: &mut Context| {
                *ctx.triggered = true;
            }, &mut context);

            assert_eq!(*context.triggered, true);
        }
    }

    #[test]
    fn test_trigger_on_dependency_change_with_historical_data() {
        let initial_data_a = ImmutableHashMap::unit(String::from("status"), String::from("A"));
        let mut historical_data_a = HistoricalData::new(initial_data_a);

        {
            let mut registry = ReactivityDatabase::new();
            let mut context = HistoryContext {
                data: &mut historical_data_a,
            };

            make_triggerable(&mut registry, |ctx: &mut HistoryContext| {
                ctx.data.update(ImmutableHashMap::unit(String::from("status"), String::from("changed again")));
            }, &mut context);
        }
    }
}

