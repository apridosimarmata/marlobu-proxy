//! Prometheus metrics for observability.

use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_gauge, register_histogram_vec, CounterVec, Gauge, HistogramVec,
};

lazy_static! {
    // Connection metrics
    pub static ref PROXY_CONNECTIONS_ACTIVE: Gauge = register_gauge!(
        "marlobu_proxy_connections_active",
        "Number of active proxy connections"
    )
    .expect("failed to register PROXY_CONNECTIONS_ACTIVE metric");

    pub static ref PROXY_CONNECTIONS_TOTAL: CounterVec = register_counter_vec!(
        "marlobu_proxy_connections_total",
        "Total number of proxy connections",
        &["status"]  // "accepted", "rejected", "error"
    )
    .expect("failed to register PROXY_CONNECTIONS_TOTAL metric");

    // Query metrics
    pub static ref QUERIES_TOTAL: CounterVec = register_counter_vec!(
        "marlobu_queries_total",
        "Total number of queries processed",
        &["type", "status"]  // type: select/insert/update/delete, status: success/error
    )
    .expect("failed to register QUERIES_TOTAL metric");

    pub static ref QUERY_DURATION_SECONDS: HistogramVec = register_histogram_vec!(
        "marlobu_query_duration_seconds",
        "Query processing duration in seconds",
        &["type"],
        vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
    )
    .expect("failed to register QUERY_DURATION_SECONDS metric");

    // Session metrics
    pub static ref SESSIONS_ACTIVE: Gauge = register_gauge!(
        "marlobu_sessions_active",
        "Number of active sessions"
    )
    .expect("failed to register SESSIONS_ACTIVE metric");

    pub static ref SESSIONS_TOTAL: CounterVec = register_counter_vec!(
        "marlobu_sessions_total",
        "Total number of sessions",
        &["status"]  // "created", "approved", "rejected", "expired"
    )
    .expect("failed to register SESSIONS_TOTAL metric");

    // Approval metrics
    pub static ref APPROVALS_TOTAL: CounterVec = register_counter_vec!(
        "marlobu_approvals_total",
        "Total number of approval attempts",
        &["result"]  // "success", "conflict", "fk_violation", "error"
    )
    .expect("failed to register APPROVALS_TOTAL metric");

    pub static ref APPROVAL_CHANGES_APPLIED: CounterVec = register_counter_vec!(
        "marlobu_approval_changes_applied",
        "Number of changes applied during approvals",
        &["type"]  // "insert", "update", "delete"
    )
    .expect("failed to register APPROVAL_CHANGES_APPLIED metric");

    // Rewriter metrics
    pub static ref REWRITE_DURATION_SECONDS: HistogramVec = register_histogram_vec!(
        "marlobu_rewrite_duration_seconds",
        "SQL rewrite duration in seconds",
        &["type"],
        vec![0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1]
    )
    .expect("failed to register REWRITE_DURATION_SECONDS metric");

    // Infrastructure metrics
    pub static ref INFRA_CREATED_TOTAL: CounterVec = register_counter_vec!(
        "marlobu_infra_created_total",
        "Total infrastructure objects created",
        &["type"]  // "view", "shadow_table", "deleted_table"
    )
    .expect("failed to register INFRA_CREATED_TOTAL metric");
}

/// Export metrics in Prometheus text format.
/// Returns an error message if encoding fails.
pub fn export_metrics() -> String {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();

    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        return format!("# ERROR: failed to encode metrics: {}\n", e);
    }

    String::from_utf8(buffer)
        .unwrap_or_else(|e| format!("# ERROR: metrics contained invalid UTF-8: {}\n", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: These tests use global static state and must be run with:
    // cargo test -- --test-threads=1
    // or individually to avoid race conditions.

    #[test]
    #[ignore = "uses global state, run with --test-threads=1"]
    fn test_metrics_export() {
        // Increment some metrics
        PROXY_CONNECTIONS_TOTAL
            .with_label_values(&["accepted"])
            .inc();
        SESSIONS_TOTAL.with_label_values(&["created"]).inc();

        let output = export_metrics();
        assert!(output.contains("marlobu_proxy_connections_total"));
        assert!(output.contains("marlobu_sessions_total"));
    }

    #[test]
    #[ignore = "uses global state, run with --test-threads=1"]
    fn test_gauge_operations() {
        PROXY_CONNECTIONS_ACTIVE.set(5.0);
        assert_eq!(PROXY_CONNECTIONS_ACTIVE.get(), 5.0);

        PROXY_CONNECTIONS_ACTIVE.inc();
        assert_eq!(PROXY_CONNECTIONS_ACTIVE.get(), 6.0);

        PROXY_CONNECTIONS_ACTIVE.dec();
        assert_eq!(PROXY_CONNECTIONS_ACTIVE.get(), 5.0);
    }
}
