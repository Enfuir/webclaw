/// Cloud API client for automatic fallback when local extraction fails.
///
/// When WEBCLAW_API_KEY is set (or --api-key is passed), the CLI can fall back
/// to api.webclaw.io for bot-protected or JS-rendered sites. With --cloud flag,
/// all requests go through the cloud API directly.
use serde_json::{Value, json};

const API_BASE: &str = "https://api.webclaw.io/v1";

pub struct CloudClient {
    api_key: String,
    http: reqwest::Client,
}

impl CloudClient {
    /// Create from explicit key or WEBCLAW_API_KEY env var.
    pub fn new(explicit_key: Option<&str>) -> Option<Self> {
        let key = explicit_key
            .map(String::from)
            .or_else(|| std::env::var("WEBCLAW_API_KEY").ok())
            .filter(|k| !k.is_empty())?;

        Some(Self {
            api_key: key,
            http: reqwest::Client::new(),
        })
    }

    /// Scrape via the cloud API.
    pub async fn scrape(
        &self,
        url: &str,
        formats: &[&str],
        include_selectors: &[String],
        exclude_selectors: &[String],
        only_main_content: bool,
    ) -> Result<Value, String> {
        let mut body = json!({
            "url": url,
            "formats": formats,
        });
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

    /// Summarize via cloud API.
    pub async fn summarize(
        &self,
        url: &str,
        max_sentences: Option<usize>,
    ) -> Result<Value, String> {
        let mut body = json!({ "url": url });
        if let Some(n) = max_sentences {
            body["max_sentences"] = json!(n);
        }
        self.post("summarize", body).await
    }

    /// Brand extraction via cloud API.
    pub async fn brand(&self, url: &str) -> Result<Value, String> {
        self.post("brand", json!({ "url": url })).await
    }

    /// Diff via cloud API.
    pub async fn diff(&self, url: &str) -> Result<Value, String> {
        self.post("diff", json!({ "url": url })).await
    }

    /// Extract via cloud API.
    pub async fn extract(
        &self,
        url: &str,
        schema: Option<&str>,
        prompt: Option<&str>,
    ) -> Result<Value, String> {
        let mut body = json!({ "url": url });
        if let Some(s) = schema {
            body["schema"] = serde_json::from_str(s).unwrap_or(json!(s));
        }
        if let Some(p) = prompt {
            body["prompt"] = json!(p);
        }
        self.post("extract", body).await
    }

    async fn post(&self, endpoint: &str, body: Value) -> Result<Value, String> {
        let resp = self
            .http
            .post(format!("{API_BASE}/{endpoint}"))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| format!("cloud API request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("cloud API error {status}: {text}"));
        }

        resp.json::<Value>()
            .await
            .map_err(|e| format!("cloud API response parse failed: {e}"))
    }
}

/// Check if HTML is a bot protection challenge page.
pub fn is_bot_protected(html: &str) -> bool {
    let html_lower = html.to_lowercase();

    // Cloudflare
    if html_lower.contains("_cf_chl_opt") || html_lower.contains("challenge-platform") {
        return true;
    }
    if (html_lower.contains("just a moment") || html_lower.contains("checking your browser"))
        && html_lower.contains("cf-spinner")
    {
        return true;
    }
    if (html_lower.contains("cf-turnstile")
        || html_lower.contains("challenges.cloudflare.com/turnstile"))
        && html.len() < 100_000
    {
        return true;
    }

    // DataDome
    if html_lower.contains("geo.captcha-delivery.com") {
        return true;
    }

    // AWS WAF
    if html_lower.contains("awswaf-captcha") {
        return true;
    }

    false
}

/// Check if a page likely needs JS rendering.
pub fn needs_js_rendering(word_count: usize, html: &str) -> bool {
    let has_scripts = html.contains("<script");

    if word_count < 50 && html.len() > 5_000 && has_scripts {
        return true;
    }

    if word_count < 800 && html.len() > 50_000 && has_scripts {
        let html_lower = html.to_lowercase();
        if html_lower.contains("react-app")
            || html_lower.contains("id=\"__next\"")
            || html_lower.contains("id=\"root\"")
            || html_lower.contains("id=\"app\"")
        {
            return true;
        }
    }

    false
}
