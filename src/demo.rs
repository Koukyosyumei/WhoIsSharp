//! Local paper-trading account for demo/agent trading.
//!
//! This never places exchange orders. It persists a simulated cash account and
//! paper positions under `~/.whoissharp/demo_account.json`.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::markets::{Market, Platform};
use crate::portfolio::{Position, Side};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DemoOrderStatus {
    Open,
    Filled,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemoOrder {
    pub id: String,
    #[serde(with = "chrono::serde::ts_seconds")]
    pub created_at: DateTime<Utc>,
    #[serde(default, with = "chrono::serde::ts_seconds_option")]
    pub expires_at: Option<DateTime<Utc>>,
    pub platform: Platform,
    pub market_id: String,
    pub title: String,
    pub side: Side,
    pub limit_price: f64,
    pub notional: f64,
    pub rationale: String,
    pub status: DemoOrderStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemoTrade {
    #[serde(with = "chrono::serde::ts_seconds")]
    pub ts: DateTime<Utc>,
    pub action: String,
    pub platform: Platform,
    pub market_id: String,
    pub title: String,
    pub side: Side,
    pub price: f64,
    pub shares: f64,
    pub notional: f64,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemoSnapshot {
    #[serde(with = "chrono::serde::ts_seconds")]
    pub ts: DateTime<Utc>,
    pub cash: f64,
    pub reserved: f64,
    pub position_value: f64,
    pub equity: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemoAccount {
    pub enabled: bool,
    pub starting_cash: f64,
    pub cash: f64,
    pub positions: Vec<Position>,
    #[serde(default)]
    pub open_orders: Vec<DemoOrder>,
    pub trades: Vec<DemoTrade>,
    #[serde(default)]
    pub snapshots: Vec<DemoSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemoAutoConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    pub max_trade_notional_pct: f64,
    pub max_position_pct: f64,
    pub min_cash_pct: f64,
    pub last_run_ts: i64,
}

impl Default for DemoAccount {
    fn default() -> Self {
        Self {
            enabled: false,
            starting_cash: 0.0,
            cash: 0.0,
            positions: Vec::new(),
            open_orders: Vec::new(),
            trades: Vec::new(),
            snapshots: Vec::new(),
        }
    }
}

impl Default for DemoAutoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 900,
            max_trade_notional_pct: 0.03,
            max_position_pct: 0.10,
            min_cash_pct: 0.20,
            last_run_ts: 0,
        }
    }
}

impl DemoAccount {
    pub fn reserved_cash(&self) -> f64 {
        self.open_orders
            .iter()
            .filter(|o| o.status == DemoOrderStatus::Open)
            .map(|o| o.notional)
            .sum()
    }

    pub fn equity(&self) -> f64 {
        self.cash + self.reserved_cash() + self.positions.iter().map(|p| p.market_value()).sum::<f64>()
    }

    pub fn position_value(&self) -> f64 {
        self.positions.iter().map(|p| p.market_value()).sum()
    }

    pub fn pnl(&self) -> f64 {
        self.equity() - self.starting_cash
    }

    pub fn update_marks(&mut self, markets: &[Market]) {
        self.positions.update_marks(markets.iter().map(|m| (m.platform.clone(), m.id.clone(), m.yes_price)));
    }
}

trait PositionMarks {
    fn update_marks(&mut self, market_prices: impl Iterator<Item = (Platform, String, f64)>);
}

impl PositionMarks for Vec<Position> {
    fn update_marks(&mut self, market_prices: impl Iterator<Item = (Platform, String, f64)>) {
        let prices: Vec<(Platform, String, f64)> = market_prices.collect();
        for pos in self {
            for (plat, id, price) in &prices {
                if *plat == pos.platform && *id == pos.market_id {
                    pos.mark_price = Some(match pos.side {
                        Side::Yes => *price,
                        Side::No => 1.0 - *price,
                    });
                    break;
                }
            }
        }
    }
}

fn demo_path() -> PathBuf {
    let mut p = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".whoissharp");
    p.push("demo_account.json");
    p
}

fn demo_auto_path() -> PathBuf {
    let mut p = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".whoissharp");
    p.push("demo_auto.json");
    p
}

pub fn load() -> DemoAccount {
    let path = demo_path();
    if !path.exists() {
        return DemoAccount::default();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

pub fn load_auto_config() -> DemoAutoConfig {
    let path = demo_auto_path();
    if !path.exists() {
        return DemoAutoConfig::default();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(account: &DemoAccount) -> Result<()> {
    let path = demo_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create directory '{}'", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(account)?)
        .with_context(|| format!("Cannot write demo account to '{}'", path.display()))?;
    Ok(())
}

pub fn save_auto_config(config: &DemoAutoConfig) -> Result<()> {
    let path = demo_auto_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(config)?;
    std::fs::write(&path, data)
        .with_context(|| format!("Cannot write demo automation config to '{}'", path.display()))?;
    Ok(())
}

pub fn reset(starting_cash: f64) -> Result<DemoAccount> {
    if starting_cash <= 0.0 {
        return Err(anyhow!("Starting cash must be positive."));
    }
    let account = DemoAccount {
        enabled: true,
        starting_cash,
        cash: starting_cash,
        positions: Vec::new(),
        open_orders: Vec::new(),
        trades: Vec::new(),
        snapshots: Vec::new(),
    };
    let mut account = account;
    record_snapshot(&mut account);
    save(&account)?;
    Ok(account)
}

pub fn place_limit_order(
    mut account: DemoAccount,
    market: &Market,
    side: Side,
    limit_price: f64,
    notional: f64,
    ttl_hours: Option<i64>,
    rationale: impl Into<String>,
) -> Result<DemoAccount> {
    if !account.enabled {
        return Err(anyhow!("Demo mode is not enabled. Initialize it with /demo <cash>."));
    }
    if notional <= 0.0 {
        return Err(anyhow!("Notional must be positive."));
    }
    if notional > account.cash + 1e-9 {
        return Err(anyhow!("Insufficient available demo cash: requested ${:.2}, cash ${:.2}.", notional, account.cash));
    }
    let now = Utc::now();
    let order = DemoOrder {
        id: stable_id(&market.id),
        created_at: now,
        expires_at: ttl_hours.map(|h| now + chrono::Duration::hours(h.max(1))),
        platform: market.platform.clone(),
        market_id: market.id.clone(),
        title: market.title.clone(),
        side,
        limit_price: limit_price.clamp(0.001, 0.999),
        notional,
        rationale: rationale.into(),
        status: DemoOrderStatus::Open,
    };
    account.cash -= notional;
    account.open_orders.push(order);
    record_snapshot(&mut account);
    save(&account)?;
    Ok(account)
}

pub fn cancel_order(mut account: DemoAccount, order_id: &str) -> Result<DemoAccount> {
    let Some(order) = account.open_orders.iter_mut().find(|o| o.id.starts_with(order_id)) else {
        return Err(anyhow!("Open order not found: {}", order_id));
    };
    if order.status != DemoOrderStatus::Open {
        return Err(anyhow!("Order is not open."));
    }
    order.status = DemoOrderStatus::Cancelled;
    account.cash += order.notional;
    record_snapshot(&mut account);
    save(&account)?;
    Ok(account)
}

pub fn process_orders(account: &mut DemoAccount, markets: &[Market]) -> Result<Vec<String>> {
    let now = Utc::now();
    let mut events = Vec::new();
    for order in &mut account.open_orders {
        if order.status != DemoOrderStatus::Open {
            continue;
        }
        if order.expires_at.map(|t| now >= t).unwrap_or(false) {
            order.status = DemoOrderStatus::Expired;
            account.cash += order.notional;
            events.push(format!("Expired demo order [{}] {}", &order.id[..8], trunc(&order.title, 40)));
            continue;
        }
        let Some(market) = markets.iter().find(|m| m.platform == order.platform && m.id == order.market_id) else {
            continue;
        };
        let side_price = match order.side {
            Side::Yes => market.yes_price,
            Side::No => market.no_price,
        }.clamp(0.001, 0.999);
        if side_price <= order.limit_price {
            let shares = order.notional / side_price;
            let mut pos = Position::new(
                market.platform.clone(),
                market.id.clone(),
                market.title.clone(),
                side_price,
                shares,
                order.side.clone(),
                Some(format!("demo order {}: {}", order.id, order.rationale)),
            );
            pos.mark_price = Some(side_price);
            account.positions.push(pos);
            account.trades.push(DemoTrade {
                ts: now,
                action: "FILL".to_string(),
                platform: market.platform.clone(),
                market_id: market.id.clone(),
                title: market.title.clone(),
                side: order.side.clone(),
                price: side_price,
                shares,
                notional: order.notional,
                rationale: order.rationale.clone(),
            });
            order.status = DemoOrderStatus::Filled;
            events.push(format!(
                "Filled demo order [{}] {} {} ${:.0} @ {:.1}¢",
                &order.id[..8],
                order.side.label(),
                trunc(&order.title, 32),
                order.notional,
                side_price * 100.0,
            ));
        }
    }
    if !events.is_empty() {
        record_snapshot(account);
    }
    save(account)?;
    Ok(events)
}

pub fn buy(
    mut account: DemoAccount,
    market: &Market,
    side: Side,
    notional: f64,
    rationale: impl Into<String>,
) -> Result<DemoAccount> {
    if !account.enabled {
        return Err(anyhow!("Demo mode is not enabled. Initialize it with /demo <cash>."));
    }
    if notional <= 0.0 {
        return Err(anyhow!("Notional must be positive."));
    }
    if notional > account.cash + 1e-9 {
        return Err(anyhow!("Insufficient demo cash: requested ${:.2}, cash ${:.2}.", notional, account.cash));
    }

    let price = match side {
        Side::Yes => market.yes_price,
        Side::No => market.no_price,
    }.clamp(0.001, 0.999);
    let shares = notional / price;
    let rationale = rationale.into();
    let mut pos = Position::new(
        market.platform.clone(),
        market.id.clone(),
        market.title.clone(),
        price,
        shares,
        side.clone(),
        Some(format!("demo: {}", rationale)),
    );
    pos.mark_price = Some(price);
    account.cash -= notional;
    account.positions.push(pos);
    account.trades.push(DemoTrade {
        ts: Utc::now(),
        action: "BUY".to_string(),
        platform: market.platform.clone(),
        market_id: market.id.clone(),
        title: market.title.clone(),
        side,
        price,
        shares,
        notional,
        rationale,
    });
    record_snapshot(&mut account);
    save(&account)?;
    Ok(account)
}

pub fn record_snapshot(account: &mut DemoAccount) {
    if !account.enabled {
        return;
    }
    let snap = DemoSnapshot {
        ts: Utc::now(),
        cash: account.cash,
        reserved: account.reserved_cash(),
        position_value: account.position_value(),
        equity: account.equity(),
    };
    let should_push = account.snapshots.last()
        .map(|last| (last.equity - snap.equity).abs() > 0.01 || (snap.ts - last.ts).num_minutes() >= 5)
        .unwrap_or(true);
    if should_push {
        account.snapshots.push(snap);
        if account.snapshots.len() > 20_000 {
            let keep_from = account.snapshots.len() - 20_000;
            account.snapshots = account.snapshots.split_off(keep_from);
        }
    }
}

pub fn summary(account: &DemoAccount) -> String {
    if !account.enabled {
        return "Demo mode is disabled. Run /demo <cash> to start a paper-trading account.".to_string();
    }
    let equity = account.equity();
    let pnl = equity - account.starting_cash;
    let pnl_pct = if account.starting_cash > 1e-9 { pnl / account.starting_cash * 100.0 } else { 0.0 };
    let mut lines = vec![
        format!("=== DEMO TRADING ACCOUNT ==="),
        format!("Starting cash: ${:.2}", account.starting_cash),
        format!("Cash:          ${:.2}", account.cash),
        format!("Reserved:      ${:.2}", account.reserved_cash()),
        format!("Equity:        ${:.2}", equity),
        format!("P&L:           {:+.2} ({:+.2}%)", pnl, pnl_pct),
        format!("Positions:     {}", account.positions.len()),
        format!("Open orders:   {}", account.open_orders.iter().filter(|o| o.status == DemoOrderStatus::Open).count()),
    ];
    if !account.positions.is_empty() {
        lines.push(String::new());
        lines.push(format!("{:<6} {:<42} {:>8} {:>8} {:>8}", "Side", "Market", "Entry", "Mark", "P&L"));
        for p in &account.positions {
            lines.push(format!(
                "{:<6} {:<42} {:>7.1}¢ {:>7.1}¢ {:>+8.2}",
                p.side.label(),
                trunc(&p.title, 42),
                p.entry_price * 100.0,
                p.mark_price.unwrap_or(p.entry_price) * 100.0,
                p.pnl(),
            ));
        }
    }
    let open: Vec<&DemoOrder> = account.open_orders.iter()
        .filter(|o| o.status == DemoOrderStatus::Open)
        .collect();
    if !open.is_empty() {
        lines.push(String::new());
        lines.push(format!("{:<10} {:<6} {:<38} {:>8} {:>9}", "Order", "Side", "Market", "Limit", "Reserved"));
        for o in open {
            lines.push(format!(
                "{:<10} {:<6} {:<38} {:>7.1}¢ {:>8.2}",
                &o.id[..o.id.len().min(8)],
                o.side.label(),
                trunc(&o.title, 38),
                o.limit_price * 100.0,
                o.notional,
            ));
        }
    }
    lines.join("\n")
}

pub fn backtest_report(account: &DemoAccount) -> String {
    if !account.enabled {
        return "Demo mode is disabled. Run /demo <cash> to start collecting performance history.".to_string();
    }

    let equity = account.equity();
    let pnl = account.pnl();
    let ret_pct = if account.starting_cash > 1e-9 { pnl / account.starting_cash * 100.0 } else { 0.0 };
    let reserved = account.reserved_cash();
    let invested = account.position_value();
    let cash_drag = if equity > 1e-9 { account.cash / equity * 100.0 } else { 0.0 };
    let exposure = if equity > 1e-9 { invested / equity * 100.0 } else { 0.0 };

    let open_orders = account.open_orders.iter().filter(|o| o.status == DemoOrderStatus::Open).count();
    let filled_orders = account.open_orders.iter().filter(|o| o.status == DemoOrderStatus::Filled).count();
    let cancelled_orders = account.open_orders.iter().filter(|o| o.status == DemoOrderStatus::Cancelled).count();
    let expired_orders = account.open_orders.iter().filter(|o| o.status == DemoOrderStatus::Expired).count();
    let decided_orders = filled_orders + cancelled_orders + expired_orders;
    let fill_rate = if decided_orders > 0 { filled_orders as f64 / decided_orders as f64 * 100.0 } else { 0.0 };

    let winners: Vec<&Position> = account.positions.iter().filter(|p| p.pnl() > 0.0).collect();
    let losers: Vec<&Position> = account.positions.iter().filter(|p| p.pnl() < 0.0).collect();
    let hit_rate = if account.positions.is_empty() { 0.0 } else { winners.len() as f64 / account.positions.len() as f64 * 100.0 };
    let gross_win: f64 = winners.iter().map(|p| p.pnl()).sum();
    let gross_loss: f64 = losers.iter().map(|p| p.pnl().abs()).sum();
    let profit_factor = if gross_loss > 1e-9 { gross_win / gross_loss } else if gross_win > 0.0 { f64::INFINITY } else { 0.0 };
    let avg_win = if winners.is_empty() { 0.0 } else { gross_win / winners.len() as f64 };
    let avg_loss = if losers.is_empty() { 0.0 } else { -gross_loss / losers.len() as f64 };
    let turnover = if account.starting_cash > 1e-9 {
        account.trades.iter().map(|t| t.notional).sum::<f64>() / account.starting_cash * 100.0
    } else {
        0.0
    };
    let max_dd = max_drawdown(&account.snapshots);

    let mut lines = vec![
        "=== DEMO TRADING BACKTEST ===".to_string(),
        format!("Snapshots:       {}", account.snapshots.len()),
        format!("Trades/fills:    {}", account.trades.len()),
        format!("Return:          {:+.2} ({:+.2}%)", pnl, ret_pct),
        format!("Max drawdown:    {:.2}%", max_dd * 100.0),
        format!("Equity:          ${:.2}", equity),
        format!("Cash / reserved: ${:.2} / ${:.2}", account.cash, reserved),
        format!("Exposure:        {:.1}% invested, {:.1}% idle cash", exposure, cash_drag),
        String::new(),
        "--- Order discipline ---".to_string(),
        format!("Open / filled / cancelled / expired: {} / {} / {} / {}", open_orders, filled_orders, cancelled_orders, expired_orders),
        format!("Limit fill rate: {:.1}%", fill_rate),
        format!("Turnover:        {:.1}% of starting cash", turnover),
        String::new(),
        "--- Mark-to-market position quality ---".to_string(),
        format!("Hit rate:        {:.1}%  ({} winners / {} positions)", hit_rate, winners.len(), account.positions.len()),
        format!("Avg win/loss:    {:+.2} / {:+.2}", avg_win, avg_loss),
        format!("Profit factor:   {}", if profit_factor.is_infinite() { "∞".to_string() } else { format!("{:.2}", profit_factor) }),
    ];

    let mut by_pnl: Vec<&Position> = account.positions.iter().collect();
    by_pnl.sort_by(|a, b| a.pnl().partial_cmp(&b.pnl()).unwrap_or(std::cmp::Ordering::Equal));
    if !by_pnl.is_empty() {
        lines.push(String::new());
        lines.push("--- Biggest mark-to-market drivers ---".to_string());
        for p in by_pnl.iter().take(3) {
            lines.push(format!("LOSS {:+.2}  {}", p.pnl(), trunc(&p.title, 54)));
        }
        for p in by_pnl.iter().rev().take(3) {
            lines.push(format!("GAIN {:+.2}  {}", p.pnl(), trunc(&p.title, 54)));
        }
    }

    lines.join("\n")
}

fn max_drawdown(snaps: &[DemoSnapshot]) -> f64 {
    let mut peak = 0.0_f64;
    let mut max_dd = 0.0_f64;
    for s in snaps {
        peak = peak.max(s.equity);
        if peak > 1e-9 {
            max_dd = max_dd.max((peak - s.equity) / peak);
        }
    }
    max_dd
}

fn stable_id(seed: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    Utc::now().timestamp_nanos_opt().unwrap_or(0).hash(&mut h);
    seed.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn trunc(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let end = s.char_indices().nth(max_chars.saturating_sub(1)).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}
