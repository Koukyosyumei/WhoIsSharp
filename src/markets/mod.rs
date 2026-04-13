//! Universal market types shared across Polymarket and Kalshi.

pub mod kalshi;
pub mod polymarket;

use serde::{Deserialize, Serialize};

// ─── Platform ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    Polymarket,
    Kalshi,
}

impl Platform {
    pub fn label(&self) -> &str {
        match self {
            Platform::Polymarket => "PM",
            Platform::Kalshi    => "KL",
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Platform::Polymarket => "Polymarket",
            Platform::Kalshi    => "Kalshi",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// ─── Market ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Market {
    /// Stable identifier: conditionId for Polymarket, ticker for Kalshi.
    pub id:          String,
    pub platform:    Platform,
    pub title:       String,
    pub description: Option<String>,
    /// YES implied probability (0.0 – 1.0).
    pub yes_price:   f64,
    pub no_price:    f64,
    pub volume:      Option<f64>,
    pub liquidity:   Option<f64>,
    pub end_date:    Option<String>,
    pub category:    Option<String>,
    pub status:      String,
    /// Polymarket CLOB token ID for the YES outcome (orderbook / price history).
    pub token_id:    Option<String>,
    /// Kalshi event ticker (e.g. "KXMLB-26"). Used to derive series_ticker for
    /// the candlestick endpoint: first hyphen-delimited segment = "KXMLB".
    pub event_ticker: Option<String>,
}

impl Market {
    /// Format YES price as a percentage string with 1 decimal place.
    pub fn yes_pct(&self) -> String {
        format!("{:.1}%", self.yes_price * 100.0)
    }

    /// A short summary line for list views.
    pub fn summary_line(&self) -> String {
        format!(
            "{:<50} {} YES",
            truncate(&self.title, 50),
            self.yes_pct()
        )
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .nth(max_chars.saturating_sub(1))
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}

// ─── Orderbook ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Orderbook {
    /// Bids sorted descending by price (highest first).
    pub bids:       Vec<PriceLevel>,
    /// Asks sorted ascending by price (lowest first).
    pub asks:       Vec<PriceLevel>,
    pub last_price: Option<f64>,
}

impl Orderbook {
    pub fn spread(&self) -> Option<f64> {
        let best_bid = self.bids.first()?.price;
        let best_ask = self.asks.first()?.price;
        Some(best_ask - best_bid)
    }

    pub fn mid(&self) -> Option<f64> {
        let best_bid = self.bids.first()?.price;
        let best_ask = self.asks.first()?.price;
        Some((best_bid + best_ask) / 2.0)
    }
}

#[derive(Debug, Clone)]
pub struct PriceLevel {
    pub price: f64,
    pub size:  f64,
}

// ─── Candle ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Candle {
    /// Unix timestamp (seconds).
    pub ts:     i64,
    pub open:   f64,
    pub high:   f64,
    pub low:    f64,
    pub close:  f64,
    pub volume: Option<f64>,
}

// ─── Event ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Event {
    pub id:           String,
    pub platform:     Platform,
    pub title:        String,
    pub category:     Option<String>,
    pub market_count: usize,
    pub description:  Option<String>,
}

// ─── Chart interval ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartInterval {
    OneHour,
    SixHours,
    OneDay,
    OneWeek,
    OneMonth,
}

impl ChartInterval {
    pub fn label(&self) -> &str {
        match self {
            ChartInterval::OneHour  => "1h",
            ChartInterval::SixHours => "6h",
            ChartInterval::OneDay   => "1d",
            ChartInterval::OneWeek  => "1w",
            ChartInterval::OneMonth => "1m",
        }
    }

    pub fn next(self) -> Self {
        match self {
            ChartInterval::OneHour  => ChartInterval::SixHours,
            ChartInterval::SixHours => ChartInterval::OneDay,
            ChartInterval::OneDay   => ChartInterval::OneWeek,
            ChartInterval::OneWeek  => ChartInterval::OneMonth,
            ChartInterval::OneMonth => ChartInterval::OneHour,
        }
    }

    /// Duration in seconds.
    pub fn seconds(&self) -> i64 {
        match self {
            ChartInterval::OneHour  => 3_600,
            ChartInterval::SixHours => 21_600,
            ChartInterval::OneDay   => 86_400,
            ChartInterval::OneWeek  => 604_800,
            ChartInterval::OneMonth => 2_592_000,
        }
    }

    /// Candle resolution in minutes for Kalshi.
    pub fn kalshi_period_interval(&self) -> u32 {
        match self {
            ChartInterval::OneHour  => 1,
            ChartInterval::SixHours => 1,
            ChartInterval::OneDay   => 60,
            ChartInterval::OneWeek  => 60,
            ChartInterval::OneMonth => 1440,
        }
    }

    /// Fidelity (minutes per point) for Polymarket.
    pub fn polymarket_fidelity(&self) -> u32 {
        match self {
            ChartInterval::OneHour  => 1,
            ChartInterval::SixHours => 5,
            ChartInterval::OneDay   => 60,
            ChartInterval::OneWeek  => 60,
            ChartInterval::OneMonth => 1440,
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ob(bids: &[(f64, f64)], asks: &[(f64, f64)]) -> Orderbook {
        Orderbook {
            bids: bids.iter().map(|&(p, s)| PriceLevel { price: p, size: s }).collect(),
            asks: asks.iter().map(|&(p, s)| PriceLevel { price: p, size: s }).collect(),
            last_price: None,
        }
    }

    #[test]
    fn orderbook_spread_and_mid() {
        let ob = make_ob(&[(0.60, 10.0)], &[(0.65, 5.0)]);
        assert!((ob.spread().unwrap() - 0.05).abs() < 1e-9);
        assert!((ob.mid().unwrap() - 0.625).abs() < 1e-9);
    }

    #[test]
    fn orderbook_empty_returns_none() {
        let ob = make_ob(&[], &[]);
        assert!(ob.spread().is_none());
        assert!(ob.mid().is_none());
    }

    #[test]
    fn orderbook_one_side_returns_none() {
        let ob = make_ob(&[(0.60, 1.0)], &[]);
        assert!(ob.spread().is_none());
        assert!(ob.mid().is_none());
    }

    #[test]
    fn chart_interval_cycle_wraps() {
        use ChartInterval::*;
        assert_eq!(OneHour.next(),  SixHours);
        assert_eq!(SixHours.next(), OneDay);
        assert_eq!(OneDay.next(),   OneWeek);
        assert_eq!(OneWeek.next(),  OneMonth);
        assert_eq!(OneMonth.next(), OneHour); // wraps
    }

    #[test]
    fn chart_interval_seconds_ordered() {
        use ChartInterval::*;
        assert!(OneHour.seconds() < SixHours.seconds());
        assert!(SixHours.seconds() < OneDay.seconds());
        assert!(OneDay.seconds() < OneWeek.seconds());
        assert!(OneWeek.seconds() < OneMonth.seconds());
    }

    #[test]
    fn chart_interval_labels() {
        use ChartInterval::*;
        assert_eq!(OneHour.label(),  "1h");
        assert_eq!(SixHours.label(), "6h");
        assert_eq!(OneDay.label(),   "1d");
        assert_eq!(OneWeek.label(),  "1w");
        assert_eq!(OneMonth.label(), "1m");
    }

    #[test]
    fn platform_labels() {
        assert_eq!(Platform::Polymarket.label(), "PM");
        assert_eq!(Platform::Kalshi.label(),     "KL");
        assert_eq!(Platform::Polymarket.name(),  "Polymarket");
        assert_eq!(Platform::Kalshi.name(),      "Kalshi");
    }
}
