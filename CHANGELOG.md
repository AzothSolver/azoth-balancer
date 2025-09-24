# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## \[Unreleased]

### Planned
- Evironnement variable .env handling
- Batch splitting for large requests
- Response caching system for common RPC methods
- MEV-aware routing for transaction protection
- HTTPS/TLS termination and authentication
- Production monitoring dashboards

## [0.3.0] - 2025-09-23

### Added
- **Tiered Routing Strategy**: Implemented strict 3-tier endpoint fallback (Tier 1 → Tier 2 → Tier 3) with exponential backoff for failing endpoints
- **Multi-Chain Support**: Verified compatibility with Ethereum, Arbitrum, and Solana JSON-RPC endpoints
- **Enhanced Testing**: Fixed and expanded test coverage for tier fallback scenarios and concurrency edge cases
- **Documentation**: Comprehensive README with deployment guides, CLI usage, multi-config examples, and architecture diagram

### Changed
- **Project Structure**: Finalized module organization with `utils/` folder for shared components
- **Code Quality**: Improved error handling patterns and async/await consistency
- **Configuration**: Enhanced validation for endpoint weights, cooldowns, and rate limiting settings

### Fixed
- **Rate Limiting**: Improved token reservation logic for batch requests
- **Health Checks**: Enhanced endpoint availability detection during cooldown periods
- **Metrics**: Fixed Prometheus label consistency across all endpoint measurements

## \[0.2.0] - 2025-09-17

### Changed
* **Project Structure**
  * Introduced `utils/` folder for helper modules: `cooldown.rs`, `health.rs`, `metrics.rs`.
  * Core modules (`balancer.rs`, `config.rs`, `config_reloader.rs`, `shutdown.rs`, `endpoint.rs`, `strategy.rs`, `forwarder.rs`) remain flat in `src/` for maintainability.
  * Added `lib.rs` as a public API entry point for internal testing and external integration.

* **Imports & References**
  * Updated all module imports and paths to reflect new file structure.
  * Verified doc-tests and unit tests against new structure.

* **Configuration & Hot Reload**
  * Confirmed `config_reloader.rs` works with `utils` structure and maintains hot-reload functionality.

* **Shutdown System**
  * No functional change, but `shutdown.rs` properly exposed via `lib.rs` for modular testing.

### Added
* `utils` folder for shared helper logic.
* `lib.rs` exports to improve public API exposure.
* Documentation and doc-tests updated to reflect the new structure.

### Fixed
* Corrected imports in `main.rs` and other modules after reorganization.
* Fixed any broken references caused by module movement.

### Notes
* No functional change in runtime behavior or RPC handling.
* All unit tests and integration tests confirmed passing with the new structure.

## \[0.1.0] - 2025-09-16

This is the initial public release of AzothBalancer, a production-grade RPC load balancer engineered for DeFi applications. This release establishes a robust foundation for reliable and performant RPC routing with comprehensive observability and operational controls.

### Added
**Core Infrastructure**
* High-Performance Engine: Built on tokio, axum, and reqwest for asynchronous, non-blocking performance
* Graceful Shutdown System: Structured shutdown coordinator with timeout enforcement and panic handling
* Configuration Management: Dynamic TOML-based configuration with validation and hot-reload capabilities

**Load Balancing & Routing**
* Priority-Based Routing: Advanced routing strategy prioritizing endpoints by user-defined weight
* EMA Latency Optimization: Real-time latency-based tie-breaking using Exponential Moving Average
* Batch Request Support: Native handling of JSON-RPC batch requests with atomic execution
* Concurrency Control: Global semaphore-based limiter to prevent service overload

**Resilience & Health Management**
* Active Health Checking: Background health checks (`eth_blockNumber`) with configurable intervals
* Exponential Backoff Cooldown: Intelligent cooldown system for failing or rate-limited endpoints
* Multi-Provider Redundancy: Support for diverse upstream providers with automatic failover

**Observability & Metrics**
* Prometheus Integration: Comprehensive metrics exposed at `/metrics` endpoint
* Request Telemetry: Detailed tracking of success/failure rates, latency distributions, and method usage
* Endpoint Health Monitoring: Real-time health status, cooldown states, and performance metrics
* Rate Limit Tracking: Monitoring of both client-side and upstream rate limiting events

**Configuration & Operations**
* Hot Reload API: `/reload` endpoint for configuration updates without service interruption
* Dynamic Client Settings: Configurable HTTP client timeouts and connection pooling
* Validation & Sanitization: Robust configuration validation with sensible defaults
* Endpoint Deduplication: Automatic detection and removal of duplicate endpoints

**API Endpoints**
* Main RPC Proxy (`/`): Handles both single and batch JSON-RPC requests
* Status Dashboard (`/status`): Detailed introspection of endpoint states and balancer settings
* Health Check (`/health`): Simple health endpoint for orchestration systems
* Metrics (`/metrics`): Prometheus-formatted metrics for monitoring

### Changed
* Project Rename: Rebranded from "RPC Balancer" to "AzothBalancer" to reflect production readiness
* Architecture Refinement: Modular architecture separating concerns into dedicated components
* Configuration Defaults: Optimized default settings for blockchain RPC workloads
* Error Handling: Comprehensive error types with proper conversion and user-friendly messages

### Fixed
* Metric Cleanup: Proper removal of metrics for endpoints that are deleted during config reload
* URL Canonicalization: Consistent URL handling with scheme normalization and trailing slash removal
* URL Scheme Validation: Ensures endpoint URLs use a valid `http://` or `https://` protocol scheme
* Batch Size Validation: Both pre-parse and post-parse validation to prevent oversized batches
* Edge Case Handling: Improved handling of extreme values and boundary conditions in configuration

### Performance
* Connection Pooling: Configurable keep-alive and connection reuse for reduced latency
* Lock Optimization: Fine-grained locking using parking\_lot RwLock for better concurrency
* Atomic Operations: Lock-free latency updates using atomic operations for the hot path
* Efficient JSON Processing: Lightweight error detection without full parsing on response path

### Security
* Input Validation: Size limits and structure validation on incoming requests
* Control Character Sanitization: Filtering of potentially malicious input in endpoint URLs

