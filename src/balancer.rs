//! The core logic for the load balancer, including endpoint management,
//! selection, and state tracking.

use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use reqwest::Client;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tracing::{info, warn};

use crate::{
    config::Config,
    config_reloader::{self, ReloadResponse},
    cooldown,
    endpoint::{EndpointMetrics, LoadBalancerError, RpcEndpoint},
    forwarder, health,
    metrics::{
        COOLDOWNS_TRIGGERED, COOLDOWN_SECONDS_GAUGE, ENDPOINT_RATE_LIMIT_DEFERRED,
        HEALTHCHECK_FAILED, HEALTHY_ENDPOINTS, RPC_REQUESTS_FAILED, RPC_REQUESTS_SUCCEEDED,
        TOTAL_ENDPOINTS,
    },
    shutdown::ShutdownManager,
    strategy,
};

#[derive(Debug)]
pub struct LoadBalancer {
    pub endpoints: Arc<RwLock<Vec<RpcEndpoint>>>,
    pub rate_limiters: Arc<RwLock<HashMap<String, Arc<Mutex<ratelimit_meter::DirectRateLimiter>>>>>,
    pub client: Arc<RwLock<Client>>,
    pub bind_addr: String,
    pub health_check_interval_secs: Arc<RwLock<u64>>,
    pub health_check_timeout_secs: Arc<RwLock<u64>>,
    pub base_cooldown_secs: Arc<RwLock<u64>>,
    pub max_cooldown_secs: Arc<RwLock<u64>>,
    pub max_batch_size: Arc<RwLock<usize>>,
    pub latency_smoothing_factor: Arc<RwLock<f64>>,
    pub concurrency_limiter: Arc<tokio::sync::Semaphore>,
    pub config_path: Arc<RwLock<String>>,
    // Store current client settings to detect changes for hot-reloading
    pub connect_timeout_ms: Arc<RwLock<u64>>,
    pub timeout_secs: Arc<RwLock<u64>>,
    pub pool_idle_timeout_secs: Arc<RwLock<u64>>,
    pub pool_max_idle_per_host: Arc<RwLock<usize>>,
}

impl LoadBalancer {
    pub fn new(config: Option<Config>, config_path: String) -> Self {
        // Finalize the config first: applies defaults, validates, and sanitizes.
        let config = config
            .unwrap_or_default()
            .finalize()
            .expect("Configuration failed to finalize at startup");

        // Now, all values are guaranteed to be present and valid.
        let server_cfg = config.server.unwrap();
        let balancer_cfg = config.balancer.unwrap();

        let bind_addr = server_cfg.bind_addr.unwrap();
        let health_check_interval = balancer_cfg.health_check_interval_secs.unwrap();
        let health_check_timeout = balancer_cfg.health_check_timeout_secs.unwrap();
        let base_cooldown = balancer_cfg.base_cooldown_secs.unwrap();
        let max_cooldown = balancer_cfg.max_cooldown_secs.unwrap();
        let max_batch_size = balancer_cfg.max_batch_size.unwrap();
        let latency_smoothing_factor = balancer_cfg.latency_smoothing_factor.unwrap();
        let max_concurrency = balancer_cfg.max_concurrency.unwrap();
        let connect_timeout_ms = balancer_cfg.connect_timeout_ms.unwrap();
        let timeout_secs = balancer_cfg.timeout_secs.unwrap();
        let pool_idle_timeout_secs = balancer_cfg.pool_idle_timeout_secs.unwrap();
        let pool_max_idle_per_host = balancer_cfg.pool_max_idle_per_host.unwrap();

        let endpoints_list = balancer_cfg.endpoints.unwrap();

        let endpoints: Vec<RpcEndpoint> = endpoints_list
            .into_iter()
            .map(|e| RpcEndpoint {
                url: e.url.clone(),
                healthy: true,
                last_check: Instant::now(),
                cooldown_until: None,
                cooldown_attempts: 0,
                rate_limit_per_sec: e.rate_limit_per_sec,
                burst_size: e.burst_size,
                weight: e.weight.unwrap(),
                metrics: Arc::new(EndpointMetrics {
                    ema_latency_ms: AtomicU64::new(timeout_secs * 1000), // Use configured timeout
                    ..Default::default()
                }),
            })
            .collect();

        TOTAL_ENDPOINTS.set(endpoints.len() as i64);
        HEALTHY_ENDPOINTS.set(endpoints.len() as i64);

        for ep in &endpoints {
            COOLDOWN_SECONDS_GAUGE.with_label_values(&[&ep.url]).set(0);
            COOLDOWNS_TRIGGERED.with_label_values(&[&ep.url]).inc_by(0);
            ENDPOINT_RATE_LIMIT_DEFERRED.with_label_values(&[&ep.url]).inc_by(0);
            RPC_REQUESTS_SUCCEEDED.with_label_values(&[&ep.url]).inc_by(0);
            RPC_REQUESTS_FAILED.with_label_values(&[&ep.url]).inc_by(0);
            HEALTHCHECK_FAILED.with_label_values(&[&ep.url]).inc_by(0);
        }

        let rate_limiters = Arc::new(RwLock::new(HashMap::new()));
        for ep in &endpoints {
            rate_limiters.write().insert(
                ep.url.clone(),
                config_reloader::create_rate_limiter(ep.burst_size, ep.rate_limit_per_sec),
            );
        }

        let client = Client::builder()
            .tcp_nodelay(true)
            .connect_timeout(Duration::from_millis(connect_timeout_ms))
            .timeout(Duration::from_secs(timeout_secs))
            .pool_idle_timeout(Some(Duration::from_secs(pool_idle_timeout_secs)))
            .pool_max_idle_per_host(pool_max_idle_per_host)
            .http1_title_case_headers()
            .build()
            .expect("Failed to create HTTP client");

        info!(
            bind_addr = %bind_addr,
            health_check_interval = health_check_interval,
            health_check_timeout = health_check_timeout,
            base_cooldown = base_cooldown,
            max_cooldown = max_cooldown,
            max_batch_size = max_batch_size,
            max_concurrency = max_concurrency,
            latency_smoothing_factor = latency_smoothing_factor,
            connect_timeout_ms = connect_timeout_ms,
            timeout_secs = timeout_secs,
            pool_idle_timeout_secs = pool_idle_timeout_secs,
            pool_max_idle_per_host = pool_max_idle_per_host,
            endpoints_count = endpoints.len(),
            "LoadBalancer initialized"
        );

        Self {
            endpoints: Arc::new(RwLock::new(endpoints)),
            rate_limiters,
            client: Arc::new(RwLock::new(client)),
            bind_addr,
            health_check_interval_secs: Arc::new(RwLock::new(health_check_interval)),
            health_check_timeout_secs: Arc::new(RwLock::new(health_check_timeout)),
            base_cooldown_secs: Arc::new(RwLock::new(base_cooldown)),
            max_cooldown_secs: Arc::new(RwLock::new(max_cooldown)),
            max_batch_size: Arc::new(RwLock::new(max_batch_size)),
            latency_smoothing_factor: Arc::new(RwLock::new(latency_smoothing_factor)),
            concurrency_limiter: Arc::new(tokio::sync::Semaphore::new(max_concurrency)),
            config_path: Arc::new(RwLock::new(config_path)),
            connect_timeout_ms: Arc::new(RwLock::new(connect_timeout_ms)),
            timeout_secs: Arc::new(RwLock::new(timeout_secs)),
            pool_idle_timeout_secs: Arc::new(RwLock::new(pool_idle_timeout_secs)),
            pool_max_idle_per_host: Arc::new(RwLock::new(pool_max_idle_per_host)),
        }
    }

    pub fn reload_config(&self) -> Result<ReloadResponse, LoadBalancerError> {
        config_reloader::reload(self)
    }

    /// Selects the best available endpoint, updates its last_selected timestamp, and returns its URL.
    pub fn get_next_endpoint(&self, batch_size: usize) -> Option<String> {
        let endpoints = self.endpoints.read();
        let limiters = self.rate_limiters.read();

        if let Some(endpoint) = strategy::select_best_endpoint(&endpoints, &limiters, batch_size) {
            // Set the timestamp to now before returning the URL.
            endpoint.metrics.last_selected.store(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                std::sync::atomic::Ordering::Relaxed,
            );
            Some(endpoint.url.clone())
        } else {
            None
        }
    }

    /// A wrapper method that calls the main request forwarding logic.
    pub async fn forward_raw_request(
        &self,
        request_body: Bytes,
        endpoint_url: &str,
        method: &str,
    ) -> Result<Bytes, LoadBalancerError> {
        forwarder::forward_request(self, request_body, endpoint_url, method).await
    }

    pub async fn mark_rate_limited(&self, url: &str) {
        let mut endpoints = self.endpoints.write();
        if let Some(ep) = endpoints.iter_mut().find(|e| e.url == url) {
            cooldown::trigger_cooldown(
                ep,
                *self.base_cooldown_secs.read(),
                *self.max_cooldown_secs.read(),
            );
            warn!(
                url = %url,
                cooldown_secs = ep.cooldown_remaining_secs(),
                "Endpoint put into cooldown due to rate limiting"
            );
        }
    }

    pub async fn mark_unhealthy(&self, url: &str) {
        let mut endpoints = self.endpoints.write();
        if let Some(ep) = endpoints.iter_mut().find(|e| e.url == url) {
            if ep.healthy {
                ep.healthy = false;
                cooldown::trigger_cooldown(
                    ep,
                    *self.base_cooldown_secs.read(),
                    *self.max_cooldown_secs.read(),
                );
                warn!(
                    url = %url,
                    cooldown_secs = ep.cooldown_remaining_secs(),
                    "Marked endpoint as unhealthy and put into cooldown"
                );
            }
        }
    }

    pub async fn update_healthy_count(&self) {
        let endpoints = self.endpoints.read();
        let healthy_count = endpoints.iter().filter(|e| e.is_available()).count() as i64;
        HEALTHY_ENDPOINTS.set(healthy_count);
    }

    pub fn run_background_tasks(self: &Arc<Self>, shutdown_manager: &mut ShutdownManager) {
        let health_checker = self.clone();
        shutdown_manager
            .spawn_task(health::health_check_loop(health_checker, shutdown_manager.subscribe()));

        let cooldown_updater = self.clone();
        shutdown_manager.spawn_task(cooldown::cooldown_gauge_updater(
            cooldown_updater,
            shutdown_manager.subscribe(),
        ));
    }

    fn get_endpoint_priority_stats(&self) -> Value {
        let endpoints = self.endpoints.read();
        let mut stats: Vec<_> = endpoints
            .iter()
            .map(|ep| {
                serde_json::json!({
                    "url": ep.url,
                    "weight": ep.weight,
                    "healthy": ep.healthy,
                    "available": ep.is_available(),
                    "in_cooldown": ep.is_in_cooldown(),
                    "cooldown_remaining_secs": ep.cooldown_remaining_secs(),
                    "total_cost_ms": ep.metrics.get_total_cost(),
                    "last_selected_secs_ago": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        .saturating_sub(ep.metrics.last_selected.load(Ordering::Relaxed)),
                })
            })
            .collect();

        stats.sort_by(|a, b| {
            let weight_a = a["weight"].as_u64().unwrap_or(0);
            let weight_b = b["weight"].as_u64().unwrap_or(0);
            let cost_a = a["total_cost_ms"].as_u64().unwrap_or(u64::MAX);
            let cost_b = b["total_cost_ms"].as_u64().unwrap_or(u64::MAX);
            let time_a = a["last_selected_secs_ago"].as_u64().unwrap_or(0);
            let time_b = b["last_selected_secs_ago"].as_u64().unwrap_or(0);

            weight_b
                .cmp(&weight_a)
                .then_with(|| cost_a.cmp(&cost_b))
                .then_with(|| time_b.cmp(&time_a)) // Higher secs ago (older) is better
        });

        let ranked_stats: Vec<_> = stats
            .into_iter()
            .enumerate()
            .map(|(i, mut stat)| {
                stat["priority_rank"] = serde_json::json!(i + 1);
                stat
            })
            .collect();

        serde_json::json!({
            "strategy": "priority_based_with_lru_tie_breaking",
            "endpoints": ranked_stats
        })
    }

    pub fn get_status(&self) -> Value {
        let endpoints = self.endpoints.read();
        let available_endpoints: Vec<&RpcEndpoint> =
            endpoints.iter().filter(|ep| ep.is_available()).collect();

        let healthy_count = available_endpoints.len();
        let config_path = self.config_path.read().clone();

        serde_json::json!({
            "bind_addr": self.bind_addr,
            "config_path": config_path,
            "total_endpoints": endpoints.len(),
            "healthy_endpoints": healthy_count,
            "balancer_settings": {
                "health_check_interval_secs": *self.health_check_interval_secs.read(),
                "health_check_timeout_secs": *self.health_check_timeout_secs.read(),
                "base_cooldown_secs": *self.base_cooldown_secs.read(),
                "max_cooldown_secs": *self.max_cooldown_secs.read(),
                "max_batch_size": *self.max_batch_size.read(),
                "latency_smoothing_factor": *self.latency_smoothing_factor.read(),
            },
            "client_settings": {
                "connect_timeout_ms": *self.connect_timeout_ms.read(),
                "timeout_secs": *self.timeout_secs.read(),
                "pool_idle_timeout_secs": *self.pool_idle_timeout_secs.read(),
                "pool_max_idle_per_host": *self.pool_max_idle_per_host.read(),
            },
            "priority_info": self.get_endpoint_priority_stats(),
            "endpoints_details": endpoints.iter().map(|ep| {
                serde_json::json!({
                    "url": ep.url,
                    "healthy": ep.healthy,
                    "available": ep.is_available(),
                    "in_cooldown": ep.is_in_cooldown(),
                    "cooldown_remaining_secs": ep.cooldown_remaining_secs(),
                    "cooldown_attempts": ep.cooldown_attempts,
                    "consecutive_failures": ep.metrics.consecutive_failures.load(Ordering::Relaxed),
                    "ema_latency_ms": ep.metrics.ema_latency_ms.load(Ordering::Relaxed),
                    "ema_error_penalty_ms": ep.metrics.ema_error_penalty_ms.load(Ordering::Relaxed),
                    "total_cost_ms": ep.metrics.get_total_cost(),
                    "last_selected_timestamp": ep.metrics.last_selected.load(Ordering::Relaxed),
                    "last_check_secs_ago": ep.last_check.elapsed().as_secs(),
                    "rate_limit_per_sec": ep.rate_limit_per_sec,
                    "burst_size": ep.burst_size,
                    "weight": ep.weight
                })
            }).collect::<Vec<_>>()
        })
    }
}
