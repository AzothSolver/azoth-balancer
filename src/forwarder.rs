//! This module is responsible for the I/O layer of the application.
//!
//! It handles the task of forwarding a prepared JSON-RPC request to a
//! specific upstream endpoint, managing the HTTP client interaction,
//! and parsing the response.

use bytes::Bytes;
use reqwest::StatusCode;
use std::time::Instant;
use tracing::{error, warn};

use crate::balancer::LoadBalancer;
use crate::endpoint::{ErrorKind, LoadBalancerError, RequestOutcome};
use crate::metrics::{
    REQUEST_LATENCY_PER_ENDPOINT, REQUEST_TIMEOUTS, RPC_REQUESTS_FAILED, RPC_REQUESTS_METHOD,
    RPC_REQUESTS_SUCCEEDED, RPC_REQUESTS_TOTAL, UPSTREAM_RATE_LIMITED_TOTAL,
};

/// A lightweight, non-parsing check to see if a JSON-RPC response body contains an error.
/// It searches for the `"error":` key and checks that the following value is not `null`.
/// This avoids a full JSON parse for performance on the hot path.
pub fn response_has_error(bytes: &Bytes) -> bool {
    let key = b"\"error\":";
    if let Some(start) = bytes.windows(key.len()).position(|window| window == key) {
        let mut idx = start + key.len();
        while idx < bytes.len() && (bytes[idx] as char).is_whitespace() {
            idx += 1;
        }
        let null_val = b"null";
        if bytes.get(idx..idx + null_val.len()) == Some(null_val) {
            return false;
        }
        return true;
    }
    false
}

/// Forwards a raw RPC request to a specified endpoint and handles the response.
///
/// This function performs the following steps:
/// 1. Records latency and increments request metrics.
/// 2. Sends the request using the shared `reqwest::Client`.
/// 3. Handles network errors, distinguishing timeouts for specific metric tracking.
/// 4. Handles non-success or 429 status codes by marking the endpoint as unhealthy/rate-limited.
/// 5. Upon every request outcome, updates the endpoint's performance metrics (latency and penalty).
/// 6. Increments success or failure metrics based on the response body.
pub async fn forward_request(
    balancer: &LoadBalancer,
    request_body: Bytes,
    endpoint_url: &str,
    method: &str,
) -> Result<Bytes, LoadBalancerError> {
    let timer = REQUEST_LATENCY_PER_ENDPOINT.with_label_values(&[endpoint_url]).start_timer();
    let start_time = Instant::now();

    RPC_REQUESTS_TOTAL.inc();
    RPC_REQUESTS_METHOD.with_label_values(&[method]).inc();

    let client = balancer.client.read().clone();
    let response_result = client
        .post(endpoint_url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(request_body)
        .send()
        .await;

    let elapsed_ms = start_time.elapsed().as_millis() as u64;
    timer.observe_duration();

    // --- Metrics Update Logic ---
    if let Some(ep) = balancer.endpoints.read().iter().find(|e| e.url == endpoint_url) {
        let smoothing_factor = *balancer.latency_smoothing_factor.read();
        let outcome: RequestOutcome;

        match &response_result {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    outcome = RequestOutcome::Success { latency_ms: elapsed_ms };
                } else if status == StatusCode::TOO_MANY_REQUESTS {
                    outcome = RequestOutcome::Failure {
                        error_kind: ErrorKind::RateLimit,
                        latency_ms: elapsed_ms,
                    };
                } else if status.is_server_error() {
                    outcome = RequestOutcome::Failure {
                        error_kind: ErrorKind::Http5xx,
                        latency_ms: elapsed_ms,
                    };
                } else if status.is_client_error() {
                    outcome = RequestOutcome::Failure {
                        error_kind: ErrorKind::Http4xx,
                        latency_ms: elapsed_ms,
                    };
                } else {
                    // Treat other non-success codes as generic connection errors
                    outcome = RequestOutcome::Failure {
                        error_kind: ErrorKind::ConnectionError,
                        latency_ms: elapsed_ms,
                    };
                }
            }
            Err(e) => {
                if e.is_timeout() {
                    // For a timeout, the effective latency is the timeout duration itself.
                    let timeout_ms = (*balancer.timeout_secs.read()) * 1000;
                    outcome = RequestOutcome::Failure {
                        error_kind: ErrorKind::Timeout,
                        latency_ms: timeout_ms,
                    };
                } else {
                    outcome = RequestOutcome::Failure {
                        error_kind: ErrorKind::ConnectionError,
                        latency_ms: elapsed_ms,
                    };
                }
            }
        }
        ep.metrics.update(&outcome, smoothing_factor);
    }
    // --- End Metrics Update Logic ---

    let response = match response_result {
        Ok(resp) => resp,
        Err(e) => {
            if e.is_timeout() {
                REQUEST_TIMEOUTS.with_label_values(&[endpoint_url]).inc();
            }
            balancer.mark_unhealthy(endpoint_url).await;
            RPC_REQUESTS_FAILED.with_label_values(&[endpoint_url]).inc();
            error!(endpoint = %endpoint_url, error = %e, "Network error from endpoint");
            return Err(LoadBalancerError::UpstreamError(e.to_string()));
        }
    };

    match response.status() {
        reqwest::StatusCode::TOO_MANY_REQUESTS => {
            UPSTREAM_RATE_LIMITED_TOTAL.with_label_values(&[endpoint_url]).inc();
            RPC_REQUESTS_FAILED.with_label_values(&[endpoint_url]).inc();
            balancer.mark_rate_limited(endpoint_url).await;
            error!(endpoint = %endpoint_url, "Upstream rate-limited (429)");
            return Err(LoadBalancerError::RateLimited(endpoint_url.to_string()));
        }
        status if !status.is_success() => {
            balancer.mark_unhealthy(endpoint_url).await;
            RPC_REQUESTS_FAILED.with_label_values(&[endpoint_url]).inc();
            warn!(endpoint = %endpoint_url, status = %status, "Non-success response");
            return Err(LoadBalancerError::UpstreamError(format!(
                "Upstream returned non-success status: {}",
                status
            )));
        }
        _ => {}
    }

    let response_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            balancer.mark_unhealthy(endpoint_url).await;
            RPC_REQUESTS_FAILED.with_label_values(&[endpoint_url]).inc();
            error!(endpoint = %endpoint_url, error = %e, "Failed to read response body");
            return Err(LoadBalancerError::UpstreamError(e.to_string()));
        }
    };

    if response_has_error(&response_bytes) {
        RPC_REQUESTS_FAILED.with_label_values(&[endpoint_url]).inc();
    } else {
        RPC_REQUESTS_SUCCEEDED.with_label_values(&[endpoint_url]).inc();
    }

    Ok(response_bytes)
}
