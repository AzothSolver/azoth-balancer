//! This module defines the core data structures for the load balancer.
//!
//! It contains the `RpcEndpoint` struct, which represents the state and
//! configuration of a single upstream node, and the `LoadBalancerError` enum
//! for handling all possible error conditions within the application.

use crate::config::ConfigError;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::{sync::Arc, time::Instant};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LoadBalancerError {
    #[error("Rate limit exceeded for endpoint: {0}")]
    RateLimited(String),
    #[error("Upstream error: {0}")]
    UpstreamError(String),
    #[error("Configuration error: {0}")]
    ConfigError(String),
    #[error("Bad request: {0}")]
    BadRequest(String),
}

impl From<ConfigError> for LoadBalancerError {
    fn from(err: ConfigError) -> Self {
        LoadBalancerError::ConfigError(err.to_string())
    }
}

// --- New Endpoint Metrics Logic ---

/// Represents the type of error encountered during a request.
#[derive(Copy, Clone, Debug)]
pub enum ErrorKind {
    Timeout,
    ConnectionError,
    Http5xx,
    Http4xx,
    RateLimit,
}

/// Represents the outcome of a forwarded request for metrics tracking.
pub enum RequestOutcome {
    Success { latency_ms: u64 },
    Failure { error_kind: ErrorKind, latency_ms: u64 },
}

/// A dedicated structure for tracking the performance metrics of an endpoint.
#[derive(Debug, Default)]
pub struct EndpointMetrics {
    /// EMA of latency for successful requests (in milliseconds).
    pub ema_latency_ms: AtomicU64,
    /// EMA of a "penalty" score that increases on failures (in milliseconds).
    pub ema_error_penalty_ms: AtomicU64,
    /// Count of consecutive failures for exponential backoff of penalty.
    pub consecutive_failures: AtomicU32,
    /// Timestamp (seconds since UNIX_EPOCH) of when this endpoint was last selected.
    pub last_selected: AtomicU64,
}

impl EndpointMetrics {
    /// Updates the endpoint's metrics based on the outcome of a request.
    pub fn update(&self, outcome: &RequestOutcome, latency_smoothing_factor: f64) {
        match outcome {
            RequestOutcome::Success { latency_ms } => {
                // Update the success latency EMA.
                self.update_ema(&self.ema_latency_ms, *latency_ms, latency_smoothing_factor);
                // Reset failure count and decay error penalty quickly on success.
                self.consecutive_failures.store(0, Ordering::Relaxed);
                self.update_ema(&self.ema_error_penalty_ms, 0, 0.5); // Fast decay
            }
            RequestOutcome::Failure { error_kind, latency_ms } => {
                let penalty = self.calculate_penalty(*error_kind, *latency_ms);
                // Update the error penalty EMA aggressively.
                self.update_ema(&self.ema_error_penalty_ms, penalty, 0.8); // Fast attack
                let _ = self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// A generic helper to update an atomic U64 using an EMA formula.
    pub fn update_ema(&self, atomic: &AtomicU64, new_value: u64, alpha: f64) {
        let _ = atomic.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current_ema| {
            // Use floating point for accurate calculation, then convert back to u64.
            let new_ema = (alpha * new_value as f64) + ((1.0 - alpha) * current_ema as f64);
            Some(new_ema.round() as u64)
        });
    }

    /// Calculates a penalty value based on the type of error.
    fn calculate_penalty(&self, error_kind: ErrorKind, latency_ms: u64) -> u64 {
        let base_penalty = match error_kind {
            ErrorKind::Timeout => 30_000,         // 30 seconds - very bad
            ErrorKind::ConnectionError => 20_000, // 20 seconds - very bad
            ErrorKind::Http5xx => 15_000,         // 15 seconds - bad
            ErrorKind::Http4xx => 5_000,          // 5 seconds - not great
            ErrorKind::RateLimit => 10_000,       // 10 seconds - bad, but often temporary
        };
        // The penalty is the worse of the actual latency or the base penalty for the error type.
        base_penalty.max(latency_ms)
    }

    /// Calculates the total "cost" of using an endpoint, combining latency and penalties.
    /// This is the primary value used by the selection strategy.
    pub fn get_total_cost(&self) -> u64 {
        let success_latency = self.ema_latency_ms.load(Ordering::Relaxed);
        let error_penalty = self.ema_error_penalty_ms.load(Ordering::Relaxed);
        let raw_cost = success_latency.saturating_add(error_penalty);

        // Prevent costs from dropping too low and creating feedback loops
        raw_cost.max(1000) // Minimum 1000ms cost
    }
}

/// Represents the state and configuration of a single upstream JSON-RPC endpoint.
#[derive(Debug, Clone)]
pub struct RpcEndpoint {
    /// The URL of the RPC endpoint.
    pub url: String,
    /// Whether the endpoint is currently considered healthy based on the last health check.
    pub healthy: bool,
    /// The timestamp of the last health check.
    pub last_check: Instant,
    /// If the endpoint is in cooldown, this holds the time until it is available again.
    pub cooldown_until: Option<Instant>,
    /// The number of times this endpoint has been put into cooldown. Used for exponential backoff.
    pub cooldown_attempts: u32,
    /// The configured rate limit in requests per second.
    pub rate_limit_per_sec: u32,
    /// The configured burst size for the rate limiter.
    pub burst_size: u32,
    /// The manual priority weight assigned to this endpoint. Higher is better.
    pub weight: u32,
    /// A shared, atomically updatable struct for tracking performance metrics.
    pub metrics: Arc<EndpointMetrics>,
}

impl RpcEndpoint {
    /// Returns `true` if the endpoint is currently in a cooldown period.
    pub fn is_in_cooldown(&self) -> bool {
        if let Some(until) = self.cooldown_until {
            Instant::now() < until
        } else {
            false
        }
    }

    /// Returns the number of seconds remaining in the cooldown period.
    /// Returns 0 if the endpoint is not in cooldown.
    pub fn cooldown_remaining_secs(&self) -> i64 {
        match self.cooldown_until {
            Some(until) => {
                let now = Instant::now();
                if until > now {
                    (until.duration_since(now).as_secs()) as i64
                } else {
                    0
                }
            }
            None => 0,
        }
    }

    /// Returns `true` if the endpoint is healthy and not in cooldown.
    /// This is the primary check to determine if an endpoint can be used.
    pub fn is_available(&self) -> bool {
        self.healthy && !self.is_in_cooldown()
    }
}
