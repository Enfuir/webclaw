//! Optional bearer-token middleware.
//!
//! When the server is started without `--api-key`, every request is allowed
//! through (server runs in "open" mode — appropriate for `localhost`-only
//! deployments). When a key is configured, every `/v1/*` request must
//! present `Authorization: Bearer <key>` and the comparison is constant-
//! time to avoid timing-leaking the key.

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use subtle::ConstantTimeEq;

use crate::state::AppState;

/// Axum middleware. Mount with `axum::middleware::from_fn_with_state`.
pub async fn require_bearer(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = state.api_key() else {
        // Open mode — no key configured. Allow everything.
        return Ok(next.run(request).await);
    };

    let Some(header) = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let presented = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if presented.as_bytes().ct_eq(expected.as_bytes()).into() {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}
