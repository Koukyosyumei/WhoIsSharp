//! newsdata.io client — fetches news articles for prediction market context.
//!
//! API: https://newsdata.io/api/1/latest
//! Auth: `apikey` query parameter
//! Rate limit: 200 requests/day on free tier — fetch conservatively (on demand
//! only, with a 5-minute TTL cache).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::Arc;

use crate::cache::TtlCache;

const BASE_URL: &str = "https://newsdata.io/api/1";
const CACHE_TTL: u64 = 300; // 5 minutes

// ─── Public types ─────────────────────────────────────────────────────────────

/// A single news article returned by newsdata.io.
#[derive(Debug, Clone)]
pub struct NewsArticle {
    pub title:       String,
    pub description: String,
    pub link:        String,
    pub source_name: String,
    /// ISO-8601 publication date string.
    pub pub_date:    String,
    /// "positive" / "negative" / "neutral" / None
    pub sentiment:   Option<String>,
    pub keywords:    Option<Vec<String>>,
    pub category:    Vec<String>,
}

impl NewsArticle {
    /// Elapsed time as a short human-readable string ("2h", "3d", etc.).
    pub fn age_label(&self) -> String {
        use chrono::{DateTime, Utc};
        let Ok(dt) = DateTime::parse_from_rfc3339(&self.pub_date)
                         .or_else(|_| {
                             // Some articles use "YYYY-MM-DD HH:MM:SS" without T/Z
                             let with_t = self.pub_date.replace(' ', "T");
                             let with_z = if with_t.ends_with('Z') { with_t } else { format!("{}Z", with_t) };
                             DateTime::parse_from_rfc3339(&with_z)
                         })
        else {
            return String::new();
        };
        let secs = (Utc::now() - dt.to_utc()).num_seconds().max(0);
        if secs < 3_600     { format!("{}m", secs / 60) }
        else if secs < 86_400 { format!("{}h", secs / 3_600) }
        else                { format!("{}d", secs / 86_400) }
    }

    /// Single-character sentiment badge: "+" / "-" / "~" / " "
    pub fn sentiment_char(&self) -> char {
        match self.sentiment.as_deref() {
            Some("positive") => '+',
            Some("negative") => '-',
            Some("neutral")  => '~',
            _                => ' ',
        }
    }
}

// ─── Client ───────────────────────────────────────────────────────────────────

pub struct NewsClient {
    http:    reqwest::Client,
    api_key: String,
    cache:   Arc<TtlCache>,
}

impl NewsClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        NewsClient {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            api_key: api_key.into(),
            cache:   Arc::new(TtlCache::new(CACHE_TTL)),
        }
    }

    /// Fetch latest news matching `query` (AND semantics, max `limit` ≤ 10).
    pub async fn fetch_latest(&self, query: &str, limit: u8) -> Result<Vec<NewsArticle>> {
        let limit = limit.min(10);
        // Only use parameters available on the free tier.
        // `removeduplicate` and `sentiment` are premium-only — omit them.
        let url = format!(
            "{}/latest?apikey={}&q={}&language=en&size={}",
            BASE_URL,
            self.api_key,
            urlencode(query),
            limit,
        );
        self.fetch_articles(&url).await
    }

    /// Fetch news contextually relevant to a prediction market title.
    ///
    /// Extracts the most informative terms from the title (removing question
    /// words and common stop-words) and uses them as the search query.
    pub async fn fetch_for_market(&self, market_title: &str, limit: u8) -> Result<Vec<NewsArticle>> {
        let query = market_query(market_title);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_latest(&query, limit).await
    }

    async fn fetch_articles(&self, url: &str) -> Result<Vec<NewsArticle>> {
        // Cache hit
        if let Some(body) = self.cache.get(url).await {
            return parse_news_body(&body);
        }

        let resp = crate::http::retry_get(&self.http, url).await
            .context("newsdata.io request failed")?;

        if resp.status() == 429 {
            anyhow::bail!("newsdata.io rate limit exceeded — try again later");
        }
        if !resp.status().is_success() {
            anyhow::bail!("newsdata.io HTTP {}", resp.status());
        }

        let body = resp.text().await.context("Failed to read newsdata.io body")?;

        // Validate before caching — don't cache error responses.
        let articles = parse_news_body(&body)?;

        self.cache.set(url.to_string(), body).await;
        Ok(articles)
    }
}

// ─── Query extraction ─────────────────────────────────────────────────────────

const STOP_WORDS: &[&str] = &[
    "will", "the", "a", "an", "be", "is", "are", "was", "were", "been",
    "to", "of", "in", "on", "at", "by", "for", "with", "from", "into",
    "do", "does", "did", "have", "has", "had",
    "this", "that", "these", "those",
    "or", "and", "but", "nor", "so", "yet",
    "if", "when", "before", "after", "since", "until", "while",
    "over", "under", "more", "than", "up", "down",
    "any", "all", "each", "no", "not",
    "his", "her", "its", "their", "our", "your", "my",
    "how", "what", "who", "why", "where", "which",
    "first", "last", "new", "next", "win", "reach", "hit",
    "least", "most", "least", "ever", "2025", "2026", "2027",
];

/// Extract the 4 most informative terms from a market title for use as a
/// newsdata.io search query.  Returns an empty string if nothing is left
/// after stop-word removal.
fn market_query(title: &str) -> String {
    title
        .chars()
        .map(|c| if c.is_alphabetic() || c.is_whitespace() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|w| {
            let l = w.to_lowercase();
            w.len() >= 3 && !STOP_WORDS.contains(&l.as_str())
        })
        .take(4)
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── Raw JSON deserialization ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct NewsResponse {
    #[serde(default)]
    results: Vec<RawArticle>,
}

#[derive(Deserialize)]
struct RawArticle {
    #[serde(default)]
    title:       String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    link:        String,
    #[serde(rename = "source_name", default)]
    source_name: String,
    #[serde(rename = "pubDate", default)]
    pub_date:    String,
    #[serde(default)]
    sentiment:   Option<String>,
    #[serde(default)]
    keywords:    Option<Vec<String>>,
    #[serde(default)]
    category:    Option<Vec<String>>,
}

fn raw_to_article(r: RawArticle) -> NewsArticle {
    NewsArticle {
        title:       r.title,
        description: r.description.unwrap_or_default(),
        link:        r.link,
        source_name: r.source_name,
        pub_date:    r.pub_date,
        sentiment:   r.sentiment,
        keywords:    r.keywords,
        category:    r.category.unwrap_or_default(),
    }
}

/// Parse a newsdata.io JSON response body.
///
/// Handles the case where the API returns an error object in `results` instead
/// of the normal array (e.g. when a premium-only parameter is supplied).
fn parse_news_body(body: &str) -> Result<Vec<NewsArticle>> {
    // First parse as a raw JSON value so we can inspect the structure.
    let v: serde_json::Value = serde_json::from_str(body)
        .context("Failed to parse newsdata.io response as JSON")?;

    // Check top-level status field.
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("unknown");
    if status != "success" {
        // Try to extract a human-readable message from the error object.
        let msg = v.get("results")
            .and_then(|r| r.get("message"))
            .and_then(|m| m.as_str())
            .or_else(|| v.get("message").and_then(|m| m.as_str()))
            .unwrap_or("unknown API error");
        anyhow::bail!("newsdata.io error: {}", msg);
    }

    // Now deserialize the typed structure.
    let resp: NewsResponse = serde_json::from_value(v)
        .context("Failed to deserialize newsdata.io response")?;
    Ok(resp.results.into_iter().map(raw_to_article).collect())
}

// ─── Minimal URL percent-encoding (letters/digits/- safe) ────────────────────

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b' ' => {
                if b == b' ' { out.push('+'); } else { out.push(b as char); }
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_query_extracts_key_terms() {
        let q = market_query("Will Donald Trump impose tariffs on China before June 2026?");
        // "Will", "on", "before", years → stripped; "Donald", "Trump", "impose", "tariffs", "China" → kept (take 4)
        assert_eq!(q, "Donald Trump impose tariffs");
    }

    #[test]
    fn market_query_short_words_filtered() {
        // "be", "a", "do", "it" all filtered — must be ≥3 chars and not stop-word
        let q = market_query("Will it be done");
        assert!(q.is_empty() || !q.contains("it"));
    }

    #[test]
    fn age_label_recent() {
        // pubDate in the future or very recent should give "0m"
        let a = NewsArticle {
            title: String::new(), description: String::new(),
            link: String::new(), source_name: String::new(),
            pub_date: chrono::Utc::now().to_rfc3339(),
            sentiment: None, keywords: None, category: vec![],
        };
        let label = a.age_label();
        assert!(label.ends_with('m') || label.ends_with('h') || label.ends_with('d'));
    }

    #[test]
    fn sentiment_char_mapping() {
        let mut a = NewsArticle {
            title: String::new(), description: String::new(),
            link: String::new(), source_name: String::new(),
            pub_date: String::new(), keywords: None, category: vec![],
            sentiment: Some("positive".into()),
        };
        assert_eq!(a.sentiment_char(), '+');
        a.sentiment = Some("negative".into());
        assert_eq!(a.sentiment_char(), '-');
        a.sentiment = Some("neutral".into());
        assert_eq!(a.sentiment_char(), '~');
        a.sentiment = None;
        assert_eq!(a.sentiment_char(), ' ');
    }
}
