use opentelemetry_sdk::Resource;

pub fn build_resource() -> Resource {
    // OTEL_SERVICE_NAME can overwrite this value
    Resource::builder().with_service_name("celo-kona").build()
}
