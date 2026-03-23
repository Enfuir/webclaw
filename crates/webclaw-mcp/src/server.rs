/// MCP server implementation for webclaw.
/// Exposes web extraction capabilities as tools for AI agents.
///
/// Uses a local-first architecture: fetches pages directly, then falls back
/// to the webclaw cloud API (api.webclaw.io) when bot protection or
/// JS rendering is detected. Set WEBCLAW_API_KEY for automatic fallback.
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde_json::json;
use tracing::{info, warn};

use crate::cloud::{self, CloudClient, SmartFetchResult};
use crate::tools::*;

pub struct WebclawMcp {
    tool_router: ToolRouter<Self>,
    fetch_client: Arc<webclaw_fetch::FetchClient>,
    llm_chain: Option<webclaw_llm::ProviderChain>,
    cloud: Option<CloudClient>,
}

/// Parse a browser string into a BrowserProfile.
fn parse_browser(browser: Option<&str>) -> webclaw_fetch::BrowserProfile {
    match browser {
        Some("firefox") => webclaw_fetch::BrowserProfile::Firefox,
        Some("random") => webclaw_fetch::BrowserProfile::Random,
        _ => webclaw_fetch::BrowserProfile::Chrome,
    }
}

#[tool_router]
impl WebclawMcp {
    pub async fn new() -> Self {
        let mut config = webclaw_fetch::FetchConfig::default();

        // Auto-load proxies.txt if present
        if std::path::Path::new("proxies.txt").exists()
            && let Ok(pool) = webclaw_fetch::parse_proxy_file("proxies.txt")
            && !pool.is_empty()
        {
            info!(count = pool.len(), "loaded proxy pool from proxies.txt");
            config.proxy_pool = pool;
        }

        let fetch_client =
            webclaw_fetch::FetchClient::new(config).expect("failed to build FetchClient");

        let chain = webclaw_llm::ProviderChain::default().await;
        let llm_chain = if chain.is_empty() {
            warn!("no LLM providers available -- extract/summarize tools will fail");
            None
        } else {
            info!(providers = chain.len(), "LLM provider chain ready");
            Some(chain)
        };

        let cloud = CloudClient::from_env();
        if cloud.is_some() {
            info!("cloud API fallback enabled (WEBCLAW_API_KEY set)");
        } else {
            warn!(
                "WEBCLAW_API_KEY not set -- bot-protected sites will return challenge pages. \
                 Get a key at https://webclaw.io"
            );
        }

        Self {
            tool_router: Self::tool_router(),
            fetch_client: Arc::new(fetch_client),
            llm_chain,
            cloud,
        }
    }

    /// Helper: smart fetch with LLM format for extract/summarize tools.
    async fn smart_fetch_llm(&self, url: &str) -> Result<SmartFetchResult, String> {
        cloud::smart_fetch(
            &self.fetch_client,
            self.cloud.as_ref(),
            url,
            &[],
            &[],
            false,
            &["llm", "markdown"],
        )
        .await
    }

    /// Scrape a single URL and extract its content as markdown, LLM-optimized text, plain text, or full JSON.
    /// Automatically falls back to the webclaw cloud API when bot protection or JS rendering is detected.
    #[tool]
    async fn scrape(&self, Parameters(params): Parameters<ScrapeParams>) -> Result<String, String> {
        let format = params.format.as_deref().unwrap_or("markdown");
        let browser = parse_browser(params.browser.as_deref());
        let include = params.include_selectors.unwrap_or_default();
        let exclude = params.exclude_selectors.unwrap_or_default();
        let main_only = params.only_main_content.unwrap_or(false);

        // Use a custom client if a non-default browser is requested
        let is_default_browser = matches!(browser, webclaw_fetch::BrowserProfile::Chrome);
        let custom_client;
        let client: &webclaw_fetch::FetchClient = if is_default_browser {
            &self.fetch_client
        } else {
            let config = webclaw_fetch::FetchConfig {
                browser,
                ..Default::default()
            };
            custom_client = webclaw_fetch::FetchClient::new(config)
                .map_err(|e| format!("Failed to build client: {e}"))?;
            &custom_client
        };

        let formats = [format];
        let result = cloud::smart_fetch(
            client,
            self.cloud.as_ref(),
            &params.url,
            &include,
            &exclude,
            main_only,
            &formats,
        )
        .await?;

        match result {
            SmartFetchResult::Local(extraction) => {
                let output = match format {
                    "llm" => webclaw_core::to_llm_text(&extraction, Some(&params.url)),
                    "text" => extraction.content.plain_text,
                    "json" => serde_json::to_string_pretty(&extraction).unwrap_or_default(),
                    _ => extraction.content.markdown,
                };
                Ok(output)
            }
            SmartFetchResult::Cloud(resp) => {
                // Extract the requested format from the API response
                let content = resp
                    .get(format)
                    .or_else(|| resp.get("markdown"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if content.is_empty() {
                    // Return full JSON if no content in the expected format
                    Ok(serde_json::to_string_pretty(&resp).unwrap_or_default())
                } else {
                    Ok(content.to_string())
                }
            }
        }
    }

    /// Crawl a website starting from a seed URL, following links breadth-first up to a configurable depth and page limit.
    #[tool]
    async fn crawl(&self, Parameters(params): Parameters<CrawlParams>) -> Result<String, String> {
        let format = params.format.as_deref().unwrap_or("markdown");

        let config = webclaw_fetch::CrawlConfig {
            max_depth: params.depth.unwrap_or(2) as usize,
            max_pages: params.max_pages.unwrap_or(50),
            concurrency: params.concurrency.unwrap_or(5),
            use_sitemap: params.use_sitemap.unwrap_or(false),
            ..Default::default()
        };

        let crawler = webclaw_fetch::Crawler::new(&params.url, config)
            .map_err(|e| format!("Crawler init failed: {e}"))?;

        let result = crawler.crawl(&params.url).await;

        let mut output = format!(
            "Crawled {} pages ({} ok, {} errors) in {:.1}s\n\n",
            result.total, result.ok, result.errors, result.elapsed_secs
        );

        for page in &result.pages {
            output.push_str(&format!("--- {} (depth {}) ---\n", page.url, page.depth));
            if let Some(ref extraction) = page.extraction {
                let content = match format {
                    "llm" => webclaw_core::to_llm_text(extraction, Some(&page.url)),
                    "text" => extraction.content.plain_text.clone(),
                    _ => extraction.content.markdown.clone(),
                };
                output.push_str(&content);
            } else if let Some(ref err) = page.error {
                output.push_str(&format!("Error: {err}"));
            }
            output.push_str("\n\n");
        }

        Ok(output)
    }

    /// Discover URLs from a website's sitemaps (robots.txt + sitemap.xml).
    #[tool]
    async fn map(&self, Parameters(params): Parameters<MapParams>) -> Result<String, String> {
        let entries = webclaw_fetch::sitemap::discover(&self.fetch_client, &params.url)
            .await
            .map_err(|e| format!("Sitemap discovery failed: {e}"))?;

        let urls: Vec<&str> = entries.iter().map(|e| e.url.as_str()).collect();
        Ok(format!(
            "Discovered {} URLs:\n\n{}",
            urls.len(),
            urls.join("\n")
        ))
    }

    /// Extract content from multiple URLs concurrently.
    #[tool]
    async fn batch(&self, Parameters(params): Parameters<BatchParams>) -> Result<String, String> {
        if params.urls.is_empty() {
            return Err("urls must not be empty".into());
        }

        let format = params.format.as_deref().unwrap_or("markdown");
        let concurrency = params.concurrency.unwrap_or(5);
        let url_refs: Vec<&str> = params.urls.iter().map(String::as_str).collect();

        let results = self
            .fetch_client
            .fetch_and_extract_batch(&url_refs, concurrency)
            .await;

        let mut output = format!("Extracted {} URLs:\n\n", results.len());

        for r in &results {
            output.push_str(&format!("--- {} ---\n", r.url));
            match &r.result {
                Ok(extraction) => {
                    let content = match format {
                        "llm" => webclaw_core::to_llm_text(extraction, Some(&r.url)),
                        "text" => extraction.content.plain_text.clone(),
                        _ => extraction.content.markdown.clone(),
                    };
                    output.push_str(&content);
                }
                Err(e) => {
                    output.push_str(&format!("Error: {e}"));
                }
            }
            output.push_str("\n\n");
        }

        Ok(output)
    }

    /// Extract structured data from a web page using an LLM. Provide either a JSON schema or a natural language prompt.
    /// Automatically falls back to the webclaw cloud API when bot protection is detected.
    #[tool]
    async fn extract(
        &self,
        Parameters(params): Parameters<ExtractParams>,
    ) -> Result<String, String> {
        let chain = self.llm_chain.as_ref().ok_or(
            "No LLM providers available. Set OPENAI_API_KEY or ANTHROPIC_API_KEY, or run Ollama locally.",
        )?;

        if params.schema.is_none() && params.prompt.is_none() {
            return Err("Either 'schema' or 'prompt' is required for extraction.".into());
        }

        // For extract, if we get a cloud fallback we call the cloud extract endpoint directly
        let llm_content = match self.smart_fetch_llm(&params.url).await? {
            SmartFetchResult::Local(extraction) => {
                webclaw_core::to_llm_text(&extraction, Some(&params.url))
            }
            SmartFetchResult::Cloud(resp) => {
                // Use the LLM format from cloud, fall back to markdown
                resp.get("llm")
                    .or_else(|| resp.get("markdown"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            }
        };

        let data = if let Some(ref schema) = params.schema {
            webclaw_llm::extract::extract_json(&llm_content, schema, chain, None)
                .await
                .map_err(|e| format!("LLM extraction failed: {e}"))?
        } else {
            let prompt = params.prompt.as_deref().unwrap();
            webclaw_llm::extract::extract_with_prompt(&llm_content, prompt, chain, None)
                .await
                .map_err(|e| format!("LLM extraction failed: {e}"))?
        };

        Ok(serde_json::to_string_pretty(&data).unwrap_or_default())
    }

    /// Summarize the content of a web page using an LLM.
    /// Automatically falls back to the webclaw cloud API when bot protection is detected.
    #[tool]
    async fn summarize(
        &self,
        Parameters(params): Parameters<SummarizeParams>,
    ) -> Result<String, String> {
        let chain = self.llm_chain.as_ref().ok_or(
            "No LLM providers available. Set OPENAI_API_KEY or ANTHROPIC_API_KEY, or run Ollama locally.",
        )?;

        let llm_content = match self.smart_fetch_llm(&params.url).await? {
            SmartFetchResult::Local(extraction) => {
                webclaw_core::to_llm_text(&extraction, Some(&params.url))
            }
            SmartFetchResult::Cloud(resp) => resp
                .get("llm")
                .or_else(|| resp.get("markdown"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        };

        webclaw_llm::summarize::summarize(&llm_content, params.max_sentences, chain, None)
            .await
            .map_err(|e| format!("Summarization failed: {e}"))
    }

    /// Compare the current content of a URL against a previous extraction snapshot, showing what changed.
    /// Automatically falls back to the webclaw cloud API when bot protection is detected.
    #[tool]
    async fn diff(&self, Parameters(params): Parameters<DiffParams>) -> Result<String, String> {
        let previous: webclaw_core::ExtractionResult =
            serde_json::from_str(&params.previous_snapshot)
                .map_err(|e| format!("Failed to parse previous_snapshot JSON: {e}"))?;

        let result = cloud::smart_fetch(
            &self.fetch_client,
            self.cloud.as_ref(),
            &params.url,
            &[],
            &[],
            false,
            &["markdown"],
        )
        .await?;

        match result {
            SmartFetchResult::Local(current) => {
                let content_diff = webclaw_core::diff::diff(&previous, &current);
                Ok(serde_json::to_string_pretty(&content_diff).unwrap_or_default())
            }
            SmartFetchResult::Cloud(resp) => {
                // Can't do local diff with cloud content, return the cloud response directly
                Ok(serde_json::to_string_pretty(&resp).unwrap_or_default())
            }
        }
    }

    /// Extract brand identity (colors, fonts, logo, favicon) from a website's HTML and CSS.
    /// Automatically falls back to the webclaw cloud API when bot protection is detected.
    #[tool]
    async fn brand(&self, Parameters(params): Parameters<BrandParams>) -> Result<String, String> {
        let fetch_result = self
            .fetch_client
            .fetch(&params.url)
            .await
            .map_err(|e| format!("Fetch failed: {e}"))?;

        // Check for bot protection before extracting brand
        if cloud::is_bot_protected(&fetch_result.html, &fetch_result.headers) {
            if let Some(ref c) = self.cloud {
                let resp = c
                    .post("brand", serde_json::json!({"url": params.url}))
                    .await?;
                return Ok(serde_json::to_string_pretty(&resp).unwrap_or_default());
            } else {
                return Err(format!(
                    "Bot protection detected on {}. Set WEBCLAW_API_KEY for automatic cloud bypass. \
                     Get a key at https://webclaw.io",
                    params.url
                ));
            }
        }

        let identity =
            webclaw_core::brand::extract_brand(&fetch_result.html, Some(&fetch_result.url));

        Ok(serde_json::to_string_pretty(&identity).unwrap_or_default())
    }

    /// Run a deep research investigation on a topic or question. Requires WEBCLAW_API_KEY.
    /// Starts an async research job on the webclaw cloud API, then polls until complete.
    #[tool]
    async fn research(
        &self,
        Parameters(params): Parameters<ResearchParams>,
    ) -> Result<String, String> {
        let cloud = self
            .cloud
            .as_ref()
            .ok_or("Research requires WEBCLAW_API_KEY. Get a key at https://webclaw.io")?;

        let mut body = json!({ "query": params.query });
        if let Some(deep) = params.deep {
            body["deep"] = json!(deep);
        }
        if let Some(ref topic) = params.topic {
            body["topic"] = json!(topic);
        }

        // Start the research job
        let start_resp = cloud.post("research", body).await?;
        let job_id = start_resp
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("Research API did not return a job ID")?
            .to_string();

        info!(job_id = %job_id, "research job started, polling for completion");

        // Poll until completed or failed
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            let status_resp = cloud.get(&format!("research/{job_id}")).await?;
            let status = status_resp
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            match status {
                "completed" => {
                    let report = status_resp
                        .get("report")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    if report.is_empty() {
                        return Ok(serde_json::to_string_pretty(&status_resp).unwrap_or_default());
                    }
                    return Ok(report.to_string());
                }
                "failed" => {
                    let error = status_resp
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    return Err(format!("Research job failed: {error}"));
                }
                _ => {
                    // Still processing, continue polling
                }
            }
        }
    }

    /// Search the web for a query and return structured results. Requires WEBCLAW_API_KEY.
    #[tool]
    async fn search(&self, Parameters(params): Parameters<SearchParams>) -> Result<String, String> {
        let cloud = self
            .cloud
            .as_ref()
            .ok_or("Search requires WEBCLAW_API_KEY. Get a key at https://webclaw.io")?;

        let mut body = json!({ "query": params.query });
        if let Some(num) = params.num_results {
            body["num_results"] = json!(num);
        }

        let resp = cloud.post("search", body).await?;

        // Format results for readability
        if let Some(results) = resp.get("results").and_then(|v| v.as_array()) {
            let mut output = format!("Found {} results:\n\n", results.len());
            for (i, result) in results.iter().enumerate() {
                let title = result.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let url = result.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let snippet = result
                    .get("snippet")
                    .or_else(|| result.get("description"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                output.push_str(&format!(
                    "{}. {}\n   {}\n   {}\n\n",
                    i + 1,
                    title,
                    url,
                    snippet
                ));
            }
            Ok(output)
        } else {
            // Fallback: return raw JSON if unexpected shape
            Ok(serde_json::to_string_pretty(&resp).unwrap_or_default())
        }
    }
}

#[tool_handler]
impl ServerHandler for WebclawMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(String::from(
                "Webclaw MCP server -- web content extraction for AI agents. \
                 Tools: scrape, crawl, map, batch, extract, summarize, diff, brand, research, search.",
            ))
    }
}
