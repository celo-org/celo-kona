use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::LogExporter;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use tracing::Level;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_tracing(
    verbosity_level: u8,
    env_filter: Option<impl Into<EnvFilter>>,
    otel_resource: Option<Resource>,
) {
    let level = match verbosity_level {
        1 => Level::ERROR,
        2 => Level::WARN,
        3 => Level::INFO,
        4 => Level::DEBUG,
        _ => Level::TRACE,
    };
    // TODO: edge-case, the u8 is 0 because it wasn't passed from cli.v
    // if verbosity_level == 0 {
    //     tracing::subscriber::set_global_default(tracing_subscriber::fmt().finish());
    //     return;
    // }
    let filter = env_filter
        .map(|e| e.into())
        .unwrap_or(EnvFilter::from_default_env())
        .add_directive(level.into())
        .add_directive("opentelemetry=info".parse().unwrap());

    let fmt_layer = tracing_subscriber::fmt::layer().with_thread_names(true);
    let otel_layer = if let Some(resource) = otel_resource {
        let provider = match LogExporter::builder().with_tonic().build() {
            Ok(otlp_exporter) => {
                // OTLP gRPC exporter path
                Some(
                    SdkLoggerProvider::builder()
                        .with_resource(resource)
                        .with_simple_exporter(otlp_exporter)
                        .build(),
                )
            }
            Err(err) => {
                eprintln!("Failed to build OTLP log exporter: {err}");
                None
            }
        };
        provider
    } else {
        None
    };

    // This might be duplicate code, but reducing duplication here means
    // dealing with getting the traits right
    match otel_layer {
        Some(ol) => {
            let otel_bridge_layer = OpenTelemetryTracingBridge::new(&ol);
            tracing_subscriber::Registry::default()
                .with(filter)
                .with(fmt_layer)
                .with(otel_bridge_layer)
                .init();
        }
        None => {
            tracing_subscriber::Registry::default().with(filter).with(fmt_layer).init();
        }
    }
}
