use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Histogram},
};
use std::sync::Arc;
use tokio::time::Duration;

use crate::verified_block_tracker::VerifiedBlockTracker;

pub(crate) struct Metrics {
    block_verification_count: Counter<u64>,
    duration_verification: Histogram<f64>,
}

impl Metrics {
    pub(crate) fn new(highest_block_tracker: Option<Arc<VerifiedBlockTracker>>) -> Self {
        let meter = global::meter("execution-verifier");
        if let Some(tracker) = highest_block_tracker {
            meter
                .u64_observable_gauge("highest_verified_block")
                .with_unit("block-number")
                .with_description("Highest verified continuous block.")
                .with_callback(move |obs| {
                    let block = tracker.highest();
                    if let Some(b) = block {
                        obs.observe(b, &[]);
                    }
                })
                .build();
        };
        Self {
            block_verification_count: meter
                .u64_counter("block_verification")
                .with_unit("count")
                .with_description("Number of blocks that have been verified")
                .build(),
            duration_verification: meter
                .f64_histogram("duration_verification")
                .with_unit("ms")
                .with_description("Duration in seconds for block verification.")
                .build(),
        }
    }

    pub(crate) fn block_verification_completed(&mut self, success: bool, duration: Duration) {
        let result = KeyValue::new("result", if success { "success" } else { "failed" });
        self.block_verification_count.add(1, std::slice::from_ref(&result));
        self.duration_verification.record(duration.as_secs_f64() * 1000.0, &[result])
    }
}

/// Metrics for RPC requests in the timeout tower layer
#[derive(Clone)]
pub(crate) struct RpcMetrics {
    /// Duration of RPC requests, labeled by method and result
    rpc_request_duration: Histogram<f64>,
    /// Count of RPC requests, labeled by method and result
    rpc_request_count: Counter<u64>,
    /// Count of RPC retries, labeled by method
    rpc_retry_count: Counter<u64>,
    /// Total number of timeouts across all methods
    rpc_total_timeouts: Counter<u64>,
}

impl RpcMetrics {
    pub(crate) fn new() -> Self {
        let meter = global::meter("execution-verifier");
        Self {
            rpc_request_duration: meter
                .f64_histogram("rpc_request_duration")
                .with_unit("ms")
                .with_description("Duration of RPC requests labeled by method and result")
                .build(),
            rpc_request_count: meter
                .u64_counter("rpc_request_count")
                .with_unit("count")
                .with_description("Total number of RPC requests labeled by method and result")
                .build(),
            rpc_retry_count: meter
                .u64_counter("rpc_retry_count")
                .with_unit("count")
                .with_description("Number of RPC request retries labeled by method")
                .build(),
            rpc_total_timeouts: meter
                .u64_counter("rpc_total_timeouts")
                .with_unit("count")
                .with_description("Total number of RPC timeout errors")
                .build(),
        }
    }

    /// Record a completed RPC request
    pub(crate) fn record_request(
        &self,
        method: &str,
        duration: Duration,
        result: RpcRequestResult,
    ) {
        let result_str = match result {
            RpcRequestResult::Success => "success",
            RpcRequestResult::Error => "error",
            RpcRequestResult::Timeout => "timeout",
        };

        let attrs = [
            KeyValue::new("method", method.to_string()),
            KeyValue::new("result", result_str),
        ];

        self.rpc_request_duration.record(duration.as_secs_f64() * 1000.0, &attrs);
        self.rpc_request_count.add(1, &attrs);

        if matches!(result, RpcRequestResult::Timeout) {
            self.rpc_total_timeouts.add(1, &[]);
        }
    }

    /// Record a retry attempt for an RPC request
    pub(crate) fn record_retry(&self, method: &str, attempt: u32) {
        let attrs = [
            KeyValue::new("method", method.to_string()),
            KeyValue::new("attempt", attempt as i64),
        ];
        self.rpc_retry_count.add(1, &attrs);
    }
}

/// Result of an RPC request for metrics tracking
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RpcRequestResult {
    Success,
    Error,
    Timeout,
}
