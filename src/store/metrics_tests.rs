//! Tests for the metrics the operator views read: the request window, the
//! wait percentiles, and the per-database counters.
//!
//! These are the facts a dashboard claims to show, so they are checked
//! here rather than only through the CLI: a view that renders a wrong
//! number is worse than a view that shows nothing.

use super::*;
use crate::store::metrics::{WAIT_SAMPLES, WINDOW_BUCKETS};
use std::time::Duration;

#[test]
fn the_request_window_counts_requests_and_stays_bounded() {
    let metrics = Metrics::new();
    for _ in 0..5 {
        metrics.record_request();
    }
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.total, 5);
    // The window is a fixed number of buckets, and the five requests
    // landed in the current one.
    assert_eq!(snapshot.request_window.len(), WINDOW_BUCKETS);
    let recent: u64 = snapshot.request_window.iter().sum();
    assert_eq!(recent, 5, "the window lost a request: {snapshot:?}");
    // The window never grows, however many requests arrive.
    for _ in 0..10_000 {
        metrics.record_request();
    }
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.request_window.len(), WINDOW_BUCKETS);
    assert_eq!(snapshot.total, 10_005);
}

#[test]
fn the_window_span_is_honest_about_a_fresh_service() {
    let metrics = Metrics::new();
    metrics.record_request();
    let snapshot = metrics.snapshot();
    // A service up for a second has observed one second, not a full
    // window, so a rate computed from it is scaled honestly.
    assert!(snapshot.window_span_secs >= 1);
    assert!(snapshot.window_span_secs <= WINDOW_BUCKETS as u64);
}

#[test]
fn wait_percentiles_are_exact_and_absent_when_nothing_waited() {
    let metrics = Metrics::new();
    // No waits yet: a percentile of nothing is nothing. Reporting zero
    // would claim the queue never contended.
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.wait_p50_ms, None);
    assert_eq!(snapshot.wait_p95_ms, None);
    assert_eq!(snapshot.wait_samples, 0);

    // Ten waits of 1ms..10ms. The nearest-rank p50 is the 5th value
    // (5ms) and the p95 is the 10th (10ms).
    for millis in 1..=10u64 {
        metrics.record_queue_wait("demo", Duration::from_millis(millis), false);
    }
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.wait_p50_ms, Some(5));
    assert_eq!(snapshot.wait_p95_ms, Some(10));
    assert_eq!(snapshot.wait_samples, 10);
}

#[test]
fn the_wait_sample_ring_is_bounded() {
    let metrics = Metrics::new();
    for _ in 0..(WAIT_SAMPLES * 2 + 17) {
        metrics.record_queue_wait("demo", Duration::from_millis(3), false);
    }
    let snapshot = metrics.snapshot();
    // The ring keeps its size; it does not grow with the traffic.
    assert_eq!(snapshot.wait_samples, WAIT_SAMPLES);
    assert_eq!(snapshot.wait_p50_ms, Some(3));
}

#[test]
fn per_database_usage_and_waits_are_counted_separately() {
    let metrics = Metrics::new();
    metrics.record_usage("alpha", 10, 5);
    metrics.record_usage("alpha", 2, 1);
    metrics.record_usage("beta", 7, 0);
    metrics.record_queue_wait("alpha", Duration::from_millis(30), false);
    metrics.record_queue_wait("alpha", Duration::from_millis(10), true);

    let usage = metrics.usage_snapshot();
    assert_eq!(usage["alpha"].rows_read, 12);
    assert_eq!(usage["alpha"].rows_written, 6);
    assert_eq!(usage["beta"].rows_read, 7);
    assert_eq!(usage["beta"].rows_written, 0);

    let queue = metrics.queue_snapshot();
    assert_eq!(queue["alpha"].waits, 2);
    assert_eq!(queue["alpha"].wait_ms, 40);
    assert_eq!(queue["alpha"].timeouts, 1);
    // The worst wait is kept, because that is the one an operator
    // feels.
    assert_eq!(queue["alpha"].max_wait_ms, 30);
}

#[test]
fn a_purged_database_forgets_its_counters() {
    let metrics = Metrics::new();
    metrics.record_usage("gone", 1, 1);
    metrics.record_queue_wait("gone", Duration::from_millis(5), false);
    metrics.forget("gone");
    assert!(!metrics.usage_snapshot().contains_key("gone"));
    assert!(!metrics.queue_snapshot().contains_key("gone"));
}

#[test]
fn the_error_counters_separate_the_three_signals() {
    let metrics = Metrics::new();
    metrics.record_error("capacity_exceeded");
    metrics.record_error("query_deadline_exceeded");
    metrics.record_error("internal_error");
    metrics.record_error("not_a_counter");
    let snapshot = metrics.snapshot();
    assert_eq!(snapshot.capacity_exceeded, 1);
    assert_eq!(snapshot.deadline_exceeded, 1);
    assert_eq!(snapshot.internal_error, 1);
}
