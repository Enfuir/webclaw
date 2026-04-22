//! Trustpilot company reviews extractor.
//!
//! Trustpilot pages at `trustpilot.com/review/{domain}` embed a rich
//! JSON-LD `LocalBusiness` / `Organization` block with aggregate
//! rating + up to 20 recent reviews. No auth, no antibot for the
//! page HTML itself.
//!
//! Auto-dispatch safe because the host is unique.

use serde_json::{Value, json};

use super::ExtractorInfo;
use crate::client::FetchClient;
use crate::error::FetchError;

pub const INFO: ExtractorInfo = ExtractorInfo {
    name: "trustpilot_reviews",
    label: "Trustpilot reviews",
    description: "Returns company aggregate rating + recent reviews for a business on Trustpilot.",
    url_patterns: &["https://www.trustpilot.com/review/{domain}"],
};

pub fn matches(url: &str) -> bool {
    let host = host_of(url);
    if !matches!(host, "www.trustpilot.com" | "trustpilot.com") {
        return false;
    }
    url.contains("/review/")
}

pub async fn extract(client: &FetchClient, url: &str) -> Result<Value, FetchError> {
    let resp = client.fetch(url).await?;
    if !(200..300).contains(&resp.status) {
        return Err(FetchError::Build(format!(
            "trustpilot_reviews: status {} for {url}",
            resp.status
        )));
    }

    let blocks = webclaw_core::structured_data::extract_json_ld(&resp.html);
    let business = find_business(&blocks).ok_or_else(|| {
        FetchError::BodyDecode(format!(
            "trustpilot_reviews: no Organization/LocalBusiness JSON-LD on {url}"
        ))
    })?;

    let aggregate_rating = business.get("aggregateRating").map(|r| {
        json!({
            "rating_value":  get_text(r, "ratingValue"),
            "best_rating":   get_text(r, "bestRating"),
            "review_count":  get_text(r, "reviewCount"),
        })
    });

    let reviews: Vec<Value> = business
        .get("review")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .map(|r| {
                    json!({
                        "author":         r.get("author")
                                              .and_then(|a| a.get("name"))
                                              .and_then(|n| n.as_str())
                                              .map(String::from)
                                              .or_else(|| r.get("author").and_then(|a| a.as_str()).map(String::from)),
                        "date_published": get_text(r, "datePublished"),
                        "name":           get_text(r, "name"),
                        "body":           get_text(r, "reviewBody"),
                        "rating_value":   r.get("reviewRating")
                                              .and_then(|rr| rr.get("ratingValue"))
                                              .and_then(|v| v.as_str().map(String::from)
                                                  .or_else(|| v.as_f64().map(|n| n.to_string()))),
                        "language":       get_text(r, "inLanguage"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(json!({
        "url":              url,
        "name":             get_text(&business, "name"),
        "description":      get_text(&business, "description"),
        "logo":             business.get("logo").and_then(|l| l.as_str()).map(String::from)
                                .or_else(|| business.get("logo").and_then(|l| l.get("url")).and_then(|v| v.as_str()).map(String::from)),
        "telephone":        get_text(&business, "telephone"),
        "address":          business.get("address").cloned(),
        "same_as":          business.get("sameAs").cloned(),
        "aggregate_rating": aggregate_rating,
        "review_count_listed": reviews.len(),
        "reviews":          reviews,
        "business_schema":  business.get("@type").cloned(),
    }))
}

// ---------------------------------------------------------------------------
// JSON-LD walker — same pattern as ecommerce_product
// ---------------------------------------------------------------------------

fn find_business(blocks: &[Value]) -> Option<Value> {
    for b in blocks {
        if let Some(found) = find_business_in(b) {
            return Some(found);
        }
    }
    None
}

fn find_business_in(v: &Value) -> Option<Value> {
    if is_business_type(v) {
        return Some(v.clone());
    }
    if let Some(graph) = v.get("@graph").and_then(|g| g.as_array()) {
        for item in graph {
            if let Some(found) = find_business_in(item) {
                return Some(found);
            }
        }
    }
    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(found) = find_business_in(item) {
                return Some(found);
            }
        }
    }
    None
}

fn is_business_type(v: &Value) -> bool {
    let t = match v.get("@type") {
        Some(t) => t,
        None => return false,
    };
    let match_str = |s: &str| {
        matches!(
            s,
            "Organization"
                | "LocalBusiness"
                | "Corporation"
                | "OnlineBusiness"
                | "Store"
                | "Service"
        )
    };
    match t {
        Value::String(s) => match_str(s),
        Value::Array(arr) => arr.iter().any(|x| x.as_str().is_some_and(match_str)),
        _ => false,
    }
}

fn get_text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| match x {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn host_of(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_trustpilot_review_urls() {
        assert!(matches("https://www.trustpilot.com/review/stripe.com"));
        assert!(matches("https://trustpilot.com/review/example.com"));
        assert!(!matches("https://www.trustpilot.com/"));
        assert!(!matches("https://example.com/review/foo"));
    }

    #[test]
    fn is_business_type_handles_variants() {
        use serde_json::json;
        assert!(is_business_type(&json!({"@type": "Organization"})));
        assert!(is_business_type(&json!({"@type": "LocalBusiness"})));
        assert!(is_business_type(
            &json!({"@type": ["Organization", "Corporation"]})
        ));
        assert!(!is_business_type(&json!({"@type": "Product"})));
    }
}
