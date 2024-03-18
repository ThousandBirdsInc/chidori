pub mod telemetry;

use std::error::Error;
use axum::handler::HandlerWithoutStateExt;
use opentelemetry::{global, KeyValue};
use opentelemetry_sdk;
use opentelemetry_otlp::WithExportConfig;
use tracing::dispatcher::DefaultGuard;
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_subscriber::Registry;
use tracing_subscriber::{prelude::*, EnvFilter};

const SERVICE_NAME: &'static str = "chidori-core";

pub fn init_telemetry(exporter_endpoint: &str) -> Result<DefaultGuard, Box<dyn Error>>  {
    // Create a gRPC exporter
    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint(exporter_endpoint);

    // Define a tracer
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter)
        .with_trace_config(
            opentelemetry_sdk::trace::config().with_resource(opentelemetry_sdk::Resource::new(vec![KeyValue::new(
                opentelemetry_semantic_conventions::resource::SERVICE_NAME,
                SERVICE_NAME.to_string(),
            )])),
        )
        .install_batch(opentelemetry_sdk::runtime::Tokio)
        .expect("Error: Failed to initialize the tracer.");

    // Define a subscriber.
    let subscriber = Registry::default();
    // Level filter layer to filter traces based on level (trace, debug, info, warn, error).
    // let level_filter_layer = EnvFilter::try_from_default_env().unwrap_or(EnvFilter::new("info"));
    // Layer for adding our configured tracer.
    let tracing_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    // Layer for printing spans to stdout
    let formatting_layer = BunyanFormattingLayer::new(
        SERVICE_NAME.to_string(),
        std::io::stdout,
    );

    global::set_text_map_propagator(opentelemetry_sdk::propagation::TraceContextPropagator::new());

    let subscriber = subscriber
        // .with(level_filter_layer)
        // .with(telemetry::CustomLayer::new())
        .with(tracing_layer)
        .with(JsonStorageLayer)
        .with(formatting_layer);

    Ok(tracing::subscriber::set_default(subscriber))
}