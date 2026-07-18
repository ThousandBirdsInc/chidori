//! Typed error taxonomy for the durable runtime.
//!
//! The crate's anyhow regime (see `docs/rust-style-guide.md`) stays: one
//! error chain that prints well. This module exists for the two places a
//! caller genuinely needs to *match* on a failure:
//!
//! - [`RunInterrupt`] — the pause control-flow signal. A `chidori.input()`
//!   in Pause mode, an empty-mailbox `chidori.signal(...)` listen point, or
//!   a policy approval gate suspends the run by unwinding with an error.
//!   That "error" is not a failure; every layer between the host effect and
//!   the engine must recognize it and let it pass.
//! - [`RunErrorKind`] — a coarse classification of terminal run failures for
//!   library consumers of the `framework` facade, who otherwise see only an
//!   opaque `anyhow::Error`.
//!
//! # The JS-boundary constraint (why the wire string exists)
//!
//! A pause raised inside a host effect crosses the JavaScript engine on its
//! way back to the engine loop: host bindings return `Result<_, String>` into
//! the VM (`runtime/typescript/bindings.rs`), the VM throws that string as a
//! JS exception, and — when agent code doesn't catch it — the entrypoint
//! error surfaces back to Rust as a *stringified* exception. A Rust enum
//! cannot survive that hop, so the interrupt is encoded as a marker-tagged
//! wire string ([`RunInterrupt::to_wire`]) and re-parsed on re-entry
//! ([`RunInterrupt::from_message`]). The wire format is the legacy
//! [`PAUSE_MARKER`]-prefixed text, kept byte-for-byte so durable artifacts,
//! SDKs, and tests keyed on the marker keep working.
//!
//! Rust-side raisers construct `anyhow::Error::new(RunInterrupt::...)`
//! directly (its `Display` *is* the wire string), and detection always goes
//! through [`RunInterrupt::from_error`]: downcast first for errors that never
//! left Rust, string parse second for errors that round-tripped through JS.
//! The parsed payload (prompt, signal name) is best-effort diagnostics — the
//! authoritative pause payload is the `RuntimeContext` pending slot
//! (`pending_input` / `pending_signal` / `pending_approval`) set by the
//! raiser before unwinding.

/// Marker text tagging the pause sentinel so it can be told apart from a
/// genuine failure after a JS round trip. Compose and parse the surrounding
/// message ONLY via [`RunInterrupt::to_wire`] / [`RunInterrupt::from_message`];
/// the constant stays public for external consumers keyed on the substring.
pub const PAUSE_MARKER: &str = "__CHIDORI_PAUSED_FOR_INPUT__";

/// The pause control-flow signal: why a run suspended, with the payload the
/// raiser had at hand. See the module docs for why this doubles as a wire
/// string.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunInterrupt {
    /// `chidori.input(prompt)` in Pause mode with no answer available — the
    /// run parks until a response is delivered (server `/resume`).
    Input { prompt: String },
    /// A `chidori.signal(name)` listen point with an empty mailbox.
    Signal { name: String },
    /// A fan-in `chidori.signal(names[])` listen point with an empty mailbox.
    SignalAny { names: Vec<String> },
    /// A policy `AskBefore` decision in Pause mode — the run parks until the
    /// approval is granted or denied. The target/reason payload rides in the
    /// context's `pending_approval` slot, not the wire string (the legacy
    /// wire shape for approvals is the bare marker).
    Approval,
}

impl RunInterrupt {
    /// Encode the interrupt as the legacy marker-tagged wire string — the
    /// exact bytes historically produced by the `format!`-based raisers, so
    /// stored session errors and external consumers keyed on the marker are
    /// unchanged. This is also the `Display` impl, which is what makes
    /// `anyhow::Error::new(interrupt).to_string()` byte-compatible with the
    /// old `anyhow!("{PAUSE_MARKER}: ...")` errors.
    pub fn to_wire(&self) -> String {
        match self {
            Self::Input { prompt } => format!("{PAUSE_MARKER}: {prompt}"),
            Self::Signal { name } => format!("{PAUSE_MARKER}: signal {name}"),
            Self::SignalAny { names } => {
                format!("{PAUSE_MARKER}: signalAny [{}]", names.join(", "))
            }
            Self::Approval => PAUSE_MARKER.to_string(),
        }
    }

    /// Parse an interrupt back out of a message that may have round-tripped
    /// through the JS engine (picking up an `Error: ` prefix or trailing
    /// stack frames along the way). `None` means the message carries no pause
    /// marker — a genuine failure.
    ///
    /// The kind/payload split is best-effort: the legacy wire format is not
    /// self-delimiting (a prompt that happens to start with `signal ` parses
    /// as a signal pause), which is why matchers consult the context's
    /// pending slots for the authoritative pause payload.
    pub fn from_message(msg: &str) -> Option<Self> {
        let idx = msg.find(PAUSE_MARKER)?;
        let rest = &msg[idx + PAUSE_MARKER.len()..];
        let Some(payload) = rest.strip_prefix(": ") else {
            // The bare marker is the approval wire shape; an unrecognized
            // suffix still means "some pause", so fold it in rather than
            // dropping the interrupt on the floor.
            return Some(Self::Approval);
        };
        if let Some(name) = payload.strip_prefix("signal ") {
            return Some(Self::Signal {
                name: name.to_string(),
            });
        }
        if let Some(joined) = payload
            .strip_prefix("signalAny [")
            .and_then(|s| s.strip_suffix(']'))
        {
            return Some(Self::SignalAny {
                names: joined.split(", ").map(str::to_string).collect(),
            });
        }
        Some(Self::Input {
            prompt: payload.to_string(),
        })
    }

    /// Detect a pause on an `anyhow` error: downcast first (the raiser built
    /// `anyhow::Error::new(RunInterrupt::...)` and it never left Rust), then
    /// fall back to parsing the rendered chain for errors that round-tripped
    /// through the JS engine as strings. ALL pause detection goes through
    /// here — no call site matches on the marker text itself.
    pub fn from_error(err: &anyhow::Error) -> Option<Self> {
        if let Some(interrupt) = err.downcast_ref::<Self>() {
            return Some(interrupt.clone());
        }
        // `{:#}` renders the whole context chain, so a pause survives being
        // wrapped with `.context(...)` (plain `to_string()` shows only the
        // outermost message).
        Self::from_message(&format!("{err:#}"))
    }
}

impl std::fmt::Display for RunInterrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_wire())
    }
}

impl std::error::Error for RunInterrupt {}

/// Coarse classification of a run failure for library consumers of the
/// `framework` facade. Centralizes the string heuristics that would otherwise
/// be re-derived by every embedder from the rendered error chain.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
#[allow(dead_code)] // Lib-facade type; the bin target compiles the module tree separately and never matches on it.
pub enum RunErrorKind {
    /// Not a failure: the run suspended (input / signal / approval pause).
    /// `Engine::run_agent_file` normally surfaces these as a paused
    /// `RunResult` rather than an `Err`, but lower-level entry points
    /// propagate the interrupt as an error.
    Interrupt(RunInterrupt),
    /// A policy rule denied the call, the operator declined an approval, or
    /// an approval was required in a context that could not pause for one.
    PolicyDenied,
    /// A resume re-executed the agent and it called something other than what
    /// the journal recorded at that position (`Replay divergence at seq ...`).
    ReplayDivergence,
    /// A resume was refused before replaying anything: the agent source,
    /// module graph, or snapshot ABI no longer matches the run's checkpoint.
    SourceMismatch,
    /// An uncaught JavaScript exception from agent code (the
    /// `JavaScript exception: ...` framing from `runtime::rust_engine`).
    JsException,
    /// Anything else — render the chain (`{err:#}`) for the details.
    Other,
}

impl RunErrorKind {
    /// Classify a run failure. Typed downcasts win ([`RunInterrupt`]);
    /// otherwise the rendered context chain is matched against the stable
    /// message shapes the runtime produces. Domain classifications
    /// (policy/divergence/mismatch) take precedence over [`Self::JsException`]
    /// because a host-side failure that unwound through agent code arrives
    /// wearing the JS-exception framing on top.
    ///
    /// ```
    /// use chidori::framework::{RunErrorKind, RunInterrupt};
    ///
    /// let err = anyhow::anyhow!("policy: `http` denied (network disabled)");
    /// match RunErrorKind::classify(&err) {
    ///     RunErrorKind::Interrupt(RunInterrupt::Input { prompt }) => {
    ///         println!("paused for input: {prompt}");
    ///     }
    ///     RunErrorKind::PolicyDenied => println!("blocked by policy"),
    ///     RunErrorKind::ReplayDivergence | RunErrorKind::SourceMismatch => {
    ///         println!("checkpoint no longer matches the code");
    ///     }
    ///     _ => println!("failed: {err:#}"),
    /// }
    /// ```
    #[allow(dead_code)] // Lib-facade entry point; the bin target compiles the module tree separately and never calls it.
    pub fn classify(err: &anyhow::Error) -> Self {
        if let Some(interrupt) = RunInterrupt::from_error(err) {
            return Self::Interrupt(interrupt);
        }
        let text = format!("{err:#}");
        if text.contains("Replay divergence") {
            return Self::ReplayDivergence;
        }
        // "resume refused" is the CLI/server context line; the "runtime
        // snapshot ... mismatch" family is what snapshot validation itself
        // raises (source, module graph, ABI).
        if text.contains("resume refused")
            || (text.contains("runtime snapshot") && text.contains("mismatch"))
        {
            return Self::SourceMismatch;
        }
        if text.contains("policy: `") {
            return Self::PolicyDenied;
        }
        if text.contains("JavaScript exception: ") {
            return Self::JsException;
        }
        Self::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<RunInterrupt> {
        vec![
            RunInterrupt::Input {
                prompt: "Approve the deploy?".to_string(),
            },
            RunInterrupt::Signal {
                name: "review".to_string(),
            },
            RunInterrupt::SignalAny {
                names: vec!["review".to_string(), "steer".to_string()],
            },
            RunInterrupt::Approval,
        ]
    }

    #[test]
    fn wire_round_trips_every_variant() {
        for interrupt in every_variant() {
            let wire = interrupt.to_wire();
            assert!(wire.starts_with(PAUSE_MARKER), "wire must keep the marker");
            assert_eq!(RunInterrupt::from_message(&wire), Some(interrupt));
        }
    }

    #[test]
    fn wire_strings_are_byte_compatible_with_the_legacy_format() {
        assert_eq!(
            RunInterrupt::Input {
                prompt: "ok?".to_string()
            }
            .to_wire(),
            format!("{PAUSE_MARKER}: ok?"),
        );
        assert_eq!(
            RunInterrupt::Signal {
                name: "review".to_string()
            }
            .to_wire(),
            format!("{PAUSE_MARKER}: signal review"),
        );
        assert_eq!(
            RunInterrupt::SignalAny {
                names: vec!["a".to_string(), "b".to_string()]
            }
            .to_wire(),
            format!("{PAUSE_MARKER}: signalAny [a, b]"),
        );
        assert_eq!(RunInterrupt::Approval.to_wire(), PAUSE_MARKER);
    }

    #[test]
    fn from_error_downcasts_a_typed_interrupt() {
        let interrupt = RunInterrupt::Input {
            prompt: "continue?".to_string(),
        };
        let err = anyhow::Error::new(interrupt.clone());
        // Display stays the wire string, so legacy `to_string()` consumers
        // (stored session errors, logs) see the exact old bytes.
        assert_eq!(err.to_string(), interrupt.to_wire());
        assert_eq!(RunInterrupt::from_error(&err), Some(interrupt));
    }

    #[test]
    fn from_error_survives_context_wrapping() {
        let err = anyhow::Error::new(RunInterrupt::Signal {
            name: "review".to_string(),
        })
        .context("running tool `ask`");
        assert_eq!(
            RunInterrupt::from_error(&err),
            Some(RunInterrupt::Signal {
                name: "review".to_string()
            }),
        );
    }

    #[test]
    fn from_error_falls_back_to_string_parsing_after_a_js_round_trip() {
        for interrupt in every_variant() {
            // A pause thrown inside the VM comes back as a stringified JS
            // exception: `Error: ` prefix, original wire string embedded.
            let err = anyhow::anyhow!("Error: {}", interrupt.to_wire());
            assert_eq!(RunInterrupt::from_error(&err), Some(interrupt));
        }
    }

    #[test]
    fn from_message_rejects_ordinary_failures() {
        assert_eq!(RunInterrupt::from_message("connection refused"), None);
        assert_eq!(
            RunInterrupt::from_error(&anyhow::anyhow!("policy: `http` denied")),
            None,
        );
    }

    #[test]
    fn classify_recognizes_policy_denials() {
        let denied = anyhow::anyhow!("policy: `http:https://example.test` denied (no network)");
        assert_eq!(RunErrorKind::classify(&denied), RunErrorKind::PolicyDenied);
        let operator = anyhow::anyhow!("policy: `shell` denied by operator");
        assert_eq!(
            RunErrorKind::classify(&operator),
            RunErrorKind::PolicyDenied
        );
    }

    #[test]
    fn classify_recognizes_replay_divergence_even_through_js_framing() {
        let err = anyhow::anyhow!(
            "JavaScript exception: Replay divergence at seq 3: checkpoint has `llm` \
             but agent called `http`."
        );
        assert_eq!(RunErrorKind::classify(&err), RunErrorKind::ReplayDivergence);
    }

    #[test]
    fn classify_recognizes_resume_refusal_in_the_context_chain() {
        let err = anyhow::anyhow!("runtime snapshot source mismatch: snapshot has 2 files")
            .context("resume refused: the agent source no longer matches this run's checkpoint");
        assert_eq!(RunErrorKind::classify(&err), RunErrorKind::SourceMismatch);
    }

    #[test]
    fn classify_maps_pauses_and_leftovers() {
        let paused = anyhow::Error::new(RunInterrupt::Approval);
        assert_eq!(
            RunErrorKind::classify(&paused),
            RunErrorKind::Interrupt(RunInterrupt::Approval),
        );
        let js = anyhow::anyhow!("JavaScript exception: boom");
        assert_eq!(RunErrorKind::classify(&js), RunErrorKind::JsException);
        let other = anyhow::anyhow!("connection refused");
        assert_eq!(RunErrorKind::classify(&other), RunErrorKind::Other);
    }
}
