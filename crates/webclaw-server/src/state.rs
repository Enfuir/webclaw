//! Shared application state. Cheap to clone via Arc; held by the axum
//! Router for the life of the process.

use std::sync::Arc;
use webclaw_fetch::{BrowserProfile, FetchClient, FetchConfig};

/// Single-process state shared across all request handlers.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    /// Wrapped in `Arc` because `fetch_and_extract_batch_with_options`
    /// (used by the /v1/batch handler) takes `self: &Arc<Self>` so it
    /// can clone the client into spawned tasks. The single-call handlers
    /// auto-deref `&Arc<FetchClient>` -> `&FetchClient`, so this costs
    /// them nothing.
    pub fetch: Arc<FetchClient>,
    pub api_key: Option<String>,
}

impl AppState {
    /// Build the application state. The fetch client is constructed once
    /// and shared across requests so connection pools + browser profile
    /// state don't churn per request.
    pub fn new(api_key: Option<String>) -> anyhow::Result<Self> {
        let config = FetchConfig {
            browser: BrowserProfile::Firefox,
            ..FetchConfig::default()
        };
        let fetch = FetchClient::new(config)
            .map_err(|e| anyhow::anyhow!("failed to build fetch client: {e}"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                fetch: Arc::new(fetch),
                api_key,
            }),
        })
    }

    pub fn fetch(&self) -> &Arc<FetchClient> {
        &self.inner.fetch
    }

    pub fn api_key(&self) -> Option<&str> {
        self.inner.api_key.as_deref()
    }
}
