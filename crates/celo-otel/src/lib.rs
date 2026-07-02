//! Shared OpenTelemetry setup for the celo-kona binaries: tracing/log export,
//! metrics, and resource detection.

/// Tracing subscriber wiring and OTLP log export.
pub mod logger;
/// Meter provider construction with OTLP or stdout export.
pub mod metrics;
/// OpenTelemetry resource (service identity) construction.
pub mod resource;
