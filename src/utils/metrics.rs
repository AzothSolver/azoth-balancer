use prometheus::{
    register_histogram_vec, register_int_counter, register_int_counter_vec, register_int_gauge,
    register_int_gauge_vec, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec,
};
use std::sync::LazyLock;

// --- Request Metrics ---

/// Total number of RPC requests forwarded to any endpoint.
///
/// This counter tracks the overall request volume handled by the load balancer.
/// Use it to monitor throughput and detect traffic spikes or drops.
/// Example Prometheus query: `rate(rpc_requests_total[5m])` for requests per second.
pub static RPC_REQUESTS_TOTAL: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!("rpc_requests_total", "Total number of forwarded RPC requests").unwrap()
});

/// Total number of RPC requests forwarded, broken down by method.
///
/// This counter tracks requests per RPC method (e.g., `eth_call`, `eth_blockNumber`).
/// Use it to analyze method usage patterns and detect anomalies in specific methods.
/// Example query: `rate(rpc_requests_method_total{method=\"eth_call\"}[5m])`.
pub static RPC_REQUESTS_METHOD: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rpc_requests_method_total",
        "Total number of forwarded RPC requests per method",
        &["method"]
    )
    .unwrap()
});

/// Total number of successful RPC requests per endpoint.
///
/// Tracks requests that return a 2xx status and valid JSON-RPC response.
/// Use to assess endpoint reliability and compare success rates across endpoints.
/// Example query: `rate(rpc_requests_succeeded_total{endpoint=\"...\"}[5m])`.
pub static RPC_REQUESTS_SUCCEEDED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rpc_requests_succeeded_total",
        "Total number of successful requests per endpoint",
        &["endpoint"]
    )
    .unwrap()
});

/// Total number of failed RPC requests per endpoint.
///
/// Tracks requests that fail due to non-2xx status, network errors, or invalid responses.
/// Use to identify problematic endpoints and trigger investigations or reconfigurations.
/// Example query: `rate(rpc_requests_failed_total{endpoint=\"...\"}[5m])`.
pub static RPC_REQUESTS_FAILED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rpc_requests_failed_total",
        "Total number of failed requests per endpoint",
        &["endpoint"]
    )
    .unwrap()
});

/// Total number of request timeouts per endpoint.
///
/// This counter is a subset of `rpc_requests_failed_total` and specifically tracks
/// failures caused by the request timing out. Use to diagnose network congestion or
/// overloaded upstream nodes.
/// Example query: `rate(rpc_request_timeouts_total{endpoint=\"...\"}[5m])`.
pub static REQUEST_TIMEOUTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rpc_request_timeouts_total",
        "Total number of request timeouts per endpoint",
        &["endpoint"]
    )
    .unwrap()
});

/// Total number of batch requests rejected due to exceeding the maximum batch size.
///
/// Tracks rejections when the request batch size exceeds `max_batch_size`.
/// Use to detect oversized batch requests and adjust configuration or client behavior.
/// Example query: `rate(batch_size_exceeded_total[5m])`.
pub static BATCH_SIZE_EXCEEDED: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!(
        "batch_size_exceeded_total",
        "Total number of batch requests rejected due to exceeding size limit"
    )
    .unwrap()
});

/// Total number of times an upstream provider returned an HTTP 429 Too Many Requests.
///
/// Tracks explicit rate-limiting signals from providers. A high rate of these
/// indicates that the configured rate limit in `config.toml` may be too high
/// for the provider's plan.
/// Example query: `rate(upstream_rate_limited_total{endpoint=\"...\"}[5m])`.
pub static UPSTREAM_RATE_LIMITED_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "upstream_rate_limited_total",
        "Total number of times an upstream provider returned HTTP 429",
        &["endpoint"]
    )
    .unwrap()
});

// --- Endpoint State Metrics ---

/// Total number of failed health checks per endpoint.
///
/// Tracks health check failures (e.g., `eth_blockNumber` requests that timeout or return non-2xx).
/// Use to monitor endpoint health and detect persistent issues.
/// Example query: `rate(rpc_healthcheck_failed_total{endpoint=\"...\"}[5m])`.
pub static HEALTHCHECK_FAILED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rpc_healthcheck_failed_total",
        "Total number of failed health checks per endpoint",
        &["endpoint"]
    )
    .unwrap()
});

/// Total number of times an endpoint was put into cooldown.
///
/// Tracks when endpoints are temporarily removed from rotation due to failures or rate limiting.
/// Use to assess endpoint stability and adjust cooldown configurations.
/// Example query: `rate(rpc_endpoint_cooldowns_total{endpoint=\"...\"}[5m])`.
pub static COOLDOWNS_TRIGGERED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "rpc_endpoint_cooldowns_total",
        "Total number of times an endpoint was put into cooldown",
        &["endpoint"]
    )
    .unwrap()
});

/// Seconds remaining for an endpoint's cooldown period (0 when not in cooldown).
///
/// Updated every second by the `cooldown_gauge_updater` task.
/// Use to monitor how long endpoints are unavailable due to cooldowns.
/// Example query: `rpc_endpoint_cooldown_seconds{endpoint=\"...\"} > 0`.
pub static COOLDOWN_SECONDS_GAUGE: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "rpc_endpoint_cooldown_seconds",
        "Seconds remaining for endpoint cooldown (0 when not cooling)",
        &["endpoint"]
    )
    .unwrap()
});

/// Number of currently healthy RPC endpoints (healthy and not in cooldown).
///
/// Updated after health checks and cooldown events.
/// Use to monitor the overall health of the endpoint pool.
/// Example query: `healthy_endpoints / total_endpoints` for health ratio.
pub static HEALTHY_ENDPOINTS: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!("healthy_endpoints", "Number of currently healthy RPC endpoints").unwrap()
});

/// Total number of configured RPC endpoints.
///
/// Set during initialization based on the configuration.
/// Use as a baseline to compare against `healthy_endpoints`.
/// Example query: `total_endpoints`.
pub static TOTAL_ENDPOINTS: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!("total_endpoints", "Total number of RPC endpoints configured").unwrap()
});

// --- Balancer Logic Metrics ---

/// The number of currently active, in-flight requests being processed by the balancer.
///
/// This gauge tracks the number of permits currently held from the global
/// concurrency limiter semaphore. If this value approaches `max_concurrency`,
/// it's an indicator of system saturation.
/// Example query: `concurrency_limiter_active_permits`.
pub static CONCURRENCY_ACTIVE_PERMITS: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "concurrency_limiter_active_permits",
        "Number of currently active in-flight requests"
    )
    .unwrap()
});

/// Total number of requests deferred due to per-endpoint rate limiting.
///
/// Tracks when the local `RateLimiter` defers requests to other endpoints due to exceeding
/// the configured quota. Use to detect capacity issues and adjust `rate_limit_per_sec` or `burst_size`.
/// Example query: `rate(endpoint_rate_limit_deferred_total{endpoint=\"...\"}[5m])`.
pub static ENDPOINT_RATE_LIMIT_DEFERRED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "endpoint_rate_limit_deferred_total",
        "Total requests deferred due to per-endpoint rate limiting",
        &["endpoint"]
    )
    .unwrap()
});

/// Total number of times no endpoint was available due to rate limiting or unavailability.
///
/// Tracks critical events where all endpoints are rate-limited, in cooldown, or unhealthy.
/// Use to detect systemic capacity issues requiring additional endpoints.
/// Example query: `rate(all_endpoints_rate_limited_total[5m])`.
pub static ALL_ENDPOINTS_RATE_LIMITED: LazyLock<IntCounter> = LazyLock::new(|| {
    register_int_counter!(
        "all_endpoints_rate_limited_total",
        "Total times no endpoint was available due to rate limiting"
    )
    .unwrap()
});

/// Total number of times each endpoint is selected by the priority-based algorithm.
///
/// Tracks endpoint selection by priority rank (based on weight).
/// Use to verify the priority-based selection algorithm's behavior.
/// Example query: `rate(priority_endpoint_selected_total{endpoint=\"...\", priority_rank=\"1\"}[5m])`.
pub static PRIORITY_ENDPOINT_SELECTED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "priority_endpoint_selected_total",
        "Total number of times each priority level was selected",
        &["endpoint", "priority_rank"]
    )
    .unwrap()
});

// --- Latency Metrics ---

/// Histogram of RPC request durations in seconds per endpoint.
///
/// Buckets: `[0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0]`.
/// Use to analyze latency distribution and identify slow endpoints.
/// Example query: `histogram_quantile(0.95, sum(rate(rpc_request_duration_seconds_bucket{endpoint=\"...\"}[5m])) by (le))`.
pub static REQUEST_LATENCY_PER_ENDPOINT: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "rpc_request_duration_seconds",
        "Histogram of RPC request duration in seconds per endpoint",
        &["endpoint"],
        vec![0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0]
    )
    .unwrap()
});

/// Histogram of health check durations in seconds per endpoint.
///
/// Buckets: `[0.1, 0.5, 1.0, 2.0, 5.0]`.
/// Use to assess endpoint responsiveness during health checks.
/// Example query: `histogram_quantile(0.99, sum(rate(rpc_health_check_duration_seconds_bucket{endpoint=\"...\"}[5m])) by (le))`.
pub static HEALTH_CHECK_LATENCY: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "rpc_health_check_duration_seconds",
        "Health check latency per endpoint",
        &["endpoint"],
        vec![0.1, 0.5, 1.0, 2.0, 5.0]
    )
    .unwrap()
});
