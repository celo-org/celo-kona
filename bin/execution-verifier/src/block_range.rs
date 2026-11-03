use anyhow::Result;
use std::future::Future;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Runs `verify` for every block in `start_block..=end_block`, at most `concurrency` at a time.
///
/// A failed or panicked block is logged and verification continues with the rest of the range.
/// Once the range is done, returns an error if any block failed, so a run that hit failures
/// does not report success. Blocks skipped because `cancel_token` fired do not count as
/// failures.
pub(crate) async fn verify_range<F, Fut>(
    start_block: u64,
    end_block: u64,
    concurrency: usize,
    cancel_token: CancellationToken,
    verify: F,
) -> Result<()>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let mut handles = JoinSet::new();
    let mut next_block = start_block;
    let mut failed_blocks = 0usize;

    let spawn_verify = |handles: &mut JoinSet<(u64, Result<()>)>, block_number: u64| {
        let verification = verify(block_number);
        handles.spawn(async move { (block_number, verification.await) });
    };

    // Spawn initial batch
    for _ in 0..concurrency {
        if cancel_token.is_cancelled() {
            break;
        }
        if next_block <= end_block {
            spawn_verify(&mut handles, next_block);
            next_block += 1;
        }
    }

    // Process results and spawn new tasks continuously
    while let Some(joined) = handles.join_next().await {
        match joined {
            Ok((_, Ok(()))) => {}
            Ok((block_number, Err(e))) => {
                tracing::error!(block_number, "Block verification task failed: {e:#}");
                failed_blocks += 1;
            }
            Err(e) => {
                tracing::error!("Block verification task panicked: {e}");
                failed_blocks += 1;
            }
        }
        // Spawn next task if available
        if next_block <= end_block && !cancel_token.is_cancelled() {
            spawn_verify(&mut handles, next_block);
            next_block += 1;
        }
    }

    anyhow::ensure!(
        failed_blocks == 0,
        "{failed_blocks} block(s) in {start_block}..={end_block} failed verification"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// Runs [`verify_range`] over `1..=10`, failing the blocks in `failing`. Returns the result
    /// and the sorted blocks that were verified.
    async fn run_range(failing: &'static [u64]) -> (Result<()>, Vec<u64>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let result = verify_range(1, 10, 3, CancellationToken::new(), {
            let seen = seen.clone();
            move |block_number| {
                let seen = seen.clone();
                async move {
                    seen.lock().push(block_number);
                    if failing.contains(&block_number) {
                        anyhow::bail!("block {block_number} broke");
                    }
                    Ok(())
                }
            }
        })
        .await;
        let mut seen = seen.lock().clone();
        seen.sort_unstable();
        (result, seen)
    }

    #[tokio::test]
    async fn failed_blocks_fail_the_range_after_verifying_all_of_it() {
        let (result, seen) = run_range(&[2, 7]).await;
        assert_eq!(result.unwrap_err().to_string(), "2 block(s) in 1..=10 failed verification");
        assert_eq!(seen, (1..=10).collect::<Vec<_>>(), "failures must not stop the range");
    }

    #[tokio::test]
    async fn panicked_block_fails_the_range() {
        let result = verify_range(1, 3, 2, CancellationToken::new(), |block_number| async move {
            assert_ne!(block_number, 2, "block 2 panics");
            Ok(())
        })
        .await;
        assert_eq!(result.unwrap_err().to_string(), "1 block(s) in 1..=3 failed verification");
    }

    #[tokio::test]
    async fn cancelled_range_skips_remaining_blocks_without_failing() {
        let cancel_token = CancellationToken::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let result = verify_range(1, 100, 1, cancel_token.clone(), {
            let seen = seen.clone();
            move |block_number| {
                let seen = seen.clone();
                let cancel_token = cancel_token.clone();
                async move {
                    seen.lock().push(block_number);
                    if block_number == 3 {
                        cancel_token.cancel();
                    }
                    Ok(())
                }
            }
        })
        .await;
        result.unwrap();
        assert_eq!(*seen.lock(), vec![1, 2, 3]);
    }
}
