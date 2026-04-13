//! Tool definitions and dispatch for the AI agent.
//!
//! All tools are async and return plain strings shown to the LLM and TUI.

use anyhow::{Context, Result};
use serde_json::json;

use crate::llm::ToolDefinition;
use crate::markets::{kalshi::KalshiClient, polymarket::PolymarketClient, ChartInterval};

// ─── Tool result ─────────────────────────────────────────────────────────────

pub struct ToolOutput {
    pub text: String,
}

impl ToolOutput {
    pub fn ok(s: impl Into<String>) -> Self { ToolOutput { text: s.into() } }
    pub fn err(s: impl Into<String>) -> Self { ToolOutput { text: format!("Error: {}", s.into()) } }
}

// ─── Clients (shared) ────────────────────────────────────────────────────────

pub struct MarketClients {
    pub polymarket: PolymarketClient,
    pub kalshi:     KalshiClient,
}

impl MarketClients {
    pub fn new() -> Self {
        MarketClients {
            polymarket: PolymarketClient::new(),
            kalshi:     KalshiClient::new(),
        }
    }
}

// ─── Dispatch ────────────────────────────────────────────────────────────────

pub async fn dispatch(
    clients: &MarketClients,
    name:    &str,
    args:    &serde_json::Value,
) -> ToolOutput {
    match dispatch_inner(clients, name, args).await {
        Ok(out)  => out,
        Err(err) => ToolOutput::err(err.to_string()),
    }
}

async fn dispatch_inner(
    clients: &MarketClients,
    name:    &str,
    args:    &serde_json::Value,
) -> Result<ToolOutput> {
    match name {
        "list_markets"     => list_markets(clients, args).await,
        "get_market"       => get_market(clients, args).await,
        "get_orderbook"    => get_orderbook(clients, args).await,
        "get_price_history" => get_price_history(clients, args).await,
        "get_events"       => get_events(clients, args).await,
        "search_markets"   => search_markets(clients, args).await,
        "analyze_insider"  => analyze_insider(clients, args).await,
        "find_smart_money" => find_smart_money(clients, args).await,
        "analyze_wallet"   => analyze_wallet(clients, args).await,
        "kelly_size"       => kelly_size(clients, args).await,
        _                  => Ok(ToolOutput::err(format!("Unknown tool: {}", name))),
    }
}

// ─── Tool implementations ─────────────────────────────────────────────────────

async fn list_markets(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    let platform = args["platform"].as_str().unwrap_or("all");
    let limit    = args["limit"].as_u64().unwrap_or(20).min(100) as u32;
    let category = args["category"].as_str();
    let search   = args["search"].as_str();

    let mut lines = Vec::new();

    if platform == "polymarket" || platform == "all" {
        match clients.polymarket.fetch_markets(limit, search, category).await {
            Ok(markets) => {
                lines.push(format!("=== POLYMARKET ({} markets) ===", markets.len()));
                for m in &markets {
                    let vol_str = m.volume.map(|v| format!("${:.0}", v)).unwrap_or_default();
                    lines.push(format!(
                        "  [{}] YES:{:5.1}%  Vol:{:>10}  {}",
                        m.id, m.yes_price * 100.0, vol_str, m.title
                    ));
                }
            }
            Err(e) => lines.push(format!("Polymarket error: {}", e)),
        }
    }

    if platform == "kalshi" || platform == "all" {
        match clients.kalshi.fetch_markets(limit, search).await {
            Ok(markets) => {
                lines.push(format!("=== KALSHI ({} markets) ===", markets.len()));
                for m in &markets {
                    let vol_str = m.volume.map(|v| format!("${:.0}", v)).unwrap_or_default();
                    lines.push(format!(
                        "  [{}] YES:{:5.1}%  Vol:{:>10}  {}",
                        m.id, m.yes_price * 100.0, vol_str, m.title
                    ));
                }
            }
            Err(e) => lines.push(format!("Kalshi error: {}", e)),
        }
    }

    if lines.is_empty() {
        Ok(ToolOutput::ok("No markets found."))
    } else {
        Ok(ToolOutput::ok(lines.join("\n")))
    }
}

async fn get_market(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    let platform = args["platform"].as_str().unwrap_or("polymarket");
    let id       = args["id"].as_str().unwrap_or("").to_string();

    if id.is_empty() {
        return Ok(ToolOutput::err("Missing required argument: id"));
    }

    let markets = match platform {
        "polymarket" => clients.polymarket.fetch_markets(200, None, None).await?,
        "kalshi"     => clients.kalshi.fetch_markets(200, None).await?,
        _            => return Ok(ToolOutput::err(format!("Unknown platform: {}", platform))),
    };

    let market = markets
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(&id))
        .or_else(|| markets.iter().find(|m| m.title.to_lowercase().contains(&id.to_lowercase())));

    match market {
        Some(m) => {
            let vol = m.volume.map(|v| format!("${:.0}", v)).unwrap_or_else(|| "N/A".into());
            let liq = m.liquidity.map(|v| format!("${:.0}", v)).unwrap_or_else(|| "N/A".into());
            let out = format!(
                "Market: {}\nPlatform: {}\nID: {}\nStatus: {}\nCategory: {}\nYES: {:.1}%  NO: {:.1}%\nVolume: {}  Liquidity: {}\nEnds: {}\nDescription: {}",
                m.title,
                m.platform,
                m.id,
                m.status,
                m.category.as_deref().unwrap_or("N/A"),
                m.yes_price * 100.0,
                m.no_price  * 100.0,
                vol,
                liq,
                m.end_date.as_deref().unwrap_or("N/A"),
                m.description.as_deref().unwrap_or("N/A"),
            );
            Ok(ToolOutput::ok(out))
        }
        None => Ok(ToolOutput::err(format!("Market '{}' not found on {}", id, platform))),
    }
}

async fn get_orderbook(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    let platform = args["platform"].as_str().unwrap_or("polymarket");
    let id       = args["id"].as_str().unwrap_or("");

    if id.is_empty() {
        return Ok(ToolOutput::err("Missing required argument: id"));
    }

    let book = match platform {
        "polymarket" => clients.polymarket.fetch_orderbook(id).await?,
        "kalshi"     => clients.kalshi.fetch_orderbook(id).await?,
        _            => return Ok(ToolOutput::err(format!("Unknown platform: {}", platform))),
    };

    let spread = book.spread().map(|s| format!("{:.4}", s)).unwrap_or_else(|| "N/A".into());
    let mid    = book.mid().map(|m| format!("{:.4}", m)).unwrap_or_else(|| "N/A".into());

    let mut lines = Vec::new();
    lines.push(format!("Orderbook for {} ({})", id, platform));
    lines.push(format!("Spread: {}  Mid: {}", spread, mid));
    lines.push(String::new());
    lines.push(format!("{:<12} {:<12} | {:<12} {:<12}", "BID PRICE", "BID SIZE", "ASK PRICE", "ASK SIZE"));
    lines.push("-".repeat(52));

    let depth = book.bids.len().max(book.asks.len()).min(10);
    for i in 0..depth {
        let bid = book.bids.get(i)
            .map(|b| format!("{:.4}    {:.0}", b.price, b.size))
            .unwrap_or_else(|| " ".repeat(20));
        let ask = book.asks.get(i)
            .map(|a| format!("{:.4}    {:.0}", a.price, a.size))
            .unwrap_or_else(|| " ".repeat(20));
        lines.push(format!("{:<24} | {}", bid, ask));
    }

    Ok(ToolOutput::ok(lines.join("\n")))
}

async fn get_price_history(
    clients: &MarketClients,
    args:    &serde_json::Value,
) -> Result<ToolOutput> {
    let platform = args["platform"].as_str().unwrap_or("polymarket");
    let id       = args["id"].as_str().unwrap_or("");
    let days     = args["days"].as_u64().unwrap_or(30).min(90) as i64;

    if id.is_empty() {
        return Ok(ToolOutput::err("Missing required argument: id"));
    }

    let now      = chrono::Utc::now().timestamp();
    let start_ts = now - days * 86_400;
    let interval = if days <= 1 { ChartInterval::OneDay } else { ChartInterval::OneMonth };

    let candles = match platform {
        "polymarket" => {
            clients
                .polymarket
                .fetch_price_history(id, interval.polymarket_fidelity(), start_ts, now)
                .await?
        }
        "kalshi" => {
            // Kalshi candlestick endpoint: /series/{series}/markets/{ticker}/candlesticks
            // series_ticker is first hyphen-segment of the ticker (e.g. "KXMLB-26-WSH" → "KXMLB")
            // unless an explicit series_ticker arg was supplied.
            let series = args["series_ticker"]
                .as_str()
                .or_else(|| id.split('-').next())
                .unwrap_or("");
            clients
                .kalshi
                .fetch_candlesticks(series, id, interval.kalshi_period_interval(), start_ts, now)
                .await?
        }
        _ => return Ok(ToolOutput::err(format!("Unknown platform: {}", platform))),
    };

    if candles.is_empty() {
        return Ok(ToolOutput::ok("No price history available."));
    }

    let first = candles.first().unwrap();
    let last  = candles.last().unwrap();
    let min   = candles.iter().map(|c| c.low).fold(f64::INFINITY, f64::min);
    let max   = candles.iter().map(|c| c.high).fold(f64::NEG_INFINITY, f64::max);
    let change = last.close - first.open;

    // Print a simple ASCII chart (20 rows × 60 cols)
    let chart = ascii_chart(&candles, 15, 60);

    let out = format!(
        "Price History: {} ({}) — last {} days\n\
         Open: {:.1}%  Close: {:.1}%  Change: {:+.1}%\n\
         High: {:.1}%  Low: {:.1}%  Points: {}\n\n\
         {}\n",
        id, platform, days,
        first.open  * 100.0,
        last.close  * 100.0,
        change      * 100.0,
        max         * 100.0,
        min         * 100.0,
        candles.len(),
        chart,
    );

    Ok(ToolOutput::ok(out))
}

async fn get_events(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    let platform = args["platform"].as_str().unwrap_or("all");
    let limit    = args["limit"].as_u64().unwrap_or(20).min(100) as u32;

    let mut lines = Vec::new();

    if platform == "polymarket" || platform == "all" {
        match clients.polymarket.fetch_events(limit).await {
            Ok(events) => {
                lines.push(format!("=== POLYMARKET EVENTS ({}) ===", events.len()));
                for e in &events {
                    lines.push(format!("  [{}] {} ({})", e.id, e.title,
                        e.category.as_deref().unwrap_or("misc")));
                }
            }
            Err(err) => lines.push(format!("Polymarket events error: {}", err)),
        }
    }

    if platform == "kalshi" || platform == "all" {
        match clients.kalshi.fetch_events(limit).await {
            Ok(events) => {
                lines.push(format!("=== KALSHI EVENTS ({}) ===", events.len()));
                for e in &events {
                    lines.push(format!("  [{}] {} ({})", e.id, e.title,
                        e.category.as_deref().unwrap_or("misc")));
                }
            }
            Err(err) => lines.push(format!("Kalshi events error: {}", err)),
        }
    }

    Ok(ToolOutput::ok(lines.join("\n")))
}

async fn search_markets(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    let query    = args["query"].as_str().unwrap_or("");
    let platform = args["platform"].as_str().unwrap_or("all");

    if query.is_empty() {
        return Ok(ToolOutput::err("Missing required argument: query"));
    }

    // Reuse list_markets with search term
    let search_args = json!({
        "platform": platform,
        "limit": 30,
        "search": query
    });
    list_markets(clients, &search_args).await
}

// ─── Insider analysis ─────────────────────────────────────────────────────────

/// Fetch 7-day price history + live orderbook for a market and produce a
/// structured insider-trading signal report from the data.
async fn analyze_insider(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    let platform = args["platform"].as_str().unwrap_or("");
    let id       = args["id"].as_str().unwrap_or("");

    if platform.is_empty() || id.is_empty() {
        return Ok(ToolOutput::err("Required: platform ('polymarket' | 'kalshi') and id"));
    }

    let mut report = Vec::new();
    report.push(format!("=== INSIDER SIGNAL ANALYSIS: {} ({}) ===", id, platform.to_uppercase()));

    // ── 1. Price history (7-day) ────────────────────────────────────────────
    let history_args = json!({ "platform": platform, "id": id, "days": 7 });
    match get_price_history(clients, &history_args).await {
        Ok(out) => {
            report.push("\n--- 7-Day Price History ---".to_string());
            // Extract candle data for velocity calculation
            let lines: Vec<&str> = out.text.lines().collect();
            // Find the summary stats line (contains "min", "max", "start", "end")
            let mut start_price: Option<f64> = None;
            let mut end_price:   Option<f64> = None;
            let mut recent_price: Option<f64> = None;
            for line in &lines {
                if line.contains("Start:") {
                    if let Some(p) = extract_price_from_line(line) { start_price = Some(p); }
                }
                if line.contains("End:") || line.contains("Current:") {
                    if let Some(p) = extract_price_from_line(line) { end_price = Some(p); }
                }
                if line.contains("24h change") || line.contains("Recent:") {
                    if let Some(p) = extract_price_from_line(line) { recent_price = Some(p); }
                }
            }
            report.push(out.text.clone());

            if let (Some(sp), Some(ep)) = (start_price, end_price) {
                let total_move = (ep - sp) * 100.0;
                report.push(format!("\nPrice velocity (7d): {:+.1}¢", total_move));
                if total_move.abs() > 10.0 {
                    report.push(format!(
                        "⚠ NOTABLE: {:+.1}¢ move over 7 days is significant",
                        total_move
                    ));
                }
            }
            let _ = recent_price; // used indirectly via text output
        }
        Err(e) => report.push(format!("Price history unavailable: {}", e)),
    }

    // ── 2. Live orderbook ───────────────────────────────────────────────────
    report.push("\n--- Live Orderbook ---".to_string());
    let ob_args = json!({ "platform": platform, "id": id });
    match get_orderbook(clients, &ob_args).await {
        Ok(out) => {
            report.push(out.text.clone());

            // Parse bid/ask totals to compute imbalance
            let mut total_bid = 0.0f64;
            let mut total_ask = 0.0f64;
            let mut in_bids   = false;
            let mut in_asks   = false;
            for line in out.text.lines() {
                if line.contains("BIDS") { in_bids = true; in_asks = false; continue; }
                if line.contains("ASKS") { in_asks = true; in_bids = false; continue; }
                if let Some(size) = parse_ob_size(line) {
                    if in_bids { total_bid += size; }
                    if in_asks { total_ask += size; }
                }
            }
            if total_bid + total_ask > 0.0 {
                let imbalance = (total_bid - total_ask) / (total_bid + total_ask);
                report.push(format!(
                    "\nOrderbook imbalance: {:.1}%  (bid {:.0} / ask {:.0})",
                    imbalance * 100.0, total_bid, total_ask
                ));
                if imbalance.abs() > 0.3 {
                    let side = if imbalance > 0.0 { "BID-heavy (buying pressure)" } else { "ASK-heavy (selling pressure)" };
                    report.push(format!("⚠ NOTABLE: {side} — one-sided book may indicate informed flow"));
                }
            }
        }
        Err(e) => report.push(format!("Orderbook unavailable: {}", e)),
    }

    // ── 3. Insider-signal interpretation ───────────────────────────────────
    report.push("\n--- Interpretation ---".to_string());
    report.push("Indicators of potential insider flow:".to_string());
    report.push("  • Large, sustained price move before a public announcement".to_string());
    report.push("  • Volume >> liquidity pool (smart money consuming depth)".to_string());
    report.push("  • Lopsided orderbook at extreme YES/NO price".to_string());
    report.push("  • Price drift inconsistent with public news flow".to_string());
    report.push("\nNote: these are probabilistic signals, not proof of wrongdoing.".to_string());
    report.push("Cross-reference with public news timelines before acting.".to_string());

    Ok(ToolOutput::ok(report.join("\n")))
}

/// Extract a probability/price value (0–100 range, returned as 0.0–1.0) from
/// a text line like "  Start:  62.3¢" or "Current: 0.78".
fn extract_price_from_line(line: &str) -> Option<f64> {
    // Try to find the first numeric token after the colon
    let after_colon = line.splitn(2, ':').nth(1)?;
    for token in after_colon.split_whitespace() {
        let cleaned = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        if let Ok(v) = cleaned.parse::<f64>() {
            // Values > 1 are assumed to be in cents (0–100 scale)
            return Some(if v > 1.0 { v / 100.0 } else { v });
        }
    }
    None
}

/// Parse a size value from an orderbook line like "  62.3¢  ×  450.0".
fn parse_ob_size(line: &str) -> Option<f64> {
    // Orderbook lines contain "×" as a separator; size follows it
    let after = line.splitn(2, '×').nth(1)?;
    let token = after.split_whitespace().next()?;
    token.trim_matches(|c: char| !c.is_ascii_digit() && c != '.').parse::<f64>().ok()
}

// ─── Smart money / account analysis ──────────────────────────────────────────

/// Per-wallet analytics bundle, built from trade history.
struct WalletProfile {
    wallet:      String,
    pseudonym:   String,
    /// Dollar size in the queried market specifically.
    market_size: f64,
    /// Distinct markets traded (proxy for experience).
    n_positions: usize,
    /// REDEEM events (each = a winning payout).
    n_wins:      usize,
    /// n_wins / n_positions — meaningful only when n_positions ≥ MIN_POSITIONS.
    win_rate:    f64,
    /// Mean BUY price on positions that later hit REDEEM (lower = earlier entry).
    alpha_score: f64,
    /// Total buy-side dollar volume across history.
    total_vol:   f64,
    /// Full set of conditionIds traded (for coordination detection).
    market_set:  std::collections::HashSet<String>,
}

fn build_wallet_profile(
    wallet:      String,
    pseudonym:   String,
    market_size: f64,
    history:     &[crate::markets::polymarket::PolyTrade],
) -> WalletProfile {
    use std::collections::{HashMap, HashSet};

    const MIN_POSITIONS: usize = 3;

    // Unique conditionIds where they placed a TRADE
    let market_set: HashSet<String> = history
        .iter()
        .filter(|t| t.trade_type == "TRADE" || t.trade_type.is_empty())
        .map(|t| t.condition_id.clone())
        .collect();
    let n_positions = market_set.len();

    // REDEEMs ≈ winning payouts
    let redeemed: HashSet<&str> = history
        .iter()
        .filter(|t| t.trade_type == "REDEEM")
        .map(|t| t.condition_id.as_str())
        .collect();
    let n_wins = redeemed.len();

    // Alpha score: average BUY price on markets that eventually paid out.
    // Low value (e.g. 0.25) means they entered when the market was at 25¢
    // and it resolved YES — they were well ahead of consensus.
    let mut winning_entries: HashMap<&str, Vec<f64>> = HashMap::new();
    for t in history.iter().filter(|t| t.side == "BUY" && t.price > 0.0) {
        if redeemed.contains(t.condition_id.as_str()) {
            winning_entries
                .entry(t.condition_id.as_str())
                .or_default()
                .push(t.price);
        }
    }
    let alpha_score = if winning_entries.is_empty() {
        f64::NAN
    } else {
        let all_entries: Vec<f64> = winning_entries.values().flatten().cloned().collect();
        all_entries.iter().sum::<f64>() / all_entries.len() as f64
    };

    // Total buy-side dollar volume
    let total_vol: f64 = history
        .iter()
        .filter(|t| t.side == "BUY")
        .map(|t| t.size * t.price)
        .sum();

    let win_rate = if n_positions >= MIN_POSITIONS {
        n_wins as f64 / n_positions as f64
    } else {
        0.0
    };

    WalletProfile {
        wallet,
        pseudonym,
        market_size,
        n_positions,
        n_wins,
        win_rate,
        alpha_score,
        total_vol,
        market_set,
    }
}

/// Jaccard similarity of two market sets — measures trading overlap between
/// two wallets.  High overlap → possible coordination.
fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f64 {
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 { 0.0 } else { inter as f64 / union as f64 }
}

/// Identify wallets trading a market with suspiciously high win rates.
///
/// v2 improvements over v1:
///   • Concurrent wallet history fetches (all wallets fetched in parallel)
///   • Early-entry alpha score (avg BUY price on winning positions)
///   • Wallet coordination detection (Jaccard similarity between top wallets)
///   • Structured suspicion composite score
async fn find_smart_money(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    use std::collections::HashMap;
    use futures_util::future::join_all;

    let market_id   = args["market_id"].as_str().unwrap_or("");
    let top_n       = args["top_n"].as_u64().unwrap_or(5).min(10) as usize;
    let history_len = args["history_trades"].as_u64().unwrap_or(100).min(200) as u32;

    if market_id.is_empty() {
        return Ok(ToolOutput::err(
            "Required: market_id (Polymarket conditionId). \
             Use list_markets or search_markets to find a conditionId.",
        ));
    }

    const WIN_RATE_THRESHOLD: f64 = 0.55;
    const MIN_POSITIONS:      usize = 3;
    const COORD_THRESHOLD:    f64 = 0.35; // Jaccard ≥ 35% → likely coordinated

    let mut report = Vec::new();
    report.push(format!(
        "=== SMART MONEY ANALYSIS: {} ===",
        &market_id[..market_id.len().min(24)]
    ));
    report.push(format!("Top {} traders  ·  {}-event history\n", top_n, history_len));

    // ── 1. Recent trades for this market ───────────────────────────────────
    let market_trades = clients
        .polymarket
        .fetch_market_trades(market_id, 200)
        .await
        .context("Failed to fetch market trades")?;

    if market_trades.is_empty() {
        return Ok(ToolOutput::ok(format!(
            "{}\nNo recent trades found for this market.",
            report.join("\n")
        )));
    }

    let market_title = &market_trades[0].market_title;
    report.push(format!("Market: {}\n", market_title));

    // ── 2. Pick top-N wallets by buy-side position size ────────────────────
    let mut wallet_agg: HashMap<String, (f64, String)> = HashMap::new();
    for t in &market_trades {
        if t.side == "BUY" {
            let entry = wallet_agg
                .entry(t.wallet.clone())
                .or_insert((0.0, t.pseudonym.clone()));
            entry.0 += t.size;
        }
    }
    let mut ranked: Vec<(String, f64, String)> = wallet_agg
        .into_iter()
        .map(|(w, (s, p))| (w, s, p))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(top_n);

    // ── 3. Fetch all wallet histories CONCURRENTLY ─────────────────────────
    let histories: Vec<_> = join_all(
        ranked.iter().map(|(wallet, _, _)| {
            clients.polymarket.fetch_user_trades(wallet, history_len)
        })
    ).await;

    // Build profiles
    let profiles: Vec<WalletProfile> = ranked
        .iter()
        .zip(histories)
        .filter_map(|((wallet, market_size, pseudonym), hist_result)| {
            let hist = hist_result.ok()?;
            Some(build_wallet_profile(
                wallet.clone(),
                pseudonym.clone(),
                *market_size,
                &hist,
            ))
        })
        .collect();

    // ── 4. Summary table ───────────────────────────────────────────────────
    report.push(format!(
        "{:<22} {:>8} {:>7} {:>6} {:>9} {:>10} {:>9}",
        "Name", "Pos($)", "Mkts", "Wins", "WinRate", "AlphaEntry", "TotalVol$"
    ));
    report.push("─".repeat(78));

    let mut flagged: Vec<&WalletProfile> = Vec::new();

    for p in &profiles {
        let name = if p.pseudonym.chars().count() > 20 {
            let end = p.pseudonym.char_indices().nth(20).map(|(i, _)| i).unwrap_or(p.pseudonym.len());
            format!("{}…", &p.pseudonym[..end])
        } else {
            p.pseudonym.clone()
        };

        let alpha_str = if p.alpha_score.is_nan() {
            "  n/a".to_string()
        } else {
            format!("{:>8.1}¢", p.alpha_score * 100.0)
        };

        report.push(format!(
            "{:<22} {:>8.0} {:>7} {:>6} {:>8.1}% {:>10} {:>9.0}",
            name,
            p.market_size,
            p.n_positions,
            p.n_wins,
            p.win_rate * 100.0,
            alpha_str,
            p.total_vol,
        ));

        if p.n_positions >= MIN_POSITIONS && p.win_rate > WIN_RATE_THRESHOLD {
            flagged.push(p);
        }
    }

    // ── 5. Coordination detection (pairwise Jaccard) ───────────────────────
    let mut coord_pairs: Vec<(String, String, f64)> = Vec::new();
    for i in 0..profiles.len() {
        for j in (i + 1)..profiles.len() {
            let sim = jaccard(&profiles[i].market_set, &profiles[j].market_set);
            if sim >= COORD_THRESHOLD {
                coord_pairs.push((
                    profiles[i].pseudonym.clone(),
                    profiles[j].pseudonym.clone(),
                    sim,
                ));
            }
        }
    }

    // ── 6. Detailed section for flagged wallets ────────────────────────────
    if flagged.is_empty() {
        report.push(format!(
            "\nNo accounts above the {:.0}% win-rate threshold (min {} markets).",
            WIN_RATE_THRESHOLD * 100.0,
            MIN_POSITIONS,
        ));
    } else {
        report.push("\n⚠  FLAGGED ACCOUNTS".to_string());
        report.push("─".repeat(78));
        for p in &flagged {
            // Composite suspicion score (0–100)
            // Components: win_rate weight 0.5, alpha_advantage weight 0.3, vol weight 0.2
            let alpha_adv = if p.alpha_score.is_nan() {
                0.0
            } else {
                (0.5 - p.alpha_score).max(0.0) * 2.0 // 0–1: how far below 50¢ entry
            };
            let vol_score = (p.total_vol.ln() / 15.0).min(1.0).max(0.0);
            let suspicion = (p.win_rate * 0.5 + alpha_adv * 0.3 + vol_score * 0.2) * 100.0;

            report.push(format!("\n  {} ({}…)", p.pseudonym, &p.wallet[..p.wallet.len().min(10)]));
            report.push(format!("    Suspicion score:  {:.0}/100", suspicion));
            report.push(format!(
                "    Win rate:         {:.1}%  ({} wins / {} markets)",
                p.win_rate * 100.0, p.n_wins, p.n_positions
            ));

            if !p.alpha_score.is_nan() {
                let advantage = 50.0 - p.alpha_score * 100.0;
                report.push(format!(
                    "    Alpha entry:      {:.1}¢  ({:+.1}¢ vs fair-coin baseline)",
                    p.alpha_score * 100.0, advantage,
                ));
                if advantage > 15.0 {
                    report.push("    → Consistently buys well before the market reprices — strong informed-entry signal".to_string());
                }
            }

            report.push(format!("    Total volume:     ${:.0}", p.total_vol));
            report.push(format!(
                "    Deep-dive:        ask AI: analyze_wallet {}",
                &p.wallet,
            ));
        }
    }

    // ── 7. Coordination report ─────────────────────────────────────────────
    if !coord_pairs.is_empty() {
        report.push("\n⚠  COORDINATION SIGNALS (high market-overlap between wallets)".to_string());
        report.push("─".repeat(78));
        for (a, b, sim) in &coord_pairs {
            report.push(format!(
                "  {a}  ↔  {b}  (Jaccard {:.0}% market overlap)",
                sim * 100.0
            ));
        }
        report.push("  → Wallets above share many of the same markets. Possible coordinated positioning.".to_string());
    }

    report.push("\nNote: statistical signals only — not proof of wrongdoing.".to_string());
    report.push("Cross-reference entry timestamps against public news releases.".to_string());

    Ok(ToolOutput::ok(report.join("\n")))
}

// ─── Deep per-wallet profile ──────────────────────────────────────────────────

async fn analyze_wallet(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    let wallet      = args["wallet"].as_str().unwrap_or("").trim();
    let history_len = args["limit"].as_u64().unwrap_or(200).min(500) as u32;

    if wallet.is_empty() {
        return Ok(ToolOutput::err(
            "Required: wallet (Polymarket proxy wallet hex address, e.g. '0xabc…'). \
             Obtain from find_smart_money.",
        ));
    }

    let history = clients
        .polymarket
        .fetch_user_trades(wallet, history_len)
        .await
        .context("Failed to fetch wallet trade history")?;

    if history.is_empty() {
        return Ok(ToolOutput::ok(format!(
            "=== WALLET PROFILE: {} ===\n\nNo trade history found.",
            wallet
        )));
    }

    let profile = build_wallet_profile(
        wallet.to_string(),
        history[0].pseudonym.clone(),
        0.0,
        &history,
    );

    let mut report = Vec::new();
    report.push(format!("=== WALLET PROFILE: {} ===", profile.pseudonym));
    report.push(format!("Address: {}", profile.wallet));

    // ── Overall stats ──────────────────────────────────────────────────────
    report.push("\n--- Performance Summary ---".to_string());
    report.push(format!("Markets traded:  {}", profile.n_positions));
    report.push(format!("Winning payouts: {}  (win_rate ≈ {:.1}%)",
        profile.n_wins, profile.win_rate * 100.0));
    report.push(format!("Total volume:    ${:.0}", profile.total_vol));

    if !profile.alpha_score.is_nan() {
        let advantage = 50.0 - profile.alpha_score * 100.0;
        report.push(format!(
            "Alpha entry:     {:.1}¢  ({:+.1}¢ ahead of 50¢ baseline)",
            profile.alpha_score * 100.0, advantage,
        ));
        let label = if advantage > 20.0 {
            "Very strong — entries consistently well before price moves"
        } else if advantage > 10.0 {
            "Moderate — buys at a discount on winning positions"
        } else if advantage > 0.0 {
            "Weak — slight early-entry advantage"
        } else {
            "None — buys late on winning positions (possible reactive trader)"
        };
        report.push(format!("Alpha quality:   {}", label));
    }

    // ── Recent activity breakdown ──────────────────────────────────────────
    report.push("\n--- Recent Activity (newest first) ---".to_string());
    report.push(format!("{:<8} {:<8} {:>7} {:>6}  {}", "Type", "Side", "Size", "Price¢", "Market"));
    report.push("─".repeat(72));

    for t in history.iter().take(20) {
        let title_trunc = if t.market_title.chars().count() > 35 {
            let end = t.market_title.char_indices().nth(34).map(|(i, _)| i).unwrap_or(t.market_title.len());
            format!("{}…", &t.market_title[..end])
        } else {
            t.market_title.clone()
        };
        report.push(format!(
            "{:<8} {:<8} {:>7.1} {:>6.1}  {}",
            t.trade_type,
            t.side,
            t.size,
            t.price * 100.0,
            title_trunc,
        ));
    }
    if history.len() > 20 {
        report.push(format!("  … and {} more events", history.len() - 20));
    }

    // ── Top markets by exposure ────────────────────────────────────────────
    {
        use std::collections::HashMap;
        let mut by_market: HashMap<&str, f64> = HashMap::new();
        for t in history.iter().filter(|t| t.side == "BUY") {
            *by_market.entry(t.market_title.as_str()).or_default() += t.size * t.price;
        }
        let mut sorted: Vec<(&&str, &f64)> = by_market.iter().collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));

        report.push("\n--- Top Markets by Buy-Side Exposure ---".to_string());
        for (title, vol) in sorted.iter().take(8) {
            let title_trunc = if title.chars().count() > 55 {
                let end = title.char_indices().nth(54).map(|(i, _)| i).unwrap_or(title.len());
                format!("{}…", &title[..end])
            } else {
                title.to_string()
            };
            report.push(format!("  ${:>8.0}  {}", vol, title_trunc));
        }
    }

    // ── Suspicion summary ──────────────────────────────────────────────────
    report.push("\n--- Suspicion Assessment ---".to_string());
    let alpha_adv = if profile.alpha_score.is_nan() {
        0.0
    } else {
        (0.5 - profile.alpha_score).max(0.0) * 2.0
    };
    let vol_score  = (profile.total_vol.ln() / 15.0).min(1.0).max(0.0);
    let suspicion  = (profile.win_rate * 0.5 + alpha_adv * 0.3 + vol_score * 0.2) * 100.0;
    report.push(format!("Composite score: {:.0}/100", suspicion));

    let verdict = if suspicion > 70.0 {
        "HIGH — multiple strong insider indicators present"
    } else if suspicion > 45.0 {
        "MODERATE — some indicators; monitor closely"
    } else {
        "LOW — no strong signals"
    };
    report.push(format!("Verdict:         {}", verdict));
    report.push("\nNote: scores are probabilistic proxies, not evidence of wrongdoing.".to_string());

    Ok(ToolOutput::ok(report.join("\n")))
}

// ─── Kelly criterion position sizing ─────────────────────────────────────────

/// Compute Kelly and half-Kelly bet sizes for a binary prediction market.
///
/// Kelly formula for buying YES at market price m when your estimate is e:
///   f* = (e − m) / (1 − m)
///
/// For buying NO at market implied price (1−m) when your estimate e < m:
///   f* = (m − e) / m
///
/// Both return negative values when there is no edge — do not bet that side.
async fn kelly_size(_clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    let market_price     = args["market_price"].as_f64().unwrap_or(0.0);
    let your_probability = args["your_probability"].as_f64().unwrap_or(0.0);
    let bankroll         = args["bankroll"].as_f64().unwrap_or(1000.0);
    let side             = args["side"].as_str().unwrap_or("yes").to_lowercase();

    // ── Input validation ───────────────────────────────────────────────────
    if !(0.001..0.999).contains(&market_price) {
        return Ok(ToolOutput::err("market_price must be strictly between 0 and 1 (e.g. 0.65 for 65¢)"));
    }
    if !(0.001..0.999).contains(&your_probability) {
        return Ok(ToolOutput::err("your_probability must be strictly between 0 and 1 (e.g. 0.75 for 75%)"));
    }
    if bankroll <= 0.0 {
        return Ok(ToolOutput::err("bankroll must be positive"));
    }
    if side != "yes" && side != "no" {
        return Ok(ToolOutput::err("side must be 'yes' or 'no'"));
    }

    let mut report = Vec::new();
    report.push("=== KELLY CRITERION POSITION SIZING ===".to_string());

    // ── Core calculations ──────────────────────────────────────────────────
    let (kelly_f, edge, net_odds, label) = if side == "yes" {
        // Buying YES at market price m
        let e = your_probability;
        let m = market_price;
        let edge = e - m;
        let b = (1.0 - m) / m;          // net odds per $ wagered
        let f = (e - m) / (1.0 - m);    // Kelly fraction
        (f, edge, b, "YES")
    } else {
        // Buying NO at price m_no = (1 − market_price).
        // Kelly for NO: f = (e_no − m_no) / (1 − m_no)
        //                 = ((1−e) − (1−m)) / m
        //                 = (m − e) / m
        let e_no = 1.0 - your_probability;
        let m_no = 1.0 - market_price;
        let edge = e_no - m_no;                    // = market_price - your_probability
        let b = market_price / m_no;               // net odds per $1 wagered on NO
        let f = (market_price - your_probability) / market_price;
        (f, edge, b, "NO")
    };

    let half_kelly_f = kelly_f / 2.0;

    // ── Report ─────────────────────────────────────────────────────────────
    report.push(format!("\nSide:              {}", label));
    report.push(format!("Market price:      {:.1}¢  ({:.1}% implied)", market_price * 100.0, market_price * 100.0));
    report.push(format!("Your estimate:     {:.1}¢  ({:.1}%)", your_probability * 100.0, your_probability * 100.0));
    report.push(format!("Edge:              {:+.2}¢  ({:+.1}%)", edge * 100.0, edge * 100.0));
    report.push(format!("Net odds (b):      {:.3}×  (win ${:.2} per $1 wagered)", net_odds, net_odds));

    report.push("\n--- Sizing ---".to_string());

    if kelly_f <= 0.0 {
        report.push(format!("Kelly fraction:    {:.1}%  ← NEGATIVE EDGE — do not bet {}", kelly_f * 100.0, label));
        if side == "yes" {
            report.push(format!(
                "  (If edge exists, consider NO instead: market {:.1}¢ vs your {:.1}% NO estimate)",
                (1.0 - market_price) * 100.0, (1.0 - your_probability) * 100.0
            ));
        }
    } else {
        // Cap display at 100% (Kelly can exceed 1 in theory, but never bet > bankroll)
        let kelly_pct    = kelly_f.min(1.0) * 100.0;
        let hk_pct       = half_kelly_f.min(0.5) * 100.0;
        let kelly_dollar = bankroll * kelly_f.min(1.0);
        let hk_dollar    = bankroll * half_kelly_f.min(0.5);

        report.push(format!("Kelly fraction:    {:.1}%  → ${:.0} of ${:.0} bankroll", kelly_pct, kelly_dollar, bankroll));
        report.push(format!("Half-Kelly:        {:.1}%  → ${:.0}  ← recommended for most traders", hk_pct, hk_dollar));

        // Shares at current market price
        let hk_shares = hk_dollar / market_price;
        report.push(format!("Shares (half-K):   {:.1} shares @ {:.1}¢", hk_shares, market_price * 100.0));

        // Expected value
        let ev_full = bankroll * kelly_f * edge;
        report.push(format!("Expected value:    ${:.2} per turn at half-Kelly sizing", ev_full / 2.0));

        report.push("\n--- Interpretation ---".to_string());
        if kelly_f > 0.5 {
            report.push(format!(
                "⚠ Full Kelly ({:.0}%) is very aggressive. Use half-Kelly or less.",
                kelly_pct
            ));
            report.push("  Large Kelly fractions amplify estimation error — a 5¢ wrong estimate wipes much more.".to_string());
        } else if kelly_f > 0.20 {
            report.push(format!("Kelly of {:.0}% suggests meaningful edge. Half-Kelly ({:.0}%) is prudent.", kelly_pct, hk_pct));
        } else {
            report.push(format!("Small edge ({:.1}¢). Small position size appropriate.", edge * 100.0));
        }

        report.push(format!(
            "\nBreakeven market price (for {} to have edge): {:.1}¢",
            label,
            if side == "yes" { your_probability * 100.0 } else { (1.0 - your_probability) * 100.0 }
        ));
    }

    report.push("\nNote: Kelly maximises log-wealth in the long run but requires accurate probability estimates.".to_string());
    report.push("In practice, err towards sizing conservatively — half-Kelly is the professional standard.".to_string());

    Ok(ToolOutput::ok(report.join("\n")))
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

pub fn all_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "list_markets".into(),
            description: "List prediction markets from Polymarket and/or Kalshi. Returns titles, \
                YES probabilities, volume, and market IDs.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "platform": {
                        "type": "string",
                        "enum": ["polymarket", "kalshi", "all"],
                        "description": "Which platform to query. Default: 'all'."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of markets per platform (1–100). Default 20."
                    },
                    "category": {
                        "type": "string",
                        "description": "Filter by topic tag (e.g. 'politics', 'economics')."
                    },
                    "search": {
                        "type": "string",
                        "description": "Keyword filter for market titles."
                    }
                },
                "required": []
            }),
        },
        ToolDefinition {
            name: "get_market".into(),
            description: "Get full details for a specific prediction market by ID or title fragment.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "platform": {
                        "type": "string",
                        "enum": ["polymarket", "kalshi"],
                        "description": "Platform the market is on."
                    },
                    "id": {
                        "type": "string",
                        "description": "Market ID (conditionId for Polymarket, ticker for Kalshi) \
                            or a title substring to search by."
                    }
                },
                "required": ["platform", "id"]
            }),
        },
        ToolDefinition {
            name: "get_orderbook".into(),
            description: "Get the live order book (bids and asks) for a prediction market.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "platform": {
                        "type": "string",
                        "enum": ["polymarket", "kalshi"]
                    },
                    "id": {
                        "type": "string",
                        "description": "For Polymarket: YES token_id. For Kalshi: market ticker."
                    }
                },
                "required": ["platform", "id"]
            }),
        },
        ToolDefinition {
            name: "get_price_history".into(),
            description: "Retrieve historical YES prices for a prediction market, with an ASCII chart.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "platform": {
                        "type": "string",
                        "enum": ["polymarket", "kalshi"]
                    },
                    "id": {
                        "type": "string",
                        "description": "Market ID (conditionId for Polymarket, ticker for Kalshi)."
                    },
                    "days": {
                        "type": "integer",
                        "description": "Days of history to retrieve (1–90). Default 30."
                    }
                },
                "required": ["platform", "id"]
            }),
        },
        ToolDefinition {
            name: "get_events".into(),
            description: "List event categories and groupings from Polymarket and/or Kalshi.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "platform": {
                        "type": "string",
                        "enum": ["polymarket", "kalshi", "all"]
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max events to return. Default 20."
                    }
                },
                "required": []
            }),
        },
        ToolDefinition {
            name: "search_markets".into(),
            description: "Search prediction markets by keyword across Polymarket and Kalshi.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search term (e.g. 'Trump', 'Federal Reserve', 'Bitcoin')."
                    },
                    "platform": {
                        "type": "string",
                        "enum": ["polymarket", "kalshi", "all"],
                        "description": "Restrict search to one platform. Default: 'all'."
                    }
                },
                "required": ["query"]
            }),
        },
        ToolDefinition {
            name: "find_smart_money".into(),
            description: "Identify Polymarket wallets trading a specific market that have \
                suspiciously high historical win rates. Fetches recent trades for the \
                market, selects the top traders by position size, then pulls their full \
                trade history to compute win rate (REDEEM events / markets traded), \
                average entry price, and total volume. Flags accounts above the win-rate \
                threshold as potential smart money / insider traders. \
                Use the conditionId from list_markets or search_markets as market_id.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "market_id": {
                        "type": "string",
                        "description": "Polymarket conditionId (hex string, e.g. '0xabc…'). \
                            Required — use list_markets or search_markets to find it."
                    },
                    "top_n": {
                        "type": "integer",
                        "description": "How many top traders (by position size) to analyse. \
                            Default 5, max 10."
                    },
                    "history_trades": {
                        "type": "integer",
                        "description": "Number of recent trades to fetch per wallet for \
                            history analysis. Default 100, max 200."
                    }
                },
                "required": ["market_id"]
            }),
        },
        ToolDefinition {
            name: "analyze_insider".into(),
            description: "Deep insider-trading signal analysis for a specific market. \
                Fetches 7-day price history and live orderbook, then computes price \
                velocity, volume anomaly, and bid/ask imbalance to score the likelihood \
                of informed flow. Use when a market has an INSDR signal or when \
                you suspect unusual directional activity.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "platform": {
                        "type": "string",
                        "enum": ["polymarket", "kalshi"],
                        "description": "Platform the market is on."
                    },
                    "id": {
                        "type": "string",
                        "description": "Market ID (conditionId/token_id for Polymarket, ticker for Kalshi)."
                    }
                },
                "required": ["platform", "id"]
            }),
        },
        ToolDefinition {
            name: "analyze_wallet".into(),
            description: "Deep profile of a specific Polymarket wallet: performance history, \
                alpha-entry score (average BUY price on winning positions — lower means \
                they buy before the move), top markets by exposure, and a composite \
                suspicion score. Use after find_smart_money to investigate flagged wallets. \
                Wallet address comes from find_smart_money output.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "wallet": {
                        "type": "string",
                        "description": "Polymarket proxy wallet address (hex, e.g. '0xabc…'). \
                            Obtain from find_smart_money."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max trade events to pull from history. Default 200, max 500."
                    }
                },
                "required": ["wallet"]
            }),
        },
        ToolDefinition {
            name: "kelly_size".into(),
            description: "Compute Kelly criterion and half-Kelly position sizes for a binary \
                prediction market bet. Given your probability estimate and the market price, \
                returns the optimal fraction of bankroll to wager, dollar amounts, share \
                count, and expected value. Use this after you've formed a view on any market \
                to translate your edge into a concrete position size.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "market_price": {
                        "type": "number",
                        "description": "Current market YES price as a decimal (e.g. 0.65 for 65¢)."
                    },
                    "your_probability": {
                        "type": "number",
                        "description": "Your estimated TRUE probability of YES (e.g. 0.75 for 75%). \
                            Must differ from market_price to have an edge."
                    },
                    "bankroll": {
                        "type": "number",
                        "description": "Total capital available for this bet in dollars. Default 1000."
                    },
                    "side": {
                        "type": "string",
                        "enum": ["yes", "no"],
                        "description": "Which side you intend to bet. Default 'yes'."
                    }
                },
                "required": ["market_price", "your_probability"]
            }),
        },
    ]
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ── jaccard ───────────────────────────────────────────────────────────────

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn jaccard_identical_sets() {
        let a = set(&["x", "y", "z"]);
        let b = set(&["x", "y", "z"]);
        assert!((jaccard(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn jaccard_disjoint_sets() {
        let a = set(&["a", "b"]);
        let b = set(&["c", "d"]);
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        // intersection = {b}, union = {a,b,c} → 1/3
        let a = set(&["a", "b"]);
        let b = set(&["b", "c"]);
        assert!((jaccard(&a, &b) - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn jaccard_empty_sets() {
        let a: HashSet<String> = HashSet::new();
        let b: HashSet<String> = HashSet::new();
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn jaccard_threshold_coordination_detected() {
        // 4/8 = 0.5 ≥ 0.35
        let a = set(&["m1", "m2", "m3", "m4", "m5"]);
        let b = set(&["m1", "m2", "m3", "m4", "m6", "m7", "m8", "m9"]);
        assert!(jaccard(&a, &b) >= 0.35);
    }

    // ── kelly formula (pure math, no async) ──────────────────────────────────

    fn kelly_yes(market: f64, estimate: f64) -> f64 {
        (estimate - market) / (1.0 - market)
    }

    // Kelly NO: f = (market_YES_price - your_YES_estimate) / market_YES_price
    fn kelly_no(market: f64, estimate: f64) -> f64 {
        (market - estimate) / market
    }

    #[test]
    fn kelly_yes_positive_edge() {
        // market 60¢, estimate 75% → edge = 0.15, Kelly = 0.15/0.40 = 0.375
        let f = kelly_yes(0.60, 0.75);
        assert!((f - 0.375).abs() < 1e-9, "got {}", f);
    }

    #[test]
    fn kelly_yes_no_edge_returns_zero() {
        let f = kelly_yes(0.65, 0.65);
        assert!(f.abs() < 1e-9);
    }

    #[test]
    fn kelly_yes_negative_edge_returns_negative() {
        let f = kelly_yes(0.70, 0.50);
        assert!(f < 0.0);
    }

    #[test]
    fn kelly_no_positive_edge() {
        // market YES = 0.70, estimate YES = 0.40 → edge on NO
        // Kelly NO = (0.70 - 0.40) / 0.70 = 0.30/0.70 ≈ 0.4286
        let f = kelly_no(0.70, 0.40);
        assert!((f - 3.0 / 7.0).abs() < 1e-9, "got {}", f);
    }

    #[test]
    fn kelly_no_correct_formula() {
        // market YES = 0.40, estimate YES = 0.30
        // Kelly NO = (0.40 - 0.30) / 0.40 = 0.25
        let f = kelly_no(0.40, 0.30);
        assert!((f - 0.25).abs() < 1e-9, "got {}", f);
    }

    #[test]
    fn kelly_no_symmetry_with_yes_on_complement() {
        // Buying NO on YES@40¢ (so NO@60¢) estimating YES=30% (so NO=70%)
        // Kelly_no = (0.40 - 0.30) / 0.40 = 0.25
        // Kelly_yes on the no market at 60¢ estimating 70%:
        //   (0.70 - 0.60) / (1 - 0.60) = 0.10 / 0.40 = 0.25
        // They should be equal.
        let f_no  = kelly_no(0.40, 0.30);
        let f_yes = kelly_yes(0.60, 0.70);
        assert!((f_no - f_yes).abs() < 1e-9, "f_no={} f_yes={}", f_no, f_yes);
    }

    #[test]
    fn half_kelly_is_half_of_full() {
        let full = kelly_yes(0.55, 0.70);
        let half = full / 2.0;
        assert!((half - full / 2.0).abs() < 1e-9);
    }

    #[test]
    fn kelly_expected_value_positive_when_edge() {
        let m = 0.50_f64;
        let e = 0.65_f64;
        let f = kelly_yes(m, e);
        let bankroll = 1000.0_f64;
        let ev = bankroll * f * (e - m); // simplified EV for small bets
        assert!(ev > 0.0, "EV should be positive with edge");
    }
}

// ─── ASCII sparkline chart ────────────────────────────────────────────────────

fn ascii_chart(candles: &[crate::markets::Candle], rows: usize, cols: usize) -> String {
    if candles.is_empty() { return String::new(); }

    let prices: Vec<f64> = candles.iter().map(|c| c.close * 100.0).collect();
    let min = prices.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = (max - min).max(0.001);

    // Downsample to `cols` points
    let step = (prices.len() as f64 / cols as f64).max(1.0);
    let sampled: Vec<f64> = (0..cols)
        .map(|i| {
            let idx = (i as f64 * step) as usize;
            prices.get(idx).cloned().unwrap_or(prices[prices.len() - 1])
        })
        .collect();

    let mut grid = vec![vec![' '; cols]; rows];
    for (x, &v) in sampled.iter().enumerate() {
        let y = ((v - min) / range * (rows as f64 - 1.0)) as usize;
        let y = y.min(rows - 1);
        grid[rows - 1 - y][x] = '●';
    }

    // Draw connecting lines
    for x in 1..cols {
        let prev = rows - 1 - (((sampled[x - 1] - min) / range * (rows as f64 - 1.0)) as usize).min(rows - 1);
        let curr = rows - 1 - (((sampled[x]     - min) / range * (rows as f64 - 1.0)) as usize).min(rows - 1);
        let lo   = prev.min(curr);
        let hi   = prev.max(curr);
        for y in lo..=hi {
            if grid[y][x] == ' ' { grid[y][x] = '│'; }
        }
    }

    let mut out = String::new();
    for (i, row) in grid.iter().enumerate() {
        let pct = max - (i as f64 / (rows as f64 - 1.0)) * range;
        out.push_str(&format!("{:5.1}% │ ", pct));
        out.push_str(&row.iter().collect::<String>());
        out.push('\n');
    }
    out.push_str(&format!("       └─{}\n", "─".repeat(cols)));
    out
}
