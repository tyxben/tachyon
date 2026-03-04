//! tachyon-gateway: API gateway for the Tachyon matching engine.
//!
//! Provides WebSocket, REST, and FIX protocol interfaces for
//! client connectivity. This is NOT on the hot path.

pub mod bridge;
pub mod rest;
pub mod types;
pub mod ws;

pub use bridge::EngineBridge;
pub use rest::{rest_router, AppState};
pub use types::*;
pub use ws::{ws_handler, WsState};

/// Configuration for the gateway server.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub ws_port: u16,
    pub rest_port: u16,
    pub bind_address: String,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            ws_port: 8080,
            rest_port: 8081,
            bind_address: "0.0.0.0".to_string(),
        }
    }
}
