//! Twitter/X post extractor using the public v2 API.
//!
//! Falls back to nitter.net RSS when no API key is available.
//! For full tweet data with replies, the v2 API requires Bearer token.

use serde_json::{Value, json};

use super::ExtractorInfo;
use crate::error::FetchError;
use crate::fetcher::Fetcher;

/// Maximum age (in days) of a cached guest token before we re-fetch.
const GUEST_TOKEN_MAX_AGE_SECS: i64 = 3600;

pub const INFO: ExtractorInfo = ExtractorInfo {
    name: "twitter",
    label: "Twitter/X post",
    description: "Returns tweet/post data: text, author, metrics, media, entities. Falls back to nitter RSS for unauthenticated requests.",
    url_patterns: &[
        "https://twitter.com/*",
        "https://x.com/*",
        "https://nitter.net/*",
    ],
};

pub fn matches(url: &str) -> bool {
    let host = host_of(url);
    host == "twitter.com" || host == "x.com" || host == "www.twitter.com" || host == "www.x.com"
}

pub async fn extract(client: &dyn Fetcher, url: &str) -> Result<Value, FetchError> {
    // If it's a nitter URL, use the RSS fallback
    if host_of(url) == "nitter.net" || host_of(url) == "nitter.privacydev.net" {
        return extract_via_nitter(client, url).await;
    }

    // Try v2 guest token approach for twitter.com/x.com
    extract_via_guest_token(client, url).await
}

fn host_of(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
}

fn parse_twitter_id(url: &str) -> Option<String> {
    // https://twitter.com/user/status/123456789
    // https://x.com/user/status/123456789
    let parts: Vec<&str> = url.split('/').collect();
    let mut iter = parts.iter().rev();
    // First from end should be the ID
    let id = iter.next()?;
    if id.chars().all(|c| c.is_ascii_digit()) {
        Some(id.to_string())
    } else {
        None
    }
}

fn parse_username(url: &str) -> Option<String> {
    let parts: Vec<&str> = url.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if (*part == "status" || *part == "photo" || *part == "video") && i > 0 {
            return Some(parts[i - 1].to_string());
        }
    }
    // Try username in path
    let host = host_of(url);
    if host == "twitter.com" || host == "x.com" || host == "www.twitter.com" || host == "www.x.com" {
        let path: Vec<&str> = url.split('/').filter(|s| !s.is_empty()).collect();
        if path.len() >= 2 {
            return Some(path[1].to_string());
        }
    }
    None
}

/// Fetch a guest token from Twitter to access the v2 API.
async fn get_guest_token(client: &dyn Fetcher) -> Result<String, FetchError> {
    let resp = client
        .fetch("https://api.twitter.com/1.1/guest/activate.json")
        .await?;

    if resp.status != 200 {
        return Err(FetchError::Status("guest token", resp.status));
    }

    let v: Value = serde_json::from_str(&resp.html)
        .map_err(|e| FetchError::BodyDecode(format!("guest token JSON: {e}")))?;

    v.get("guest_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| FetchError::BodyDecode("no guest_token in response".into()))
}

/// Fetch tweet via Twitter's undocumented v2 API using a guest token.
async fn extract_via_guest_token(client: &dyn Fetcher, url: &str) -> Result<Value, FetchError> {
    let tweet_id = parse_twitter_id(url)
        .ok_or_else(|| FetchError::Build(format!("twitter: cannot parse tweet ID from '{url}'")))?;

    let guest_token = get_guest_token(client).await?;

    let api_url = format!(
        "https://twitter.com/i/api/graphql/oVO0BTq4B7F_F6k0AXT4Vw/TweetDetail?variables=%7B%22focalTweetId%22%3A%22{}%22%2C%22with_rux_injections%22%3Afalse%2C%22withCommunityResponse%22%3Atrue%2C%22withBirdwatchNotes%22%3Atrue%2C%22withQuickPromoteEligibility%22%3Atrue%7D",
        tweet_id
    );

    let mut req = http::Request::builder()
        .uri(&api_url)
        .method("GET")
        .header("Authorization", "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8YteUuR1V0gFVJMx2ZqUhLFdCTKOHqJeXCCyMIDqAfwanMmwA28G8WqjeX8R4P09b6NuG6WMTqWcAQxgT2Mq29zH6rBoSHKQdAAAAAA==")
        .header("x-guest-token", &guest_token)
        .header("x-twitter-active-user", "yes")
        .header("accept", "application/json");

    let resp = client.fetch_request(req.body(()).unwrap()).await?;

    if resp.status == 401 || resp.status == 403 {
        // Fall back to nitter
        let nitter_url = format!("https://nitter.privacydev.net/i/status/{}", tweet_id);
        return extract_via_nitter(client, &nitter_url).await;
    }

    if resp.status != 200 {
        return Err(FetchError::Status("twitter v2 API", resp.status));
    }

    let v: Value = serde_json::from_str(&resp.html)
        .map_err(|e| FetchError::BodyDecode(format!("twitter v2 JSON: {e}")))?;

    // Parse the nested GraphQL response
    let tweet_result = v
        .pointer("/data/tweetResult/result")
        .or_else(|| v.pointer("/data/threaded_conversation_with_injections/0/"))

        .ok_or_else(|| FetchError::BodyDecode("could not find tweet result in response".into()))?;

    let legacy = tweet_result.pointer("/legacy");
    let core = tweet_result.pointer("/core/user_results/rslt/legacy");

    let text = legacy
        .and_then(|l| l.get("full_text"))
        .or_else(|| legacy.and_then(|l| l.get("text")))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let user_name = core.and_then(|c| c.get("name")).and_then(|v| v.as_str()).unwrap_or("");
    let user_screen = core.and_then(|c| c.get("screen_name")).and_then(|v| v.as_str()).unwrap_or("");
    let user_followers = core.and_then(|c| c.get("followers_count")).and_then(|v| v.as_i64()).unwrap_or(0);
    let user_verified = core.and_then(|c| c.get("verified")).and_then(|v| v.as_bool()).unwrap_or(false);

    let likes = legacy.and_then(|l| l.get("favorite_count")).and_then(|v| v.as_i64()).unwrap_or(0);
    let retweets = legacy.and_then(|l| l.get("retweet_count")).and_then(|v| v.as_i64()).unwrap_or(0);
    let replies = legacy.and_then(|l| l.get("reply_count")).and_then(|v| v.as_i64()).unwrap_or(0);
    let quotes = legacy.and_then(|l| l.get("quote_count")).and_then(|v| v.as_i64()).unwrap_or(0);

    let created_at = legacy.and_then(|l| l.get("created_at")).and_then(|v| v.as_str()).unwrap_or("");
    let lang = legacy.and_then(|l| l.get("lang")).and_then(|v| v.as_str()).unwrap_or("en");

    let media: Vec<Value> = legacy
        .and_then(|l| l.get("entities/media"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(json!({
                        "url": m.get("media_url_https").or_else(|| m.get("media_url")).and_then(|v| v.as_str()).unwrap_or(""),
                        "type": m.get("type").and_then(|v| v.as_str()).unwrap_or(""),
                        "alt": m.get("ext_alt_text").and_then(|v| v.as_str()).unwrap_or(""),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let hashtags: Vec<&str> = legacy
        .and_then(|l| l.get("entities/hashtags"))
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|h| h.get("text").and_then(|v| v.as_str())).collect())
        .unwrap_or_default();

    let mentions: Vec<&str> = legacy
        .and_then(|l| l.get("entities/user_mentions"))
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|m| m.get("screen_name").and_then(|v| v.as_str())).collect())
        .unwrap_or_default();

    let urls_in_tweet: Vec<&str> = legacy
        .and_then(|l| l.get("entities/urls"))
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|u| u.get("expanded_url").and_then(|v| v.as_str())).collect())
        .unwrap_or_default();

    Ok(json!({
        "url": url,
        "id": tweet_id,
        "text": text,
        "lang": lang,
        "created_at": created_at,
        "author": {
            "name": user_name,
            "username": user_screen,
            "followers": user_followers,
            "verified": user_verified,
        },
        "metrics": {
            "likes": likes,
            "retweets": retweets,
            "replies": replies,
            "quotes": quotes,
        },
        "entities": {
            "hashtags": hashtags,
            "mentions": mentions,
            "urls": urls_in_tweet,
            "media": media,
        },
        "source": if url.contains("nitter") { "nitter" } else { "twitter_v2_guest" },
    }))
}

/// Fallback: parse tweet via nitter.privacydev.net RSS or HTML.
async fn extract_via_nitter(client: &dyn Fetcher, url: &str) -> Result<Value, FetchError> {
    // Try RSS first
    let rss_url = if url.contains("/i/status/") {
        let id = url.split("/i/status/").nth(1).unwrap_or("");
        format!("https://nitter.privacydev.net/i/status/{}/rss", id)
    } else {
        url.replace("://", "://nitter.privacydev.net/").to_string()
    };

    let resp = client.fetch(&rss_url).await.ok()
        .filter(|r| r.status == 200)
        .map(|r| r.html.clone());

    if let Some(body) = resp {
        if body.contains("<item>") || body.contains("<entry>") {
            if let Ok(parsed) = parse_nitter_rss(&body, url) {
                return Ok(parsed);
            }
        }
    }

    // Fall back to HTML parsing
    let resp = client.fetch(url).await?;
    if resp.status != 200 {
        return Err(FetchError::Status("nitter", resp.status));
    }

    parse_nitter_html(&resp.html, url)
}

fn parse_nitter_rss(body: &str, orig_url: &str) -> Result<Value, FetchError> {
    let title = extract_rss_tag(body, "title").unwrap_or_default();
    let link = extract_rss_tag(body, "link").unwrap_or_else(|| orig_url.to_string());
    let description = extract_rss_tag(body, "description").unwrap_or_default();
    let pub_date = extract_rss_tag(body, "pubDate").unwrap_or_default();
    let author = extract_rss_tag(body, "dc:creator").or_else(|| extract_rss_tag(body, "author")).unwrap_or_default();

    // Strip HTML from description
    let clean_desc = description
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("<p>", "\n")
        .replace("</p>", "\n")
        .replace("<br>", "\n")
        .chars()
        .filter(|c| !c.is_ascii_control() || *c == '\n')
        .collect::<String>()
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    let id = parse_twitter_id(&link).or_else(|| parse_twitter_id(orig_url));
    let username = parse_username(&link).or_else(|| parse_username(orig_url));

    Ok(json!({
        "url": orig_url,
        "id": id,
        "text": clean_desc,
        "author": {
            "username": username,
        },
        "published_at": pub_date,
        "source": "nitter_rss",
    }))
}

fn extract_rss_tag(body: &str, tag: &str) -> Option<String> {
    let pattern = format!("<{}>", tag);
    let end_pattern = format!("</{}>", tag);
    body.find(&pattern).and_then(|start| {
        body[start..].find(&end_pattern).map(|end| {
            body[start + pattern.len()..start + end].to_string()
        })
    })
}

fn parse_nitter_html(html: &str, url: &str) -> Result<Value, FetchError> {
    // Extract text content between class="tweet-content" tags
    let text = extract_html_class(html, "tweet-content")
        .or_else(|| extract_html_class(html, "main-tweet-content"))
        .unwrap_or_default();

    // Extract username
    let username = extract_html_class(html, "username")
        .map(|s| s.trim_start_matches('@').to_string())
        .unwrap_or_default();

    // Extract date
    let date = extract_html_class(html, "tweet-date")
        .or_else(|| extract_html_class(html, "publish-time"))
        .unwrap_or_default();

    Ok(json!({
        "url": url,
        "text": text,
        "author": { "username": username },
        "date": date,
        "source": "nitter_html",
    }))
}

/// Extract text content from first HTML element with the given class.
fn extract_html_class(html: &str, class_name: &str) -> Option<String> {
    let tag_pattern = format!("class=\"{}\"", class_name);
    let start = html.find(&tag_pattern)?;
    let after_tag = &html[start..];

    // Find the parent element's content - look for closing tag after opening
    let content_start = after_tag.find('>')? + 1;
    let slice = &after_tag[content_start..];

    // Find the next closing tag for common inline/block elements
    let end_markers = ["</p>", "</div>", "</span>", "</a>", "</h1>", "</h2>", "</h3>"];
    let mut end_pos = None;
    for marker in &end_markers {
        if let Some(pos) = slice.find(marker) {
            if end_pos.map(|e| pos < e).unwrap_or(true) {
                end_pos = Some(pos);
            }
        }
    }

    end_pos.map(|pos| {
        slice[..pos]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("<br>", "\n")
            .replace("</br>", "\n")
            .chars()
            .filter(|c| !c.is_ascii_control() || *c == '\n')
            .collect::<String>()
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matches() {
        assert!(matches("https://twitter.com/user/status/123"));
        assert!(matches("https://x.com/user/status/123"));
        assert!(matches("https://nitter.net/user/status/123"));
        assert!(!matches("https://example.com/twitter/user/status/123"));
    }

    #[test]
    fn test_parse_twitter_id() {
        assert_eq!(parse_twitter_id("https://twitter.com/user/status/1234567890"), Some("1234567890".into()));
        assert_eq!(parse_twitter_id("https://x.com/user/status/9876543210"), Some("9876543210".into()));
        assert!(parse_twitter_id("https://twitter.com/user").is_none());
    }
}
