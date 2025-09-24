//! This module contains the endpoint selection strategies for the load balancer.
//!
//! The primary goal is to decouple the logic of *how* an endpoint is chosen
//! from the main `balancer` module, which is responsible for managing state.
//! This makes the selection algorithm swappable and easier to test in isolation.

use parking_lot::{Mutex, RwLockReadGuard};
use ratelimit_meter::DirectRateLimiter;
use std::{cmp::Ordering, collections::HashMap, num::NonZeroU32, sync::Arc};
use tracing::info;

use crate::{
    endpoint::{LoadBalancerError, RpcEndpoint},
    metrics::{
        ALL_ENDPOINTS_RATE_LIMITED, ENDPOINT_RATE_LIMIT_DEFERRED, PRIORITY_ENDPOINT_SELECTED,
    },
};

// --- Tier Weight Definitions ---
const TIER1_MIN_WEIGHT: u32 = 100;
const TIER2_MIN_WEIGHT: u32 = 50;
const TIER3_MIN_WEIGHT: u32 = 1;

/// Selects the best available endpoint based on a tiered-priority strategy.
///
/// The selection process is strictly ordered by tiers, with fallback:
/// 1.  **Tier 1 (Local, weight >= 100):** Tries to find an available endpoint first.
///     - Sorted by `weight` (desc), then `last_selected` (desc).
///     - If all Tier 1 endpoints are unavailable or rate-limited, it falls back to Tier 2.
///
/// 2.  **Tier 2 (Premium, weight 50-99):** Tries if no suitable Tier 1 endpoint is found.
///     - Sorted by `weight` (desc), then `total_cost` (asc), then `last_selected` (desc).
///     - Falls back to Tier 3 if no suitable endpoint is found.
///
/// 3.  **Tier 3 (Free, weight 1-49):** The final fallback tier.
///     - Sorted by `weight` (desc), then `last_selected` (desc).
pub fn select_best_endpoint<'a>(
    endpoints: &'a RwLockReadGuard<Vec<RpcEndpoint>>,
    rate_limiters: &RwLockReadGuard<HashMap<String, Arc<Mutex<DirectRateLimiter>>>>,
    batch_size: usize,
) -> Option<&'a RpcEndpoint> {
    let available_endpoints: Vec<&RpcEndpoint> =
        endpoints.iter().filter(|ep| ep.is_available() && ep.weight > 0).collect();

    if available_endpoints.is_empty() {
        return None;
    }

    // Collect endpoints by tier
    let mut tier1 = Vec::new();
    let mut tier2 = Vec::new();
    let mut tier3 = Vec::new();

    for ep in available_endpoints {
        match ep.weight {
            w if w >= TIER1_MIN_WEIGHT => tier1.push(ep),
            w if w >= TIER2_MIN_WEIGHT => tier2.push(ep),
            w if w >= TIER3_MIN_WEIGHT => tier3.push(ep),
            _ => {} // weight == 0, already filtered out
        }
    }

    let tiers = [&tier1, &tier2, &tier3];
    let sort_strategies = [false, true, false]; // Whether to use cost for each tier

    for (tier_index, (candidates, &use_cost)) in
        tiers.iter().zip(sort_strategies.iter()).enumerate()
    {
        if candidates.is_empty() {
            continue;
        }

        let tier = (tier_index + 1) as u8;
        info!("Attempting selection from Tier {} ({} candidates)", tier, candidates.len());

        let mut sorted_candidates = (*candidates).clone();
        sorted_candidates.sort_by(|&a, &b| {
            b.weight
                .cmp(&a.weight)
                .then_with(|| {
                    if use_cost {
                        a.metrics.get_total_cost().cmp(&b.metrics.get_total_cost())
                    } else {
                        Ordering::Equal
                    }
                })
                .then_with(|| {
                    let time_a = a.metrics.last_selected.load(std::sync::atomic::Ordering::Relaxed);
                    let time_b = b.metrics.last_selected.load(std::sync::atomic::Ordering::Relaxed);
                    time_b.cmp(&time_a) // Descending order = prefer larger timestamps (MRU)
                })
        });

        for (rank, &endpoint) in sorted_candidates.iter().enumerate() {
            if reserve_tokens(rate_limiters, &endpoint.url, batch_size).is_ok() {
                log_selection(endpoint, tier, rank);
                return Some(endpoint);
            }
            log_rate_limited(endpoint, tier, rank);
        }
    }

    ALL_ENDPOINTS_RATE_LIMITED.inc();
    None
}

/// A helper function to check and reserve tokens from the rate limiter for a given endpoint.
fn reserve_tokens(
    limiters: &RwLockReadGuard<HashMap<String, Arc<Mutex<DirectRateLimiter>>>>,
    endpoint_url: &str,
    batch_size: usize,
) -> Result<(), LoadBalancerError> {
    if batch_size > (u32::MAX as usize) {
        return Err(LoadBalancerError::UpstreamError("Batch size too large".to_string()));
    }

    if let Some(limiter) = limiters.get(endpoint_url) {
        if batch_size == 0 {
            return Ok(());
        }

        if let Some(n) = NonZeroU32::new(batch_size as u32) {
            if limiter.lock().check_n(n.get()).is_err() {
                return Err(LoadBalancerError::RateLimited(endpoint_url.to_string()));
            }
        }
    }
    Ok(())
}

fn log_selection(endpoint: &RpcEndpoint, tier: u8, rank: usize) {
    let priority_rank = (rank + 1).to_string();
    PRIORITY_ENDPOINT_SELECTED.with_label_values(&[&endpoint.url, &priority_rank]).inc();
    info!(
        endpoint = %endpoint.url,
        tier = tier,
        priority_rank = rank + 1,
        "Selected endpoint"
    );
}

fn log_rate_limited(endpoint: &RpcEndpoint, tier: u8, rank: usize) {
    ENDPOINT_RATE_LIMIT_DEFERRED.with_label_values(&[&endpoint.url]).inc();
    info!(
        endpoint = %endpoint.url,
        tier = tier,
        priority_rank = rank + 1,
        "Endpoint is rate-limited, trying next"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{EndpointMetrics, RpcEndpoint};
    use parking_lot::RwLock;
    use ratelimit_meter::DirectRateLimiter;
    use std::sync::atomic::{AtomicU32, AtomicU64};
    use std::time::{Duration, Instant};

    // Helper to create a test endpoint
    fn create_test_endpoint(
        url: &str,
        weight: u32,
        cost: u64,
        healthy: bool,
        cooldown_secs: Option<u64>,
        last_selected: u64,
    ) -> RpcEndpoint {
        RpcEndpoint {
            url: url.to_string(),
            weight,
            metrics: Arc::new(EndpointMetrics {
                ema_latency_ms: AtomicU64::new(cost),
                ema_error_penalty_ms: AtomicU64::new(0),
                consecutive_failures: AtomicU32::new(0),
                last_selected: AtomicU64::new(last_selected),
            }),
            healthy,
            cooldown_until: cooldown_secs.map(|secs| Instant::now() + Duration::from_secs(secs)),
            last_check: Instant::now(),
            cooldown_attempts: 0,
            rate_limit_per_sec: 100,
            burst_size: 100,
        }
    }

    // Helper to create a rate limiter
    fn create_rate_limiter(burst: u32) -> Arc<Mutex<DirectRateLimiter>> {
        let capacity = NonZeroU32::new(burst).unwrap();
        Arc::new(Mutex::new(DirectRateLimiter::new(capacity, Duration::from_secs(1))))
    }

    fn run_selection<'a>(
        endpoints_lock: &'a RwLockReadGuard<Vec<RpcEndpoint>>,
        limiters_lock: &RwLockReadGuard<HashMap<String, Arc<Mutex<DirectRateLimiter>>>>,
        batch_size: usize,
    ) -> Option<&'a RpcEndpoint> {
        select_best_endpoint(endpoints_lock, limiters_lock, batch_size)
    }

    // --- Tiered Logic Tests ---

    #[test]
    fn test_tier1_is_always_prioritized() {
        let endpoints = vec![
            create_test_endpoint("tier3.com", 10, 10, true, None, 0),
            create_test_endpoint("tier2.com", 60, 20, true, None, 0),
            create_test_endpoint("tier1.com", 100, 500, true, None, 0), // High cost, but Tier 1
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(selected.url, "tier1.com");
    }

    #[test]
    fn test_tier1_sorts_by_mru_on_equal_weight() {
        let endpoints = vec![
            create_test_endpoint("tier1-b.com", 110, 100, true, None, 20), // Used more recently
            create_test_endpoint("tier1-a.com", 110, 100, true, None, 10), // Used least recently
            create_test_endpoint("tier1-c.com", 100, 100, true, None, 5),  // Lower weight
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(selected.url, "tier1-b.com");
    }

    #[test]
    fn test_tier1_rate_limited_falls_back_to_tier2() {
        let endpoints = vec![
            create_test_endpoint("tier1-rate-limited.com", 100, 50, true, None, 0),
            create_test_endpoint("tier2-available.com", 60, 20, true, None, 0),
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();

        let mut limiters = HashMap::new();
        let limiter1 = create_rate_limiter(1);
        limiter1.lock().check_n(1).unwrap(); // Use up all tokens
        limiters.insert("tier1-rate-limited.com".to_string(), limiter1);
        limiters.insert("tier2-available.com".to_string(), create_rate_limiter(1));

        let limiters_rwlock = RwLock::new(limiters);
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(selected.url, "tier2-available.com");
    }

    #[test]
    fn test_tier1_unavailable_falls_back_to_tier2() {
        let endpoints = vec![
            create_test_endpoint("tier1-unhealthy.com", 100, 50, false, None, 0),
            create_test_endpoint("tier2-best.com", 60, 20, true, None, 0),
            create_test_endpoint("tier2-worst.com", 55, 80, true, None, 0),
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(selected.url, "tier2-best.com");
    }

    #[test]
    fn test_tier2_sorts_by_weight_then_cost() {
        let endpoints = vec![
            create_test_endpoint("tier2-a.com", 60, 1200, true, None, 1),
            create_test_endpoint("tier2-b.com", 70, 1500, true, None, 2), // Higher weight, worse cost
            create_test_endpoint("tier2-c.com", 70, 1100, true, None, 3), // Higher weight, better cost
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(selected.url, "tier2-c.com");
    }

    #[test]
    fn test_tier2_sorts_by_cost_then_mru() {
        let endpoints = vec![
            create_test_endpoint("tier2-a.com", 70, 100, true, None, 5), // Same weight/cost, used last
            create_test_endpoint("tier2-b.com", 70, 100, true, None, 2), // Same weight/cost, used first
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(selected.url, "tier2-a.com");
    }

    #[test]
    fn test_tier2_full_tie_break_is_stable() {
        let endpoints = vec![
            create_test_endpoint("tier2-c.com", 70, 100, true, None, 5),
            create_test_endpoint("tier2-a.com", 80, 50, true, None, 1), // Identical to B
            create_test_endpoint("tier2-b.com", 80, 50, true, None, 1), // Identical to A
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        // Because Rust's sort is stable, "a" should be chosen because it appeared first in the vec
        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(selected.url, "tier2-a.com");
    }

    #[test]
    fn test_tier2_unavailable_falls_back_to_tier3() {
        let endpoints = vec![
            create_test_endpoint("tier2-unhealthy.com", 60, 20, false, None, 0),
            create_test_endpoint("tier3-best.com", 20, 100, true, None, 0),
            create_test_endpoint("tier3-worst.com", 10, 50, true, None, 0),
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(selected.url, "tier3-best.com");
    }

    #[test]
    fn test_tier3_sorts_by_weight_then_mru_and_ignores_cost() {
        let endpoints = vec![
            create_test_endpoint("tier3-a.com", 20, 500, true, None, 5), // Better weight, worse cost, used last
            create_test_endpoint("tier3-b.com", 10, 50, true, None, 2),
            create_test_endpoint("tier3-c.com", 20, 100, true, None, 2), // Better weight, better cost, used first
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(selected.url, "tier3-a.com");
    }

    // --- Reintegrated V1 Tests (Adapted for Tiers) ---

    #[test]
    fn test_skip_cooldown_endpoint() {
        let endpoints = vec![
            create_test_endpoint("tier2-cooldown.com", 70, 50, true, Some(60), 0), // in cooldown
            create_test_endpoint("tier2-ok.com", 60, 100, true, None, 0), // next best in tier
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(selected.url, "tier2-ok.com");
    }

    #[test]
    fn test_batch_selects_highest_priority_when_available() {
        let endpoints = vec![
            create_test_endpoint("tier2-a.com", 60, 100, true, None, 0),
            create_test_endpoint("tier2-b.com", 70, 100, true, None, 0), // highest weight
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();

        let mut limiters = HashMap::new();
        limiters.insert("tier2-a.com".to_string(), create_rate_limiter(20));
        limiters.insert("tier2-b.com".to_string(), create_rate_limiter(20));
        let limiters_rwlock = RwLock::new(limiters);
        let limiters_lock = limiters_rwlock.read();

        // Batch of 10 should select tier2-b, as it has higher weight and enough capacity
        let selected = run_selection(&endpoints_lock, &limiters_lock, 10).unwrap();
        assert_eq!(selected.url, "tier2-b.com");
    }

    #[test]
    fn test_select_endpoint_with_no_rate_limiter_entry() {
        let endpoints = vec![create_test_endpoint("no-limiter.com", 100, 50, true, None, 0)];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new()); // Limiters map is empty
        let limiters_lock = limiters_rwlock.read();

        // Should be selected as it's considered to have no rate limit
        let selected = run_selection(&endpoints_lock, &limiters_lock, 100).unwrap();
        assert_eq!(selected.url, "no-limiter.com");
    }

    // --- Edge Case Tests ---

    #[test]
    fn test_no_available_endpoints() {
        let endpoints = vec![
            create_test_endpoint("unhealthy.com", 100, 50, false, None, 0),
            create_test_endpoint("cooldown.com", 50, 100, true, Some(60), 0),
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1);
        assert!(selected.is_none());
    }

    #[test]
    fn test_all_endpoints_rate_limited() {
        let endpoints = vec![
            create_test_endpoint("tier1.com", 100, 50, true, None, 0),
            create_test_endpoint("tier2.com", 50, 100, true, None, 0),
        ];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();

        let mut limiters = HashMap::new();
        let limiter1 = create_rate_limiter(5);
        limiter1.lock().check_n(5).unwrap(); // Use up all tokens
        limiters.insert("tier1.com".to_string(), limiter1);
        let limiter2 = create_rate_limiter(5);
        limiter2.lock().check_n(5).unwrap(); // Use up all tokens
        limiters.insert("tier2.com".to_string(), limiter2);

        let limiters_rwlock = RwLock::new(limiters);
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1);
        assert!(selected.is_none());
    }

    #[test]
    fn test_empty_endpoints_list() {
        let endpoints: Vec<RpcEndpoint> = vec![];
        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        let selected = run_selection(&endpoints_lock, &limiters_lock, 1);
        assert!(selected.is_none());
    }

    // --- Helper Function Tests ---

    #[test]
    fn test_reserve_tokens_zero_batch_size() {
        let mut limiters = HashMap::new();
        let limiter = create_rate_limiter(1);
        limiter.lock().check_n(1).unwrap(); // Exhaust the limiter
        limiters.insert("e1.com".to_string(), limiter);
        let limiters_rwlock = RwLock::new(limiters);
        let limiters_lock = limiters_rwlock.read();

        // A batch of zero should always be Ok, even if the limiter is full.
        let result = reserve_tokens(&limiters_lock, "e1.com", 0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_reserve_tokens_oversized_batch() {
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();
        let oversized_batch = (u32::MAX as usize) + 1;

        let result = reserve_tokens(&limiters_lock, "e1.com", oversized_batch);
        assert!(matches!(result, Err(LoadBalancerError::UpstreamError(_))));
    }

    #[test]
    fn test_reserve_tokens_rate_limited() {
        let mut limiters = HashMap::new();
        let limiter = create_rate_limiter(1);
        limiter.lock().check_n(1).unwrap(); // Exhaust the limiter
        limiters.insert("e1.com".to_string(), limiter);
        let limiters_rwlock = RwLock::new(limiters);
        let limiters_lock = limiters_rwlock.read();

        let result = reserve_tokens(&limiters_lock, "e1.com", 1);
        assert!(matches!(result, Err(LoadBalancerError::RateLimited(_))));
    }

    #[test]
    fn test_tier3_ignores_cost_even_with_different_weights() {
        // Test that cost is ignored in Tier 3, even when weights differ
        let endpoints = vec![
            create_test_endpoint("tier3-lowweight-lowcost.com", 10, 10, true, None, 5), // Low weight, low cost
            create_test_endpoint("tier3-highweight-highcost.com", 30, 1000, true, None, 10), // High weight, high cost
        ];

        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        // Should select higher weight despite much higher cost
        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(
            selected.url, "tier3-highweight-highcost.com",
            "Tier 3 should prioritize weight over cost"
        );
    }

    #[test]
    fn test_tier3_mixed_weights_still_prioritize_weight() {
        // Different weights within Tier 3 - weight should still dominate
        let endpoints = vec![
            create_test_endpoint("tier3-weight25.com", 25, 1000, true, None, 1), // Higher weight, recent, high cost
            create_test_endpoint("tier3-weight20.com", 20, 10, true, None, 100), // Lower weight, very old, low cost
        ];

        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();
        let limiters_rwlock = RwLock::new(HashMap::new());
        let limiters_lock = limiters_rwlock.read();

        // Higher weight should win despite worse cost and recency
        let selected = run_selection(&endpoints_lock, &limiters_lock, 1).unwrap();
        assert_eq!(
            selected.url, "tier3-weight25.com",
            "Tier 3 should prioritize higher weight even with worse cost/recency"
        );
    }

    #[test]
    fn test_debug_tier3_sorting() {
        let endpoints = vec![
            create_test_endpoint("recent.com", 20, 1000, true, None, 5), // timestamp 5
            create_test_endpoint("oldest.com", 20, 10, true, None, 20),  // timestamp 20 (oldest)
        ];

        let endpoints_rwlock = RwLock::new(endpoints);
        let endpoints_lock = endpoints_rwlock.read();

        // Manually check the sorting
        let mut tier3 = Vec::new();
        for ep in endpoints_lock
            .iter()
            .filter(|ep| ep.weight >= TIER3_MIN_WEIGHT && ep.weight < TIER2_MIN_WEIGHT)
        {
            tier3.push(ep);
        }

        println!("Before sorting:");
        for ep in &tier3 {
            println!(
                "{}: weight={}, last_selected={}",
                ep.url,
                ep.weight,
                ep.metrics.last_selected.load(std::sync::atomic::Ordering::Relaxed)
            );
        }

        let mut sorted = tier3.clone();
        sorted.sort_by(|&a, &b| {
            b.weight
                .cmp(&a.weight)
                .then_with(|| {
                    // use_cost = false for Tier 3, so this should return Ordering::Equal
                    // and proceed to LRU
                    Ordering::Equal
                })
                .then_with(|| {
                    let time_a = a.metrics.last_selected.load(std::sync::atomic::Ordering::Relaxed);
                    let time_b = b.metrics.last_selected.load(std::sync::atomic::Ordering::Relaxed);
                    time_b.cmp(&time_a) // Descending order = prefer older timestamps (MRU)
                })
        });

        println!("After sorting:");
        for ep in &sorted {
            println!(
                "{}: weight={}, last_selected={}",
                ep.url,
                ep.weight,
                ep.metrics.last_selected.load(std::sync::atomic::Ordering::Relaxed)
            );
        }

        assert_eq!(sorted[0].url, "oldest.com");
    }
}
