//! This module contains the logic for the background health checking task.
//!
//! It runs in a loop, and on each tick, it assesses the health of all
//! configured endpoints by sending a simple JSON-RPC request (`eth_blockNumber`).
//! Unhealthy nodes are marked as such and put into a cooldown period, taking
//! them out of the rotation until they recover.

use crate::balancer::LoadBalancer;
use crate::cooldown;
use crate::endpoint::{ErrorKind, RequestOutcome};
use crate::metrics::{HEALTHCHECK_FAILED, HEALTH_CHECK_LATENCY};
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::{
    sync::watch,
    task::JoinSet,
    time::{interval, MissedTickBehavior},
};
use tracing::{info, warn};

/// The main background loop for performing periodic health checks.
///
/// This function continuously ticks based on the `health_check_interval_secs`
/// setting. It also listens for a shutdown signal to exit gracefully.
pub async fn health_check_loop(balancer: Arc<LoadBalancer>, mut shutdown_rx: watch::Receiver<()>) {
    let mut current_interval = *balancer.health_check_interval_secs.read();
    let mut ticker = interval(Duration::from_secs(current_interval));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased; // Prioritize the shutdown signal
            _ = shutdown_rx.changed() => {
                info!("Health checker received shutdown signal, exiting.");
                return;
            }
            _ = ticker.tick() => {
                let new_interval = *balancer.health_check_interval_secs.read();
                if new_interval != current_interval {
                    current_interval = new_interval;
                    ticker = interval(Duration::from_secs(current_interval));
                    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
                    ticker.reset();
                    info!(
                        new_interval = current_interval,
                        "Health check interval updated dynamically"
                    );
                }
                perform_health_checks(&balancer).await;
            }
        }
    }
}

/// Executes a round of health checks against all configured endpoints.
///
/// This function fans out concurrent health check requests (`eth_blockNumber`)
/// to all endpoints. Based on the results, it updates the health status of each
/// endpoint. If an endpoint transitions from healthy to unhealthy, its metrics
/// are penalized and it is put into a cooldown period. If it recovers, its
/// failure counters are reset and penalties are decayed.
async fn perform_health_checks(balancer: &Arc<LoadBalancer>) {
    let urls: Vec<String> = {
        let endpoints = balancer.endpoints.read();
        endpoints.iter().map(|ep| ep.url.clone()).collect()
    };

    let timeout_duration = Duration::from_secs(*balancer.health_check_timeout_secs.read());
    const HEALTH_CHECK_BODY: &str =
        r#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":"health_check"}"#;

    let mut set = JoinSet::new();
    for url in urls {
        let client = balancer.client.read().clone();
        set.spawn(async move {
            let start = std::time::Instant::now();
            let res = tokio::time::timeout(
                timeout_duration,
                client
                    .post(&url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(HEALTH_CHECK_BODY)
                    .send(),
            )
            .await;

            let duration = start.elapsed().as_secs_f64();
            HEALTH_CHECK_LATENCY.with_label_values(&[&url]).observe(duration);

            let success = match res {
                Ok(Ok(response)) => response.status().is_success(),
                _ => false,
            };
            (url, success)
        });
    }

    let mut updates = Vec::with_capacity(set.len());
    while let Some(res) = set.join_next().await {
        if let Ok(update) = res {
            updates.push(update);
        }
    }

    {
        let mut endpoints = balancer.endpoints.write();
        let base_cooldown = *balancer.base_cooldown_secs.read();
        let max_cooldown = *balancer.max_cooldown_secs.read();

        for (url, is_healthy) in updates {
            if let Some(ep) = endpoints.iter_mut().find(|ep| ep.url == url) {
                let was_healthy = ep.healthy;
                ep.healthy = is_healthy;
                ep.last_check = std::time::Instant::now();

                if is_healthy {
                    if !was_healthy {
                        info!(
                            url = %url,
                            "Endpoint recovered from health check failure."
                        );
                        // Reset consecutive failures and decay penalty on recovery.
                        ep.metrics.consecutive_failures.store(0, Ordering::Relaxed);
                        // Fast decay on the error penalty.
                        ep.metrics.update_ema(&ep.metrics.ema_error_penalty_ms, 0, 0.5);
                        ep.cooldown_attempts = 0;
                        ep.cooldown_until = None;
                    }
                } else {
                    HEALTHCHECK_FAILED.with_label_values(&[&url]).inc();
                    // A failed health check is a failure. Update metrics with a penalty.
                    let penalty_latency = timeout_duration.as_millis() as u64;
                    let outcome = RequestOutcome::Failure {
                        error_kind: ErrorKind::Timeout,
                        latency_ms: penalty_latency,
                    };
                    let smoothing_factor = *balancer.latency_smoothing_factor.read();
                    ep.metrics.update(&outcome, smoothing_factor);

                    if was_healthy {
                        warn!(
                            url = %url,
                            "Endpoint failed health check, triggering cooldown."
                        );
                        cooldown::trigger_cooldown(ep, base_cooldown, max_cooldown);
                    }
                }
            }
        }
    }

    balancer.update_healthy_count().await;
}
