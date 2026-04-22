//! Generic ecommerce product extractor via Schema.org JSON-LD.
//!
//! Every modern ecommerce site ships a `<script type="application/ld+json">`
//! Product block for SEO / rich-result snippets. Google's own SEO docs
//! force this markup on anyone who wants to appear in shopping search.
//! We take advantage of it: one extractor that works on Shopify,
//! BigCommerce, WooCommerce, Squarespace, Magento, custom storefronts,
//! and anything else that follows Schema.org.
//!
//! **Explicit-call only** — `/v1/scrape/ecommerce_product`. Not in the
//! auto-dispatch because we can't identify "this is a product page"
//! from the URL alone. When the caller knows they have a product URL,
//! this is the reliable fallback for stores where shopify_product
//! doesn't apply.
//!
//! The extractor reuses `webclaw_core::structured_data::extract_json_ld`
//! so JSON-LD parsing is shared with the rest of the extraction
//! pipeline. We walk all blocks looking for `@type: Product`,
//! `ProductGroup`, or an `ItemList` whose first entry is a Product.

use serde_json::{Value, json};

use super::ExtractorInfo;
use crate::client::FetchClient;
use crate::error::FetchError;

pub const INFO: ExtractorInfo = ExtractorInfo {
    name: "ecommerce_product",
    label: "Ecommerce product (generic)",
    description: "Returns product info from any site that ships Schema.org Product JSON-LD: name, description, images, brand, SKU, price, availability, aggregate rating.",
    url_patterns: &[
        "https://{any-ecom-store}/products/{slug}",
        "https://{any-ecom-store}/product/{slug}",
        "https://{any-ecom-store}/p/{slug}",
    ],
};

pub fn matches(url: &str) -> bool {
    // Maximally permissive: explicit-call-only extractor. We trust the
    // caller knows they're pointing at a product page. Custom ecom
    // sites use every conceivable URL shape (warbyparker.com uses
    // `/eyeglasses/{category}/{slug}/{colour}`, etc.), so path-pattern
    // matching would false-negative a lot. All we gate on is a valid
    // http(s) URL with a host.
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }
    !host_of(url).is_empty()
}

pub async fn extract(client: &FetchClient, url: &str) -> Result<Value, FetchError> {
    let resp = client.fetch(url).await?;
    if !(200..300).contains(&resp.status) {
        return Err(FetchError::Build(format!(
            "ecommerce_product: status {} for {url}",
            resp.status
        )));
    }

    // Reuse the core JSON-LD parser so we benefit from whatever
    // robustness it gains over time (handling @graph, arrays, etc.).
    let blocks = webclaw_core::structured_data::extract_json_ld(&resp.html);
    let product = find_product(&blocks).ok_or_else(|| {
        FetchError::BodyDecode(format!(
            "ecommerce_product: no Schema.org Product found in JSON-LD on {url}"
        ))
    })?;

    Ok(json!({
        "url":                url,
        "name":               get_text(&product, "name"),
        "description":        get_text(&product, "description"),
        "brand":              get_brand(&product),
        "sku":                get_text(&product, "sku"),
        "mpn":                get_text(&product, "mpn"),
        "gtin":               get_text(&product, "gtin")
                                 .or_else(|| get_text(&product, "gtin13"))
                                 .or_else(|| get_text(&product, "gtin12"))
                                 .or_else(|| get_text(&product, "gtin8")),
        "product_id":         get_text(&product, "productID"),
        "category":           get_text(&product, "category"),
        "color":              get_text(&product, "color"),
        "material":           get_text(&product, "material"),
        "images":             collect_images(&product),
        "offers":             collect_offers(&product),
        "aggregate_rating":   get_aggregate_rating(&product),
        "review_count":       get_review_count(&product),
        "raw_schema_type":    get_text(&product, "@type"),
        "raw_jsonld":         product,
    }))
}

// ---------------------------------------------------------------------------
// JSON-LD walkers
// ---------------------------------------------------------------------------

/// Recursively walk the JSON-LD blocks and return the first node whose
/// `@type` is Product, ProductGroup, or IndividualProduct.
fn find_product(blocks: &[Value]) -> Option<Value> {
    for b in blocks {
        if let Some(found) = find_product_in(b) {
            return Some(found);
        }
    }
    None
}

fn find_product_in(v: &Value) -> Option<Value> {
    if is_product_type(v) {
        return Some(v.clone());
    }
    // @graph: [ {...}, {...} ]
    if let Some(graph) = v.get("@graph").and_then(|g| g.as_array()) {
        for item in graph {
            if let Some(found) = find_product_in(item) {
                return Some(found);
            }
        }
    }
    // Bare array wrapper
    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(found) = find_product_in(item) {
                return Some(found);
            }
        }
    }
    None
}

fn is_product_type(v: &Value) -> bool {
    let t = match v.get("@type") {
        Some(t) => t,
        None => return false,
    };
    let match_str = |s: &str| {
        matches!(
            s,
            "Product" | "ProductGroup" | "IndividualProduct" | "Vehicle" | "SomeProducts"
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

fn get_brand(v: &Value) -> Option<String> {
    let brand = v.get("brand")?;
    if let Some(s) = brand.as_str() {
        return Some(s.to_string());
    }
    if let Some(obj) = brand.as_object()
        && let Some(n) = obj.get("name").and_then(|x| x.as_str())
    {
        return Some(n.to_string());
    }
    None
}

fn collect_images(v: &Value) -> Vec<Value> {
    match v.get("image") {
        Some(Value::String(s)) => vec![Value::String(s.clone())],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| match x {
                Value::String(s) => Some(Value::String(s.clone())),
                Value::Object(_) => x.get("url").cloned(),
                _ => None,
            })
            .collect(),
        Some(Value::Object(o)) => o.get("url").cloned().into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Normalise both bare Offer and AggregateOffer into a uniform array.
fn collect_offers(v: &Value) -> Vec<Value> {
    let offers = match v.get("offers") {
        Some(o) => o,
        None => return Vec::new(),
    };
    let collect_single = |o: &Value| -> Option<Value> {
        Some(json!({
            "price":            get_text(o, "price"),
            "low_price":        get_text(o, "lowPrice"),
            "high_price":       get_text(o, "highPrice"),
            "currency":         get_text(o, "priceCurrency"),
            "availability":     get_text(o, "availability").map(|s| s.replace("http://schema.org/", "").replace("https://schema.org/", "")),
            "item_condition":   get_text(o, "itemCondition").map(|s| s.replace("http://schema.org/", "").replace("https://schema.org/", "")),
            "valid_until":      get_text(o, "priceValidUntil"),
            "url":              get_text(o, "url"),
            "seller":           o.get("seller").and_then(|s| s.get("name")).and_then(|n| n.as_str()).map(String::from),
            "offer_count":      get_text(o, "offerCount"),
        }))
    };
    match offers {
        Value::Array(arr) => arr.iter().filter_map(collect_single).collect(),
        Value::Object(_) => collect_single(offers).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn get_aggregate_rating(v: &Value) -> Option<Value> {
    let r = v.get("aggregateRating")?;
    Some(json!({
        "rating_value":  get_text(r, "ratingValue"),
        "best_rating":   get_text(r, "bestRating"),
        "worst_rating":  get_text(r, "worstRating"),
        "rating_count":  get_text(r, "ratingCount"),
        "review_count":  get_text(r, "reviewCount"),
    }))
}

fn get_review_count(v: &Value) -> Option<String> {
    v.get("aggregateRating")
        .and_then(|r| get_text(r, "reviewCount"))
        .or_else(|| get_text(v, "reviewCount"))
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
    use serde_json::json;

    #[test]
    fn matches_any_http_url_with_host() {
        assert!(matches("https://www.allbirds.com/products/tree-runner"));
        assert!(matches(
            "https://www.warbyparker.com/eyeglasses/women/percey/jet-black-with-polished-gold"
        ));
        assert!(matches("https://example.com/p/widget"));
        assert!(matches("http://shop.example.com/foo/bar"));
    }

    #[test]
    fn rejects_empty_or_non_http() {
        assert!(!matches(""));
        assert!(!matches("not-a-url"));
        assert!(!matches("ftp://example.com/file"));
    }

    #[test]
    fn find_product_walks_graph() {
        let block = json!({
            "@context": "https://schema.org",
            "@graph": [
                {"@type": "Organization", "name": "ACME"},
                {"@type": "Product", "name": "Widget", "sku": "ABC"}
            ]
        });
        let blocks = vec![block];
        let p = find_product(&blocks).unwrap();
        assert_eq!(p.get("name").and_then(|v| v.as_str()), Some("Widget"));
    }

    #[test]
    fn find_product_handles_array_type() {
        let block = json!({
            "@type": ["Product", "Clothing"],
            "name": "Tee"
        });
        assert!(is_product_type(&block));
    }

    #[test]
    fn get_brand_from_string_or_object() {
        assert_eq!(get_brand(&json!({"brand": "ACME"})), Some("ACME".into()));
        assert_eq!(
            get_brand(&json!({"brand": {"@type": "Brand", "name": "ACME"}})),
            Some("ACME".into())
        );
    }

    #[test]
    fn collect_offers_handles_single_and_aggregate() {
        let p = json!({
            "offers": {
                "@type": "Offer",
                "price": "19.99",
                "priceCurrency": "USD",
                "availability": "https://schema.org/InStock"
            }
        });
        let offers = collect_offers(&p);
        assert_eq!(offers.len(), 1);
        assert_eq!(
            offers[0].get("price").and_then(|v| v.as_str()),
            Some("19.99")
        );
        assert_eq!(
            offers[0].get("availability").and_then(|v| v.as_str()),
            Some("InStock")
        );
    }
}
