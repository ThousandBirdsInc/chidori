#![feature(coroutines)]
#![feature(coroutine_trait)]

use std::pin::Pin;
use std::task::{Context, Poll};
use futures::Future;
use futures_util::FutureExt;
use tokio::sync::oneshot;


use std::ops::{Coroutine, CoroutineState};
use std::sync::{Arc, mpsc, Mutex};
use std::sync::mpsc::{Sender, Receiver};
use crate::execution::primitives::serialized_value::RkyvSerializedValue;


enum CoroutineYieldValue {
    Value(RkyvSerializedValue),
    Coroutine(Box<dyn Coroutine<Return=CoroutineYieldValue, Yield=CoroutineYieldValue>>),
}


//
// /// The CoroutineScheduler is a non-Send type that manages the execution of coroutines. We maintain only
// /// a single instance of it and communicate with it over channels. Coroutines are not Send, but
// /// we need ExecutionState itself to be Send.
// // TODO: schedule work across multiple threads
// struct CoroutineScheduler {
//     // we maintain the call stack of coroutines for us to evaluate
//     receiver: Receiver<CoroutineSchedulerMessage>,
//     pub coroutine_stack: Vec<Pin<Box<dyn Coroutine<Return=CoroutineYieldValue, Yield=CoroutineYieldValue>>>>,
// }
//
// enum CoroutineSchedulerMessage {
//     Resume,
//     Yield(RkyvSerializedValue),
// }
//
// impl CoroutineScheduler {
//     fn new() -> (Sender<CoroutineSchedulerMessage>, Self){
//         let (sender, receiver) = mpsc::channel::<CoroutineSchedulerMessage>();
//         (sender, Self {
//             receiver,
//             coroutine_stack: vec![],
//         })
//     }
//
//     fn loop_for_work(&self) {
//         loop {
//             match self.receiver.recv() {
//                 CoroutineSchedulerMessage::Resume => {
//                     match self.resume() {
//                         Some(v) => {
//                         }
//                         None => {}
//                     }
//                 }
//                 CoroutineSchedulerMessage::Yield(v) => {
//                     self.handle_yielded_value(v);
//                 }
//             }
//         }
//     }
//
//
//     fn handle_yielded_value(&self, v: CoroutineYieldValue) -> Option<RkyvSerializedValue> {
//         match v {
//             CoroutineYieldValue::Value(v) => {
//                 Some(v)
//             }
//             CoroutineYieldValue::Coroutine(c) => {
//                 self.coroutine_stack.push(Box::into_pin(c));
//                 None
//             }
//         }
//     }
//
//     fn resume(&mut self) -> Option<RkyvSerializedValue> {
//         let mut cr = self.coroutine_stack.last_mut().unwrap().as_mut().resume(());
//         match cr {
//             CoroutineState::Yielded(v) => {
//                 self.handle_yielded_value(v)
//             }
//             CoroutineState::Complete(c) =>  {
//                 self.coroutine_stack.pop();
//                 self.handle_yielded_value(c)
//             },
//             _ => panic!("unexpected return from resume"),
//         }
//     }
// }

#[cfg(test)]
mod test {

    use std::collections::VecDeque;
    use std::ops::DerefMut;
    use std::pin::pin;
    use std::sync::{Arc, Mutex};
    use futures_util::future::BoxFuture;
    use futures_util::FutureExt;
    use tokio::runtime::Runtime;

    use super::*;

    #[tokio::test]
    async fn test_coroutine() {
        let mut coroutine = || {
            yield 1;
            "foo"
        };

        match Pin::new(&mut coroutine).resume(()) {
            CoroutineState::Yielded(1) => {}
            _ => panic!("unexpected return from resume"),
        }
        match Pin::new(&mut coroutine).resume(()) {
            CoroutineState::Complete("foo") => {}
            _ => panic!("unexpected return from resume"),
        }
    }

    #[tokio::test]
    async fn test_nested_coroutines() {

        enum CoroutineYieldValue {
            Value(u32),
            Coroutine(Box<dyn Coroutine<Return=CoroutineYieldValue, Yield=CoroutineYieldValue>>),
        }

        let mut coroutine_stack: Vec<Pin<Box<dyn Coroutine<Return=CoroutineYieldValue, Yield=CoroutineYieldValue>>>> = Vec::new();
        let coroutine_outer = move || {
            yield CoroutineYieldValue::Value(1);
            let coroutine_inner = move || {
                println!("inner coroutine");
                yield CoroutineYieldValue::Value(3);
                yield CoroutineYieldValue::Value(4);
                CoroutineYieldValue::Value(200)
            };
            yield CoroutineYieldValue::Coroutine(Box::new(coroutine_inner));
            yield CoroutineYieldValue::Value(2);
            CoroutineYieldValue::Value(100)
        };
        coroutine_stack.push(Box::pin(coroutine_outer));

        fn handle_yielded_value(coroutine_stack: &mut Vec<Pin<Box<dyn Coroutine<Return=CoroutineYieldValue, Yield=CoroutineYieldValue>>>> , v: CoroutineYieldValue) {
            match v {
                CoroutineYieldValue::Value(v) => {
                    dbg!(v);
                }
                CoroutineYieldValue::Coroutine(c) => {
                    coroutine_stack.push(Box::into_pin(c));
                }
            }
        }

        fn get_from_stack(coroutine_stack: &mut Vec<Pin<Box<dyn Coroutine<Return=CoroutineYieldValue, Yield=CoroutineYieldValue>>>>) {
            let mut cr = coroutine_stack.last_mut().unwrap().as_mut().resume(());
            match cr {
                CoroutineState::Yielded(v) => {
                    handle_yielded_value(coroutine_stack, v);
                }
                CoroutineState::Complete(c) =>  {
                    coroutine_stack.pop();
                    handle_yielded_value(coroutine_stack, c);
                },
                _ => panic!("unexpected return from resume"),
            }
        }
        get_from_stack(&mut coroutine_stack);
        get_from_stack(&mut coroutine_stack);
        get_from_stack(&mut coroutine_stack);
        get_from_stack(&mut coroutine_stack);
        get_from_stack(&mut coroutine_stack);
        get_from_stack(&mut coroutine_stack);
        get_from_stack(&mut coroutine_stack);
    }
}