//! Polymarket API client.
//!
//! Gamma API  (public): https://gamma-api.polymarket.com
//! CLOB API   (public): https://clob.polymarket.com
//!
//! Notes from polymarket-cli SDK reference:
//!   - `outcomePrices` arrives as either a real JSON array OR a JSON-encoded
//!     string ("[\\"0.65\\",\\"0.35\\"]"). Both forms are handled below.
//!   - Numeric fields come in two variants: `volume` (may be absent or a raw
//!     number) and `volumeNum` (a numeric string like "1500000"). We prefer
//!     `volumeNum`/`liquidityNum` and fall back to `volume`/`liquidity`.
//!   - Token IDs for the CLOB can come from either the `tokens` array (objects
//!     with `outcome` + `token_id`) or the flat `clobTokenIds` array (same
//!     order as `outcomes`). We try `tokens` first, then `clobTokenIds`.

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer};

use super::{Candle, Market, Orderbook, Platform, PriceLevel};

const GAMMA_BASE: &str = "https://gamma-api.polymarket.com";
const CLOB_BASE:  &str = "https://clob.polymarket.com";

pub struct PolymarketClient {
    http: reqwest::Client,
}

impl PolymarketClient {
    pub fn new() -> Self {
        PolymarketClient {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        }
    }

    // ─── Gamma API ────────────────────────────────────────────────────────────

    pub async fn fetch_markets(
        &self,
        limit: u32,
        search: Option<&str>,
        tag: Option<&str>,
    ) -> Result<Vec<Market>> {
        let mut url = format!(
            "{}/markets?active=true&closed=false&limit={}&order=volume&ascending=false",
            GAMMA_BASE, limit
        );
        if let Some(q) = search {
            url.push_str(&format!("&_q={}", urlencoding::encode(q)));
        }
        if let Some(t) = tag {
            url.push_str(&format!("&tag={}", urlencoding::encode(t)));
        }

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("Polymarket Gamma /markets request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Polymarket /markets error {}: {}", status, body);
        }

        let raw: Vec<GammaMarket> =
            resp.json().await.context("Failed to parse Polymarket /markets")?;

        Ok(raw.into_iter().filter_map(gamma_to_market).collect())
    }

    pub async fn fetch_events(&self, limit: u32) -> Result<Vec<super::Event>> {
        let url = format!(
            "{}/events?active=true&closed=false&limit={}&order=volume&ascending=false",
            GAMMA_BASE, limit
        );

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("Polymarket /events request failed")?;

        if !resp.status().is_success() {
            anyhow::bail!("Polymarket /events error {}", resp.status());
        }

        let raw: Vec<GammaEvent> =
            resp.json().await.context("Failed to parse Polymarket /events")?;

        Ok(raw.into_iter().map(|e| super::Event {
            id:           e.id,
            platform:     Platform::Polymarket,
            title:        e.title,
            category:     e.category,
            market_count: e.markets.len(),
            description:  e.description,
        }).collect())
    }

    // ─── CLOB API ─────────────────────────────────────────────────────────────

    pub async fn fetch_orderbook(&self, token_id: &str) -> Result<Orderbook> {
        let url = format!("{}/order-book?token_id={}", CLOB_BASE, token_id);

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("Polymarket /order-book request failed")?;

        if !resp.status().is_success() {
            anyhow::bail!("Polymarket /order-book error {}", resp.status());
        }

        let raw: ClobOrderbook =
            resp.json().await.context("Failed to parse Polymarket /order-book")?;

        let mut bids: Vec<PriceLevel> = raw
            .bids
            .into_iter()
            .filter_map(|b| {
                let p: f64 = b.price.parse().ok()?;
                let s: f64 = b.size.parse().ok()?;
                Some(PriceLevel { price: p, size: s })
            })
            .collect();
        bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));

        let mut asks: Vec<PriceLevel> = raw
            .asks
            .into_iter()
            .filter_map(|a| {
                let p: f64 = a.price.parse().ok()?;
                let s: f64 = a.size.parse().ok()?;
                Some(PriceLevel { price: p, size: s })
            })
            .collect();
        asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

        Ok(Orderbook { bids, asks, last_price: None })
    }

    /// Fetch YES-price history.  `market_id` is the token_id (YES CLOB token).
    pub async fn fetch_price_history(
        &self,
        market_id: &str,
        fidelity:  u32,
        start_ts:  i64,
        end_ts:    i64,
    ) -> Result<Vec<Candle>> {
        // The CLOB endpoint takes `fidelity` (minute granularity) and optional
        // `startTs`/`endTs` timestamps (seconds). The `interval` param is a
        // human-readable alias for the same granularity and is not required when
        // `fidelity` is supplied — omitting it avoids sending a redundant param.
        let url = format!(
            "{}/prices-history?market={}&fidelity={}&startTs={}&endTs={}",
            CLOB_BASE, market_id, fidelity, start_ts, end_ts
        );

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("Polymarket /prices-history request failed")?;

        if !resp.status().is_success() {
            anyhow::bail!("Polymarket /prices-history error {}", resp.status());
        }

        let raw: PricesHistoryResponse =
            resp.json().await.context("Failed to parse Polymarket /prices-history")?;

        let candles = raw
            .history
            .into_iter()
            .map(|h| Candle {
                ts:     h.t,
                open:   h.p,
                high:   h.p,
                low:    h.p,
                close:  h.p,
                volume: None,
            })
            .collect();

        Ok(candles)
    }
}

// ─── Raw JSON types ───────────────────────────────────────────────────────────

/// Deserializer for fields that the Gamma API sends as either:
///   - a real JSON array:    `["0.65", "0.35"]`
///   - a JSON-encoded string: `"[\"0.65\",\"0.35\"]"`
///
/// Used for both `outcomePrices` and `clobTokenIds`.
fn deserialize_string_array<'de, D>(de: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let v = serde_json::Value::deserialize(de)?;
    let extract = |arr: Vec<serde_json::Value>| -> Result<Vec<String>, D::Error> {
        arr.into_iter()
            .map(|item| match item {
                serde_json::Value::String(s) => Ok(s),
                serde_json::Value::Number(n) => Ok(n.to_string()),
                _ => Err(D::Error::custom("expected string element in array")),
            })
            .collect()
    };

    match v {
        serde_json::Value::Array(arr) => extract(arr),
        serde_json::Value::String(s)  => {
            let inner: Vec<serde_json::Value> = serde_json::from_str(&s)
                .map_err(|e| D::Error::custom(format!("array field decode: {}", e)))?;
            extract(inner)
        }
        serde_json::Value::Null => Ok(vec![]),
        _ => Err(D::Error::custom("field must be an array or JSON-encoded string")),
    }
}

#[derive(Deserialize, Debug)]
struct GammaMarket {
    /// Hex condition ID — primary stable ID for CLOB lookups.
    #[serde(rename = "conditionId", default)]
    condition_id:     String,
    /// Numeric / legacy ID (fallback).
    #[serde(default)]
    id:               String,
    question:         Option<String>,
    description:      Option<String>,
    #[serde(rename = "endDate", default)]
    end_date:         Option<String>,

    // Volume — prefer `volumeNum` (reliable numeric string), fall back to `volume`.
    #[serde(rename = "volumeNum", default)]
    volume_num:       Option<serde_json::Value>,
    #[serde(default)]
    volume:           Option<serde_json::Value>,

    // Liquidity — same dual-field pattern.
    #[serde(rename = "liquidityNum", default)]
    liquidity_num:    Option<serde_json::Value>,
    #[serde(default)]
    liquidity:        Option<serde_json::Value>,

    /// YES/NO prices — arrives as a real array or a JSON-encoded string.
    #[serde(rename = "outcomePrices", default, deserialize_with = "deserialize_string_array")]
    outcome_prices:   Vec<String>,

    #[serde(default)]
    category:         Option<String>,

    /// Token objects with `token_id` + `outcome` labels.
    #[serde(default)]
    tokens:           Vec<GammaToken>,

    /// Flat list of CLOB token IDs in the same order as `outcomes`.
    /// Index 0 = YES token, index 1 = NO token.
    /// Arrives as a real JSON array OR a JSON-encoded string — same dual-form
    /// as `outcomePrices`.
    #[serde(rename = "clobTokenIds", default, deserialize_with = "deserialize_string_array")]
    clob_token_ids:   Vec<String>,
}

#[derive(Deserialize, Debug)]
struct GammaToken {
    token_id: String,
    outcome:  String,
}

#[derive(Deserialize, Debug)]
struct GammaEvent {
    id:          String,
    title:       String,
    description: Option<String>,
    category:    Option<String>,
    #[serde(default)]
    markets:     Vec<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct ClobOrderbook {
    #[serde(default)]
    bids: Vec<ClobLevel>,
    #[serde(default)]
    asks: Vec<ClobLevel>,
}

#[derive(Deserialize, Debug)]
struct ClobLevel {
    price: String,
    size:  String,
}

#[derive(Deserialize, Debug)]
struct PricesHistoryResponse {
    history: Vec<HistoryPoint>,
}

#[derive(Deserialize, Debug)]
struct HistoryPoint {
    t: i64,
    p: f64,
}

// ─── Conversion ───────────────────────────────────────────────────────────────

fn gamma_to_market(g: GammaMarket) -> Option<Market> {
    let id = if !g.condition_id.is_empty() {
        g.condition_id.clone()
    } else {
        g.id.clone()
    };

    if id.is_empty() {
        return None;
    }

    let title = g.question.as_deref().unwrap_or("Unknown").to_string();

    let (yes_price, no_price) = parse_outcome_prices(&g.outcome_prices);

    // Prefer the `*Num` fields (clean numeric strings) over the raw fields.
    let volume = g.volume_num.as_ref().and_then(parse_f64_value)
        .or_else(|| g.volume.as_ref().and_then(parse_f64_value));
    let liquidity = g.liquidity_num.as_ref().and_then(parse_f64_value)
        .or_else(|| g.liquidity.as_ref().and_then(parse_f64_value));

    // YES token ID: try the labelled `tokens` array first, then the flat
    // `clobTokenIds` list (index 0 = YES by convention).
    let token_id = g
        .tokens
        .iter()
        .find(|t| t.outcome.eq_ignore_ascii_case("yes"))
        .map(|t| t.token_id.clone())
        .or_else(|| g.clob_token_ids.into_iter().next());

    Some(Market {
        id,
        platform:     Platform::Polymarket,
        title,
        description:  g.description,
        yes_price,
        no_price,
        volume,
        liquidity,
        end_date:     g.end_date,
        category:     g.category,
        status:       "open".to_string(),
        token_id,
        event_ticker: None,
    })
}

fn parse_outcome_prices(prices: &[String]) -> (f64, f64) {
    let yes = prices.first().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.5);
    let no  = prices.get(1).and_then(|s| s.parse::<f64>().ok()).unwrap_or(1.0 - yes);
    (yes, no)
}

fn parse_f64_value(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

// ─── Minimal URL percent-encoding ────────────────────────────────────────────

mod urlencoding {
    pub fn encode(input: &str) -> String {
        let mut out = String::new();
        for b in input.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
                | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
                _ => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }
}
