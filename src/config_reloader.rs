//! This module handles the logic for hot-reloading the configuration from disk.
//!
//! It detects changes in the configuration file, updates the load balancer's state
//! accordingly, and manages the addition, removal, or modification of endpoints
//! without requiring a full application restart.

use crate::{
    balancer::LoadBalancer,
    config::try_load_config,
    endpoint::{EndpointMetrics, LoadBalancerError, RpcEndpoint},
    metrics::{
        COOLDOWNS_TRIGGERED, COOLDOWN_SECONDS_GAUGE, ENDPOINT_RATE_LIMIT_DEFERRED,
        HEALTHCHECK_FAILED, HEALTHY_ENDPOINTS, REQUEST_LATENCY_PER_ENDPOINT, RPC_REQUESTS_FAILED,
        RPC_REQUESTS_SUCCEEDED, TOTAL_ENDPOINTS,
    },
};
use parking_lot::Mutex;
use ratelimit_meter::DirectRateLimiter;
use reqwest::Client;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    sync::{atomic::AtomicU64, Arc},
    time::{Duration, Instant},
};
use tracing::info;

#[derive(Serialize)]
pub struct ReloadResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub changes: ConfigChanges,
}

#[derive(Serialize, Default)]
pub struct ConfigChanges {
    pub endpoints_added: Vec<String>,
    pub endpoints_removed: Vec<String>,
    pub endpoints_modified: Vec<String>,
    pub config_updated: bool,
    pub client_rebuilt: bool,
}

/// Helper to remove all Prometheus metrics associated with an endpoint URL.
fn cleanup_endpoint_metrics(url: &str) {
    let labels = &[url];
    let _ = COOLDOWN_SECONDS_GAUGE.remove_label_values(labels);
    let _ = COOLDOWNS_TRIGGERED.remove_label_values(labels);
    let _ = ENDPOINT_RATE_LIMIT_DEFERRED.remove_label_values(labels);
    let _ = RPC_REQUESTS_SUCCEEDED.remove_label_values(labels);
    let _ = RPC_REQUESTS_FAILED.remove_label_values(labels);
    let _ = HEALTHCHECK_FAILED.remove_label_values(labels);
    let _ = REQUEST_LATENCY_PER_ENDPOINT.remove_label_values(labels);
    info!(url = %url, "Cleaned up metrics for removed endpoint");
}

/// Helper to initialize all Prometheus metrics for a new endpoint URL.
fn initialize_endpoint_metrics(url: &str) {
    let labels = &[url];
    COOLDOWN_SECONDS_GAUGE.with_label_values(labels).set(0);
    COOLDOWNS_TRIGGERED.with_label_values(labels).inc_by(0);
    ENDPOINT_RATE_LIMIT_DEFERRED.with_label_values(labels).inc_by(0);
    RPC_REQUESTS_SUCCEEDED.with_label_values(labels).inc_by(0);
    RPC_REQUESTS_FAILED.with_label_values(labels).inc_by(0);
    HEALTHCHECK_FAILED.with_label_values(labels).inc_by(0);
    info!(url = %url, "Initialized metrics for new endpoint");
}

/// Creates a new thread-safe rate limiter.
pub fn create_rate_limiter(
    burst_size: u32,
    rate_limit_per_sec: u32,
) -> Arc<Mutex<DirectRateLimiter>> {
    // This logic can panic if burst_size or rate_limit_per_sec are 0.
    // The config finalizer should prevent this by ensuring they are > 0.
    let capacity = NonZeroU32::new(burst_size).expect("Burst size from config must be > 0");
    let period_nanos = (burst_size as u64 * 1_000_000_000) / rate_limit_per_sec as u64;
    let period = Duration::from_nanos(period_nanos);
    Arc::new(Mutex::new(DirectRateLimiter::new(capacity, period)))
}

/// Reloads the configuration and applies changes to the load balancer state.
pub fn reload(balancer: &LoadBalancer) -> Result<ReloadResponse, LoadBalancerError> {
    let config_path = balancer.config_path.read().clone();
    info!(config_path = %config_path, "Starting config reload");

    // Load and finalize the new configuration. This handles all validation and defaults.
    let new_config_raw = try_load_config(&config_path)?.unwrap_or_default();
    let new_config = new_config_raw.finalize()?;

    // Destructure the config for cleaner access. `finalize` guarantees values are present.
    let new_balancer_cfg = new_config.balancer.unwrap();
    let crate::config::BalancerConfig {
        max_batch_size: Some(new_max_batch_size),
        base_cooldown_secs: Some(new_base_cooldown_secs),
        max_cooldown_secs: Some(new_max_cooldown_secs),
        health_check_interval_secs: Some(new_health_check_interval_secs),
        health_check_timeout_secs: Some(new_health_check_timeout_secs),
        latency_smoothing_factor: Some(new_latency_smoothing_factor),
        connect_timeout_ms: Some(new_connect_timeout_ms),
        timeout_secs: Some(new_timeout_secs),
        pool_idle_timeout_secs: Some(new_pool_idle_timeout_secs),
        pool_max_idle_per_host: Some(new_pool_max_idle_per_host),
        endpoints: Some(new_endpoints_list),
        ..
    } = new_balancer_cfg
    else {
        // This case should be unreachable due to `finalize()`
        return Err(LoadBalancerError::ConfigError(
            "Finalized config is missing required values.".to_string(),
        ));
    };

    let client_settings_changed = {
        *balancer.connect_timeout_ms.read() != new_connect_timeout_ms
            || *balancer.timeout_secs.read() != new_timeout_secs
            || *balancer.pool_idle_timeout_secs.read() != new_pool_idle_timeout_secs
            || *balancer.pool_max_idle_per_host.read() != new_pool_max_idle_per_host
    };

    if client_settings_changed {
        info!("HTTP client settings have changed, rebuilding client.");
        let new_client = Client::builder()
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_millis(new_connect_timeout_ms))
            .timeout(Duration::from_secs(new_timeout_secs))
            .pool_idle_timeout(Some(Duration::from_secs(new_pool_idle_timeout_secs)))
            .pool_max_idle_per_host(new_pool_max_idle_per_host)
            .http1_title_case_headers()
            .build()
            .expect("Failed to build new HTTP client");

        *balancer.client.write() = new_client;
        *balancer.connect_timeout_ms.write() = new_connect_timeout_ms;
        *balancer.timeout_secs.write() = new_timeout_secs;
        *balancer.pool_idle_timeout_secs.write() = new_pool_idle_timeout_secs;
        *balancer.pool_max_idle_per_host.write() = new_pool_max_idle_per_host;
    }

    let current_endpoints = balancer.endpoints.read();
    let current_urls: HashSet<String> = current_endpoints.iter().map(|e| e.url.clone()).collect();
    let new_urls: HashSet<String> = new_endpoints_list.iter().map(|e| e.url.clone()).collect();

    let endpoints_added: Vec<String> = new_urls.difference(&current_urls).cloned().collect();
    let endpoints_removed: Vec<String> = current_urls.difference(&new_urls).cloned().collect();

    let mut endpoints_modified = Vec::new();
    for new_ep_cfg in &new_endpoints_list {
        if let Some(current_ep) = current_endpoints.iter().find(|e| e.url == new_ep_cfg.url) {
            if current_ep.rate_limit_per_sec != new_ep_cfg.rate_limit_per_sec
                || current_ep.burst_size != new_ep_cfg.burst_size
                || current_ep.weight != new_ep_cfg.weight.unwrap()
            {
                endpoints_modified.push(new_ep_cfg.url.clone());
            }
        }
    }

    drop(current_endpoints);

    for removed_url in &endpoints_removed {
        cleanup_endpoint_metrics(removed_url);
    }

    let mut new_rpc_endpoints = Vec::new();
    let mut new_rate_limiters = HashMap::new();

    {
        let current_endpoints = balancer.endpoints.read();
        let current_limiters = balancer.rate_limiters.read();

        for new_ep_cfg in &new_endpoints_list {
            let new_weight = new_ep_cfg.weight.unwrap();
            let new_rate_limit = new_ep_cfg.rate_limit_per_sec;
            let new_burst = new_ep_cfg.burst_size;

            if let Some(existing_ep) = current_endpoints.iter().find(|e| e.url == new_ep_cfg.url) {
                // Preserve state of existing endpoint, including metrics
                let mut updated_ep = existing_ep.clone();
                updated_ep.rate_limit_per_sec = new_rate_limit;
                updated_ep.burst_size = new_burst;
                updated_ep.weight = new_weight;
                new_rpc_endpoints.push(updated_ep);

                // Update rate limiter only if changed
                if existing_ep.rate_limit_per_sec != new_rate_limit
                    || existing_ep.burst_size != new_burst
                {
                    new_rate_limiters.insert(
                        new_ep_cfg.url.clone(),
                        create_rate_limiter(new_burst, new_rate_limit),
                    );
                } else if let Some(existing_limiter) = current_limiters.get(&new_ep_cfg.url) {
                    new_rate_limiters.insert(new_ep_cfg.url.clone(), existing_limiter.clone());
                }
            } else {
                // This is a new endpoint
                let new_ep = RpcEndpoint {
                    url: new_ep_cfg.url.clone(),
                    healthy: true,
                    last_check: Instant::now(),
                    cooldown_until: None,
                    cooldown_attempts: 0,
                    rate_limit_per_sec: new_rate_limit,
                    burst_size: new_burst,
                    weight: new_weight,
                    metrics: Arc::new(EndpointMetrics {
                        ema_latency_ms: AtomicU64::new(*balancer.timeout_secs.read() * 1000),
                        ..Default::default()
                    }),
                };
                new_rpc_endpoints.push(new_ep);

                new_rate_limiters
                    .insert(new_ep_cfg.url.clone(), create_rate_limiter(new_burst, new_rate_limit));

                initialize_endpoint_metrics(&new_ep_cfg.url);
            }
        }
    }

    // Atomically update the load balancer state
    *balancer.endpoints.write() = new_rpc_endpoints;
    *balancer.rate_limiters.write() = new_rate_limiters;
    *balancer.max_batch_size.write() = new_max_batch_size;
    *balancer.base_cooldown_secs.write() = new_base_cooldown_secs;
    *balancer.max_cooldown_secs.write() = new_max_cooldown_secs;
    *balancer.health_check_interval_secs.write() = new_health_check_interval_secs;
    *balancer.health_check_timeout_secs.write() = new_health_check_timeout_secs;
    *balancer.latency_smoothing_factor.write() = new_latency_smoothing_factor;

    TOTAL_ENDPOINTS.set(new_endpoints_list.len() as i64);

    let healthy_count = {
        let endpoints = balancer.endpoints.read();
        endpoints.iter().filter(|e| e.is_available()).count() as i64
    };
    HEALTHY_ENDPOINTS.set(healthy_count);

    let changes = ConfigChanges {
        endpoints_added: endpoints_added.clone(),
        endpoints_removed: endpoints_removed.clone(),
        endpoints_modified: endpoints_modified.clone(),
        config_updated: true,
        client_rebuilt: client_settings_changed,
    };

    info!(
        added = endpoints_added.len(),
        removed = endpoints_removed.len(),
        modified = endpoints_modified.len(),
        total_endpoints = new_endpoints_list.len(),
        client_rebuilt = client_settings_changed,
        "Configuration reload complete."
    );

    Ok(ReloadResponse {
        success: true,
        message: "Configuration reloaded successfully".to_string(),
        error: None,
        changes,
    })
}
