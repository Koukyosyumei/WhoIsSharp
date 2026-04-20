//! FRED (Federal Reserve Economic Data) client.
//!
//! Fetches a small snapshot of macro indicators shown in the TUI header.
//! Optional: no-op when `FRED_API_KEY` is not set.
//!
//! Free API keys: https://fred.stlouisfed.org/docs/api/api_key.html
//! Series used:
//!   FEDFUNDS  — Effective Federal Funds Rate (%)
//!   UNRATE    — Civilian Unemployment Rate (%)
//!   DGS10     — 10-Year Treasury Constant Maturity Rate (%)
//!   T5YIE     — 5-Year Breakeven Inflation Rate (%) — proxy for inflation expectations

use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::Arc;

use crate::cache::TtlCache;

const BASE_URL: &str = "https://api.stlouisfed.org/fred";
const CACHE_TTL: u64 = 3600; // 1 hour — indicators move slowly

// ─── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct MacroSnapshot {
    /// Effective Fed Funds Rate (%)
    pub fed_rate:     Option<f64>,
    /// 5-Year Breakeven Inflation Rate (%)
    pub inflation:    Option<f64>,
    /// Unemployment Rate (%)
    pub unemployment: Option<f64>,
    /// 10-Year Treasury Yield (%)
    pub t10yr:        Option<f64>,
}

impl MacroSnapshot {
    pub fn is_empty(&self) -> bool {
        self.fed_rate.is_none()
            && self.inflation.is_none()
            && self.unemployment.is_none()
            && self.t10yr.is_none()
    }

    /// Compact single-line representation for TUI header / status bar.
    pub fn header_str(&self) -> String {
        let mut parts = Vec::new();
        if let Some(r) = self.fed_rate    { parts.push(format!("FFR {:.2}%", r)); }
        if let Some(i) = self.inflation   { parts.push(format!("5yBE {:.2}%", i)); }
        if let Some(u) = self.unemployment{ parts.push(format!("U {:.1}%", u)); }
        if let Some(t) = self.t10yr       { parts.push(format!("10Y {:.2}%", t)); }
        parts.join("  ")
    }
}

// ─── Client ───────────────────────────────────────────────────────────────────

pub struct FredClient {
    http:    reqwest::Client,
    api_key: String,
    cache:   Arc<TtlCache>,
}

impl FredClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        FredClient {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            api_key: api_key.into(),
            cache:   Arc::new(TtlCache::new(CACHE_TTL)),
        }
    }

    /// Fetch the most recent non-missing value for `series_id`.
    async fn latest(&self, series_id: &str) -> Option<f64> {
        let url = format!(
            "{}/series/observations?series_id={}&api_key={}&file_type=json\
             &sort_order=desc&limit=3",
            BASE_URL, series_id, self.api_key
        );

        let body = if let Some(cached) = self.cache.get(&url).await {
            cached
        } else {
            let resp = crate::http::retry_get(&self.http, &url).await.ok()?;
            if !resp.status().is_success() { return None; }
            let b = resp.text().await.ok()?;
            self.cache.set(url, b.clone()).await;
            b
        };

        parse_latest_value(&body).ok().flatten()
    }

    /// Fetch all four indicators concurrently.
    pub async fn fetch_snapshot(&self) -> MacroSnapshot {
        let (fed_rate, inflation, unemployment, t10yr) = tokio::join!(
            self.latest("FEDFUNDS"),
            self.latest("T5YIE"),
            self.latest("UNRATE"),
            self.latest("DGS10"),
        );
        MacroSnapshot { fed_rate, inflation, unemployment, t10yr }
    }
}

// ─── Raw FRED JSON ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FredResponse {
    #[serde(default)]
    observations: Vec<FredObs>,
}

#[derive(Deserialize)]
struct FredObs {
    value: String,
}

fn parse_latest_value(body: &str) -> Result<Option<f64>> {
    let resp: FredResponse = serde_json::from_str(body)
        .context("Failed to parse FRED response")?;
    for obs in &resp.observations {
        // "." means the value is not yet released — skip
        if obs.value != "." {
            if let Ok(v) = obs.value.parse::<f64>() {
                return Ok(Some(v));
            }
        }
    }
    Ok(None)
}
