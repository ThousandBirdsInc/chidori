/// This stores, generates and manages instances of persistent data structures
/// in addition these data structures can be initialized in a manner which supports
/// durability.


use im::{HashMap as ImmutableHashMap};
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct HistoricalData<T>
    where
        T: Clone + PartialEq
{
    current: T,
    pub(crate) history: Vec<T>,
    counter: AtomicUsize,
}

impl<T> HistoricalData<T>
    where
        T: Clone + PartialEq
{
    pub(crate) fn new(initial_data: T) -> Self {
        Self {
            current: initial_data,
            history: Vec::new(),
            counter: AtomicUsize::new(0),
        }
    }

    pub(crate) fn update(&mut self, new_data: T) {
        self.history.push(self.current.clone());
        self.current = new_data;
        self.counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn has_processed_change_at(&self, index: usize) -> bool {
        if index >= self.history.len() {
            return false;
        }
        &self.history[index] != &self.current
    }
}
