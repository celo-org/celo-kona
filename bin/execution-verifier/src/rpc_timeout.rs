//! RPC timeout layer for alloy transports.
//!
//! Provides a custom Tower layer that adds timeout functionality to RPC requests
//! while properly mapping errors to alloy's TransportError type.

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_transport::{TransportError, TransportErrorKind};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::time::{Duration, Instant};
use tower::{Layer, Service};

use crate::metrics::{RpcMetrics, RpcRequestResult};

/// Configuration for retry behavior with exponential backoff
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryConfig {
    /// Initial timeout duration for the first attempt
    pub initial_timeout: Duration,
    /// Maximum number of retry attempts (0 means no retries)
    pub max_retries: u32,
    /// Multiplier for exponential backoff (e.g., 2.0 for doubling)
    pub backoff_multiplier: f64,
    /// Optional maximum timeout duration to cap exponential growth
    pub max_timeout: Option<Duration>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            initial_timeout: Duration::from_secs(30),
            max_retries: 5,
            backoff_multiplier: 1.5,
            max_timeout: Some(Duration::from_secs(120)),
        }
    }
}

impl RetryConfig {
    /// Create a new RetryConfig with the specified parameters
    pub(crate) fn new(
        initial_timeout: Duration,
        max_retries: u32,
        backoff_multiplier: f64,
        max_timeout: Option<Duration>,
    ) -> Self {
        Self { initial_timeout, max_retries, backoff_multiplier, max_timeout }
    }

    /// Calculate the timeout for a given attempt number (0-indexed)
    fn timeout_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return self.initial_timeout;
        }

        let multiplier = self.backoff_multiplier.powi(attempt as i32);
        let timeout_secs = self.initial_timeout.as_secs_f64() * multiplier;
        let calculated_timeout = Duration::from_secs_f64(timeout_secs);

        match self.max_timeout {
            Some(max) => calculated_timeout.min(max),
            None => calculated_timeout,
        }
    }
}

/// Custom timeout layer that properly maps errors for alloy transports
#[derive(Clone)]
pub(crate) struct RpcTimeoutLayer {
    retry_config: RetryConfig,
    metrics: Option<Arc<RpcMetrics>>,
}

impl RpcTimeoutLayer {
    /// Create a new RpcTimeoutLayer with optional retry configuration and metrics
    ///
    /// # Arguments
    /// * `retry_config` - Optional retry configuration. If None, uses default configuration.
    /// * `metrics` - Optional metrics collector. If None, no metrics will be collected.
    pub(crate) fn new(retry_config: Option<RetryConfig>, metrics: Option<Arc<RpcMetrics>>) -> Self {
        Self { retry_config: retry_config.unwrap_or_default(), metrics }
    }
}

impl<S> Layer<S> for RpcTimeoutLayer {
    type Service = RpcRetryService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RpcRetryService { inner, retry_config: self.retry_config, metrics: self.metrics.clone() }
    }
}

/// Service that wraps an inner service with timeout functionality
#[derive(Clone)]
pub(crate) struct RpcRetryService<S> {
    inner: S,
    retry_config: RetryConfig,
    metrics: Option<Arc<RpcMetrics>>,
}

impl<S> Service<RequestPacket> for RpcRetryService<S>
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>
        + Send
        + Clone
        + 'static,
    S::Future: Send + 'static,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let retry_config = self.retry_config;
        let mut inner = self.inner.clone();
        let metrics = self.metrics.clone();

        // FIXME: is this really required?
        //
        // heap allocation and pin memory
        Box::pin(async move {
            // Extract method names for metrics and logging
            let method_names: Vec<String> = request.method_names().map(|s| s.to_string()).collect();
            let method_name_for_log = if method_names.len() == 1 {
                method_names[0].clone()
            } else {
                format!("batch({})", method_names.join(", "))
            };

            let start_time = Instant::now();
            let mut last_error = None;

            for attempt in 0..=retry_config.max_retries {
                let timeout = retry_config.timeout_for_attempt(attempt);

                // Clone the request for each retry attempt
                let request_clone = request.clone();
                let fut = inner.call(request_clone);

                match tokio::time::timeout(timeout, fut).await {
                    Ok(Ok(response)) => {
                        // Success - log retry info if this wasn't the first attempt
                        if attempt > 0 {
                            tracing::info!(
                                method = %method_name_for_log,
                                attempt = attempt + 1,
                                total_attempts = retry_config.max_retries + 1,
                                timeout_ms = timeout.as_millis(),
                                "RPC request succeeded after retry"
                            );
                        }

                        // Record success metrics for all methods in the request
                        // Divide duration by number of methods for batch requests
                        if let Some(ref metrics) = metrics {
                            let total_duration = start_time.elapsed();
                            let per_method_duration = total_duration / method_names.len() as u32;
                            for method in &method_names {
                                metrics.record_request(
                                    method,
                                    per_method_duration,
                                    RpcRequestResult::Success,
                                );
                            }
                        }

                        return Ok(response);
                    }
                    Ok(Err(e)) => {
                        // Non-timeout error from the inner service - don't retry
                        tracing::debug!(
                            method = %method_name_for_log,
                            attempt = attempt + 1,
                            error = %e,
                            "RPC request failed with non-timeout error"
                        );

                        // Record error metrics with divided duration
                        if let Some(ref metrics) = metrics {
                            let total_duration = start_time.elapsed();
                            let per_method_duration = total_duration / method_names.len() as u32;
                            for method in &method_names {
                                metrics.record_request(
                                    method,
                                    per_method_duration,
                                    RpcRequestResult::Error,
                                );
                            }
                        }

                        return Err(e);
                    }
                    Err(_timeout_elapsed) => {
                        // Timeout occurred
                        last_error = Some(timeout);

                        // Record retry metrics (not on the first attempt, only on retries)
                        if attempt > 0 {
                            if let Some(ref metrics) = metrics {
                                for method in &method_names {
                                    metrics.record_retry(method, attempt);
                                }
                            }
                        }

                        if attempt < retry_config.max_retries {
                            tracing::debug!(
                                method = %method_name_for_log,
                                attempt = attempt + 1,
                                total_attempts = retry_config.max_retries + 1,
                                timeout_ms = timeout.as_millis(),
                                next_timeout_ms =
                                    retry_config.timeout_for_attempt(attempt + 1).as_millis(),
                                "RPC request timed out, retrying with exponential backoff"
                            );
                        }
                    }
                }
            }

            // All retries exhausted - record timeout metrics with divided duration
            if let Some(ref metrics) = metrics {
                let total_duration = start_time.elapsed();
                let per_method_duration = total_duration / method_names.len() as u32;
                for method in &method_names {
                    metrics.record_request(method, per_method_duration, RpcRequestResult::Timeout);
                }
            }

            let final_timeout = last_error.unwrap_or(retry_config.initial_timeout);
            Err(TransportErrorKind::custom_str(&format!(
                "RPC request timed out after {} attempts (final timeout: {:?})",
                retry_config.max_retries + 1,
                final_timeout
            )))
        })
    }
}
