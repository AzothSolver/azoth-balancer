//! This module contains the logic for managing endpoint cooldowns.
//!
//! When an endpoint fails a health check or is rate-limited, it is put into
//! an exponential backoff cooldown to prevent it from being used while it is
//! unhealthy. This avoids hammering a failing node and gives it time to recover.

use crate::balancer::LoadBalancer;
use crate::endpoint::RpcEndpoint;
use crate::metrics::COOLDOWN_SECONDS_GAUGE;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::watch;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};

/// Puts an endpoint into an exponential backoff cooldown period.
///
/// This function is called when an endpoint is detected as unhealthy or rate-limited.
/// It calculates the next cooldown duration based on the number of consecutive cooldown
/// attempts and updates the endpoint's state. The actual failure counting is handled
/// by the module that detects the failure.
pub fn trigger_cooldown(ep: &mut RpcEndpoint, base_cooldown_secs: u64, max_cooldown_secs: u64) {
    ep.cooldown_attempts = ep.cooldown_attempts.saturating_add(1);

    // Calculate cooldown with exponential backoff: base * 2^(attempts)
    let exponential_cooldown = base_cooldown_secs.saturating_mul(2u64.pow(ep.cooldown_attempts));

    // Cap the cooldown at the configured maximum.
    let cooldown_duration = exponential_cooldown.min(max_cooldown_secs);

    ep.cooldown_until = Some(Instant::now() + Duration::from_secs(cooldown_duration));
    ep.last_check = Instant::now();

    warn!(
        url = %ep.url,
        cooldown_secs = cooldown_duration,
        attempts = ep.cooldown_attempts,
        "Endpoint has been put into cooldown."
    );

    COOLDOWN_SECONDS_GAUGE.with_label_values(&[&ep.url]).set(cooldown_duration as i64);
}

/// A background task that periodically updates the `cooldown_seconds` Prometheus gauge.
///
/// This function runs in a loop, and on each tick, it iterates through all
/// endpoints to update their remaining cooldown time in the metrics. It also listens
/// for a shutdown signal to exit gracefully.
pub async fn cooldown_gauge_updater(
    balancer: Arc<LoadBalancer>,
    mut shutdown_rx: watch::Receiver<()>,
) {
    let mut ticker = interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased; // Prioritize the shutdown signal
            _ = shutdown_rx.changed() => {
                info!("Cooldown gauge updater received shutdown signal, exiting.");
                return;
            }
            _ = ticker.tick() => {
                {
                    let endpoints = balancer.endpoints.read();
                    for ep in endpoints.iter() {
                        let remaining = ep.cooldown_remaining_secs();
                        COOLDOWN_SECONDS_GAUGE
                            .with_label_values(&[&ep.url])
                            .set(remaining);
                    }
                }
                // This task also conveniently updates the healthy count periodically.
                balancer.update_healthy_count().await;
            }
        }
    }
}
