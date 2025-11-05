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
