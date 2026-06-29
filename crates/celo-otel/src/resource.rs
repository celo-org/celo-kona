use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;
use uuid::Uuid;

/// Builds the OpenTelemetry [`Resource`] identifying this service.
///
/// Sets the service name, the `celo-kona` namespace, the given version, and a
/// random per-process instance id.
pub fn build_resource(service_name: String, version: String) -> Resource {
    Resource::builder()
        .with_service_name(service_name)
        .with_attribute(KeyValue::new("service.namespace", "celo-kona"))
        .with_attribute(KeyValue::new("service.version", version))
        .with_attribute(KeyValue::new("service.instance.id", Uuid::new_v4().to_string()))
        .build()
}
