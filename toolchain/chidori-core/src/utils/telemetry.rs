use tracing::{Subscriber, span::{Attributes, Record}, Event, span, Metadata};
use tracing_subscriber::{layer::Context, Layer, registry::LookupSpan, fmt};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use tracing::subscriber::Interest;
use tracing_subscriber::layer::SubscriberExt;

pub struct CustomLayer {
    sender: Sender<String>,
}

impl CustomLayer {
    pub fn new(sender: Sender<String>) -> Self {
        CustomLayer { sender }
    }
}

impl<S> Layer<S> for CustomLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &tracing::span::Id, ctx: Context<'_, S>) {
        // Process new span here
        println!("New span: {:?}", attrs.values());
    }

    fn on_record(&self, span: &tracing::span::Id, values: &Record<'_>, ctx: Context<'_, S>) {
        // Process span record updates here
        println!("Span record: {:?}", values);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Process events here
        println!("Event: {:?}", event);
    }

    fn on_enter(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        // Process enter span here
        println!("Enter span: {:?}", id);
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        // Process exit span here
        println!("Exit span: {:?}", id);
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        // Process span close here
        println!("Close span: {:?}", id);
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


pub fn init_internal_telemetry(sender: Sender<String>) -> impl Subscriber {
    let custom_layer = CustomLayer::new(sender);
    let forwarding_layer = ForwardingLayer::new(tracing_subscriber::fmt::layer());
    let subscriber = tracing_subscriber::Registry::default()
        .with(custom_layer)
        .with(forwarding_layer);
    subscriber
}
