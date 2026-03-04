//! Simple API key authentication middleware for the Tachyon gateway.
//!
//! Validates requests against a configured set of API keys via the `X-API-Key` header.
//! Public endpoints (health, metrics, symbols, orderbook) skip authentication.

use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Configuration for API key authentication.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// Whether authentication is enabled.
    pub enabled: bool,
    /// Set of valid API keys.
    pub api_keys: HashSet<String>,
}

/// Shared authentication state.
#[derive(Clone)]
pub struct AuthState {
    pub config: Arc<AuthConfig>,
}

impl AuthState {
    pub fn new(config: AuthConfig) -> Self {
        AuthState {
            config: Arc::new(config),
        }
    }
}

/// The header name for the API key.
pub const API_KEY_HEADER: &str = "x-api-key";

/// Paths that skip authentication (public endpoints).
const PUBLIC_PATHS: &[&str] = &[
    "/health",
    "/metrics",
    "/api/v1/symbols",
    "/api/v1/orderbook/",
    "/api/v1/trades/",
];

/// Returns true if the given path is a public endpoint that skips authentication.
fn is_public_path(path: &str) -> bool {
    for prefix in PUBLIC_PATHS {
        if path == *prefix || path.starts_with(prefix) {
            return true;
        }
    }
    false
}

/// Axum middleware function for API key authentication.
///
/// Must be used with `axum::middleware::from_fn_with_state`.
pub async fn auth_middleware(
    State(auth): State<AuthState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !auth.config.enabled {
        return next.run(request).await;
    }

    // Skip auth for public endpoints
    let path = request.uri().path().to_string();
    if is_public_path(&path) {
        return next.run(request).await;
    }

    // Check for API key header
    match request.headers().get(API_KEY_HEADER) {
        Some(value) => match value.to_str() {
            Ok(key) if auth.config.api_keys.contains(key) => next.run(request).await,
            _ => (
                StatusCode::UNAUTHORIZED,
                serde_json::json!({"error": "invalid API key"}).to_string(),
            )
                .into_response(),
        },
        None => (
            StatusCode::UNAUTHORIZED,
            serde_json::json!({"error": "missing API key"}).to_string(),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware;
    use axum::routing::{get, post};
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_router(auth_config: AuthConfig) -> Router {
        let auth_state = AuthState::new(auth_config);
        Router::new()
            .route("/api/v1/order", post(|| async { "ok" }))
            .route("/health", get(|| async { "healthy" }))
            .route("/api/v1/symbols", get(|| async { "symbols" }))
            .route("/api/v1/orderbook/BTCUSDT", get(|| async { "book" }))
            .route("/api/v1/trades/BTCUSDT", get(|| async { "trades" }))
            .layer(middleware::from_fn_with_state(auth_state, auth_middleware))
    }

    #[tokio::test]
    async fn test_auth_disabled_allows_all() {
        let config = AuthConfig {
            enabled: false,
            api_keys: HashSet::new(),
        };
        let app = test_router(config);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/order")
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_valid_key_passes() {
        let mut keys = HashSet::new();
        keys.insert("test-key-123".to_string());
        let config = AuthConfig {
            enabled: true,
            api_keys: keys,
        };
        let app = test_router(config);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/order")
            .header("content-type", "application/json")
            .header("x-api-key", "test-key-123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_invalid_key_rejected() {
        let mut keys = HashSet::new();
        keys.insert("test-key-123".to_string());
        let config = AuthConfig {
            enabled: true,
            api_keys: keys,
        };
        let app = test_router(config);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/order")
            .header("content-type", "application/json")
            .header("x-api-key", "wrong-key")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains("invalid API key"));
    }

    #[tokio::test]
    async fn test_auth_missing_key_rejected() {
        let mut keys = HashSet::new();
        keys.insert("test-key-123".to_string());
        let config = AuthConfig {
            enabled: true,
            api_keys: keys,
        };
        let app = test_router(config);

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/order")
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains("missing API key"));
    }

    #[tokio::test]
    async fn test_auth_public_endpoints_skip_auth() {
        let mut keys = HashSet::new();
        keys.insert("secret".to_string());
        let config = AuthConfig {
            enabled: true,
            api_keys: keys,
        };

        // Test /health
        let app = test_router(config.clone());
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Test /api/v1/symbols
        let app = test_router(config.clone());
        let req = Request::builder()
            .uri("/api/v1/symbols")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Test /api/v1/orderbook/BTCUSDT
        let app = test_router(config.clone());
        let req = Request::builder()
            .uri("/api/v1/orderbook/BTCUSDT")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Test /api/v1/trades/BTCUSDT
        let app = test_router(config);
        let req = Request::builder()
            .uri("/api/v1/trades/BTCUSDT")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_is_public_path() {
        assert!(is_public_path("/health"));
        assert!(is_public_path("/metrics"));
        assert!(is_public_path("/api/v1/symbols"));
        assert!(is_public_path("/api/v1/orderbook/BTCUSDT"));
        assert!(is_public_path("/api/v1/trades/ETHUSDT"));
        assert!(!is_public_path("/api/v1/order"));
        assert!(!is_public_path("/api/v1/order/123"));
        assert!(!is_public_path("/admin"));
    }
}
