//! Kalshi Trade API v2 client.
//!
//! Base: https://api.elections.kalshi.com/trade-api/v2
//! All read-only endpoints are public (no auth required).
//!
//! Field-name conventions from the Kalshi API (as documented in pykalshi):
//!   Prices  → `*_dollars`  strings in [0, 1] range (e.g. "0.45")
//!   Volumes → `*_fp`       fixed-point strings      (e.g. "1234.00")
//!   Orderbook levels → `[[price_dollars_str, qty_fp_str], ...]`

use anyhow::{Context, Result};
use serde::Deserialize;

use std::sync::Arc;

use super::{Candle, Market, Orderbook, Platform, PriceLevel};
use crate::cache::TtlCache;

const KALSHI_BASE:         &str = "https://api.elections.kalshi.com/trade-api/v2";
const CACHE_TTL_MARKETS:   u64  = 60;
const CACHE_TTL_EVENTS:    u64  = 300;
const CACHE_TTL_CANDLES:   u64  = 300;

pub struct KalshiClient {
    http:  reqwest::Client,
    cache: Arc<TtlCache>,
}

impl KalshiClient {
    pub fn new() -> Self {
        KalshiClient {
            http:  reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent("WhoIsSharp/0.1")
                .build()
                .unwrap_or_default(),
            cache: Arc::new(TtlCache::new(CACHE_TTL_MARKETS)),
        }
    }

    /// Fetch `url` with Accept: application/json, retry, and optional caching.
    async fn kalshi_get(&self, url: &str, ttl_secs: u64) -> Result<String> {
        if ttl_secs > 0 {
            let key = format!("{}#{}", url, ttl_secs);
            if let Some(body) = self.cache.get(&key).await {
                return Ok(body);
            }
        }

        let resp = crate::http::retry_builder(|| {
            self.http.get(url).header("Accept", "application/json")
        })
        .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("HTTP {}: {}", status, &body[..body.len().min(200)]);
        }

        let body = resp.text().await.context("Failed to read Kalshi response")?;

        if ttl_secs > 0 {
            let key = format!("{}#{}", url, ttl_secs);
            self.cache.set(key, body.clone()).await;
        }

        Ok(body)
    }

    pub async fn fetch_markets(
        &self,
        limit: u32,
        search: Option<&str>,
    ) -> Result<Vec<Market>> {
        // When searching by keyword, map to event_ticker filter directly.
        if let Some(q) = search {
            return self.fetch_markets_for_event(q, limit).await;
        }

        // The Kalshi /markets?status=open endpoint currently returns only MVE
        // (multi-variate event) parlay markets, not regular prediction markets.
        // Real markets must be fetched per-event via the /events endpoint.
        self.fetch_markets_via_events(limit).await
    }

    /// Fetch real prediction markets by going through the events list first.
    async fn fetch_markets_via_events(&self, limit: u32) -> Result<Vec<Market>> {
        // Fetch events — these are the real prediction categories.
        let events_url = format!("{}/events?limit=50&status=open", KALSHI_BASE);
        let body = self.kalshi_get(&events_url, CACHE_TTL_EVENTS).await
            .context("Kalshi /events (for markets) request failed")?;
        let events: KalshiEventsResponse =
            serde_json::from_str(&body).context("Failed to parse Kalshi /events")?;

        // For each event, fetch its markets. Stop once we have enough.
        let per_event = ((limit / events.events.len().max(1) as u32) + 1).max(5);
        let mut all: Vec<Market> = Vec::new();

        for event in &events.events {
            if all.len() >= limit as usize {
                break;
            }
            if let Ok(mut mkts) = self
                .fetch_markets_for_event(&event.event_ticker, per_event)
                .await
            {
                all.append(&mut mkts);
            }
        }

        all.truncate(limit as usize);
        Ok(all)
    }

    /// Fetch markets belonging to a specific event_ticker.
    async fn fetch_markets_for_event(&self, event_ticker: &str, limit: u32) -> Result<Vec<Market>> {
        let url = format!(
            "{}/markets?limit={}&status=open&event_ticker={}",
            KALSHI_BASE, limit, event_ticker
        );
        let body = self.kalshi_get(&url, CACHE_TTL_MARKETS).await
            .context("Kalshi /markets request failed")?;
        let raw: KalshiMarketsResponse =
            serde_json::from_str(&body).context("Failed to parse Kalshi /markets")?;

        Ok(raw.markets.into_iter().map(kalshi_to_market).collect())
    }

    pub async fn fetch_events(&self, limit: u32) -> Result<Vec<super::Event>> {
        let url = format!("{}/events?limit={}&status=open", KALSHI_BASE, limit);

        let body = self.kalshi_get(&url, CACHE_TTL_EVENTS).await
            .context("Kalshi /events request failed")?;
        let raw: KalshiEventsResponse =
            serde_json::from_str(&body).context("Failed to parse Kalshi /events")?;

        Ok(raw.events.into_iter().map(|e| super::Event {
            id:           e.event_ticker.clone(),
            platform:     Platform::Kalshi,
            title:        e.title,
            category:     e.category,
            market_count: 0,
            description:  None,
        }).collect())
    }

    pub async fn fetch_orderbook(&self, ticker: &str) -> Result<Orderbook> {
        let url = format!("{}/markets/{}/orderbook", KALSHI_BASE, ticker);

        // Orderbooks are volatile — no caching
        let body = self.kalshi_get(&url, 0).await
            .context("Kalshi /orderbook request failed")?;
        let raw: KalshiOrderbookResponse =
            serde_json::from_str(&body).context("Failed to parse Kalshi /orderbook")?;

        // API returns [[price_dollars_str, qty_fp_str], ...] for yes and no sides.
        // YES bids: yes_dollars levels, prices already in [0, 1] dollar range.
        let bids: Vec<PriceLevel> = {
            let mut v: Vec<PriceLevel> = raw
                .orderbook
                .yes_dollars
                .into_iter()
                .filter_map(|pair| {
                    if pair.len() < 2 { return None; }
                    let price = pair[0].parse::<f64>().ok()?;
                    let size  = pair[1].parse::<f64>().ok()?;
                    Some(PriceLevel { price, size })
                })
                .collect();
            v.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
            v
        };

        // NO bids at price X imply YES asks at (1 − X).
        let asks: Vec<PriceLevel> = {
            let mut v: Vec<PriceLevel> = raw
                .orderbook
                .no_dollars
                .into_iter()
                .filter_map(|pair| {
                    if pair.len() < 2 { return None; }
                    let no_price   = pair[0].parse::<f64>().ok()?;
                    let yes_implied = 1.0 - no_price;
                    let size        = pair[1].parse::<f64>().ok()?;
                    Some(PriceLevel { price: yes_implied, size })
                })
                .collect();
            v.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
            v
        };

        Ok(Orderbook { bids, asks, last_price: None })
    }

    pub async fn fetch_candlesticks(
        &self,
        series_ticker:   &str,
        ticker:          &str,
        period_interval: u32,
        start_ts:        i64,
        end_ts:          i64,
    ) -> Result<Vec<Candle>> {
        let url = format!(
            "{}/series/{}/markets/{}/candlesticks?period_interval={}&start_ts={}&end_ts={}",
            KALSHI_BASE, series_ticker, ticker, period_interval, start_ts, end_ts
        );

        let body = self.kalshi_get(&url, CACHE_TTL_CANDLES).await
            .context("Kalshi /candlesticks request failed")?;
        let raw: KalshiCandlesticksResponse =
            serde_json::from_str(&body).context("Failed to parse Kalshi /candlesticks")?;

        let parse_dollar = |s: Option<&str>| -> Option<f64> {
            s.and_then(|s| s.parse::<f64>().ok())
        };

        let candles = raw
            .candlesticks
            .into_iter()
            .map(|c| {
                // When no trades occurred, only `previous_dollars` is set.
                let prev = parse_dollar(c.price.previous_dollars.as_deref()).unwrap_or(0.0);
                let open  = parse_dollar(c.price.open_dollars.as_deref()).unwrap_or(prev);
                let high  = parse_dollar(c.price.high_dollars.as_deref()).unwrap_or(prev);
                let low   = parse_dollar(c.price.low_dollars.as_deref()).unwrap_or(prev);
                let close = parse_dollar(c.price.close_dollars.as_deref()).unwrap_or(prev);
                let volume = c.volume_fp.as_deref().and_then(|s| s.parse::<f64>().ok());
                Candle { ts: c.end_period_ts, open, high, low, close, volume }
            })
            .collect();

        Ok(candles)
    }
}

// ─── Raw JSON types ───────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
struct KalshiMarketsResponse {
    markets: Vec<KalshiMarket>,
}

/// Kalshi market as returned by the v2 API.
/// Prices are dollar strings in [0, 1]; volumes are fixed-point strings.
#[derive(Deserialize, Debug)]
struct KalshiMarket {
    ticker:              String,
    title:               String,
    /// YES bid in dollars (e.g. "0.45")
    #[serde(default)]
    yes_bid_dollars:     Option<String>,
    /// YES ask in dollars (e.g. "0.47")
    #[serde(default)]
    yes_ask_dollars:     Option<String>,
    /// NO bid in dollars
    #[serde(default)]
    no_bid_dollars:      Option<String>,
    /// NO ask in dollars
    #[serde(default)]
    no_ask_dollars:      Option<String>,
    /// Total volume as fixed-point string (e.g. "12345.00")
    #[serde(default)]
    volume_fp:           Option<String>,
    /// Available liquidity in dollars (e.g. "5000.00")
    #[serde(default)]
    liquidity_dollars:   Option<String>,
    #[serde(default)]
    close_time:          Option<String>,
    #[serde(default)]
    event_ticker:        Option<String>,
    #[serde(default)]
    category:            Option<String>,
    #[serde(default)]
    status:              Option<String>,
    #[serde(default)]
    result:              Option<String>,
    #[serde(default)]
    subtitle:            Option<String>,
}

#[derive(Deserialize, Debug)]
struct KalshiEventsResponse {
    events: Vec<KalshiEvent>,
}

#[derive(Deserialize, Debug)]
struct KalshiEvent {
    event_ticker: String,
    title:        String,
    #[serde(default)]
    category:     Option<String>,
}

#[derive(Deserialize, Debug)]
struct KalshiOrderbookResponse {
    // The Kalshi v2 API wraps orderbook data under the key "orderbook_fp".
    #[serde(rename = "orderbook_fp")]
    orderbook: KalshiOrderbookData,
}

/// Kalshi orderbook levels: `[[price_dollars_str, qty_fp_str], ...]`
/// YES levels are bids; NO levels imply YES asks via (1 − no_price).
#[derive(Deserialize, Debug)]
struct KalshiOrderbookData {
    #[serde(default)]
    yes_dollars: Vec<Vec<String>>,
    #[serde(default)]
    no_dollars:  Vec<Vec<String>>,
}

#[derive(Deserialize, Debug)]
struct KalshiCandlesticksResponse {
    candlesticks: Vec<KalshiCandle>,
}

#[derive(Deserialize, Debug)]
struct KalshiCandle {
    end_period_ts: i64,
    price:         KalshiCandlePrice,
    /// Total volume for the period as a fixed-point string.
    #[serde(default)]
    volume_fp:     Option<String>,
}

/// OHLC price data from the live candlestick endpoint.
/// Fields use the `_dollars` suffix and are dollar strings in [0, 1].
/// When no trades occurred in a period, only `previous_dollars` is present.
#[derive(Deserialize, Debug)]
struct KalshiCandlePrice {
    #[serde(default)]
    previous_dollars: Option<String>,
    #[serde(default)]
    open_dollars:     Option<String>,
    #[serde(default)]
    high_dollars:     Option<String>,
    #[serde(default)]
    low_dollars:      Option<String>,
    #[serde(default)]
    close_dollars:    Option<String>,
}

// ─── Conversion ───────────────────────────────────────────────────────────────

fn parse_dollar_str(s: Option<&str>) -> Option<f64> {
    s.and_then(|s| s.parse::<f64>().ok())
}

fn kalshi_to_market(k: KalshiMarket) -> Market {
    let yes_price = {
        let bid = parse_dollar_str(k.yes_bid_dollars.as_deref());
        let ask = parse_dollar_str(k.yes_ask_dollars.as_deref());
        match (bid, ask) {
            (Some(b), Some(a)) => (b + a) / 2.0,
            (Some(b), None)    => b,
            (None, Some(a))    => a,
            _ => {
                let nb = parse_dollar_str(k.no_bid_dollars.as_deref());
                let na = parse_dollar_str(k.no_ask_dollars.as_deref());
                match (nb, na) {
                    (Some(nb), Some(na)) => 1.0 - (nb + na) / 2.0,
                    (Some(nb), None)     => 1.0 - nb,
                    (None, Some(na))     => 1.0 - na,
                    _                   => 0.5,
                }
            }
        }
    };
    let no_price = 1.0 - yes_price;

    let volume    = parse_dollar_str(k.volume_fp.as_deref());
    let liquidity = parse_dollar_str(k.liquidity_dollars.as_deref());

    Market {
        id:           k.ticker,
        platform:     Platform::Kalshi,
        title:        k.title,
        description:  k.subtitle,
        yes_price,
        no_price,
        volume,
        liquidity,
        end_date:     k.close_time,
        category:     k.category,
        status:       k.status.unwrap_or_else(|| "open".to_string()),
        token_id:     None,
        event_ticker: k.event_ticker,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_dollar_str ──────────────────────────────────────────────────────

    #[test]
    fn parse_dollar_valid() {
        assert!((parse_dollar_str(Some("0.65")).unwrap() - 0.65).abs() < 1e-9);
        assert!((parse_dollar_str(Some("0.00")).unwrap()).abs() < 1e-9);
        assert!((parse_dollar_str(Some("1.00")).unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_dollar_none_input() {
        assert!(parse_dollar_str(None).is_none());
    }

    #[test]
    fn parse_dollar_invalid_string() {
        assert!(parse_dollar_str(Some("not_a_number")).is_none());
        assert!(parse_dollar_str(Some("")).is_none());
    }

    // ── kalshi_to_market ──────────────────────────────────────────────────────

    fn market_json(extra: &str) -> String {
        format!(r#"{{"ticker":"ABC-123","title":"Test market"{}}}"#, extra)
    }

    #[test]
    fn mid_price_from_yes_bid_ask() {
        let json = market_json(r#","yes_bid_dollars":"0.44","yes_ask_dollars":"0.46""#);
        let k: KalshiMarket = serde_json::from_str(&json).unwrap();
        let m = kalshi_to_market(k);
        assert!((m.yes_price - 0.45).abs() < 1e-9);
        assert!((m.no_price  - 0.55).abs() < 1e-9);
        assert_eq!(m.id, "ABC-123");
    }

    #[test]
    fn yes_bid_only() {
        let json = market_json(r#","yes_bid_dollars":"0.60""#);
        let k: KalshiMarket = serde_json::from_str(&json).unwrap();
        let m = kalshi_to_market(k);
        assert!((m.yes_price - 0.60).abs() < 1e-9);
    }

    #[test]
    fn no_side_fallback() {
        // No yes prices → fall back to no prices, YES = 1 - mid(no)
        let json = market_json(r#","no_bid_dollars":"0.60","no_ask_dollars":"0.62""#);
        let k: KalshiMarket = serde_json::from_str(&json).unwrap();
        let m = kalshi_to_market(k);
        assert!((m.yes_price - 0.39).abs() < 1e-9);
    }

    #[test]
    fn default_price_when_no_quotes() {
        let json = market_json("");
        let k: KalshiMarket = serde_json::from_str(&json).unwrap();
        let m = kalshi_to_market(k);
        assert!((m.yes_price - 0.5).abs() < 1e-9);
    }

    #[test]
    fn event_ticker_propagated() {
        let json = market_json(r#","event_ticker":"KXMLB-26""#);
        let k: KalshiMarket = serde_json::from_str(&json).unwrap();
        let m = kalshi_to_market(k);
        assert_eq!(m.event_ticker.as_deref(), Some("KXMLB-26"));
    }

    #[test]
    fn subtitle_becomes_description() {
        let json = market_json(r#","subtitle":"Extra detail here""#);
        let k: KalshiMarket = serde_json::from_str(&json).unwrap();
        let m = kalshi_to_market(k);
        assert_eq!(m.description.as_deref(), Some("Extra detail here"));
    }

    // ── Candlestick price fallback ────────────────────────────────────────────

    #[test]
    fn candle_previous_dollars_used_when_no_ohlc() {
        let json = r#"{"end_period_ts":1700000000,"price":{"previous_dollars":"0.55"},"volume_fp":"0.00"}"#;
        let c: KalshiCandle = serde_json::from_str(json).unwrap();
        assert_eq!(c.price.previous_dollars.as_deref(), Some("0.55"));
        assert!(c.price.open_dollars.is_none());
        assert!(c.price.close_dollars.is_none());
    }

    #[test]
    fn candle_full_ohlc_parsed() {
        let json = r#"{"end_period_ts":1700000000,"price":{"open_dollars":"0.45","high_dollars":"0.60","low_dollars":"0.40","close_dollars":"0.55"},"volume_fp":"100.00"}"#;
        let c: KalshiCandle = serde_json::from_str(json).unwrap();
        assert_eq!(c.price.open_dollars.as_deref(),  Some("0.45"));
        assert_eq!(c.price.high_dollars.as_deref(),  Some("0.60"));
        assert_eq!(c.price.low_dollars.as_deref(),   Some("0.40"));
        assert_eq!(c.price.close_dollars.as_deref(), Some("0.55"));
        assert_eq!(c.volume_fp.as_deref(), Some("100.00"));
    }

    // ── Series ticker derivation convention ───────────────────────────────────

    #[test]
    fn series_from_event_ticker_first_segment() {
        for (event, expected) in [
            ("KXMLB-26",    "KXMLB"),
            ("PRES-24",     "PRES"),
            ("NBA-2025-CHI", "NBA"),
        ] {
            let series = event.split('-').next().unwrap();
            assert_eq!(series, expected, "failed for event_ticker={}", event);
        }
    }
}
