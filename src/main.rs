//! JSON-RPC Load Balancer
//!
//! Main entry point for the load balancer.
//! Responsibilities:
//! 1. Parse CLI arguments to load configuration.
//! 2. Initialize `LoadBalancer` state and background tasks.
//! 3. Set up Axum web server with endpoints: `/`, `/status`, `/health`, `/reload`, `/metrics`.
//! 4. Handle graceful and forced shutdown on `Ctrl+C` or `SIGTERM`.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router, Server,
};
use clap::Parser;
use prometheus::{Encoder, TextEncoder};
use serde_json::{json, Value};
use tokio::signal;
use tracing::{error, info, trace};
use tracing_subscriber::fmt::init;

use azoth_balancer::balancer::LoadBalancer;
use azoth_balancer::config::try_load_config;
use azoth_balancer::config_reloader::{ConfigChanges, ReloadResponse};
use azoth_balancer::endpoint::LoadBalancerError;
use azoth_balancer::shutdown::ShutdownManager;
// utils
use azoth_balancer::metrics::{BATCH_SIZE_EXCEEDED, CONCURRENCY_ACTIVE_PERMITS};

/// RAII guard for tracking active concurrency permits.
struct ConcurrencyGuard;

impl ConcurrencyGuard {
    fn new() -> Self {
        CONCURRENCY_ACTIVE_PERMITS.inc();
        Self
    }
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        CONCURRENCY_ACTIVE_PERMITS.dec();
    }
}

/// Handles JSON-RPC requests (single or batch).
/// Performs pre-validation and forwards requests to the selected endpoint.
async fn handle_rpc(State(balancer): State<Arc<LoadBalancer>>, body: Bytes) -> Response {
    let _concurrency_guard = ConcurrencyGuard::new();
    trace!(body = ?String::from_utf8_lossy(&body), "Received RPC request");

    // --- Pre-emptive Validation ---
    const MAX_JSON_SIZE: usize = 10 * 1024 * 1024; // 10MB
    if body.len() > MAX_JSON_SIZE {
        error!(size = body.len(), "Request payload exceeds maximum size");
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }

    let is_likely_batch =
        body.iter().find(|b| !b.is_ascii_whitespace()).is_some_and(|b| *b == b'[');
    if is_likely_batch {
        let approx_batch_size = body.iter().filter(|&&b| b == b'{').count();
        if approx_batch_size > *balancer.max_batch_size.read() {
            error!(approx_size = approx_batch_size, "Request batch size likely exceeds maximum");
            BATCH_SIZE_EXCEEDED.inc();
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
    }
    // --- End Validation ---

    let _permit = match balancer.concurrency_limiter.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            error!("Concurrency semaphore was closed unexpectedly");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let payload_value: Value = match serde_json::from_slice(&body) {
        Ok(val) => val,
        Err(e) => {
            let err = LoadBalancerError::BadRequest(format!("Failed to parse JSON body: {}", e));
            return (StatusCode::BAD_REQUEST, err.to_string()).into_response();
        }
    };

    let (is_batch, batch_size) =
        if let Some(arr) = payload_value.as_array() { (true, arr.len()) } else { (false, 1) };

    if batch_size == 0 {
        let err = LoadBalancerError::BadRequest("Empty batch is not allowed".to_string());
        return (StatusCode::BAD_REQUEST, err.to_string()).into_response();
    }

    if batch_size > *balancer.max_batch_size.read() {
        error!(batch_size = batch_size, "Batch size exceeds maximum allowed (post-parse check)");
        BATCH_SIZE_EXCEEDED.inc();
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }

    let endpoint = match balancer.get_next_endpoint(batch_size) {
        Some(ep) => ep,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    info!(
        endpoint = %endpoint,
        batch_size = batch_size,
        is_batch = is_batch,
        "Selected priority endpoint and forwarding request(s)"
    );

    let method_label = if is_batch {
        "batch".to_string()
    } else {
        payload_value.get("method").and_then(|m| m.as_str()).unwrap_or("unknown").to_string()
    };

    match balancer.forward_raw_request(body, &endpoint, &method_label).await {
        Ok(resp_bytes) => {
            ([(header::CONTENT_TYPE, "application/json")], resp_bytes).into_response()
        }
        Err(e) => {
            let (code, message) = match &e {
                LoadBalancerError::RateLimited(msg) => (-32000, msg.as_str()),
                LoadBalancerError::UpstreamError(msg) => (-32603, msg.as_str()),
                _ => (-32603, "Internal server error"),
            };

            let id = if !is_batch { payload_value.get("id").cloned() } else { None };

            let error_payload = json!({
                "jsonrpc": "2.0",
                "error": {"code": code, "message": message},
                "id": id
            });

            let error_bytes =
                serde_json::to_vec(&error_payload).expect("Failed to serialize error response");

            (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], error_bytes)
                .into_response()
        }
    }
}

/// Returns load balancer status including endpoint health and configuration.
async fn handle_status(State(balancer): State<Arc<LoadBalancer>>) -> impl IntoResponse {
    axum::Json(balancer.get_status())
}

/// Simple health check endpoint for monitoring services.
async fn handle_health() -> impl IntoResponse {
    axum::Json(json!({"status": "healthy"}))
}

/// Hot-reloads the configuration file from disk.
async fn handle_reload(State(balancer): State<Arc<LoadBalancer>>) -> impl IntoResponse {
    match balancer.reload_config() {
        Ok(response) => (StatusCode::OK, axum::Json(response)).into_response(),
        Err(e) => {
            error!(error = %e, "Config reload failed");
            let error_response = ReloadResponse {
                success: false,
                message: "Configuration reload failed".to_string(),
                error: Some(e.to_string()),
                changes: ConfigChanges::default(),
            };
            (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(error_response)).into_response()
        }
    }
}

/// Exposes Prometheus metrics.
async fn metrics_handler() -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => {
            (StatusCode::OK, [(header::CONTENT_TYPE, encoder.format_type().to_string())], buffer)
        }
        Err(e) => {
            error!(error = %e, "Failed to encode metrics");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain".to_string())],
                format!("Error encoding metrics: {}", e).into_bytes(),
            )
        }
    }
}

/// Command-line interface for the application.
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// The path to the TOML configuration file.
    #[arg(short, long, default_value = "config.toml")]
    config: String,
}

/// Main entry point: initialize logging, load config, start server, and handle shutdown.
#[tokio::main]
async fn main() -> Result<(), LoadBalancerError> {
    init();

    let args = Cli::parse();
    let cfg = try_load_config(&args.config)?;
    let balancer = Arc::new(LoadBalancer::new(cfg, args.config));

    let mut shutdown_manager = ShutdownManager::new();

    balancer.run_background_tasks(&mut shutdown_manager);

    let app = Router::new()
        .route("/", post(handle_rpc))
        .route("/status", get(handle_status))
        .route("/health", get(handle_health))
        .route("/reload", post(handle_reload))
        .route("/metrics", get(metrics_handler))
        .with_state(balancer.clone());

    let addr = balancer
        .bind_addr
        .parse()
        .map_err(|e: std::net::AddrParseError| LoadBalancerError::ConfigError(e.to_string()))?;
    let server = Server::bind(&addr).serve(app.into_make_service());

    let force_shutdown_atomic = Arc::new(AtomicBool::new(false));
    let force_shutdown_clone = force_shutdown_atomic.clone();

    let graceful = server.with_graceful_shutdown(async move {
        let force = shutdown_signal().await;
        if force {
            force_shutdown_clone.store(true, Ordering::Relaxed);
        }
        info!(
            "Received shutdown signal, initiating {} server shutdown...",
            if force { "forced" } else { "graceful" }
        );
    });

    info!(bind_addr = %balancer.bind_addr, "Starting Azoth-Balancer");
    info!("Endpoints: /status /health /metrics /reload");

    if let Err(e) = graceful.await {
        error!("Axum server error: {}", e);
    }

    let force_shutdown = force_shutdown_atomic.load(Ordering::Relaxed);

    if force_shutdown {
        info!("Forcing shutdown of background tasks.");
        shutdown_manager.abort_all();
    } else {
        info!("Gracefully shutting down background tasks.");
        if let Err(e) = shutdown_manager.graceful_shutdown(Duration::from_secs(30)).await {
            error!("Graceful shutdown failed: {}", e);
        }
    }

    info!("Shutdown complete.");
    Ok(())
}

/// Listens for shutdown signals.
/// Returns `true` if forced shutdown is required, `false` otherwise.
async fn shutdown_signal() -> bool {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Ctrl+C received. Starting graceful shutdown. Press Ctrl+C again within 10s to force.");
            // Wait for a second Ctrl+C with a timeout.
            tokio::select! {
                _ = signal::ctrl_c() => {
                    info!("Second Ctrl+C received - forcing immediate shutdown.");
                    true // Force shutdown
                },
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    false // Graceful shutdown
                }
            }
        },
        _ = terminate => {
            info!("SIGTERM received. Starting graceful shutdown.");
            false // Graceful shutdown
        },
    }
}
