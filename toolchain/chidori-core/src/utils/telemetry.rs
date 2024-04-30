use std::ops::Deref;
use tracing::{Subscriber, span::{Attributes, Record}, Event, span, Metadata};
use tracing_subscriber::{layer::Context, Layer, registry::LookupSpan, fmt};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::time::Instant;
use tracing::subscriber::Interest;
use tracing_subscriber::layer::SubscriberExt;
use tracing::field::{ValueSet, Visit, Field};
use std::fmt::Debug;
use std::num::NonZero;
pub use serde::Serialize;

struct MatchStrVisitor<'a> {
    field: &'a str,
    captured: Option<String>,
}

impl Visit for MatchStrVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == self.field {
            self.captured = Some(format!("{:?}", value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == self.field {
            self.captured = Some(value.to_string());
        }
    }
}

fn get_value_in_valueset(valueset: &ValueSet<'_>, field: &str) -> Option<String> {
    let mut visitor = MatchStrVisitor { field, captured: None };
    valueset.record(&mut visitor);
    visitor.captured
}

// fn value_in_record(record: &Record<'_>, field: &str, value: &str) -> bool {
//     let mut visitor = MatchStrVisitor { field, value };
//     record.record(&mut visitor);
//     visitor.matched
// }

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub enum TraceEvents{
    // TODO: add support for capturing the execution id that we're observing
    NewSpan{
        id: String,
        thread_id: NonZero<u64>,
        parent_id: Option<String>,
        weight: u128,
        name: String,
        target: String,
        location: String,
        line: String,
        execution_id: Option<(usize, usize)>
    },
    Record,
    Event,
    Enter(String),
    // This means control of the span is temporarily released
    Exit(String, u128),
    // This means the span is entirely done
    Close(String, u128),
}

// https://github.com/rust-lang/rust/issues/22750
// TODO: we can't serialize these within the Tauri application due to some kind of issue
//       with serde versions once we introduced a deeper dependency on Deno.
//       we attempted the following patch to no avail:
//
//       [patch.crates-io]
//       deno = {path = "../../deno/cli"}
//       deno_runtime = {path = "../../deno/runtime"}
//       serde = {path = "../../serde/serde" }
//       serde_derive = {path = "../../serde/serde_derive" }
//       tauri = {path = "../../tauri/core/tauri" }
//
// TODO: this function exists to perform serialization to strings on THIS side of the
//       crate boundary, and then we can send the strings over to the Tauri side.
pub fn trace_event_to_string(trace_event: TraceEvents) -> String {
    serde_json::to_string(&trace_event).unwrap()
}

struct Timing {
    started_at: Instant,
}

pub struct CustomLayer {
    sender: Sender<TraceEvents>,
    started_at: Instant,
}

impl CustomLayer {
    pub fn new(sender: Sender<TraceEvents>) -> Self {
        CustomLayer {
            sender ,
            started_at: Instant::now(),
        }
    }
}

impl<S> Layer<S> for CustomLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &tracing::span::Id, ctx: Context<'_, S>) {
        // Process new span here
        let span = ctx.span(id).unwrap();
        let metadata = span.metadata();
        // span.extensions_mut().insert(Timing {
        //     started_at: Instant::now(),
        // });
        // TODO: capture Chidori execution information
        // if value_in_valueset(attrs.values(), "myfield", "myvalue") {
        //     ctx.span(id).unwrap().extensions_mut().insert(CustomLayerEnabled);
        // }

        // This weight is the start timestamp of the span, not its duration
        let weight = (Instant::now() - self.started_at).as_nanos();
        let thread_id = std::thread::current().id().as_u64();
        self.sender.send(TraceEvents::NewSpan {
            id: format!("{:?}", id),
            parent_id: span.parent().map(|p| format!("{:?}", p.id())),
            thread_id,
            weight,
            name: metadata.name().to_string(),
            target: metadata.target().to_string(),
            location: metadata.file().unwrap().to_string(),
            line: metadata.line().unwrap().to_string(),
            execution_id: get_value_in_valueset(attrs.values(), "prev_execution_id").map(|s| {
                let mut parts = s.trim_matches(|c| c == '(' || c == ')').split(',');
                let first = parts.next().unwrap().trim().parse().unwrap();
                let second = parts.next().unwrap().trim().parse().unwrap();
                (first, second)
            })
        }).unwrap();
    }

    fn on_record(&self, span: &tracing::span::Id, values: &Record<'_>, ctx: Context<'_, S>) {
        // Span with id recorded what values
        self.sender.send(TraceEvents::Record).unwrap();
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Process events here
        self.sender.send(TraceEvents::Event).unwrap();
    }

    fn on_enter(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        // Process enter span here
        self.sender.send(TraceEvents::Enter(format!("{:?}", id))).unwrap();
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        // Process exit span here
        let weight = (Instant::now() - self.started_at).as_nanos();
        self.sender.send(TraceEvents::Exit(format!("{:?}", id), weight)).unwrap();
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        let weight = (Instant::now() - self.started_at).as_nanos();
        self.sender.send(TraceEvents::Close(format!("{:?}", id), weight)).unwrap();
    }
}

pub struct ForwardingLayer<S> {
    inner: S,
}

impl<S> ForwardingLayer<S> {
    pub fn new(inner: S) -> Self {
        ForwardingLayer { inner }
    }
}

impl<S, T> Layer<T> for ForwardingLayer<S>
    where
        S: Layer<T> + Send + Sync + 'static,
        T: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, T>) {
        self.inner.on_new_span(attrs, id, ctx);
    }

    fn on_event(&self, event: &Event, ctx: Context<'_, T>) {
        self.inner.on_event(event, ctx);
    }

    fn on_enter(&self, id: &span::Id, ctx: Context<'_, T>) {
        self.inner.on_enter(id, ctx);
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, T>) {
        self.inner.on_exit(id, ctx);
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, T>) {
        self.inner.on_close(id, ctx);
    }
}


pub fn init_internal_telemetry(sender: Sender<TraceEvents>) -> impl Subscriber {
    println!("Initializing internal telemetry");
    let custom_layer = CustomLayer::new(sender);
    let forwarding_layer = ForwardingLayer::new(tracing_subscriber::fmt::layer());
    let subscriber = tracing_subscriber::Registry::default()
        .with(custom_layer)
        .with(forwarding_layer);
    subscriber
}
