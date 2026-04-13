//! Portfolio position tracking.
//!
//! Positions are persisted to `~/.whoissharp/portfolio.json` so they survive
//! restarts.  All arithmetic uses f64; no exchange integration — positions are
//! entered manually or via AI tool calls.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::markets::Platform;

// ─── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Side {
    Yes,
    No,
}

impl Side {
    pub fn label(&self) -> &str {
        match self {
            Side::Yes => "YES",
            Side::No  => "NO",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "NO" => Side::No,
            _    => Side::Yes,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    /// Unique position ID (random u64 formatted as hex).
    pub id:           String,
    pub platform:     Platform,
    pub market_id:    String,
    pub title:        String,
    /// YES (or NO) price at time of entry, 0.0–1.0.
    pub entry_price:  f64,
    /// Number of shares / contracts.
    pub shares:       f64,
    pub side:         Side,
    #[serde(with = "chrono::serde::ts_seconds")]
    pub opened_at:    DateTime<Utc>,
    /// Current mark price (updated from live market data), 0.0–1.0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mark_price:   Option<f64>,
    /// Optional note / thesis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note:         Option<String>,
}

impl Position {
    pub fn new(
        platform:    Platform,
        market_id:   impl Into<String>,
        title:       impl Into<String>,
        entry_price: f64,
        shares:      f64,
        side:        Side,
        note:        Option<String>,
    ) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let market_id: String = market_id.into();
        let title:     String = title.into();
        let mut h = DefaultHasher::new();
        Utc::now().timestamp_nanos_opt().unwrap_or(0).hash(&mut h);
        market_id.hash(&mut h);
        let id = format!("{:016x}", h.finish());

        Position {
            id,
            platform,
            market_id,
            title,
            entry_price,
            shares,
            side,
            opened_at: Utc::now(),
            mark_price: None,
            note,
        }
    }

    /// Cost basis in dollars.
    pub fn cost(&self) -> f64 {
        self.entry_price * self.shares
    }

    /// Current market value in dollars.
    pub fn market_value(&self) -> f64 {
        self.mark_price.unwrap_or(self.entry_price) * self.shares
    }

    /// Unrealised PnL in dollars.
    pub fn pnl(&self) -> f64 {
        self.market_value() - self.cost()
    }

    /// PnL as percentage of cost.
    pub fn pnl_pct(&self) -> f64 {
        if self.cost().abs() < 1e-9 {
            return 0.0;
        }
        self.pnl() / self.cost() * 100.0
    }
}

// ─── Portfolio ────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Portfolio {
    pub positions: Vec<Position>,
}

impl Portfolio {
    pub fn add(&mut self, pos: Position) {
        self.positions.push(pos);
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.positions.len();
        self.positions.retain(|p| p.id != id);
        self.positions.len() < before
    }

    /// Update mark prices from live market data.
    /// `market_prices`: iterator of (platform, market_id, yes_price).
    pub fn update_marks(&mut self, market_prices: impl Iterator<Item = (Platform, String, f64)>) {
        let prices: Vec<(Platform, String, f64)> = market_prices.collect();
        for pos in &mut self.positions {
            for (plat, id, price) in &prices {
                if *plat == pos.platform && *id == pos.market_id {
                    // For NO positions, our P&L moves inverse to YES price
                    let mark = match pos.side {
                        Side::Yes => *price,
                        Side::No  => 1.0 - price,
                    };
                    pos.mark_price = Some(mark);
                    break;
                }
            }
        }
    }

    /// Total cost basis across all positions.
    pub fn total_cost(&self) -> f64 {
        self.positions.iter().map(|p| p.cost()).sum()
    }

    /// Total current market value.
    pub fn total_value(&self) -> f64 {
        self.positions.iter().map(|p| p.market_value()).sum()
    }

    /// Total unrealised PnL.
    pub fn total_pnl(&self) -> f64 {
        self.total_value() - self.total_cost()
    }
}

// ─── Persistence ─────────────────────────────────────────────────────────────

fn portfolio_path() -> PathBuf {
    let mut p = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".whoissharp");
    p.push("portfolio.json");
    p
}

pub fn load_portfolio() -> Portfolio {
    let path = portfolio_path();
    if !path.exists() {
        return Portfolio::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_)   => Portfolio::default(),
    }
}

pub fn save_portfolio(portfolio: &Portfolio) -> Result<()> {
    let path = portfolio_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create directory '{}'", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(portfolio)?;
    std::fs::write(&path, data)
        .with_context(|| format!("Cannot write portfolio to '{}'", path.display()))?;
    Ok(())
}
