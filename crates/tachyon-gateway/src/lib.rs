//! tachyon-gateway: API gateway for the Tachyon matching engine.
//!
//! Provides WebSocket, REST, and FIX protocol interfaces for
//! client connectivity. This is NOT on the hot path.

pub mod auth;
pub mod bridge;
pub mod rate_limit;
pub mod rest;
pub mod tcp;
pub mod types;
pub mod ws;

pub use auth::{auth_middleware, AuthConfig, AuthState};
pub use bridge::EngineBridge;
pub use rate_limit::{RateLimitConfig, RateLimiter};
pub use rest::{rest_router, AppState};
pub use tcp::{run_tcp_server, TcpState};
pub use types::*;
pub use ws::{ws_handler, WsState};

/// Trait for providing metrics data to the REST API.
///
/// Implemented by the server's `Metrics` struct. This trait allows the gateway
/// crate to remain decoupled from the concrete metrics implementation.
pub trait MetricsProvider: Send + Sync {
    /// Return all metrics in Prometheus text exposition format.
    fn encode_prometheus(&self) -> String;

    /// Return all metrics as a JSON string.
    fn encode_json(&self) -> String;

    /// Return the server uptime in seconds.
    fn uptime_secs(&self) -> u64;
}

/// Configuration for the gateway server.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub ws_port: u16,
    pub rest_port: u16,
    pub tcp_port: u16,
    pub bind_address: String,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            ws_port: 8080,
            rest_port: 8081,
            tcp_port: 8082,
            bind_address: "0.0.0.0".to_string(),
        }
    }
}
