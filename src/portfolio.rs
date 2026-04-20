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
    /// Take-profit price threshold (YES price), 0.0–1.0. Alert fires when mark ≥ this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take_profit:  Option<f64>,
    /// Stop-loss price threshold (YES price), 0.0–1.0. Alert fires when mark ≤ this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss:    Option<f64>,
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
            mark_price:  None,
            note,
            take_profit: None,
            stop_loss:   None,
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

// ─── Wallet registry ─────────────────────────────────────────────────────────

fn wallets_path() -> PathBuf {
    let mut p = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".whoissharp");
    p.push("wallets.json");
    p
}

/// Load the list of registered Polymarket wallet addresses.
pub fn load_wallets() -> Vec<String> {
    let path = wallets_path();
    if !path.exists() { return Vec::new(); }
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_)   => Vec::new(),
    }
}

/// Persist the wallet address list.
pub fn save_wallets(wallets: &[String]) -> Result<()> {
    let path = wallets_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create directory '{}'", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(wallets)?;
    std::fs::write(&path, data)
        .with_context(|| format!("Cannot write wallets to '{}'", path.display()))?;
    Ok(())
}

// ─── Watchlist ────────────────────────────────────────────────────────────────

fn watchlist_path() -> PathBuf {
    let mut p = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".whoissharp");
    p.push("watchlist.json");
    p
}

/// A watched market entry — stores the market ID and an optional price alert
/// threshold so we can fire a status-bar notification when the price crosses it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchEntry {
    pub market_id: String,
    pub title:     String,
    /// Alert fires when YES price falls below this value (0.0 = no alert).
    pub alert_below: f64,
    /// Alert fires when YES price rises above this value (1.0 = no alert).
    pub alert_above: f64,
}

impl WatchEntry {
    pub fn new(market_id: impl Into<String>, title: impl Into<String>) -> Self {
        WatchEntry {
            market_id:   market_id.into(),
            title:       title.into(),
            alert_below: 0.0,
            alert_above: 1.0,
        }
    }
}

pub fn load_watchlist() -> Vec<WatchEntry> {
    let path = watchlist_path();
    if !path.exists() { return Vec::new(); }
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_)   => Vec::new(),
    }
}

pub fn save_watchlist(watchlist: &[WatchEntry]) -> Result<()> {
    let path = watchlist_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create directory '{}'", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(watchlist)?;
    std::fs::write(&path, data)
        .with_context(|| format!("Cannot write watchlist to '{}'", path.display()))?;
    Ok(())
}

// ─── Session persistence ──────────────────────────────────────────────────────
//
// A session captures the chat history and research notes for one working session.
// Saved to ~/.whoissharp/sessions/<YYYY-MM-DD_HH-MM-SS>.json on exit and loadable
// on the next startup.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    /// "user" | "assistant" | "tool_call" | "tool_result" | "error"
    pub role:    String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Session {
    /// ISO-8601 timestamp when the session was started.
    pub started_at: String,
    pub messages:   Vec<SessionMessage>,
    /// Free-form research notes appended via `!note`.
    pub notes:      Vec<String>,
}

fn sessions_dir() -> PathBuf {
    let mut p = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".whoissharp");
    p.push("sessions");
    p
}

/// Save a session to `~/.whoissharp/sessions/<timestamp>.json`.
/// Returns the path on success.
pub fn save_session(session: &Session) -> Result<PathBuf> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Cannot create sessions directory '{}'", dir.display()))?;
    let filename = format!("{}.json", session.started_at.replace([':', ' '], "-"));
    let path = dir.join(&filename);
    let data = serde_json::to_string_pretty(session)?;
    std::fs::write(&path, &data)
        .with_context(|| format!("Cannot write session to '{}'", path.display()))?;
    Ok(path)
}

/// Load the most recent session from `~/.whoissharp/sessions/`, if any.
pub fn load_last_session() -> Option<Session> {
    let dir = sessions_dir();
    if !dir.exists() { return None; }
    let mut entries: Vec<_> = std::fs::read_dir(&dir).ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.file_name());
    let last = entries.last()?;
    let data = std::fs::read_to_string(last.path()).ok()?;
    serde_json::from_str(&data).ok()
}

// ─── TUI view state (tab, market, filters) persisted across restarts ─────────

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TuiViewState {
    pub last_tab:        Option<u8>,
    pub last_market_id:  Option<String>,
    /// ChartInterval index: 0=1h 1=6h 2=1d 3=1w 4=1m
    pub chart_interval:  Option<u8>,
    /// PlatformFilter index: 0=All 1=PM 2=KL
    pub platform_filter: Option<u8>,
    /// MarketSort index: 0=~50% 1=Vol 2=End 3=A-Z
    pub market_sort:     Option<u8>,
    pub watchlist_only:  bool,
    pub search:          Option<String>,
    pub split_pane:      bool,
}

fn tui_view_state_path() -> PathBuf {
    let mut p = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".whoissharp");
    p.push("view_state.json");
    p
}

pub fn load_tui_view_state() -> TuiViewState {
    let path = tui_view_state_path();
    if !path.exists() { return TuiViewState::default(); }
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_)   => TuiViewState::default(),
    }
}

pub fn save_tui_view_state(s: &TuiViewState) -> Result<()> {
    let path = tui_view_state_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create directory '{}'", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(s)?;
    std::fs::write(&path, data)
        .with_context(|| format!("Cannot write view state to '{}'", path.display()))?;
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markets::Platform;

    fn yes_pos(entry: f64, shares: f64) -> Position {
        Position::new(Platform::Kalshi, "MKT-1", "Test market", entry, shares, Side::Yes, None)
    }
    fn no_pos(entry: f64, shares: f64) -> Position {
        Position::new(Platform::Kalshi, "MKT-1", "Test market", entry, shares, Side::No, None)
    }

    // ── Side ─────────────────────────────────────────────────────────────────

    #[test]
    fn side_from_str_yes_variants() {
        assert_eq!(Side::from_str("yes"),  Side::Yes);
        assert_eq!(Side::from_str("YES"),  Side::Yes);
        assert_eq!(Side::from_str("Yes"),  Side::Yes);
        assert_eq!(Side::from_str("blah"), Side::Yes); // default
    }

    #[test]
    fn side_from_str_no_variants() {
        assert_eq!(Side::from_str("no"), Side::No);
        assert_eq!(Side::from_str("NO"), Side::No);
        assert_eq!(Side::from_str("No"), Side::No);
    }

    #[test]
    fn side_labels() {
        assert_eq!(Side::Yes.label(), "YES");
        assert_eq!(Side::No.label(),  "NO");
    }

    // ── Position cost / value / pnl ───────────────────────────────────────────

    #[test]
    fn cost_is_entry_times_shares() {
        let p = yes_pos(0.60, 100.0);
        assert!((p.cost() - 60.0).abs() < 1e-9);
    }

    #[test]
    fn market_value_uses_entry_when_no_mark() {
        let p = yes_pos(0.60, 100.0);
        assert!((p.market_value() - 60.0).abs() < 1e-9);
    }

    #[test]
    fn pnl_gain() {
        let mut p = yes_pos(0.50, 100.0);
        p.mark_price = Some(0.65);
        assert!((p.pnl()     - 15.0).abs() < 1e-9);
        assert!((p.pnl_pct() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn pnl_loss() {
        let mut p = yes_pos(0.70, 200.0);
        p.mark_price = Some(0.50);
        assert!((p.pnl() - (-40.0)).abs() < 1e-9);
    }

    #[test]
    fn pnl_pct_zero_cost_returns_zero() {
        let p = yes_pos(0.0, 0.0);
        assert_eq!(p.pnl_pct(), 0.0);
    }

    #[test]
    fn pnl_breakeven() {
        let mut p = yes_pos(0.50, 100.0);
        p.mark_price = Some(0.50);
        assert!(p.pnl().abs() < 1e-9);
    }

    // ── Portfolio add / remove ────────────────────────────────────────────────

    #[test]
    fn add_and_remove() {
        let mut pf = Portfolio::default();
        let pos = yes_pos(0.50, 100.0);
        let id = pos.id.clone();
        pf.add(pos);
        assert_eq!(pf.positions.len(), 1);
        assert!(pf.remove(&id));
        assert!(pf.positions.is_empty());
    }

    #[test]
    fn remove_nonexistent_returns_false() {
        let mut pf = Portfolio::default();
        assert!(!pf.remove("does-not-exist"));
    }

    #[test]
    fn remove_only_matching_id() {
        let mut pf = Portfolio::default();
        let p1 = yes_pos(0.50, 10.0);
        let id1 = p1.id.clone();
        let p2 = yes_pos(0.60, 20.0);
        pf.add(p1);
        pf.add(p2);
        assert!(pf.remove(&id1));
        assert_eq!(pf.positions.len(), 1);
    }

    // ── Portfolio totals ──────────────────────────────────────────────────────

    #[test]
    fn total_pnl_net_zero() {
        let mut pf = Portfolio::default();
        let mut p1 = yes_pos(0.50, 100.0);
        p1.mark_price = Some(0.60); // +10
        let mut p2 = yes_pos(0.70, 100.0);
        p2.mark_price = Some(0.60); // -10
        pf.add(p1);
        pf.add(p2);
        assert!(pf.total_pnl().abs() < 1e-9);
        assert!((pf.total_cost()  - 120.0).abs() < 1e-9);
        assert!((pf.total_value() - 120.0).abs() < 1e-9);
    }

    #[test]
    fn total_pnl_empty_portfolio() {
        let pf = Portfolio::default();
        assert_eq!(pf.total_cost(),  0.0);
        assert_eq!(pf.total_value(), 0.0);
        assert_eq!(pf.total_pnl(),   0.0);
    }

    // ── update_marks ─────────────────────────────────────────────────────────

    #[test]
    fn update_marks_yes_position() {
        let mut pf = Portfolio::default();
        let p = yes_pos(0.50, 100.0);
        let mkt_id = p.market_id.clone();
        pf.add(p);
        pf.update_marks([(Platform::Kalshi, mkt_id, 0.80)].into_iter());
        assert!((pf.positions[0].mark_price.unwrap() - 0.80).abs() < 1e-9);
    }

    #[test]
    fn update_marks_no_position_inverts_price() {
        let mut pf = Portfolio::default();
        let p = no_pos(0.50, 100.0);
        let mkt_id = p.market_id.clone();
        pf.add(p);
        // YES price rises to 0.80 → NO mark = 1 - 0.80 = 0.20
        pf.update_marks([(Platform::Kalshi, mkt_id, 0.80)].into_iter());
        assert!((pf.positions[0].mark_price.unwrap() - 0.20).abs() < 1e-9);
    }

    #[test]
    fn update_marks_wrong_platform_ignored() {
        let mut pf = Portfolio::default();
        let p = yes_pos(0.50, 100.0);
        let mkt_id = p.market_id.clone();
        pf.add(p);
        // Send update for Polymarket, but position is on Kalshi
        pf.update_marks([(Platform::Polymarket, mkt_id, 0.90)].into_iter());
        assert!(pf.positions[0].mark_price.is_none());
    }

    #[test]
    fn update_marks_wrong_market_id_ignored() {
        let mut pf = Portfolio::default();
        let p = yes_pos(0.50, 100.0);
        pf.add(p);
        pf.update_marks([(Platform::Kalshi, "WRONG-ID".to_string(), 0.90)].into_iter());
        assert!(pf.positions[0].mark_price.is_none());
    }
}
