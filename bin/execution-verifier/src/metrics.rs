use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Histogram},
};

use tokio::time::Duration;

pub(crate) struct Metrics {
    successful_verification: Counter<u64>,
    failed_verification: Counter<u64>,
    duration_verification: Histogram<f64>,
}

impl Metrics {
    pub(crate) fn new() -> Self {
        // TODO: the meter needs to be initialized,
        // otherwise this is probably a noop.
        // is it possible to recognize that here?
        let meter = global::meter("execution-verifier");
        Self {
            successful_verification: meter
                .u64_counter("successful_verification")
                .with_unit("blocks")
                .with_description("Number of blocks that have been succesfully verified")
                .build(),
            failed_verification: meter
                .u64_counter("unsuccessful_verification")
                .with_unit("blocks")
                .with_description("Number of blocks that have been unsuccesfully verified")
                .build(),
            duration_verification: meter
                .f64_histogram("duration_verification")
                .with_unit("seconds")
                .with_description("Duration in seconds for block verification.")
                .build(),
        }
    }

    pub(crate) fn successful_block(&mut self, duration: Duration) {
        self.successful_verification.add(1, &[]);
        self.duration_verification
            .record(duration.as_secs_f64(), &[KeyValue::new("verification-result", "successful")])
    }
    pub(crate) fn failed_block(&mut self, duration: Duration) {
        self.failed_verification.add(1, &[]);
        self.duration_verification
            .record(duration.as_secs_f64(), &[KeyValue::new("verification-result", "unsuccessful")])
    }
}
