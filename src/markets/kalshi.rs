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
            description:  None,
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
            anyhow::bail!("Kalshi /orderbook error {}", resp.status());
        }

        let raw: KalshiOrderbookResponse =
            resp.json().await.context("Failed to parse Kalshi /orderbook")?;

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
