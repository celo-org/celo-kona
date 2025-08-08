use opentelemetry_otlp::MetricExporter;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::resource::Resource;

pub fn build_meter_provider(resource: Resource) -> SdkMeterProvider {
    let provider = match MetricExporter::builder().with_tonic().build() {
        Ok(otlp_exporter) => {
            // OTLP gRPC exporter path
            let reader = PeriodicReader::builder(otlp_exporter).build();
            SdkMeterProvider::builder().with_reader(reader).with_resource(resource).build()
        }
        Err(err) => {
            eprintln!("Failed to build OTLP metric exporter: {err}");
            // Fallback: stdout exporter
            let stdout_exporter = opentelemetry_stdout::MetricExporterBuilder::default().build();
            let reader = PeriodicReader::builder(stdout_exporter).build();
            SdkMeterProvider::builder().with_reader(reader).with_resource(resource).build()
        }
    };
    provider
}
