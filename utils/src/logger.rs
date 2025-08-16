use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::LogExporter;
use opentelemetry_sdk::{Resource, logs::SdkLoggerProvider};
use tracing::Level;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_tracing(
    verbosity_level: u8,
    env_filter: Option<impl Into<EnvFilter>>,
    otel_resource: Resource,
    export_telemetry: bool,
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
    let mut filter = env_filter
        .map(|e| e.into())
        .unwrap_or(EnvFilter::from_default_env())
        .add_directive(level.into());

    if verbosity_level > 3 {
        filter = filter.add_directive("opentelemetry=debug".parse().unwrap());
    } else {
        filter = filter.add_directive("opentelemetry=info".parse().unwrap());
    }

    let fmt_layer = tracing_subscriber::fmt::layer().with_thread_names(true).with_target(true);

    let otel_layer = match export_telemetry {
        true => match LogExporter::builder().with_tonic().build() {
            Ok(otlp_exporter) => SdkLoggerProvider::builder()
                .with_resource(otel_resource)
                .with_batch_exporter(otlp_exporter)
                .build(),
            Err(err) => {
                eprintln!("Failed to build OTLP log exporter: {err}");
                SdkLoggerProvider::builder().with_resource(otel_resource).build()
            }
        },
        false => SdkLoggerProvider::builder().build(),
    };

    let otel_bridge_layer = OpenTelemetryTracingBridge::new(&otel_layer);
    tracing_subscriber::Registry::default()
        .with(filter)
        .with(fmt_layer)
        .with(otel_bridge_layer)
        .init();
}
