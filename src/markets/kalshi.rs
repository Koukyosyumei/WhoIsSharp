//! Kalshi Trade API client.
//!
//! Base: https://api.elections.kalshi.com/trade-api/v2
//! All read-only endpoints are public (no auth required).

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{Candle, Market, Orderbook, Platform, PriceLevel};

const KALSHI_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";

pub struct KalshiClient {
    http: reqwest::Client,
}

impl KalshiClient {
    pub fn new() -> Self {
        KalshiClient {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent("WhoIsSharp/0.1")
                .build()
                .unwrap_or_default(),
        }
    }

    pub async fn fetch_markets(
        &self,
        limit: u32,
        search: Option<&str>,
    ) -> Result<Vec<Market>> {
        let mut url = format!(
            "{}/markets?limit={}&status=open",
            KALSHI_BASE, limit
        );
        if let Some(q) = search {
            // Kalshi doesn't have a dedicated search param, but supports event_ticker filter
            url.push_str(&format!("&event_ticker={}", q));
        }

        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Kalshi /markets request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Kalshi /markets error {}: {}", status, body);
        }

        let raw: KalshiMarketsResponse =
            resp.json().await.context("Failed to parse Kalshi /markets")?;

        Ok(raw.markets.into_iter().map(kalshi_to_market).collect())
    }

    pub async fn fetch_events(&self, limit: u32) -> Result<Vec<super::Event>> {
        let url = format!("{}/events?limit={}&status=open", KALSHI_BASE, limit);

        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Kalshi /events request failed")?;

        if !resp.status().is_success() {
            anyhow::bail!("Kalshi /events error {}", resp.status());
        }

        let raw: KalshiEventsResponse =
            resp.json().await.context("Failed to parse Kalshi /events")?;

        Ok(raw.events.into_iter().map(|e| super::Event {
            id:           e.event_ticker.clone(),
            platform:     Platform::Kalshi,
            title:        e.title,
            category:     e.category,
            market_count: 0,
            description:  e.mutually_exclusive.map(|_| String::new()),
        }).collect())
    }

    pub async fn fetch_orderbook(&self, ticker: &str) -> Result<Orderbook> {
        let url = format!("{}/markets/{}/orderbook", KALSHI_BASE, ticker);

        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Kalshi /orderbook request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            anyhow::bail!("Kalshi /orderbook error {}", status);
        }

        let raw: KalshiOrderbookResponse =
            resp.json().await.context("Failed to parse Kalshi /orderbook")?;

        // Kalshi uses [price_cents, size] pairs in the yes/no arrays.
        let bids: Vec<PriceLevel> = {
            let mut v: Vec<PriceLevel> = raw
                .orderbook
                .yes
                .into_iter()
                .filter_map(|pair| {
                    if pair.len() < 2 { return None; }
                    Some(PriceLevel {
                        price: pair[0] as f64 / 100.0,
                        size:  pair[1] as f64,
                    })
                })
                .collect();
            v.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
            v
        };

        let asks: Vec<PriceLevel> = {
            // "no" bids at price X imply "yes" asks at (100 - X) cents
            let mut v: Vec<PriceLevel> = raw
                .orderbook
                .no
                .into_iter()
                .filter_map(|pair| {
                    if pair.len() < 2 { return None; }
                    let no_price_cents = pair[0] as f64;
                    let yes_implied = (100.0 - no_price_cents) / 100.0;
                    Some(PriceLevel {
                        price: yes_implied,
                        size:  pair[1] as f64,
                    })
                })
                .collect();
            v.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
            v
        };

        Ok(Orderbook { bids, asks, last_price: None })
    }

    pub async fn fetch_candlesticks(
        &self,
        ticker:          &str,
        period_interval: u32,
        start_ts:        i64,
        end_ts:          i64,
    ) -> Result<Vec<Candle>> {
        let url = format!(
            "{}/markets/{}/candlesticks?period_interval={}&start_ts={}&end_ts={}",
            KALSHI_BASE, ticker, period_interval, start_ts, end_ts
        );

        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Kalshi /candlesticks request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Kalshi /candlesticks error {}: {}", status, body);
        }

        let raw: KalshiCandlesticksResponse =
            resp.json().await.context("Failed to parse Kalshi /candlesticks")?;

        let candles = raw
            .candlesticks
            .into_iter()
            .map(|c| Candle {
                ts:    c.end_period_ts,
                open:  c.price.open,
                high:  c.price.high,
                low:   c.price.low,
                close: c.price.close,
                volume: Some(
                    c.volume
                        .map(|v| v.yes_volume + v.no_volume)
                        .unwrap_or(0.0),
                ),
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

#[derive(Deserialize, Debug)]
struct KalshiMarket {
    ticker:         String,
    title:          String,
    #[serde(default)]
    yes_bid:        Option<f64>,
    #[serde(default)]
    yes_ask:        Option<f64>,
    #[serde(default)]
    no_bid:         Option<f64>,
    #[serde(default)]
    no_ask:         Option<f64>,
    #[serde(default)]
    volume:         Option<serde_json::Value>,
    #[serde(default)]
    open_interest:  Option<serde_json::Value>,
    #[serde(default)]
    close_time:     Option<String>,
    #[serde(default)]
    event_ticker:   Option<String>,
    #[serde(default)]
    category:       Option<String>,
    #[serde(default)]
    status:         Option<String>,
    #[serde(default)]
    result:         Option<String>,
    #[serde(default)]
    subtitle:       Option<String>,
}

#[derive(Deserialize, Debug)]
struct KalshiEventsResponse {
    events: Vec<KalshiEvent>,
}

#[derive(Deserialize, Debug)]
struct KalshiEvent {
    event_ticker:      String,
    title:             String,
    #[serde(default)]
    category:          Option<String>,
    #[serde(default)]
    mutually_exclusive: Option<bool>,
}

#[derive(Deserialize, Debug)]
struct KalshiOrderbookResponse {
    orderbook: KalshiOrderbookData,
}

#[derive(Deserialize, Debug)]
struct KalshiOrderbookData {
    #[serde(default)]
    yes: Vec<Vec<i64>>,
    #[serde(default)]
    no:  Vec<Vec<i64>>,
}

#[derive(Deserialize, Debug)]
struct KalshiCandlesticksResponse {
    candlesticks: Vec<KalshiCandle>,
}

#[derive(Deserialize, Debug)]
struct KalshiCandle {
    end_period_ts: i64,
    price:         KalshiCandlePrice,
    #[serde(default)]
    volume:        Option<KalshiCandleVolume>,
}

#[derive(Deserialize, Debug)]
struct KalshiCandlePrice {
    open:  f64,
    high:  f64,
    low:   f64,
    close: f64,
}

#[derive(Deserialize, Debug)]
struct KalshiCandleVolume {
    #[serde(default)]
    yes_volume: f64,
    #[serde(default)]
    no_volume:  f64,
}

// ─── Conversion ───────────────────────────────────────────────────────────────

fn kalshi_to_market(k: KalshiMarket) -> Market {
    // Best estimate of YES probability from bid/ask midpoint
    let yes_price = match (k.yes_bid, k.yes_ask) {
        (Some(bid), Some(ask)) => (bid + ask) / 2.0,
        (Some(bid), None)      => bid,
        (None, Some(ask))      => ask,
        _                      => {
            // Derive from NO prices
            match (k.no_bid, k.no_ask) {
                (Some(nb), Some(na)) => 1.0 - (nb + na) / 2.0,
                (Some(nb), None)     => 1.0 - nb,
                (None, Some(na))     => 1.0 - na,
                _                    => 0.5,
            }
        }
    };
    let no_price = 1.0 - yes_price;

    let volume = k.volume.as_ref().and_then(|v| match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    });

    Market {
        id:          k.ticker.clone(),
        platform:    Platform::Kalshi,
        title:       k.title,
        description: k.subtitle,
        yes_price,
        no_price,
        volume,
        liquidity:   None,
        end_date:    k.close_time,
        category:    k.category,
        status:      k.status.unwrap_or_else(|| "open".to_string()),
        token_id:    None,
    }
}
