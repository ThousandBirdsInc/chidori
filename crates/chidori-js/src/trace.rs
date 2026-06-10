//! JS-level execution tracing: a write-only observer the VM notifies on every
//! function activation. This is a pure side channel — it never touches the
//! journal, sequence counters, or any replay-relevant state, so enabling it
//! cannot change program behavior or determinism. On replay the code re-runs,
//! so trace events regenerate identically.
//!
//! Each activation gets a `u64` token at [`on_enter`](TraceObserver::on_enter).
//! The token rides on the [`Frame`](crate::vm::Frame), so a function that
//! suspends at `await`/`yield` and resumes much later (non-LIFO) still reports
//! [`on_exit`](TraceObserver::on_exit) against the right activation.

use crate::bytecode::FuncProto;
use crate::vm::Vm;

/// Metadata for a function activation that is starting.
pub struct TraceEnter<'a> {
    /// Function name (empty for anonymous).
    pub name: &'a str,
    /// Byte offset of the function in its module source (for line resolution).
    pub source_start: u32,
    pub is_async: bool,
    pub is_generator: bool,
}

/// Write-only sink for JS-level function activations. Implementors maintain
/// their own span/context stack; the VM only emits ordered notifications.
pub trait TraceObserver {
    /// A function activation is starting. Returns a token identifying it,
    /// echoed back at the matching `on_exit`/`on_suspend`/`on_resume`.
    fn on_enter(&mut self, info: TraceEnter<'_>) -> u64;
    /// The activation completed (`threw` = it unwound with an exception).
    fn on_exit(&mut self, token: u64, threw: bool);
    /// The activation paused at `await`/`yield`; control leaves it.
    fn on_suspend(&mut self, token: u64);
    /// A previously suspended activation is being resumed.
    fn on_resume(&mut self, token: u64);
}

impl Vm {
    /// Notify the sink that `proto`'s activation is starting; returns its token
    /// (None when no sink is installed — the zero-cost-off path).
    pub(crate) fn trace_enter(&mut self, proto: &FuncProto) -> Option<u64> {
        let sink = self.trace_sink.as_mut()?;
        Some(sink.on_enter(TraceEnter {
            name: &proto.name,
            source_start: proto.source_start,
            is_async: proto.kind.is_async(),
            is_generator: proto.kind.is_generator(),
        }))
    }

    pub(crate) fn trace_exit(&mut self, token: Option<u64>, threw: bool) {
        if let (Some(sink), Some(t)) = (self.trace_sink.as_mut(), token) {
            sink.on_exit(t, threw);
        }
    }

    pub(crate) fn trace_suspend(&mut self, token: Option<u64>) {
        if let (Some(sink), Some(t)) = (self.trace_sink.as_mut(), token) {
            sink.on_suspend(t);
        }
    }

    pub(crate) fn trace_resume(&mut self, token: Option<u64>) {
        if let (Some(sink), Some(t)) = (self.trace_sink.as_mut(), token) {
            sink.on_resume(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TraceEnter, TraceObserver};
    use crate::Engine;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// Records readable `enter/exit/suspend/resume <name>` lines, mapping each
    /// token back to its function name so the ordering is legible.
    struct Mock {
        events: Rc<RefCell<Vec<String>>>,
        names: HashMap<u64, String>,
        next: u64,
    }

    impl TraceObserver for Mock {
        fn on_enter(&mut self, info: TraceEnter<'_>) -> u64 {
            let token = self.next;
            self.next += 1;
            let name = if info.name.is_empty() {
                "<anon>".to_string()
            } else {
                info.name.to_string()
            };
            self.names.insert(token, name.clone());
            self.events.borrow_mut().push(format!("enter {name}"));
            token
        }
        fn on_exit(&mut self, token: u64, threw: bool) {
            let name = self.names.get(&token).cloned().unwrap_or_default();
            let suffix = if threw { " (threw)" } else { "" };
            self.events
                .borrow_mut()
                .push(format!("exit {name}{suffix}"));
        }
        fn on_suspend(&mut self, token: u64) {
            let name = self.names.get(&token).cloned().unwrap_or_default();
            self.events.borrow_mut().push(format!("suspend {name}"));
        }
        fn on_resume(&mut self, token: u64) {
            let name = self.names.get(&token).cloned().unwrap_or_default();
            self.events.borrow_mut().push(format!("resume {name}"));
        }
    }

    /// Trace `src`, returning the event lines for *named* functions (the
    /// anonymous top-level script frame is filtered out for legibility).
    fn trace_events(src: &str) -> Vec<String> {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut engine = Engine::new();
        engine.vm.trace_sink = Some(Box::new(Mock {
            events: events.clone(),
            names: HashMap::new(),
            next: 0,
        }));
        engine.eval(src).expect("eval ok");
        let out = events.borrow().clone();
        // Drop the anonymous top-level script frame for legibility.
        out.into_iter()
            .filter(|e| !e.contains("<script>") && !e.contains("<anon>"))
            .collect()
    }

    #[test]
    fn sync_calls_nest_lifo_with_matched_exits() {
        let ev = trace_events("function b(){ return 1; } function a(){ return b(); } a();");
        assert_eq!(
            ev,
            vec!["enter a", "enter b", "exit b", "exit a"],
            "sync calls nest and exit inner-first"
        );
    }

    #[test]
    fn thrown_error_still_reports_exit() {
        let ev = trace_events("function t(){ throw new Error('x'); } try { t(); } catch (e) {}");
        assert_eq!(ev, vec!["enter t", "exit t (threw)"]);
    }

    #[test]
    fn async_await_reports_suspend_resume_then_exit() {
        // f enters, suspends at await, resumes when the microtask settles, and
        // exits last — the exit attributes to the right activation despite the
        // non-LIFO resume.
        let ev = trace_events("async function f(){ await Promise.resolve(1); return 2; } f();");
        assert_eq!(ev, vec!["enter f", "suspend f", "resume f", "exit f"]);
    }

    #[test]
    fn generator_reports_suspend_per_yield_and_final_exit() {
        let ev = trace_events(
            "function* g(){ yield 1; yield 2; } \
             let it = g(); it.next(); it.next(); it.next();",
        );
        assert_eq!(
            ev,
            vec![
                "enter g",   // g() creates the generator (prologue)
                "suspend g", // parked at GeneratorStart
                "resume g",  // 1st next()
                "suspend g", // yield 1
                "resume g",  // 2nd next()
                "suspend g", // yield 2
                "resume g",  // 3rd next()
                "exit g",    // body completes
            ]
        );
    }
}
