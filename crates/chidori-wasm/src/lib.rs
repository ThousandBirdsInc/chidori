//! Browser embedding of the chidori-js engine and its durable replay runtime.
//!
//! The engine crate is pure Rust (no C, no threads, no filesystem), so it
//! compiles to `wasm32-unknown-unknown` untouched. What a browser embedding
//! adds is the *driver*: the page owns the event loop and the host effects
//! (fetch, time, randomness, prompts), so the runtime is exposed as a pump —
//! run until a host call blocks, hand the call to JavaScript, journal the
//! result, continue. That is the same record/replay contract the native
//! runtime uses; the journal a browser records is the same artifact
//! (`DurableBlob`) the native CLI can restore, and vice versa.
//!
//! Boundary conventions, chosen to keep the JS side dependency-free:
//! * values cross as JSON strings (the journal already speaks
//!   `serde_json::Value`, so this adds no new encoding);
//! * host-op ids cross as `f64` (they are small monotonic counters; `u64`
//!   would surface as `BigInt` in JS for no benefit).
//!
//! The [`driver`] module is plain Rust with `String` errors so the whole
//! record → suspend → resolve → replay cycle is exercised by native unit
//! tests; the `#[wasm_bindgen]` layer only converts errors to `JsValue`.

use wasm_bindgen::prelude::*;

pub mod transpile;

pub mod driver {
    //! The wasm-agnostic driver core: everything here is testable on the
    //! native target (`JsValue` cannot exist outside wasm, so keeping this
    //! layer free of wasm-bindgen types is what makes `cargo test
    //! --workspace` meaningful for this crate).

    use chidori_js::replay::ReplayRuntime;
    use chidori_js::vm::RunOutcome;
    use serde_json::json;

    /// A durable runtime plus the effect list it was built with (needed again
    /// at blob-export time).
    pub struct Driver {
        runtime: ReplayRuntime,
        effects: Vec<String>,
    }

    impl Driver {
        /// Start a fresh recording of `bundle` with the named host effects
        /// installed as global async functions.
        pub fn record(bundle: &str, effects: Vec<String>) -> Driver {
            let refs: Vec<&str> = effects.iter().map(String::as_str).collect();
            Driver {
                runtime: ReplayRuntime::record(bundle, &refs),
                effects,
            }
        }

        /// Restore from a persisted journal, replaying recorded effects up to
        /// the pending frontier without re-performing them.
        pub fn restore(
            bundle: &str,
            journal: &[u8],
            effects: Vec<String>,
        ) -> Result<Driver, String> {
            let refs: Vec<&str> = effects.iter().map(String::as_str).collect();
            Ok(Driver {
                runtime: ReplayRuntime::restore(bundle, journal, &refs)?,
                effects,
            })
        }

        /// Restore from a self-describing `DurableBlob` (bundle + effect list
        /// + journal), the artifact `to_blob` produces.
        pub fn from_blob(bytes: &[u8]) -> Result<Driver, String> {
            let blob: chidori_js::replay::DurableBlob =
                serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
            let effects = blob.effects.clone();
            let refs: Vec<&str> = effects.iter().map(String::as_str).collect();
            Ok(Driver {
                runtime: ReplayRuntime::restore(&blob.bundle, &blob.journal, &refs)?,
                effects,
            })
        }

        /// Pump the VM to completion or the next blocking host call. Returns a
        /// JSON status object:
        /// `{"status":"completed"}` or
        /// `{"status":"blocked","opId":n,"name":"...","args":[...]}`.
        pub fn run_until_blocked(&mut self) -> Result<String, String> {
            self.runtime.ensure_started()?;
            match self.runtime.run_until_blocked()? {
                RunOutcome::Completed => Ok(json!({ "status": "completed" }).to_string()),
                RunOutcome::BlockedOnHost(op_id) => {
                    let (name, args) = self
                        .runtime
                        .pending_op(op_id)
                        .ok_or_else(|| format!("blocked on unknown host op {op_id}"))?;
                    Ok(json!({
                        "status": "blocked",
                        "opId": op_id as f64,
                        "name": name,
                        "args": args,
                    })
                    .to_string())
                }
            }
        }

        /// Resolve a blocked host op with a JSON-encoded value, journaling it.
        pub fn resolve_op(&mut self, op_id: f64, result_json: &str) -> Result<(), String> {
            let value = serde_json::from_str(result_json)
                .map_err(|e| format!("host result is not valid JSON: {e}"))?;
            self.runtime.resolve_op(op_id as u64, Ok(value))
        }

        /// Reject a blocked host op with an error message, journaling it.
        pub fn reject_op(&mut self, op_id: f64, message: &str) -> Result<(), String> {
            self.runtime
                .resolve_op(op_id as u64, Err(message.to_string()))
        }

        /// The effect journal alone (restore needs the bundle separately).
        pub fn journal_bytes(&self) -> Vec<u8> {
            self.runtime.journal_bytes()
        }

        /// The full durable artifact: bundle + effect list + journal.
        pub fn to_blob(&self) -> Vec<u8> {
            let refs: Vec<&str> = self.effects.iter().map(String::as_str).collect();
            self.runtime.to_blob(&refs)
        }

        /// Console output accumulated by the bundle so far.
        pub fn console_lines(&self) -> Vec<String> {
            self.runtime.console().to_vec()
        }

        /// Set when replay detected an edit that diverged from the journal.
        pub fn divergence(&self) -> Option<String> {
            self.runtime.divergence()
        }

        /// True when restored against a bundle whose hash differs from the
        /// recorded one (modify-and-resume).
        pub fn bundle_changed(&self) -> bool {
            self.runtime.bundle_changed()
        }
    }
}

fn to_js(e: String) -> JsValue {
    JsValue::from_str(&e)
}

/// One-shot evaluation: compile and run a script, returning the completion
/// value as a string. The smallest possible "the engine is alive" check.
#[wasm_bindgen(js_name = evalScript)]
pub fn eval_script(src: &str) -> Result<String, JsValue> {
    chidori_js::eval_to_string(src).map_err(to_js)
}

/// Strip TypeScript syntax from an agent source, returning plain JavaScript.
/// `filename` picks the dialect (`agent.tsx` enables JSX); pass `agent.ts`
/// when in doubt. Mirrors the native runtime's transpile defaults.
#[wasm_bindgen(js_name = stripTypes)]
pub fn strip_types(source: &str, filename: &str) -> Result<String, JsValue> {
    transpile::strip_types(source, filename).map_err(to_js)
}

/// The durable runtime, driven from JavaScript. See the crate docs for the
/// pump protocol.
#[wasm_bindgen]
pub struct WasmRuntime {
    inner: driver::Driver,
}

#[wasm_bindgen]
impl WasmRuntime {
    /// Start a fresh recording of `bundle` with the named host effects.
    #[wasm_bindgen(constructor)]
    pub fn new(bundle: &str, effects: Vec<String>) -> WasmRuntime {
        WasmRuntime {
            inner: driver::Driver::record(bundle, effects),
        }
    }

    /// Restore from a journal (`journalBytes`) plus the bundle and effects.
    pub fn restore(
        bundle: &str,
        journal: &[u8],
        effects: Vec<String>,
    ) -> Result<WasmRuntime, JsValue> {
        driver::Driver::restore(bundle, journal, effects)
            .map(|inner| WasmRuntime { inner })
            .map_err(to_js)
    }

    /// Restore from a self-describing durable blob (`toBlob`).
    #[wasm_bindgen(js_name = fromBlob)]
    pub fn from_blob(bytes: &[u8]) -> Result<WasmRuntime, JsValue> {
        driver::Driver::from_blob(bytes)
            .map(|inner| WasmRuntime { inner })
            .map_err(to_js)
    }

    /// Pump to completion or the next blocking host call; returns the JSON
    /// status object described in the crate docs.
    #[wasm_bindgen(js_name = runUntilBlocked)]
    pub fn run_until_blocked(&mut self) -> Result<String, JsValue> {
        self.inner.run_until_blocked().map_err(to_js)
    }

    /// Resolve a blocked host op with a JSON-encoded value.
    #[wasm_bindgen(js_name = resolveOp)]
    pub fn resolve_op(&mut self, op_id: f64, result_json: &str) -> Result<(), JsValue> {
        self.inner.resolve_op(op_id, result_json).map_err(to_js)
    }

    /// Reject a blocked host op with an error message.
    #[wasm_bindgen(js_name = rejectOp)]
    pub fn reject_op(&mut self, op_id: f64, message: &str) -> Result<(), JsValue> {
        self.inner.reject_op(op_id, message).map_err(to_js)
    }

    /// The effect journal alone.
    #[wasm_bindgen(js_name = journalBytes)]
    pub fn journal_bytes(&self) -> Vec<u8> {
        self.inner.journal_bytes()
    }

    /// The full durable artifact (bundle + effects + journal).
    #[wasm_bindgen(js_name = toBlob)]
    pub fn to_blob(&self) -> Vec<u8> {
        self.inner.to_blob()
    }

    /// Console output accumulated by the bundle so far.
    #[wasm_bindgen(js_name = consoleLines)]
    pub fn console_lines(&self) -> Vec<String> {
        self.inner.console_lines()
    }

    /// Replay divergence message, if an edit conflicted with the journal.
    pub fn divergence(&self) -> Option<String> {
        self.inner.divergence()
    }

    /// True when restored with a modified bundle (modify-and-resume).
    #[wasm_bindgen(js_name = bundleChanged)]
    pub fn bundle_changed(&self) -> bool {
        self.inner.bundle_changed()
    }
}

#[cfg(test)]
mod tests {
    use super::driver::Driver;
    use serde_json::json;

    const BUNDLE: &str = r#"
        async function main() {
            const a = await fetchValue('a');
            const b = await fetchValue('b');
            console.log('sum: ' + (a + b));
        }
        main();
    "#;

    fn effects() -> Vec<String> {
        vec!["fetchValue".to_string()]
    }

    /// Drive `d` to completion, resolving every `fetchValue` from `values` in
    /// order. Returns how many ops were resolved live (vs served from the
    /// journal).
    fn pump(d: &mut Driver, values: &mut Vec<i64>) -> usize {
        let mut live = 0;
        loop {
            let status: serde_json::Value =
                serde_json::from_str(&d.run_until_blocked().unwrap()).unwrap();
            match status["status"].as_str().unwrap() {
                "completed" => return live,
                "blocked" => {
                    assert_eq!(status["name"], json!("fetchValue"));
                    let v = values.remove(0);
                    d.resolve_op(status["opId"].as_f64().unwrap(), &v.to_string())
                        .unwrap();
                    live += 1;
                }
                other => panic!("unexpected status {other}"),
            }
        }
    }

    #[test]
    fn record_pump_completes_and_journals() {
        let mut d = Driver::record(BUNDLE, effects());
        let live = pump(&mut d, &mut vec![10, 32]);
        assert_eq!(live, 2);
        assert_eq!(d.console_lines(), vec!["sum: 42".to_string()]);
        assert!(!d.journal_bytes().is_empty());
    }

    #[test]
    fn replay_from_blob_reuses_journal_without_live_ops() {
        let mut d = Driver::record(BUNDLE, effects());
        pump(&mut d, &mut vec![10, 32]);
        let blob = d.to_blob();

        // A fresh runtime restored from the blob replays both fetches from the
        // journal: it must complete with zero live resolutions.
        let mut d2 = Driver::from_blob(&blob).unwrap();
        let live = pump(&mut d2, &mut Vec::new());
        assert_eq!(live, 0);
        assert_eq!(d2.console_lines(), vec!["sum: 42".to_string()]);
        assert_eq!(d2.divergence(), None);
        assert!(!d2.bundle_changed());
    }

    #[test]
    fn suspend_mid_run_then_resume_in_fresh_runtime() {
        // Record, resolve the first fetch, then suspend at the second (the
        // frontier) — the browser-tab-closed case.
        let mut d = Driver::record(BUNDLE, effects());
        let status: serde_json::Value =
            serde_json::from_str(&d.run_until_blocked().unwrap()).unwrap();
        assert_eq!(status["status"], json!("blocked"));
        d.resolve_op(status["opId"].as_f64().unwrap(), "10")
            .unwrap();
        let status: serde_json::Value =
            serde_json::from_str(&d.run_until_blocked().unwrap()).unwrap();
        assert_eq!(status["status"], json!("blocked"));
        let blob = d.to_blob();

        // Restore: the first fetch replays from the journal; only the frontier
        // fetch resolves live.
        let mut d2 = Driver::from_blob(&blob).unwrap();
        let live = pump(&mut d2, &mut vec![32]);
        assert_eq!(live, 1);
        assert_eq!(d2.console_lines(), vec!["sum: 42".to_string()]);
    }

    #[test]
    fn reject_op_surfaces_as_js_exception() {
        let mut d = Driver::record(BUNDLE, effects());
        let status: serde_json::Value =
            serde_json::from_str(&d.run_until_blocked().unwrap()).unwrap();
        d.reject_op(status["opId"].as_f64().unwrap(), "network down")
            .unwrap();
        // The rejection propagates out of main() as an unhandled rejection;
        // the run still reaches quiescence rather than blocking forever.
        let status: serde_json::Value =
            serde_json::from_str(&d.run_until_blocked().unwrap()).unwrap();
        assert_eq!(status["status"], json!("completed"));
        assert!(d.console_lines().is_empty());
    }
}
