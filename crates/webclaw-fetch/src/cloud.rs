//! Cloud API fallback client for api.webclaw.io.
//!
//! When local fetch hits bot protection or a JS-only SPA, callers can
//! fall back to the hosted API which runs the full antibot / CDP
//! pipeline. This module is the shared home for that flow: previously
//! duplicated between `webclaw-mcp/src/cloud.rs` and
//! `webclaw-cli/src/cloud.rs`.
//!
//! ## Architecture
//!
//! - [`CloudClient`] — thin reqwest wrapper around the api.webclaw.io
//!   REST surface. Typed errors for the four HTTP failures callers act
//!   on differently (401 / 402 / 429 / other) plus network + parse.
//! - [`is_bot_protected`] / [`needs_js_rendering`] — pure detectors on
//!   response bodies. The detection patterns are public (CF / DataDome
//!   challenge-page signatures) so these live in OSS without leaking
//!   any moat.
//! - [`smart_fetch`] — try-local-then-escalate flow returning an
//!   [`ExtractionResult`] or raw cloud JSON. Kept on the original
//!   `Result<_, String>` signature so the existing MCP / CLI call
//!   sites work unchanged.
//! - [`smart_fetch_html`] — new convenience for the vertical-extractor
//!   pattern: just give me antibot-bypassed HTML so I can run my own
//!   parser on it. Returns the typed [`CloudError`] so extractors can
//!   emit precise "upgrade your plan" / "invalid key" messages.
//!
//! OSS users without `WEBCLAW_API_KEY` get a clear error pointing at
//! signup when a site is blocked; nothing fails silently. Cloud users
//! get the escalation for free.

use std::time::Duration;

use http::HeaderMap;
use serde_json::{Value, json};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::client::FetchClient;

// ---------------------------------------------------------------------------
// URLs + defaults — keep in one place so "change the signup link" is a
// single-commit edit.
// ---------------------------------------------------------------------------

const API_BASE_DEFAULT: &str = "https://api.webclaw.io/v1";
const DEFAULT_TIMEOUT_SECS: u64 = 120;

const SIGNUP_URL: &str = "https://webclaw.io/signup";
const PRICING_URL: &str = "https://webclaw.io/pricing";
const KEYS_URL: &str = "https://webclaw.io/dashboard/api-keys";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Structured cloud-fallback error. Variants correspond to the HTTP
/// outcomes callers act on differently — a 401 needs a different UX
/// than a 402 which needs a different UX than a network blip.
///
/// Display messages end with an actionable URL so API consumers can
/// surface them to users verbatim.
#[derive(Debug, Error)]
pub enum CloudError {
    /// No `WEBCLAW_API_KEY` configured. Returned by [`smart_fetch_html`]
    /// and friends when they hit bot protection but have no client to
    /// escalate to.
    #[error(
        "this site is behind antibot protection. \
         Set WEBCLAW_API_KEY to unlock automatic cloud bypass. \
         Free tier: {SIGNUP_URL}"
    )]
    NotConfigured,

    /// HTTP 401 — the key is present but rejected.
    #[error(
        "WEBCLAW_API_KEY rejected (HTTP 401). \
         Check or regenerate your key at {KEYS_URL}"
    )]
    Unauthorized,

    /// HTTP 402 — the key is valid but the plan doesn't cover the call.
    #[error(
        "your plan doesn't include this endpoint / site (HTTP 402). \
         Upgrade at {PRICING_URL}"
    )]
    InsufficientPlan,

    /// HTTP 429 — rate limit.
    #[error(
        "cloud API rate limit reached (HTTP 429). \
         Wait a moment or upgrade at {PRICING_URL}"
    )]
    RateLimited,

    /// HTTP 4xx / 5xx the caller probably can't do anything specific
    /// about. Body is truncated to a sensible length for logs.
    #[error("cloud API returned HTTP {status}: {body}")]
    ServerError { status: u16, body: String },

    #[error("cloud request failed: {0}")]
    Network(String),

    #[error("cloud response parse failed: {0}")]
    ParseFailed(String),
}

impl CloudError {
    /// Build from a non-success HTTP response, routing well-known
    /// statuses to dedicated variants.
    fn from_status_and_body(status: u16, body: String) -> Self {
        match status {
            401 => Self::Unauthorized,
            402 => Self::InsufficientPlan,
            429 => Self::RateLimited,
            _ => Self::ServerError {
                status,
                body: truncate(&body, 500).to_string(),
            },
        }
    }
}

impl From<reqwest::Error> for CloudError {
    fn from(e: reqwest::Error) -> Self {
        Self::Network(e.to_string())
    }
}

/// Backwards-compatibility bridge: a lot of pre-existing MCP / CLI call
/// sites `use .await?` into functions returning `Result<_, String>`.
/// Having this `From` impl means those sites keep compiling while we
/// migrate them to the typed error over time.
impl From<CloudError> for String {
    fn from(e: CloudError) -> Self {
        e.to_string()
    }
}

fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((byte_pos, _)) => &text[..byte_pos],
        None => text,
    }
}

// ---------------------------------------------------------------------------
// CloudClient
// ---------------------------------------------------------------------------

/// Thin reqwest client around api.webclaw.io. Cloneable cheaply — the
/// inner `reqwest::Client` already refcounts its connection pool.
#[derive(Clone)]
pub struct CloudClient {
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl CloudClient {
    /// Build from an explicit key (e.g. a `--api-key` CLI flag) or fall
    /// back to the `WEBCLAW_API_KEY` env var. Returns `None` when
    /// neither is set / both are empty.
    ///
    /// This is the function call sites should use by default — it's
    /// what both the CLI and MCP want.
    pub fn new(explicit_key: Option<&str>) -> Option<Self> {
        explicit_key
            .map(String::from)
            .or_else(|| std::env::var("WEBCLAW_API_KEY").ok())
            .filter(|k| !k.trim().is_empty())
            .map(Self::with_key)
    }

    /// Build from `WEBCLAW_API_KEY` env only. Thin wrapper kept for
    /// readability at call sites that never accept a flag.
    pub fn from_env() -> Option<Self> {
        Self::new(None)
    }

    /// Build with an explicit key. Useful when the caller already has
    /// a key from somewhere other than env or a flag (e.g. loaded from
    /// config).
    pub fn with_key(api_key: impl Into<String>) -> Self {
        Self::with_key_and_base(api_key, API_BASE_DEFAULT)
    }

    /// Build with an explicit key and base URL. Used by integration
    /// tests and staging deployments.
    pub fn with_key_and_base(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .expect("reqwest client builder failed with default settings");
        Self {
            api_key: api_key.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Generic POST. Endpoint may be `"scrape"` or `"/scrape"` — we
    /// normalise the slash.
    pub async fn post(&self, endpoint: &str, body: Value) -> Result<Value, CloudError> {
        let url = format!("{}/{}", self.base_url, endpoint.trim_start_matches('/'));
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await?;
        parse_cloud_response(resp).await
    }

    /// Generic GET.
    pub async fn get(&self, endpoint: &str) -> Result<Value, CloudError> {
        let url = format!("{}/{}", self.base_url, endpoint.trim_start_matches('/'));
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await?;
        parse_cloud_response(resp).await
    }

    /// `POST /v1/scrape` with the caller's extraction options. This is
    /// the public "do everything" surface: the cloud side handles
    /// fetch + antibot + JS render + extraction + formatting.
    pub async fn scrape(
        &self,
        url: &str,
        formats: &[&str],
        include_selectors: &[String],
        exclude_selectors: &[String],
        only_main_content: bool,
    ) -> Result<Value, CloudError> {
        let mut body = json!({ "url": url, "formats": formats });
        if only_main_content {
            body["only_main_content"] = json!(true);
        }
        if !include_selectors.is_empty() {
            body["include_selectors"] = json!(include_selectors);
        }
        if !exclude_selectors.is_empty() {
            body["exclude_selectors"] = json!(exclude_selectors);
        }
        self.post("scrape", body).await
    }

    /// Convenience: scrape with `formats: ["html"]` and pull out the
    /// raw HTML string. Used by vertical extractors that want to run
    /// their own parser on antibot-bypassed HTML.
    pub async fn fetch_html(&self, url: &str) -> Result<String, CloudError> {
        let resp = self.scrape(url, &["html"], &[], &[], false).await?;
        resp.get("html")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| {
                CloudError::ParseFailed(
                    "cloud /v1/scrape returned no `html` field — check cloud API version".into(),
                )
            })
    }
}

async fn parse_cloud_response(resp: reqwest::Response) -> Result<Value, CloudError> {
    let status = resp.status();
    if status.is_success() {
        return resp
            .json()
            .await
            .map_err(|e| CloudError::ParseFailed(e.to_string()));
    }
    let body = resp.text().await.unwrap_or_default();
    Err(CloudError::from_status_and_body(status.as_u16(), body))
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// True when a fetched response body is actually a bot-protection
/// challenge page rather than the content the caller asked for.
///
/// Conservative — only fires on patterns that indicate the *entire*
/// page is a challenge, not embedded CAPTCHAs on a real content page.
pub fn is_bot_protected(html: &str, headers: &HeaderMap) -> bool {
    let html_lower = html.to_lowercase();

    // Cloudflare challenge page.
    if html_lower.contains("_cf_chl_opt") || html_lower.contains("challenge-platform") {
        return true;
    }

    // Cloudflare "Just a moment" / "Checking your browser" interstitial.
    if (html_lower.contains("just a moment") || html_lower.contains("checking your browser"))
        && html_lower.contains("cf-spinner")
    {
        return true;
    }

    // Cloudflare Turnstile. Only counts when the page is small —
    // legitimate pages embed Turnstile for signup forms etc.
    if (html_lower.contains("cf-turnstile")
        || html_lower.contains("challenges.cloudflare.com/turnstile"))
        && html.len() < 100_000
    {
        return true;
    }

    // DataDome.
    if html_lower.contains("geo.captcha-delivery.com")
        || html_lower.contains("captcha-delivery.com/captcha")
    {
        return true;
    }

    // AWS WAF.
    if html_lower.contains("awswaf-captcha") || html_lower.contains("aws-waf-client-browser") {
        return true;
    }

    // AWS WAF "Verifying your connection" interstitial (used by Trustpilot).
    // Distinct from the captcha-branded path above: the challenge page is
    // a tiny HTML shell with an `interstitial-spinner` div and no content.
    // Gating on html.len() keeps false-positives off long pages that
    // happen to mention the phrase in an unrelated context.
    if html_lower.contains("interstitial-spinner")
        && html_lower.contains("verifying your connection")
        && html.len() < 10_000
    {
        return true;
    }

    // hCaptcha *blocking* page (not just an embedded widget).
    if html_lower.contains("hcaptcha.com")
        && html_lower.contains("h-captcha")
        && html.len() < 50_000
    {
        return true;
    }

    // Cloudflare via response headers + challenge body.
    let has_cf_headers = headers.get("cf-ray").is_some() || headers.get("cf-mitigated").is_some();
    if has_cf_headers
        && (html_lower.contains("just a moment") || html_lower.contains("checking your browser"))
    {
        return true;
    }

    false
}

/// True when a page likely needs JS rendering — a large HTML document
/// with almost no extractable text + an SPA framework signature.
pub fn needs_js_rendering(word_count: usize, html: &str) -> bool {
    let has_scripts = html.contains("<script");

    // Tier 1: almost no extractable text from a large-ish page.
    if word_count < 50 && html.len() > 5_000 && has_scripts {
        return true;
    }

    // Tier 2: SPA framework markers + low content-to-HTML ratio.
    if word_count < 800 && html.len() > 50_000 && has_scripts {
        let html_lower = html.to_lowercase();
        let has_spa_marker = html_lower.contains("react-app")
            || html_lower.contains("id=\"__next\"")
            || html_lower.contains("id=\"root\"")
            || html_lower.contains("id=\"app\"")
            || html_lower.contains("__next_data__")
            || html_lower.contains("nuxt")
            || html_lower.contains("ng-app");
        if has_spa_marker {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Smart-fetch: classic flow for MCP / CLI (returns either an extraction
// or raw cloud JSON)
// ---------------------------------------------------------------------------

/// Result of [`smart_fetch`]: either a local extraction or the raw
/// cloud API response when we escalated.
pub enum SmartFetchResult {
    Local(Box<webclaw_core::ExtractionResult>),
    Cloud(Value),
}

/// Try local fetch + extract first. On bot protection or detected
/// JS-render, fall back to `cloud.scrape(...)` with the caller's
/// formats. Returns `Err(String)` so existing call sites that expect
/// stringified errors keep compiling.
///
/// Prefer [`smart_fetch_html`] for new callers — it surfaces the typed
/// [`CloudError`] so you can render precise UX.
pub async fn smart_fetch(
    client: &FetchClient,
    cloud: Option<&CloudClient>,
    url: &str,
    include_selectors: &[String],
    exclude_selectors: &[String],
    only_main_content: bool,
    formats: &[&str],
) -> Result<SmartFetchResult, String> {
    let fetch_result = tokio::time::timeout(Duration::from_secs(30), client.fetch(url))
        .await
        .map_err(|_| format!("Fetch timed out after 30s for {url}"))?
        .map_err(|e| format!("Fetch failed: {e}"))?;

    if is_bot_protected(&fetch_result.html, &fetch_result.headers) {
        info!(url, "bot protection detected, falling back to cloud API");
        return cloud_scrape_fallback(
            cloud,
            url,
            include_selectors,
            exclude_selectors,
            only_main_content,
            formats,
        )
        .await;
    }

    let options = webclaw_core::ExtractionOptions {
        include_selectors: include_selectors.to_vec(),
        exclude_selectors: exclude_selectors.to_vec(),
        only_main_content,
        include_raw_html: false,
    };
    let extraction =
        webclaw_core::extract_with_options(&fetch_result.html, Some(&fetch_result.url), &options)
            .map_err(|e| format!("Extraction failed: {e}"))?;

    if needs_js_rendering(extraction.metadata.word_count, &fetch_result.html) {
        info!(
            url,
            word_count = extraction.metadata.word_count,
            html_len = fetch_result.html.len(),
            "JS-rendered page detected, falling back to cloud API"
        );
        return cloud_scrape_fallback(
            cloud,
            url,
            include_selectors,
            exclude_selectors,
            only_main_content,
            formats,
        )
        .await;
    }

    Ok(SmartFetchResult::Local(Box::new(extraction)))
}

async fn cloud_scrape_fallback(
    cloud: Option<&CloudClient>,
    url: &str,
    include_selectors: &[String],
    exclude_selectors: &[String],
    only_main_content: bool,
    formats: &[&str],
) -> Result<SmartFetchResult, String> {
    let Some(c) = cloud else {
        return Err(CloudError::NotConfigured.to_string());
    };
    let resp = c
        .scrape(
            url,
            formats,
            include_selectors,
            exclude_selectors,
            only_main_content,
        )
        .await
        .map_err(|e| e.to_string())?;
    info!(url, "cloud API fallback successful");
    Ok(SmartFetchResult::Cloud(resp))
}

// ---------------------------------------------------------------------------
// Smart-fetch-HTML: for vertical extractors
// ---------------------------------------------------------------------------

/// Where the HTML ultimately came from — useful for callers that want
/// to track "did we fall back?" for logging or pricing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchSource {
    Local,
    Cloud,
}

/// Antibot-aware HTML fetch result. The `html` field is always populated.
pub struct FetchedHtml {
    pub html: String,
    pub final_url: String,
    pub source: FetchSource,
}

/// Try local fetch; on bot protection, escalate to the cloud's
/// `/v1/scrape` with `formats=["html"]` and return the raw HTML.
///
/// Designed for the vertical-extractor pattern where the caller has
/// its own parser and just needs bytes.
pub async fn smart_fetch_html(
    client: &FetchClient,
    cloud: Option<&CloudClient>,
    url: &str,
) -> Result<FetchedHtml, CloudError> {
    let resp = client
        .fetch(url)
        .await
        .map_err(|e| CloudError::Network(e.to_string()))?;

    if !is_bot_protected(&resp.html, &resp.headers) {
        return Ok(FetchedHtml {
            html: resp.html,
            final_url: resp.url,
            source: FetchSource::Local,
        });
    }

    let Some(c) = cloud else {
        warn!(url, "bot protection detected + no cloud client configured");
        return Err(CloudError::NotConfigured);
    };
    debug!(url, "bot protection detected, escalating to cloud");
    let html = c.fetch_html(url).await?;
    Ok(FetchedHtml {
        html,
        final_url: url.to_string(),
        source: FetchSource::Cloud,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_headers() -> HeaderMap {
        HeaderMap::new()
    }

    // --- detectors ----------------------------------------------------------

    #[test]
    fn is_bot_protected_detects_cloudflare_challenge() {
        let html = "<html><body>_cf_chl_opt loaded</body></html>";
        assert!(is_bot_protected(html, &empty_headers()));
    }

    #[test]
    fn is_bot_protected_detects_turnstile_on_short_page() {
        let html = "<div class=\"cf-turnstile\"></div>";
        assert!(is_bot_protected(html, &empty_headers()));
    }

    #[test]
    fn is_bot_protected_ignores_turnstile_on_real_content() {
        let html = format!(
            "<html><body>{}<div class=\"cf-turnstile\"></div></body></html>",
            "lots of real content ".repeat(8_000)
        );
        assert!(!is_bot_protected(&html, &empty_headers()));
    }

    #[test]
    fn is_bot_protected_detects_aws_waf_verifying_connection() {
        // The exact shape Trustpilot serves under AWS WAF.
        let html = r#"<div class="container"><div id="loading-state">
            <div class="interstitial-spinner" id="spinner"></div>
            <h1>Verifying your connection...</h1></div></div>"#;
        assert!(is_bot_protected(html, &empty_headers()));
    }

    #[test]
    fn is_bot_protected_ignores_phrase_on_real_content() {
        // A real article that happens to mention the phrase in prose
        // should not trigger the short-page detector.
        let html = format!(
            "<html><body>{}<p>Verifying your connection is tricky.</p></body></html>",
            "article text ".repeat(2_000)
        );
        assert!(!is_bot_protected(&html, &empty_headers()));
    }

    #[test]
    fn needs_js_rendering_flags_spa_skeleton() {
        let html = format!(
            "<html><body><div id=\"__next\"></div>{}</body></html>",
            "<script>x</script>".repeat(500)
        );
        assert!(needs_js_rendering(10, &html));
    }

    #[test]
    fn needs_js_rendering_passes_real_article() {
        let html = format!(
            "<html><body>{}<script>x</script></body></html>",
            "Real article text ".repeat(5_000)
        );
        assert!(!needs_js_rendering(5_000, &html));
    }

    // --- CloudError mapping -------------------------------------------------

    #[test]
    fn cloud_error_maps_401() {
        let e = CloudError::from_status_and_body(401, "invalid key".into());
        assert!(matches!(e, CloudError::Unauthorized));
        assert!(e.to_string().contains(KEYS_URL));
    }

    #[test]
    fn cloud_error_maps_402() {
        let e = CloudError::from_status_and_body(402, "{}".into());
        assert!(matches!(e, CloudError::InsufficientPlan));
        assert!(e.to_string().contains(PRICING_URL));
    }

    #[test]
    fn cloud_error_maps_429() {
        let e = CloudError::from_status_and_body(429, "slow down".into());
        assert!(matches!(e, CloudError::RateLimited));
        assert!(e.to_string().contains(PRICING_URL));
    }

    #[test]
    fn cloud_error_maps_generic_5xx() {
        let e = CloudError::from_status_and_body(503, "x".repeat(2000));
        match e {
            CloudError::ServerError { status, body } => {
                assert_eq!(status, 503);
                assert!(body.len() <= 500);
            }
            _ => panic!("expected ServerError"),
        }
    }

    #[test]
    fn not_configured_error_points_at_signup() {
        let msg = CloudError::NotConfigured.to_string();
        assert!(msg.contains(SIGNUP_URL));
        assert!(msg.contains("WEBCLAW_API_KEY"));
    }

    // --- CloudClient construction ------------------------------------------

    #[test]
    fn cloud_client_explicit_key_wins_over_env() {
        // SAFETY: this test mutates process env. Serial tests only.
        // Set env to something, pass an explicit key, explicit should win.
        // (We don't actually *call* the API, just check the struct stored
        // the right key.)
        // rustc std::env::set_var is unsafe in newer toolchains.
        unsafe {
            std::env::set_var("WEBCLAW_API_KEY", "from-env");
        }
        let client = CloudClient::new(Some("from-flag")).expect("client built");
        assert_eq!(client.api_key, "from-flag");
        unsafe {
            std::env::remove_var("WEBCLAW_API_KEY");
        }
    }

    #[test]
    fn cloud_client_none_when_empty() {
        unsafe {
            std::env::remove_var("WEBCLAW_API_KEY");
        }
        assert!(CloudClient::new(None).is_none());
        assert!(CloudClient::new(Some("")).is_none());
        assert!(CloudClient::new(Some("   ")).is_none());
    }

    #[test]
    fn cloud_client_base_url_strips_trailing_slash() {
        let c = CloudClient::with_key_and_base("k", "https://api.example.com/v1/");
        assert_eq!(c.base_url(), "https://api.example.com/v1");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // Ensure we don't slice inside a multi-byte char.
        let s = "a".repeat(10) + "é"; // é is 2 bytes
        let out = truncate(&s, 11);
        assert_eq!(out.chars().count(), 11);
    }
}
