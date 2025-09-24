use serde::Deserialize;
use std::{collections::HashSet, fs};
use thiserror::Error;
use tracing::{error, info, warn};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Configuration error: {0}")]
    ConfigError(String),
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Config {
    pub server: Option<ServerConfig>,
    pub balancer: Option<BalancerConfig>,
}

impl Config {
    /// Applies defaults, validates, and sanitizes the configuration.
    /// This ensures that the configuration is in a consistent and usable state
    /// by filling in missing values and clamping others to valid ranges.
    pub fn finalize(mut self) -> Result<Self, ConfigError> {
        let mut server_cfg = self.server.take().unwrap_or_default();
        server_cfg.bind_addr = server_cfg.bind_addr.or_else(|| Some(DEFAULT_BIND_ADDR.to_string()));
        self.server = Some(server_cfg);

        let mut balancer_cfg = self.balancer.take().unwrap_or_default();

        balancer_cfg.health_check_interval_secs =
            balancer_cfg.health_check_interval_secs.or(Some(DEFAULT_HEALTH_CHECK_INTERVAL_SECS));
        balancer_cfg.health_check_timeout_secs =
            balancer_cfg.health_check_timeout_secs.or(Some(DEFAULT_HEALTH_CHECK_TIMEOUT_SECS));
        balancer_cfg.base_cooldown_secs =
            balancer_cfg.base_cooldown_secs.or(Some(DEFAULT_BASE_COOLDOWN_SECS));
        balancer_cfg.max_cooldown_secs =
            balancer_cfg.max_cooldown_secs.or(Some(DEFAULT_MAX_COOLDOWN_SECS));

        balancer_cfg.latency_smoothing_factor = Some(
            balancer_cfg
                .latency_smoothing_factor
                .unwrap_or(DEFAULT_LATENCY_SMOOTHING_FACTOR)
                .clamp(0.0, 1.0),
        );

        balancer_cfg.max_batch_size =
            Some(balancer_cfg.max_batch_size.unwrap_or(DEFAULT_MAX_BATCH_SIZE).max(1));
        balancer_cfg.max_concurrency =
            Some(balancer_cfg.max_concurrency.unwrap_or(DEFAULT_MAX_CONCURRENCY).max(1));

        // Client settings
        balancer_cfg.connect_timeout_ms =
            balancer_cfg.connect_timeout_ms.or(Some(DEFAULT_CONNECT_TIMEOUT_MS));
        balancer_cfg.timeout_secs = balancer_cfg.timeout_secs.or(Some(DEFAULT_TIMEOUT_SECS));
        balancer_cfg.pool_idle_timeout_secs =
            balancer_cfg.pool_idle_timeout_secs.or(Some(DEFAULT_POOL_IDLE_TIMEOUT_SECS));
        balancer_cfg.pool_max_idle_per_host =
            balancer_cfg.pool_max_idle_per_host.or(Some(DEFAULT_POOL_MAX_IDLE_PER_HOST));

        let mut endpoints = balancer_cfg.endpoints.take().unwrap_or_else(get_default_endpoints);
        endpoints = validate_and_dedupe_endpoints(endpoints)?;

        // Apply final constraints to endpoints after validation
        for ep in &mut endpoints {
            ep.rate_limit_per_sec = ep.rate_limit_per_sec.max(1);
            ep.burst_size = ep.burst_size.max(1);
            if ep.weight.is_none() {
                ep.weight = Some(DEFAULT_ENDPOINT_WEIGHT);
            }
        }

        balancer_cfg.endpoints = Some(endpoints);
        self.balancer = Some(balancer_cfg);

        Ok(self)
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ServerConfig {
    pub bind_addr: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct BalancerConfig {
    pub health_check_interval_secs: Option<u64>,
    pub health_check_timeout_secs: Option<u64>,
    pub base_cooldown_secs: Option<u64>,
    pub max_cooldown_secs: Option<u64>,
    pub latency_smoothing_factor: Option<f64>,
    pub endpoints: Option<Vec<EndpointConfig>>,
    pub max_batch_size: Option<usize>,
    pub max_concurrency: Option<usize>,
    // Client timeout and pool settings
    pub connect_timeout_ms: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub pool_idle_timeout_secs: Option<u64>,
    pub pool_max_idle_per_host: Option<usize>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct EndpointConfig {
    pub url: String,
    pub rate_limit_per_sec: u32,
    #[serde(default = "default_burst_size")]
    pub burst_size: u32,
    #[serde(default)]
    pub weight: Option<u32>,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            rate_limit_per_sec: DEFAULT_ENDPOINT_RATE_LIMIT,
            burst_size: DEFAULT_BURST_SIZE,
            weight: None,
        }
    }
}

// Updated defaults based on the user's provided configuration.
pub const DEFAULT_BURST_SIZE: u32 = 25;
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8549";
pub const DEFAULT_HEALTH_CHECK_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_HEALTH_CHECK_TIMEOUT_SECS: u64 = 5;
pub const DEFAULT_BASE_COOLDOWN_SECS: u64 = 3;
pub const DEFAULT_MAX_COOLDOWN_SECS: u64 = 60;
pub const DEFAULT_LATENCY_SMOOTHING_FACTOR: f64 = 0.1;
pub const DEFAULT_ENDPOINT_RATE_LIMIT: u32 = 20;
pub const DEFAULT_ENDPOINT_WEIGHT: u32 = 20;
pub const DEFAULT_MAX_BATCH_SIZE: usize = 100;

// Default client setting constants
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 500;
pub const DEFAULT_TIMEOUT_SECS: u64 = 5;
pub const DEFAULT_POOL_IDLE_TIMEOUT_SECS: u64 = 20;
pub const DEFAULT_POOL_MAX_IDLE_PER_HOST: usize = 50;
pub const DEFAULT_MAX_CONCURRENCY: usize = 50;

// A curated list of default endpoints based on user's high-quality providers.
pub const DEFAULT_ENDPOINTS: [&str; 4] = [
    "https://arbitrum-one.blastapi.io/0bbf5840-7702-4ae8-8d06-c081e6089381",
    "https://arbitrum.blockpi.network/v1/rpc/47d30766a0860ce4418ede1ab9b65b6fa8e63133",
    "https://rpc.ankr.com/arbitrum/432820e33bd905ed5a2f63eca52792fb302e1ce6c025bb591c6d2e588f52e3ee",
    "https://arbitrum.drpc.org/",
];

pub fn default_burst_size() -> u32 {
    DEFAULT_BURST_SIZE
}

pub fn try_load_config(path: &str) -> Result<Option<Config>, ConfigError> {
    match fs::read_to_string(path) {
        Ok(raw) => match toml::from_str::<Config>(&raw) {
            Ok(cfg) => {
                info!(path = %path, "Loaded config");
                Ok(Some(cfg))
            }
            Err(e) => {
                error!(path = %path, error = %e, "Failed to parse config");
                Err(ConfigError::ConfigError(e.to_string()))
            }
        },
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                info!(path = %path, "No config file found, using defaults");
                Ok(None)
            } else {
                Err(ConfigError::ConfigError(e.to_string()))
            }
        }
    }
}

pub fn validate_and_dedupe_endpoints(
    endpoints: Vec<EndpointConfig>,
) -> Result<Vec<EndpointConfig>, ConfigError> {
    let mut seen = HashSet::new();
    const MAX_URL_LEN: usize = 2048;
    const MAX_RATE_LIMIT: u32 = 100_000;
    const MAX_BURST_SIZE: u32 = 10_000;

    let validated_endpoints: Vec<EndpointConfig> = endpoints
        .into_iter()
        .filter_map(|mut e| {
            e.url = e.url.trim().to_string();

            if e.url.is_empty() {
                warn!("Skipping empty endpoint URL");
                return None;
            }

            if !e.url.to_lowercase().starts_with("http://") && !e.url.to_lowercase().starts_with("https://") {
                warn!(url = %e.url, "Skipping invalid endpoint URL");
                return None;
            }

            if e.url.len() > MAX_URL_LEN {
                warn!(url = %e.url, "Skipping endpoint exceeding max length");
                return None;
            }

            if e.url.chars().any(|c| c.is_control() || c.is_whitespace()) {
                warn!(url = %e.url, "Skipping endpoint with invalid characters");
                return None;
            }

            if e.rate_limit_per_sec == 0 || e.rate_limit_per_sec > MAX_RATE_LIMIT {
                warn!(url = %e.url, rate = e.rate_limit_per_sec, "Skipping endpoint with invalid rate_limit_per_sec");
                return None;
            }

            if e.burst_size == 0 || e.burst_size > MAX_BURST_SIZE {
                warn!(url = %e.url, burst = e.burst_size, "Skipping endpoint with invalid burst_size");
                return None;
            }

            // Canonicalize: lowercase scheme + remove trailing slash
            let mut canonical_url = e.url.clone();
            if canonical_url.to_lowercase().starts_with("http://") {
                canonical_url.replace_range(0..7, "http://");
            } else if canonical_url.to_lowercase().starts_with("https://") {
                canonical_url.replace_range(0..8, "https://");
            }
            while canonical_url.ends_with('/') {
                canonical_url.pop();
            }
            e.url = canonical_url;

            if seen.insert(e.url.clone()) {
                Some(e)
            } else {
                None
            }
        })
        .collect();

    if validated_endpoints.is_empty() {
        return Err(ConfigError::ConfigError("No valid endpoints configured".to_string()));
    }

    Ok(validated_endpoints)
}

pub fn get_default_endpoints() -> Vec<EndpointConfig> {
    DEFAULT_ENDPOINTS
        .iter()
        .map(|&s| EndpointConfig {
            url: s.to_string(),
            rate_limit_per_sec: DEFAULT_ENDPOINT_RATE_LIMIT,
            burst_size: default_burst_size(),
            weight: Some(DEFAULT_ENDPOINT_WEIGHT),
        })
        .collect()
}

// Note: This is just the structure, not the full code.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_try_load_config_valid_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "[server]\nbind_addr = \"127.0.0.1:8070\"").unwrap();
        let path = file.path().to_str().unwrap();
        let result = try_load_config(path).unwrap();
        assert!(result.is_some());
        let config = result.unwrap();
        assert_eq!(config.server.unwrap().bind_addr.unwrap(), "127.0.0.1:8070");
    }

    #[test]
    fn test_try_load_config_file_not_found() {
        let result = try_load_config("nonexistent.toml").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_try_load_config_invalid_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "[server]\nbind_addr = 12345").unwrap();
        let path = file.path().to_str().unwrap();
        let result = try_load_config(path);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_and_dedupe_endpoints() {
        let endpoints = vec![
            EndpointConfig {
                url: "https://valid1.com".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: Some(1),
            },
            EndpointConfig {
                url: "https://valid1.com".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: Some(1),
            },
            EndpointConfig {
                url: "".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: Some(1),
            },
            EndpointConfig {
                url: "invalid_url".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: Some(1),
            },
        ];
        let result = validate_and_dedupe_endpoints(endpoints);
        assert!(result.is_ok());
        let list = result.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].url, "https://valid1.com");
    }

    #[test]
    fn test_get_default_endpoints() {
        let endpoints = get_default_endpoints();
        assert_eq!(endpoints.len(), 4);
        assert!(endpoints.iter().all(|e| e.url.starts_with("https://")));
    }

    #[test]
    fn test_validate_and_dedupe_all_invalid() {
        let endpoints = vec![
            EndpointConfig {
                url: "not-a-url".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: Some(1),
            },
            EndpointConfig {
                url: "".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: Some(1),
            },
        ];
        let result = validate_and_dedupe_endpoints(endpoints);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_and_dedupe_multiple_valid() {
        let endpoints = vec![
            EndpointConfig {
                url: "https://mainnet.infura.io/v3/123".into(),
                rate_limit_per_sec: 20,
                burst_size: 5,
                weight: Some(1),
            },
            EndpointConfig {
                url: "https://rpc.ankr.com/eth".into(),
                rate_limit_per_sec: 20,
                burst_size: 5,
                weight: Some(1),
            },
        ];
        let result = validate_and_dedupe_endpoints(endpoints).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|e| e.url.contains("infura")));
        assert!(result.iter().any(|e| e.url.contains("ankr")));
    }

    #[test]
    fn test_balancer_config_optional_fields() {
        let toml = r#"
        [balancer]
        max_batch_size = 42
        max_concurrency = 10
        "#;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "{}", toml).unwrap();
        let path = file.path().to_str().unwrap();

        let cfg = try_load_config(path).unwrap().unwrap();
        let balancer = cfg.balancer.unwrap();
        assert_eq!(balancer.max_batch_size.unwrap(), 42);
        assert_eq!(balancer.max_concurrency.unwrap(), 10);
        assert!(balancer.base_cooldown_secs.is_none());
        assert!(balancer.endpoints.is_none());
    }

    #[test]
    fn test_endpoint_default_values() {
        let endpoint = EndpointConfig {
            url: "https://example.com".to_string(),
            rate_limit_per_sec: 10,
            ..Default::default()
        };
        assert_eq!(endpoint.burst_size, DEFAULT_BURST_SIZE);
        assert!(endpoint.weight.is_none());
    }

    #[test]
    fn test_get_default_endpoints_not_empty() {
        let endpoints = get_default_endpoints();
        assert!(!endpoints.is_empty());
        for e in &endpoints {
            assert!(e.url.starts_with("https://"));
            assert_eq!(e.rate_limit_per_sec, DEFAULT_ENDPOINT_RATE_LIMIT);
            assert_eq!(e.burst_size, DEFAULT_BURST_SIZE);
            assert_eq!(e.weight.unwrap(), DEFAULT_ENDPOINT_WEIGHT);
        }
    }

    #[test]
    fn test_server_config_defaults() {
        let cfg = Config::default();
        assert!(cfg.server.is_none());
    }

    #[test]
    fn test_endpoint_custom_values() {
        let endpoint = EndpointConfig {
            url: "https://example.com".to_string(),
            rate_limit_per_sec: 0,
            burst_size: 50,
            weight: Some(5),
        };
        assert_eq!(endpoint.rate_limit_per_sec, 0);
        assert_eq!(endpoint.burst_size, 50);
        assert_eq!(endpoint.weight, Some(5));
    }

    #[test]
    fn test_validate_endpoints_trim_and_canonicalize() {
        let endpoints = vec![
            EndpointConfig {
                url: " HTTPS://example.com/ ".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
            EndpointConfig {
                url: "https://example.com".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
        ];
        let validated = validate_and_dedupe_endpoints(endpoints).unwrap();
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].url, "https://example.com");
    }

    #[test]
    fn test_validate_and_dedupe_mixed() {
        let endpoints = vec![
            EndpointConfig {
                url: "https://good.com".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
            EndpointConfig {
                url: "bad".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
            EndpointConfig { url: "".into(), rate_limit_per_sec: 10, burst_size: 5, weight: None },
        ];
        let validated = validate_and_dedupe_endpoints(endpoints).unwrap();
        assert_eq!(validated.len(), 1);
        assert_eq!(validated[0].url, "https://good.com");
    }

    #[test]
    fn test_get_default_endpoints_exact() {
        let endpoints = get_default_endpoints();
        for (i, e) in endpoints.iter().enumerate() {
            assert_eq!(e.url, DEFAULT_ENDPOINTS[i]);
            assert_eq!(e.burst_size, DEFAULT_BURST_SIZE);
            assert_eq!(e.rate_limit_per_sec, DEFAULT_ENDPOINT_RATE_LIMIT);
            assert_eq!(e.weight.unwrap(), DEFAULT_ENDPOINT_WEIGHT);
        }
    }

    #[test]
    fn test_try_load_empty_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file).unwrap();
        let path = file.path().to_str().unwrap();
        let result = try_load_config(path).unwrap();
        assert!(result.is_some());
        let cfg = result.unwrap();
        assert!(cfg.server.is_none());
        assert!(cfg.balancer.is_none());
    }

    #[test]
    fn test_validate_endpoints_invalid_characters() {
        let endpoints = vec![
            EndpointConfig {
                url: "https://good.com/ bad".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
            EndpointConfig {
                url: "https://ok.com/\x07".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
        ];
        let result = validate_and_dedupe_endpoints(endpoints);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_endpoints_max_url_length() {
        let long_url = format!("https://example.com/{}", "a".repeat(2048));
        let endpoints = vec![EndpointConfig {
            url: long_url.clone(),
            rate_limit_per_sec: 10,
            burst_size: 5,
            weight: None,
        }];
        let result = validate_and_dedupe_endpoints(endpoints);
        assert!(result.is_err(), "URL exceeding max length should be rejected");
    }

    #[test]
    fn test_validate_endpoints_sane_rate_and_burst() {
        let endpoints = vec![
            EndpointConfig {
                url: "https://good.com".into(),
                rate_limit_per_sec: 0,
                burst_size: 0,
                weight: None,
            },
            EndpointConfig {
                url: "https://ok.com".into(),
                rate_limit_per_sec: 1_000_000,
                burst_size: 1_000_000,
                weight: None,
            },
        ];
        let result = validate_and_dedupe_endpoints(endpoints);
        assert!(result.is_err(), "Endpoints with insane rate or burst should be rejected");
    }

    #[test]
    fn test_validate_endpoints_canonicalization() {
        let endpoints = vec![
            EndpointConfig {
                url: "https://example.com".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
            EndpointConfig {
                url: "https://example.com/".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
            EndpointConfig {
                url: "HTTPS://example.com".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
        ];
        let result = validate_and_dedupe_endpoints(endpoints).unwrap();
        assert_eq!(
            result.len(),
            1,
            "Duplicates differing only by slash or case should be deduplicated"
        );
    }

    // --- NEW TESTS ADDED BELOW ---

    #[test]
    fn test_validate_endpoints_invalid_weight() {
        let endpoints = vec![EndpointConfig {
            url: "https://example.com".into(),
            rate_limit_per_sec: 10,
            burst_size: 5,
            weight: Some(0),
        }];
        let result = validate_and_dedupe_endpoints(endpoints);
        assert!(result.is_ok());
        let list = result.unwrap();
        assert_eq!(list[0].weight, Some(0));
    }

    #[test]
    fn test_validate_endpoints_multiple_trailing_slashes() {
        let endpoints = vec![EndpointConfig {
            url: "https://example.com///".into(),
            rate_limit_per_sec: 10,
            burst_size: 5,
            weight: None,
        }];
        let result = validate_and_dedupe_endpoints(endpoints).unwrap();
        assert_eq!(result[0].url, "https://example.com");
    }

    #[test]
    fn test_validate_endpoints_error_message() {
        let endpoints = vec![EndpointConfig {
            url: "invalid".into(),
            rate_limit_per_sec: 10,
            burst_size: 5,
            weight: None,
        }];
        let result = validate_and_dedupe_endpoints(endpoints);
        assert!(matches!(
            result,
            Err(ConfigError::ConfigError(msg)) if msg == "No valid endpoints configured"
        ));
    }

    #[test]
    fn test_balancer_config_client_settings() {
        let toml = r#"
        [balancer]
        connect_timeout_ms = 1000
        timeout_secs = 10
        pool_idle_timeout_secs = 30
        pool_max_idle_per_host = 100
        "#;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "{}", toml).unwrap();
        let path = file.path().to_str().unwrap();

        let cfg = try_load_config(path).unwrap().unwrap();
        let balancer = cfg.balancer.unwrap();

        assert_eq!(balancer.connect_timeout_ms.unwrap(), 1000);
        assert_eq!(balancer.timeout_secs.unwrap(), 10);
        assert_eq!(balancer.pool_idle_timeout_secs.unwrap(), 30);
        assert_eq!(balancer.pool_max_idle_per_host.unwrap(), 100);
    }

    #[test]
    fn test_get_default_endpoints_urls() {
        let endpoints = get_default_endpoints();
        let expected_urls: Vec<String> = DEFAULT_ENDPOINTS.iter().map(|&s| s.to_string()).collect();
        let actual_urls: Vec<String> = endpoints.iter().map(|e| e.url.clone()).collect();

        assert_eq!(actual_urls, expected_urls);
    }

    #[test]
    fn test_default_constants() {
        let endpoint = EndpointConfig::default();
        assert_eq!(endpoint.burst_size, DEFAULT_BURST_SIZE);
        assert_eq!(endpoint.rate_limit_per_sec, DEFAULT_ENDPOINT_RATE_LIMIT);

        let default_endpoints = get_default_endpoints();
        assert!(default_endpoints.iter().all(|e| e.burst_size == DEFAULT_BURST_SIZE));
        assert!(default_endpoints
            .iter()
            .all(|e| e.rate_limit_per_sec == DEFAULT_ENDPOINT_RATE_LIMIT));
    }

    #[test]
    fn test_endpoint_scheme_mixed_case() {
        let endpoints = vec![EndpointConfig {
            url: "HtTpS://example.com".into(),
            rate_limit_per_sec: 10,
            burst_size: 5,
            weight: None,
        }];
        let validated = validate_and_dedupe_endpoints(endpoints).unwrap();
        assert_eq!(validated[0].url, "https://example.com");
    }

    #[test]
    fn test_endpoint_with_query_and_fragment() {
        let endpoints = vec![EndpointConfig {
            url: "https://example.com/path?query=1#frag".into(),
            rate_limit_per_sec: 10,
            burst_size: 5,
            weight: None,
        }];
        let validated = validate_and_dedupe_endpoints(endpoints).unwrap();
        assert_eq!(validated[0].url, "https://example.com/path?query=1#frag");
    }

    #[test]
    fn test_balancer_extreme_values() {
        let cfg = BalancerConfig {
            max_batch_size: Some(0),
            max_concurrency: Some(100_000),
            ..Default::default()
        };
        assert_eq!(cfg.max_batch_size.unwrap(), 0);
        assert_eq!(cfg.max_concurrency.unwrap(), 100_000);
    }

    #[test]
    fn test_latency_smoothing_edge() {
        let cfg = BalancerConfig { latency_smoothing_factor: Some(0.0), ..Default::default() };
        assert_eq!(cfg.latency_smoothing_factor.unwrap(), 0.0);
    }

    #[test]
    fn test_validate_endpoints_all_invalid_branches() {
        let endpoints = vec![
            // Empty URL → invalid
            EndpointConfig { url: "".into(), rate_limit_per_sec: 10, burst_size: 5, weight: None },
            // Invalid URL → invalid
            EndpointConfig {
                url: "not-a-url".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
            // Rate limit zero → invalid
            EndpointConfig {
                url: "https://ok.com".into(),
                rate_limit_per_sec: 0,
                burst_size: 5,
                weight: None,
            },
            // Burst size zero → invalid
            EndpointConfig {
                url: "https://ok2.com".into(),
                rate_limit_per_sec: 10,
                burst_size: 0,
                weight: None,
            },
            // URL too long
            EndpointConfig {
                url: format!("https://example.com/{}", "a".repeat(2048)),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
        ];

        let result = validate_and_dedupe_endpoints(endpoints);
        assert!(matches!(
            result,
            Err(ConfigError::ConfigError(msg)) if msg == "No valid endpoints configured"
        ));
    }

    #[test]
    fn test_validate_endpoints_mixed_stress() {
        let endpoints = vec![
            // Valid endpoints
            EndpointConfig {
                url: "https://valid1.com".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: Some(1),
            },
            EndpointConfig {
                url: "http://valid2.com/".into(),
                rate_limit_per_sec: 20,
                burst_size: 10,
                weight: None,
            },
            // Invalid endpoints
            EndpointConfig { url: "".into(), rate_limit_per_sec: 10, burst_size: 5, weight: None },
            EndpointConfig {
                url: "bad-url".into(),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
            EndpointConfig {
                url: "https://example.com".into(),
                rate_limit_per_sec: 0,
                burst_size: 5,
                weight: None,
            },
            EndpointConfig {
                url: "https://example.com".into(),
                rate_limit_per_sec: 10,
                burst_size: 0,
                weight: None,
            },
            EndpointConfig {
                url: format!("https://toolong.com/{}", "a".repeat(2048)),
                rate_limit_per_sec: 10,
                burst_size: 5,
                weight: None,
            },
        ];

        let result = validate_and_dedupe_endpoints(endpoints).unwrap();

        // Only the valid endpoints remain
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|e| e.url == "https://valid1.com"));
        assert!(result.iter().any(|e| e.url == "http://valid2.com"));
    }
}
