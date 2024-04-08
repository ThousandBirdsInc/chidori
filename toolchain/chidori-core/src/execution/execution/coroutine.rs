use std::future::Future;
use std::pin::Pin;
use std::task::{Poll, Context};

enum State {
    Halted,
    Running,
}

struct Coroutine {
    state: State,
}

impl Coroutine {
    fn waiter<'a>(&'a mut self) -> Waiter<'a> {
        Waiter { coroutine: self }
    }
}

struct Waiter<'a> {
    coroutine: &'a mut Coroutine,
}

impl<'a> Future for Waiter<'a> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Self::Output> {
        match self.coroutine.state {
            State::Halted => {
                self.coroutine.state = State::Running;
                Poll::Ready(())
            }
            State::Running => {
                self.coroutine.state = State::Halted;
                Poll::Pending
            }
        }
    }
}

use std::collections::VecDeque;

struct Executor<'a> {
    coroutines: VecDeque<Pin<Box<dyn Future<Output=()>>>>,
    waker: Waker,
    context: Context<'a>
}

impl<'a> Executor<'a> {
    fn new() -> Self {
        let waker = create_waker();
        let context = Context::from_waker(&waker);
        Executor {
            coroutines: VecDeque::new(),
            waker: waker,
            context: context
        }
    }

    // Each push into the executor creates a new coroutine object
    // TODO: include a sender in order to capture yield values
    fn push<C, F>(&mut self, closure: C)
        where
            F: Future<Output=()> + 'static,
            C: FnOnce(Coroutine) -> F,
    {
        let coroutine = Coroutine { state: State::Running };
        self.coroutines.push_back(Box::pin(closure(coroutine)));
    }

    fn progress(&mut self) {
        let context = &mut self.context;
        if let Some(mut coroutine) = self.coroutines.pop_front() {
            match coroutine.as_mut().poll(context) {
                Poll::Pending => {
                    self.coroutines.push_back(coroutine);
                },
                Poll::Ready(()) => {},
            }
        }
    }

    fn run(&mut self) {
        let waker = create_waker();
        let mut context = Context::from_waker(&waker);

        while let Some(mut coroutine) = self.coroutines.pop_front() {
            match coroutine.as_mut().poll(&mut context) {
                Poll::Pending => {
                    self.coroutines.push_back(coroutine);
                },
                Poll::Ready(()) => {},
            }
        }
    }
}

use std::task::{RawWaker, RawWakerVTable, Waker};

pub fn create_waker() -> Waker {
    // Safety: The waker points to a vtable with functions that do nothing. Doing
    // nothing is memory-safe.
    unsafe { Waker::from_raw(RAW_WAKER) }
}

const RAW_WAKER: RawWaker = RawWaker::new(std::ptr::null(), &VTABLE);
const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);

unsafe fn clone(_: *const ()) -> RawWaker { RAW_WAKER }
unsafe fn wake(_: *const ()) { }
unsafe fn wake_by_ref(_: *const ()) { }
unsafe fn drop(_: *const ()) { }

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_coroutine(){
        let mut exec = Executor::new();

        for instance in 1..=3 {
            exec.push(move |mut coroutine| async move {
                println!("{} A", instance);
                coroutine.waiter().await;
                println!("{} B", instance);
                coroutine.waiter().await;
                println!("{} C", instance);
                coroutine.waiter().await;
                println!("{} D", instance);
            });
        }

        println!("Running");
        exec.progress();
        println!("Done");
    }
}