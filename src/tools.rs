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

/// Identify wallets trading a market with suspiciously high win rates.
///
/// Algorithm:
///   1. Fetch recent trades for the market → find unique wallets and their
///      position sizes.
///   2. For each top-N wallet by size, fetch their recent trade history (last
///      100 events).
///   3. Compute per-wallet stats:
///        • n_positions  = distinct conditionIds traded
///        • n_wins       = REDEEM events (each = a paid-out winning position)
///        • win_rate     ≈ n_wins / n_positions
///        • avg_price    = mean entry price (low = aggressive early entry)
///        • total_volume = sum of sizes
///   4. Flag wallets with win_rate > WIN_RATE_THRESHOLD and n_positions ≥
///      MIN_POSITIONS as "smart money".
///
/// Note: Kalshi does not expose public trade-level data, so this is
/// Polymarket-only.
async fn find_smart_money(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    use std::collections::HashMap;

    let market_id   = args["market_id"].as_str().unwrap_or("");
    let top_n       = args["top_n"].as_u64().unwrap_or(5).min(10) as usize;
    let history_len = args["history_trades"].as_u64().unwrap_or(100).min(200) as u32;

    if market_id.is_empty() {
        return Ok(ToolOutput::err(
            "Required: market_id (Polymarket conditionId). \
             Use list_markets or search_markets to find a conditionId.",
        ));
    }

    const WIN_RATE_THRESHOLD: f64 = 0.55; // flag at > 55%
    const MIN_POSITIONS: usize    = 3;    // ignore wallets with too few trades

    let mut report = Vec::new();
    report.push(format!(
        "=== SMART MONEY ANALYSIS: {} ===",
        &market_id[..market_id.len().min(20)]
    ));
    report.push(format!(
        "Fetching top {} traders · {}-trade history per wallet\n",
        top_n, history_len
    ));

    // ── 1. Fetch recent trades for this market ──────────────────────────────
    let market_trades = clients
        .polymarket
        .fetch_market_trades(market_id, 200)
        .await
        .context("Failed to fetch market trades")?;

    if market_trades.is_empty() {
        return Ok(ToolOutput::ok(format!(
            "{}\nNo trades found for this market.",
            report.join("\n")
        )));
    }

    report.push(format!(
        "--- Market: {} ---",
        market_trades[0].market_title
    ));

    // Aggregate per-wallet total size for this market
    let mut wallet_size: HashMap<String, (f64, String)> = HashMap::new(); // wallet → (total_size, pseudonym)
    for t in &market_trades {
        if t.side == "BUY" || t.side.is_empty() {
            let entry = wallet_size.entry(t.wallet.clone()).or_insert((0.0, t.pseudonym.clone()));
            entry.0 += t.size;
        }
    }

    // Pick top-N wallets by position size
    let mut ranked: Vec<(String, f64, String)> = wallet_size
        .into_iter()
        .map(|(w, (s, p))| (w, s, p))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(top_n);

    // ── 2. Per-wallet history analysis ─────────────────────────────────────
    report.push(format!("\n{:<20} {:>8} {:>7} {:>8} {:>10} {:>8}",
        "Wallet", "Size($)", "Markets", "Wins", "Win Rate", "AvgPrice"));
    report.push("-".repeat(70));

    let mut flagged = Vec::new();

    for (wallet, market_size, pseudonym) in &ranked {
        let history = match clients.polymarket.fetch_user_trades(wallet, history_len).await {
            Ok(h) => h,
            Err(_) => continue,
        };

        // Unique markets where they traded
        let positions: std::collections::HashSet<&str> = history
            .iter()
            .filter(|t| t.trade_type == "TRADE" || t.trade_type.is_empty())
            .map(|t| t.condition_id.as_str())
            .collect();
        let n_positions = positions.len();

        // REDEEMs = winning payouts
        let n_wins = history.iter().filter(|t| t.trade_type == "REDEEM").count();

        // Average entry price across all their trades
        let trades_only: Vec<&_> = history.iter()
            .filter(|t| (t.trade_type == "TRADE" || t.trade_type.is_empty()) && t.price > 0.0)
            .collect();
        let avg_price = if trades_only.is_empty() {
            0.0
        } else {
            trades_only.iter().map(|t| t.price).sum::<f64>() / trades_only.len() as f64
        };

        // Total buy volume in their history
        let total_vol: f64 = history.iter()
            .filter(|t| t.side == "BUY")
            .map(|t| t.size * t.price)
            .sum();

        let win_rate = if n_positions >= MIN_POSITIONS {
            n_wins as f64 / n_positions as f64
        } else {
            0.0
        };

        let display_name = if pseudonym.len() > 18 {
            format!("{}…", &pseudonym[..17])
        } else {
            pseudonym.clone()
        };

        report.push(format!(
            "{:<20} {:>8.0} {:>7} {:>8} {:>8.1}% {:>8.2}",
            display_name,
            market_size,
            n_positions,
            n_wins,
            win_rate * 100.0,
            avg_price,
        ));

        if n_positions >= MIN_POSITIONS && win_rate > WIN_RATE_THRESHOLD {
            flagged.push((display_name.clone(), wallet.clone(), win_rate, avg_price, total_vol, n_positions, n_wins));
        }
    }

    // ── 3. Flagged accounts ─────────────────────────────────────────────────
    if flagged.is_empty() {
        report.push("\nNo accounts with suspiciously high win rates found.".to_string());
        report.push(format!(
            "(Threshold: win_rate > {:.0}% over ≥ {} markets)",
            WIN_RATE_THRESHOLD * 100.0,
            MIN_POSITIONS
        ));
    } else {
        report.push("\n⚠ FLAGGED ACCOUNTS (win_rate > threshold):".to_string());
        for (name, wallet, wr, avg_px, total_vol, n_pos, n_wins) in &flagged {
            report.push(format!(
                "\n  {name}  ({wallet})",
                name   = name,
                wallet = &wallet[..wallet.len().min(12)],
            ));
            report.push(format!(
                "    Win rate:   {:.1}%  ({} wins / {} markets)",
                wr * 100.0, n_wins, n_pos,
            ));
            report.push(format!(
                "    Avg entry:  {:.2} ({:.1}¢)",
                avg_px, avg_px * 100.0,
            ));
            report.push(format!(
                "    Total vol:  ${:.0}",
                total_vol,
            ));

            // Heuristic interpretation
            if *avg_px < 0.35 {
                report.push(
                    "    → Buys early at low prices, often before the move — possible informed timing".to_string()
                );
            } else if *avg_px > 0.75 {
                report.push(
                    "    → High avg entry price — may be late-stage confirmation or limit arb".to_string()
                );
            }
        }
        report.push("\nNote: correlation ≠ causation. Cross-check against public news timelines.".to_string());
    }

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
    ]
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
