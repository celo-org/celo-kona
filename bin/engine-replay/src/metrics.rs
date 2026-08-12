//! Optional per-block Prometheus scrape, so a run records what the node *did* and not only how
//! long it took.
//!
//! The driver's clock measures the `fcu + getPayload` envelope and cannot see inside it. The
//! node's own counters can, and two of them matter more than the rest: the number of trie reads a
//! build performed, and the split between state-root time and execution time. The first decides
//! whether the plan's 5–15 ms per-read latency band is even coherent — at 50,000 reads per build,
//! 5 ms each is 250 seconds — and it is available today from counters that are already live behind
//! `--metrics`, with no patch.
//!
//! Two deliberate choices:
//!
//! * **Deltas across the build, not absolute values.** The sample is taken immediately before the
//!   forkchoice update and again immediately after `getPayload` returns — both outside the timed
//!   spans — so what is recorded is what that one build did. Cumulative counters are useless
//!   otherwise.
//! * **Only series that moved, and never `quantile=` series.** Recording every series would bury
//!   the signal, and a summary's rolling quantiles are point-in-time estimates whose difference is
//!   meaningless. Those are exactly the series the mainnet investigation was misled by; subtracting
//!   two of them would manufacture a third wrong number.

use anyhow::Context;
use std::collections::BTreeMap;

/// One scrape, flattened to `series name (with labels) -> value`.
pub(crate) type Sample = BTreeMap<String, f64>;

/// Series-name substrings kept by default.
///
/// Chosen to cover the trie read counters (`trie.walker.*`, `trie.node_iter.*`,
/// `trie.cursor.operations`) and celo-reth's payload phase histograms without having to hardcode
/// their exact exported names, which differ across reth revisions.
pub(crate) const DEFAULT_FILTERS: [&str; 2] = ["trie", "payload"];

/// A Prometheus text endpoint to sample.
#[derive(Debug)]
pub(crate) struct Scraper {
    /// HTTP client. Plain HTTP only — this is meant for a node's local metrics port.
    client: reqwest::Client,
    /// Full URL of the exposition endpoint.
    url: String,
    /// Keep a series only if its name contains one of these. Empty means keep everything.
    filters: Vec<String>,
}

impl Scraper {
    /// Build a scraper. `filters` empty means "every series".
    pub(crate) fn new(url: String, filters: Vec<String>) -> Self {
        Self { client: reqwest::Client::new(), url, filters }
    }

    /// The raw exposition text, unfiltered.
    pub(crate) async fn raw(&self) -> anyhow::Result<String> {
        self.client
            .get(&self.url)
            .send()
            .await
            .with_context(|| format!("failed to scrape {}", self.url))?
            .text()
            .await
            .with_context(|| format!("failed to read the body of {}", self.url))
    }

    /// One filtered sample.
    pub(crate) async fn sample(&self) -> anyhow::Result<Sample> {
        Ok(parse(&self.raw().await?, &self.filters))
    }
}

/// Parse the Prometheus text exposition format into `series -> value`.
///
/// Tolerant by design: an endpoint that adds a field, or a series this driver has never seen, must
/// not fail a measurement run. Unparseable lines are skipped rather than reported.
fn parse(text: &str, filters: &[String]) -> Sample {
    let mut sample = Sample::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // A summary's rolling quantile estimate is a gauge, not a counter; differencing two of them
        // produces a number that means nothing.
        if line.contains("quantile=") {
            continue;
        }
        // Split the series from `value [timestamp]` by finding where the series ends, rather than
        // by taking the last field: an optional trailing timestamp is a valid `f64`, so "does the
        // last field parse as a number" cannot tell the two apart and would silently record the
        // timestamp as the value.
        let Some((series, rest)) = split_series(line) else { continue };
        let Some(value_text) = rest.split_whitespace().next() else { continue };
        let Ok(value) = value_text.parse::<f64>() else { continue };

        let name = series.split('{').next().unwrap_or(series);
        if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        sample.insert(series.to_string(), value);
    }
    sample
}

/// Split `name{labels} value [timestamp]` into the series and the remainder.
///
/// The label block has to be walked rather than searched for a closing brace, because a label
/// *value* may legally contain `}`, `"` (escaped) or whitespace.
fn split_series(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    // The metric name runs to the first `{` or whitespace.
    while i < bytes.len() && bytes[i] != b'{' && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'{' {
        i += 1;
        let mut in_quotes = false;
        loop {
            if i >= bytes.len() {
                // Unterminated label block: not a line we can trust.
                return None;
            }
            match bytes[i] {
                b'\\' if in_quotes => i += 1, // skip the escaped byte
                b'"' => in_quotes = !in_quotes,
                b'}' if !in_quotes => {
                    i += 1;
                    break;
                }
                _ => {}
            }
            i += 1;
        }
    }
    let (series, rest) = line.split_at(i);
    let series = series.trim();
    if series.is_empty() { None } else { Some((series, rest)) }
}

/// Series that moved between two samples, as `after - before`.
///
/// A series absent from `before` is reported at its full value: it was registered during the build,
/// so everything it counts belongs to that build.
pub(crate) fn delta(before: &Sample, after: &Sample) -> Sample {
    after
        .iter()
        .filter_map(|(series, now)| {
            let then = before.get(series).copied().unwrap_or(0.0);
            let moved = now - then;
            (moved != 0.0).then(|| (series.clone(), moved))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPOSITION: &str = r#"
# HELP reth_trie_walker_branch_nodes_seeked_total Branch nodes seeked
# TYPE reth_trie_walker_branch_nodes_seeked_total counter
reth_trie_walker_branch_nodes_seeked_total 1234
reth_trie_cursor_operations{type="account",operation="seek"} 42
reth_celo_payload_state_root_duration_seconds_sum{has_best_payload="false"} 0.5
reth_celo_payload_state_root_duration_seconds{quantile="1.0"} 0.094
reth_network_peers 3
reth_trie_with_timestamp 7 1700000000000
reth_trie_awkward_label{note="a } and a \" inside",kind="x"} 11
reth_trie_unterminated{note="oops 5
malformed line without a value
"#;

    #[test]
    fn test_parses_and_filters_the_exposition_format() {
        let sample = parse(EXPOSITION, &["trie".to_string(), "payload".to_string()]);

        assert_eq!(sample.get("reth_trie_walker_branch_nodes_seeked_total"), Some(&1234.0));
        assert_eq!(
            sample.get("reth_trie_cursor_operations{type=\"account\",operation=\"seek\"}"),
            Some(&42.0)
        );
        assert_eq!(
            sample.get(
                "reth_celo_payload_state_root_duration_seconds_sum{has_best_payload=\"false\"}"
            ),
            Some(&0.5)
        );
        // A trailing timestamp must not be mistaken for the value. The timestamp parses as a
        // valid f64, so this is the case a "last numeric field wins" parser gets wrong.
        assert_eq!(sample.get("reth_trie_with_timestamp"), Some(&7.0));

        // A label value may contain `}`, an escaped quote and whitespace.
        assert_eq!(
            sample.get(r#"reth_trie_awkward_label{note="a } and a \" inside",kind="x"}"#),
            Some(&11.0)
        );

        // Filtered out by name.
        assert!(!sample.contains_key("reth_network_peers"));
        // Rolling quantile estimates are never recorded.
        assert!(sample.keys().all(|k| !k.contains("quantile=")), "{sample:?}");
        // An unterminated label block is dropped rather than guessed at.
        assert!(sample.keys().all(|k| !k.contains("unterminated")), "{sample:?}");
        // Comments, blank lines and unparseable lines are skipped, not fatal.
        assert_eq!(sample.len(), 5, "{sample:?}");
    }

    #[test]
    fn test_no_filters_keeps_everything() {
        let sample = parse(EXPOSITION, &[]);
        assert!(sample.contains_key("reth_network_peers"));
    }

    #[test]
    fn test_delta_reports_only_what_moved() {
        let before = Sample::from([
            ("a".to_string(), 10.0),
            ("unchanged".to_string(), 5.0),
            ("went_down".to_string(), 9.0),
        ]);
        let after = Sample::from([
            ("a".to_string(), 25.0),
            ("unchanged".to_string(), 5.0),
            ("went_down".to_string(), 4.0),
            ("appeared".to_string(), 3.0),
        ]);

        let moved = delta(&before, &after);
        assert_eq!(moved.get("a"), Some(&15.0));
        assert_eq!(moved.get("appeared"), Some(&3.0), "a new series counts in full");
        // A gauge that fell is still a change worth recording.
        assert_eq!(moved.get("went_down"), Some(&-5.0));
        assert!(!moved.contains_key("unchanged"));
        assert_eq!(moved.len(), 3);
    }

    #[test]
    fn test_empty_scrape_is_not_an_error() {
        assert!(parse("", &[]).is_empty());
        assert!(parse("# only comments\n", &[]).is_empty());
        assert!(delta(&Sample::new(), &Sample::new()).is_empty());
    }
}
