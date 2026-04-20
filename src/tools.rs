//! Tool definitions and dispatch for the AI agent.
//!
//! All tools are async and return plain strings shown to the LLM and TUI.

use anyhow::{Context, Result};
use serde_json::json;

use crate::fred::FredClient;
use crate::llm::ToolDefinition;
use crate::markets::{kalshi::KalshiClient, polymarket::PolymarketClient, ChartInterval};
use crate::news::NewsClient;

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
    pub polymarket:    PolymarketClient,
    pub kalshi:        KalshiClient,
    /// None when `NEWSDATA_API_KEY` is not set.
    pub news:          Option<NewsClient>,
    /// None when `FRED_API_KEY` is not set.
    pub fred:          Option<FredClient>,
    /// Max trades/redeems to fetch per wallet (CLI --history flag, default 500).
    pub history_limit: u32,
}

impl MarketClients {
    pub fn new(
        newsdata_api_key: Option<String>,
        fred_api_key:     Option<String>,
        history_limit:    u32,
    ) -> Self {
        MarketClients {
            polymarket: PolymarketClient::new(),
            kalshi:     KalshiClient::new(),
            news:       newsdata_api_key.map(NewsClient::new),
            fred:       fred_api_key.map(FredClient::new),
            history_limit,
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
        // Use alternate format to include the full anyhow error chain
        // (e.g. "Polymarket /prices-history request failed: HTTP 404: body…")
        Err(err) => ToolOutput::err(format!("{:#}", err)),
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
        "analyze_wallet"      => analyze_wallet(clients, args).await,
        "scan_smart_money"    => scan_smart_money(clients, args).await,
        "get_wallet_positions" => get_wallet_positions(clients, args).await,
        "kelly_size"          => kelly_size(clients, args).await,
        "search_news"         => search_news(clients, args).await,
        "get_market_news"     => get_market_news(clients, args).await,
        _                     => Ok(ToolOutput::err(format!("Unknown tool: {}", name))),
    }
}

// ─── Polymarket ID resolver ───────────────────────────────────────────────────

/// For Polymarket, `get_orderbook` and `get_price_history` need the YES CLOB
/// **token_id** (a long decimal), not the **conditionId** (starts with `0x`).
///
/// The AI frequently passes the conditionId because that's what `list_markets`
/// and `get_market` surface as the primary ID.  This helper detects that case
/// and resolves the token_id via the Gamma API, returning the correct ID to
/// use for CLOB calls.
///
/// If the id doesn't look like a conditionId (i.e. doesn't start with `0x`)
/// it is returned as-is — it's already a token_id.
async fn resolve_pm_token_id(clients: &MarketClients, id: &str) -> Result<String> {
    // conditionIds are 0x-prefixed 32-byte hex strings (66 chars).
    // token_ids are long decimal numbers (no 0x prefix).
    if !id.starts_with("0x") {
        return Ok(id.to_string());
    }

    let market = clients
        .polymarket
        .fetch_market_by_condition_id(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!(
            "conditionId '{}' not found on Polymarket. \
             Use list_markets or search_markets to confirm the ID.", id
        ))?;

    market.token_id.ok_or_else(|| anyhow::anyhow!(
        "Market '{}' has no CLOB token — it may not be actively traded on the CLOB.", market.title
    ))
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
            // Emit both IDs clearly so the AI always has the right one for each call:
            //  • condition_id → use for get_market, find_smart_money, analyze_insider
            //  • token_id     → use for get_orderbook, get_price_history (Polymarket CLOB)
            let token_line = if m.platform == crate::markets::Platform::Polymarket {
                format!(
                    "\nToken ID (CLOB — use for get_orderbook, get_price_history): {}",
                    m.token_id.as_deref().unwrap_or("N/A")
                )
            } else {
                String::new()
            };
            let platform_str = m.platform.to_string();
            let out = format!(
                "Market: {title}\nPlatform: {platform}\nCondition ID (use for get_market, find_smart_money, analyze_insider): {id}{token_line}\nStatus: {status}\nCategory: {cat}\nYES: {yes:.1}%  NO: {no:.1}%\nVolume: {vol}  Liquidity: {liq}\nEnds: {end}\nDescription: {desc}\n\n[Tip: call get_market_news(market_id=\"{id}\", platform=\"{platform_lc}\") to fetch recent news for this market before forming your probability estimate.]",
                title      = m.title,
                platform   = m.platform,
                platform_lc = platform_str.to_lowercase(),
                id         = m.id,
                status     = m.status,
                cat        = m.category.as_deref().unwrap_or("N/A"),
                yes        = m.yes_price * 100.0,
                no         = m.no_price  * 100.0,
                vol        = vol,
                liq        = liq,
                end        = m.end_date.as_deref().unwrap_or("N/A"),
                desc       = m.description.as_deref().unwrap_or("N/A"),
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
        "polymarket" => {
            // The CLOB /book endpoint requires a token_id, not a conditionId.
            // Auto-resolve 0x… conditionIds so the AI doesn't have to remember
            // to pass the right field.
            let token_id = match resolve_pm_token_id(clients, id).await {
                Ok(t)  => t,
                Err(e) => return Ok(ToolOutput::err(e.to_string())),
            };
            clients.polymarket.fetch_orderbook(&token_id).await?
        }
        "kalshi" => clients.kalshi.fetch_orderbook(id).await?,
        _ => return Ok(ToolOutput::err(format!("Unknown platform: {}", platform))),
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
    // Use fidelity=60 (hourly) for all tool requests — fidelity=1440 causes HTTP 400
    // on the CLOB for many markets; hourly gives enough resolution for AI analysis.
    let kalshi_interval = if days <= 1 { ChartInterval::OneDay } else { ChartInterval::OneWeek };

    let candles = match platform {
        "polymarket" => {
            // Price history uses token_id — resolve 0x conditionIds automatically.
            let token_id = match resolve_pm_token_id(clients, id).await {
                Ok(t)  => t,
                Err(e) => return Ok(ToolOutput::err(format!("{:#}", e))),
            };
            match clients
                .polymarket
                .fetch_price_history(&token_id, ChartInterval::OneWeek.polymarket_fidelity(), start_ts, now)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    return Ok(ToolOutput::err(format!(
                        "Price history unavailable for token {}: {:#}",
                        &token_id[..token_id.len().min(20)], e
                    )));
                }
            }
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
                .fetch_candlesticks(series, id, kalshi_interval.kalshi_period_interval(), start_ts, now)
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

// ─── Wallet detail (drill-down from Smart Money tab) ─────────────────────────

/// Full wallet profile fetched on demand when the user presses Enter on a
/// Smart Money row.  Contains computed stats plus raw trade history for display.
#[derive(Clone, Debug)]
pub struct WalletDetail {
    pub wallet:          String,
    pub pseudonym:       String,
    pub n_positions:     usize,
    pub n_wins:          usize,
    pub win_rate:        f64,
    pub alpha_score:     f64,
    pub total_vol:       f64,
    pub is_fresh:        bool,
    pub wallet_age_days: Option<f64>,
    /// Recent TRADE + REDEEM events, newest first.
    pub recent_trades:   Vec<crate::markets::polymarket::PolyTrade>,
    /// Top markets by buy-side dollar exposure (title, $vol), descending.
    pub top_markets:     Vec<(String, f64)>,
}

/// Fetch full wallet detail for the Smart Money drill-down view.
/// Fetches TRADE + REDEEM histories concurrently then builds the profile.
pub async fn fetch_wallet_detail(
    clients: &MarketClients,
    wallet:  &str,
) -> anyhow::Result<WalletDetail> {
    use futures_util::future::join;
    use std::collections::HashMap;

    let (trades_res, redeems_res) = join(
        clients.polymarket.fetch_user_trades(wallet, clients.history_limit),
        clients.polymarket.fetch_user_redeems(wallet, clients.history_limit),
    ).await;

    let mut history = trades_res.context("Failed to fetch wallet trade history")?;
    if let Ok(redeems) = redeems_res {
        history.extend(redeems);
    }

    if history.is_empty() {
        return Ok(WalletDetail {
            wallet:          wallet.to_string(),
            pseudonym:       wallet.to_string(),
            n_positions:     0,
            n_wins:          0,
            win_rate:        0.0,
            alpha_score:     f64::NAN,
            total_vol:       0.0,
            is_fresh:        false,
            wallet_age_days: None,
            recent_trades:   Vec::new(),
            top_markets:     Vec::new(),
        });
    }

    let pseudonym = history.iter()
        .find(|t| !t.pseudonym.is_empty())
        .map(|t| t.pseudonym.clone())
        .unwrap_or_else(|| wallet.to_string());

    let profile = build_wallet_profile(wallet.to_string(), pseudonym, 0.0, &history);

    // Sort by timestamp descending (newest first) for display
    history.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    // Top markets by buy-side dollar exposure
    let mut by_market: HashMap<String, f64> = HashMap::new();
    for t in history.iter().filter(|t| t.side == "BUY" && t.price > 0.0) {
        *by_market.entry(t.market_title.clone()).or_default() += t.size * t.price;
    }
    let mut top_markets: Vec<(String, f64)> = by_market.into_iter().collect();
    top_markets.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    top_markets.truncate(10);

    Ok(WalletDetail {
        wallet:          profile.wallet,
        pseudonym:       profile.pseudonym,
        n_positions:     profile.n_positions,
        n_wins:          profile.n_wins,
        win_rate:        profile.win_rate,
        alpha_score:     profile.alpha_score,
        total_vol:       profile.total_vol,
        is_fresh:        profile.is_fresh,
        wallet_age_days: profile.wallet_age_days,
        recent_trades:   history,
        top_markets,
    })
}

// ─── Smart money public types ────────────────────────────────────────────────

/// Wallet summary emitted to the TUI Smart Money tab.
#[derive(Clone, Debug)]
pub struct SmartMoneyWallet {
    pub wallet:          String,
    pub pseudonym:       String,
    pub market_size:     f64,
    pub n_positions:     usize,
    pub n_wins:          usize,
    pub win_rate:        f64,
    pub alpha_score:     f64,    // NaN = no winning trades in history
    pub total_vol:       f64,
    pub suspicion:       f64,    // 0–100 composite
    pub flagged:         bool,
    pub is_fresh:        bool,
    pub wallet_age_days: Option<f64>,
    pub volume_impact:   f64,
    /// Wilson score lower bound (95% CI) on win rate — the statistically
    /// conservative estimate of the wallet's true edge (NaN if < 5 positions).
    pub stat_lower_bound: f64,
    /// Fraction of the wallet's wins that come from their above-median-sized
    /// positions (0.5 = random; > 0.65 = suspects sizing up on information).
    pub informed_sizing:  f64,
    /// Realised ROI on known winning positions:
    /// (payout − cost) / cost  =  (1 − avg_entry) / avg_entry  on wins.
    /// NaN if no winning trades in history.
    pub profit_roi:       f64,
    /// Average SELL price on exits above 50¢ (NaN if fewer than 2 such sells).
    pub sell_precision:   f64,
    /// Per-signal scores [stat_edge, alpha, informed_sizing, fresh_conc, recency, sell_precision]
    /// in that order; each in [0, 1].  Useful for displaying a breakdown.
    pub signal_scores:    [f64; 6],
}

/// Result returned by `smart_money_for_market` for TUI consumption.
#[derive(Debug)]
pub struct SmartMoneyResult {
    pub market_title: String,
    pub wallets:      Vec<SmartMoneyWallet>,
    pub coord_pairs:  Vec<(String, String, f64)>, // (name_a, name_b, jaccard)
}

// ─── Too-Smart wallet scan (cross-market) ────────────────────────────────────

/// A wallet that shows suspicious behaviour across multiple markets.
/// Produced by `scan_too_smart_wallets` which aggregates per-market suspicion scores.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TooSmartWallet {
    pub wallet:          String,
    pub pseudonym:       String,
    /// Number of scanned markets where this wallet appeared as a top trader.
    pub markets_total:   usize,
    /// Number of those markets where suspicion ≥ 40.
    pub markets_flagged: usize,
    /// Average suspicion score across all appearances.
    pub avg_suspicion:   f64,
    /// Highest suspicion score in any single market.
    pub max_suspicion:   f64,
    /// Total buy-side dollar volume observed across all scanned markets.
    pub total_vol:       f64,
    /// Aggregated win rate (wins / positions from the wallet's trade history).
    pub global_win_rate: f64,
    /// True if the wallet was classified as "fresh" in any of its appearances.
    pub is_fresh:        bool,
    /// Titles of markets where this wallet was flagged (suspicion ≥ 40).
    pub flagged_markets: Vec<String>,
    /// Number of temporal clusters where this wallet entered a market first —
    /// a proxy for being a "leader" that other suspicious wallets follow.
    pub leader_score:    u32,
    /// Percentile rank within the current scan (0–100; higher = more suspicious).
    /// Populated by `headless_scan`; 0.0 in TUI mode.
    pub suspicion_pct:   f64,
}

/// A group of flagged wallets that entered the same market within a short window.
/// Temporal entry clustering is a strong coordination signal — unrelated traders
/// rarely enter niche markets within hours of each other.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TemporalCluster {
    pub market_title:  String,
    pub condition_id:  String,
    /// (wallet_address, pseudonym, first_buy_unix_secs) for each member.
    pub entries:       Vec<(String, String, i64)>,
    /// Time between earliest and latest entry in this cluster (hours).
    pub spread_hours:  f64,
}

/// Result of `scan_too_smart_wallets`.
#[derive(Debug, serde::Serialize)]
pub struct TooSmartResult {
    pub wallets:            Vec<TooSmartWallet>,
    pub markets_scanned:    usize,
    /// Markets where ≥ 2 flagged wallets entered within `TEMPORAL_WINDOW_SECS`.
    pub temporal_clusters:  Vec<TemporalCluster>,
    /// Raw avg_suspicion values for ALL wallets seen (before min_suspicion filter).
    /// Used to compute percentile ranks: where does a flagged wallet sit in the
    /// overall distribution of suspicious-ness?
    pub score_distribution: Vec<f64>,
}

/// A wallet identified by the LLM as suspicious in Too-Smart LLM mode.
#[derive(Clone, Debug)]
pub struct LlmIdentifiedWallet {
    pub wallet:      String,
    pub pseudonym:   String,
    /// LLM's confidence ranking (1 = most suspicious).
    pub rank:        usize,
    /// LLM's analytical reasoning (2–4 sentences).
    pub reasoning:   String,
    /// Specific signals the LLM cited (e.g. "Wilson LB 72% at n=12").
    pub key_signals: Vec<String>,
}

/// Internal per-wallet row produced by `market_wallet_scores`.
struct MarketWalletScore {
    wallet:          String,
    pseudonym:       String,
    suspicion:       f64,
    is_fresh:        bool,
    win_rate:        f64,
    total_vol:       f64,
    market_title:    String,
    condition_id:    String,
    /// Unix timestamp of this wallet's first BUY in this specific market (0 if unknown).
    first_entry_ts:  i64,
}

/// Fetch suspicion scores for ALL top wallets in a single market.
/// Similar to `quick_market_scan` but returns every ranked wallet, not just the best.
async fn market_wallet_scores(
    clients:         &MarketClients,
    market_id:       &str,
    market_title:    &str,
    market_category: Option<&str>,
    market_volume:   Option<f64>,
    top_n:           usize,
) -> Vec<MarketWalletScore> {
    use std::collections::HashMap;
    use futures_util::future::join_all;

    let Ok(trades) = clients.polymarket.fetch_market_trades(market_id, 100).await else {
        return Vec::new();
    };
    if trades.is_empty() { return Vec::new(); }

    let mut agg: HashMap<String, (f64, String)> = HashMap::new();
    for t in &trades {
        if t.side == "BUY" {
            let e = agg.entry(t.wallet.clone()).or_insert((0.0, t.pseudonym.clone()));
            e.0 += t.size;
        }
    }
    let mut ranked: Vec<(String, f64, String)> = agg
        .into_iter().map(|(w, (s, p))| (w, s, p)).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(top_n);

    if ranked.is_empty() { return Vec::new(); }

    // 200 each to reduce sample bias — active wallets can have 500+ trades and
    // a 100-item window may hit a lucky streak unrepresentative of full history.
    let trade_futs  = join_all(ranked.iter().map(|(w, _, _)| clients.polymarket.fetch_user_trades(w, clients.history_limit)));
    let redeem_futs = join_all(ranked.iter().map(|(w, _, _)| clients.polymarket.fetch_user_redeems(w, clients.history_limit)));
    let (trade_hists, redeem_hists) = tokio::join!(trade_futs, redeem_futs);

    let is_niche = market_volume.map(|v| v < 50_000.0).unwrap_or(false);
    let mut scores = Vec::new();

    for (i, (wallet, market_size, pseudonym)) in ranked.iter().enumerate() {
        let mut history = trade_hists[i].as_ref().ok().cloned().unwrap_or_default();
        if let Ok(r) = &redeem_hists[i] { history.extend(r.iter().cloned()); }
        let profile = build_wallet_profile(wallet.clone(), pseudonym.clone(), *market_size, &history);
        let vol_impact = match market_volume {
            Some(v) if v > 0.0 => market_size / v,
            _ => 0.0,
        };
        let cat_mult    = market_insider_risk(market_category, market_title);
        let is_spec     = is_speculation_market(market_category, market_title);
        let (suspicion, _) = compute_suspicion(&profile, vol_impact, is_niche, is_spec, cat_mult);

        // First BUY timestamp in this specific market (for temporal clustering).
        let first_entry_ts = history.iter()
            .filter(|t| t.side == "BUY" && t.condition_id == market_id && t.timestamp > 0)
            .map(|t| t.timestamp)
            .min()
            .unwrap_or(0);

        scores.push(MarketWalletScore {
            wallet:         wallet.clone(),
            pseudonym:      profile.pseudonym,
            suspicion,
            is_fresh:       profile.is_fresh,
            win_rate:       profile.win_rate,
            total_vol:      profile.total_vol,
            market_title:   market_title.to_string(),
            condition_id:   market_id.to_string(),
            first_entry_ts,
        });
    }

    scores
}

/// Scan multiple Polymarket markets and find wallets that are suspicious across
/// many of them — "too smart" traders with persistent cross-market edge.
///
/// Returns wallets that appeared as a top trader in at least `min_appearances`
/// markets and whose average suspicion score is ≥ `min_suspicion`.
pub async fn scan_too_smart_wallets(
    clients:         &MarketClients,
    market_limit:    usize,
    min_appearances: usize,
    min_suspicion:   f64,
) -> anyhow::Result<TooSmartResult> {
    use std::collections::HashMap;
    use futures_util::future::join_all;

    let markets = clients.polymarket
        .fetch_markets(market_limit as u32, None, None)
        .await
        .context("Failed to fetch markets for too-smart scan")?;

    const TEMPORAL_WINDOW_SECS: i64 = 86_400; // 24h coordination window
    // Limit concurrent market scans to avoid hammering the API.
    // Each market scan fires 2×top_n = 10 wallet-history requests, so
    // MAX_CONCURRENT_SCANS=8 caps peak concurrency at ~80 simultaneous requests.
    const MAX_CONCURRENT_SCANS: usize = 8;

    let markets_scanned = markets.len();
    if markets_scanned == 0 {
        return Ok(TooSmartResult {
            wallets: Vec::new(), markets_scanned: 0,
            temporal_clusters: Vec::new(), score_distribution: Vec::new(),
        });
    }

    // Scan markets with bounded concurrency (bounded via semaphore + join_all).
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_SCANS));
    let all_scores: Vec<Vec<MarketWalletScore>> = join_all(markets.iter().map(|m| {
        let sem   = sem.clone();
        let cid   = m.id.clone();
        let title = m.title.clone();
        let cat   = m.category.clone();
        let vol   = m.volume;
        async move {
            let _permit = sem.acquire_owned().await.unwrap();
            market_wallet_scores(clients, &cid, &title, cat.as_deref(), vol, 5).await
        }
    })).await;

    // Aggregate by wallet address and collect per-market entry data for temporal clustering
    struct Agg {
        pseudonym:       String,
        appearances:     usize,
        flagged:         usize,
        suspicion_sum:   f64,
        max_suspicion:   f64,
        total_vol:       f64,
        win_rate_sum:    f64,
        win_rate_count:  usize,
        is_fresh:        bool,
        flagged_markets: Vec<String>,
    }

    let mut map: HashMap<String, Agg> = HashMap::new();
    // market condition_id → (market_title, Vec<(wallet, pseudonym, first_entry_ts, suspicion)>)
    let mut market_entry_map: HashMap<String, (String, Vec<(String, String, i64, f64)>)> = HashMap::new();

    for scores in all_scores {
        for s in scores {
            // Track per-market entries for temporal clustering (any suspicion ≥ 30)
            if s.first_entry_ts > 0 && s.suspicion >= 30.0 {
                let e = market_entry_map
                    .entry(s.condition_id.clone())
                    .or_insert_with(|| (s.market_title.clone(), Vec::new()));
                e.1.push((s.wallet.clone(), s.pseudonym.clone(), s.first_entry_ts, s.suspicion));
            }

            let entry = map.entry(s.wallet.clone()).or_insert(Agg {
                pseudonym:      s.pseudonym.clone(),
                appearances:    0,
                flagged:        0,
                suspicion_sum:  0.0,
                max_suspicion:  0.0,
                total_vol:      0.0,
                win_rate_sum:   0.0,
                win_rate_count: 0,
                is_fresh:       false,
                flagged_markets: Vec::new(),
            });
            entry.appearances += 1;
            entry.suspicion_sum  += s.suspicion;
            entry.total_vol      += s.total_vol;
            if s.suspicion > entry.max_suspicion { entry.max_suspicion = s.suspicion; }
            if s.suspicion >= 40.0 {
                entry.flagged += 1;
                entry.flagged_markets.push(s.market_title.clone());
            }
            if s.win_rate > 0.0 {
                entry.win_rate_sum   += s.win_rate;
                entry.win_rate_count += 1;
            }
            if s.is_fresh { entry.is_fresh = true; }
        }
    }

    // ── Temporal clustering: markets where ≥ 2 suspicious wallets entered
    //    within TEMPORAL_WINDOW_SECS of each other.
    let mut temporal_clusters: Vec<TemporalCluster> = Vec::new();
    for (cid, (market_title, mut entries)) in market_entry_map {
        if entries.len() < 2 { continue; }
        entries.sort_by_key(|e| e.2); // sort by first_entry_ts ascending
        let earliest = entries[0].2;
        let latest   = entries[entries.len() - 1].2;
        if latest - earliest <= TEMPORAL_WINDOW_SECS {
            // All entries in this market fall within the window — flag as a cluster
            let spread_hours = (latest - earliest) as f64 / 3600.0;
            temporal_clusters.push(TemporalCluster {
                market_title,
                condition_id: cid,
                entries: entries.into_iter().map(|(w, p, ts, _)| (w, p, ts)).collect(),
                spread_hours,
            });
        } else {
            // Sliding-window search for sub-clusters within the window
            for i in 0..entries.len() {
                let window_end = entries[i].2 + TEMPORAL_WINDOW_SECS;
                let cluster: Vec<_> = entries[i..].iter()
                    .take_while(|e| e.2 <= window_end)
                    .collect();
                if cluster.len() >= 2 {
                    let spread_hours = (cluster.last().unwrap().2 - cluster[0].2) as f64 / 3600.0;
                    temporal_clusters.push(TemporalCluster {
                        market_title: market_title.clone(),
                        condition_id: cid.clone(),
                        entries: cluster.iter().map(|(w, p, ts, _)| (w.clone(), p.clone(), *ts)).collect(),
                        spread_hours,
                    });
                    break; // one cluster per market is enough for the report
                }
            }
        }
    }
    temporal_clusters.sort_by(|a, b| {
        let size_cmp = b.entries.len().cmp(&a.entries.len());
        if size_cmp != std::cmp::Ordering::Equal { size_cmp }
        else { a.spread_hours.partial_cmp(&b.spread_hours).unwrap_or(std::cmp::Ordering::Equal) }
    });
    temporal_clusters.truncate(10);

    // ── Leader-follower scoring ───────────────────────────────────────────
    // A wallet that enters a market FIRST in a temporal cluster across multiple
    // markets is a "leader" — other suspicious wallets appear to follow its trades.
    // leader_scores[wallet] = number of clusters where this wallet was earliest.
    let mut leader_scores: HashMap<String, u32> = HashMap::new();
    for cluster in &temporal_clusters {
        if cluster.entries.len() >= 2 {
            let first_wallet = &cluster.entries[0].0;
            *leader_scores.entry(first_wallet.clone()).or_default() += 1;
        }
    }

    // ── Score distribution for percentile ranking ─────────────────────────
    // Collect ALL avg_suspicion values (before filtering) so callers can
    // compute percentile ranks for flagged wallets.
    let score_distribution: Vec<f64> = map.values()
        .filter(|a| a.appearances >= min_appearances)
        .map(|a| a.suspicion_sum / a.appearances as f64)
        .collect();

    // Filter, score, sort wallets
    let mut wallets: Vec<TooSmartWallet> = map
        .into_iter()
        .filter_map(|(wallet, a)| {
            if a.appearances < min_appearances { return None; }
            let avg_suspicion = a.suspicion_sum / a.appearances as f64;
            if avg_suspicion < min_suspicion { return None; }
            let global_win_rate = if a.win_rate_count > 0 {
                a.win_rate_sum / a.win_rate_count as f64
            } else { 0.0 };
            let leader_score = *leader_scores.get(&wallet).unwrap_or(&0);
            Some(TooSmartWallet {
                wallet,
                pseudonym:       a.pseudonym,
                markets_total:   a.appearances,
                markets_flagged: a.flagged,
                avg_suspicion,
                max_suspicion:   a.max_suspicion,
                total_vol:       a.total_vol,
                global_win_rate,
                is_fresh:        a.is_fresh,
                flagged_markets: a.flagged_markets,
                leader_score,
                suspicion_pct:   0.0, // populated by headless_scan after collection
            })
        })
        .collect();

    wallets.sort_by(|a, b| b.avg_suspicion.partial_cmp(&a.avg_suspicion).unwrap_or(std::cmp::Ordering::Equal));
    wallets.truncate(25);

    Ok(TooSmartResult { wallets, markets_scanned, temporal_clusters, score_distribution })
}

// ─── Headless smart-money scan (--scan mode) ─────────────────────────────────

/// Compute the percentile rank of `score` within `distribution` (0–100).
fn percentile_rank(distribution: &[f64], score: f64) -> f64 {
    if distribution.is_empty() { return 0.0; }
    let below = distribution.iter().filter(|&&s| s < score).count();
    (below as f64 / distribution.len() as f64 * 100.0).round()
}

/// Human-readable confidence level based on number of completed positions.
fn confidence_label(n_positions: usize) -> &'static str {
    match n_positions {
        0..=4  => "Very Low",
        5..=9  => "Low",
        10..=29 => "Moderate",
        30..=74 => "Good",
        _ => "High",
    }
}

/// One-shot headless scan: runs the full smart-money analysis pipeline and
/// returns either a formatted text report or JSON, suitable for stdout/pipes.
///
/// Parameters:
///   `market_limit`   — markets to scan (default 30)
///   `min_suspicion`  — minimum avg suspicion to flag a wallet (default 40.0)
///   `deep_dive_n`    — top wallets to profile in depth (default 5)
///   `json_output`    — if true, emit JSON instead of human-readable text
pub async fn headless_scan(
    clients:       &MarketClients,
    market_limit:  usize,
    min_suspicion: f64,
    deep_dive_n:   usize,
    json_output:   bool,
) -> anyhow::Result<String> {
    use chrono::Utc;

    let scan_start = std::time::Instant::now();
    let ts = Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();

    eprintln!("[scan] Starting WhoIsSharp smart-money scan  {}", ts);
    eprintln!("[scan] Parameters: markets={} min_suspicion={:.0} deep_dive={}", market_limit, min_suspicion, deep_dive_n);
    eprintln!("[scan] Fetching market list and wallet histories (bounded to 8 concurrent)…");

    let mut result = scan_too_smart_wallets(clients, market_limit, 1, min_suspicion).await?;

    eprintln!("[scan] Scanned {} markets in {:.1}s  →  {} flagged wallets",
        result.markets_scanned, scan_start.elapsed().as_secs_f32(), result.wallets.len());

    // ── Populate percentile ranks for each flagged wallet ─────────────────
    let dist = result.score_distribution.clone();
    for w in result.wallets.iter_mut() {
        w.suspicion_pct = percentile_rank(&dist, w.avg_suspicion);
    }

    // ── JSON output ───────────────────────────────────────────────────────
    if json_output {
        #[derive(serde::Serialize)]
        struct JsonReport<'a> {
            timestamp:        &'a str,
            markets_scanned:  usize,
            min_suspicion:    f64,
            wallets:          &'a [TooSmartWallet],
            temporal_clusters: &'a [TemporalCluster],
        }
        let report = JsonReport {
            timestamp:         &ts,
            markets_scanned:   result.markets_scanned,
            min_suspicion,
            wallets:           &result.wallets,
            temporal_clusters: &result.temporal_clusters,
        };
        return Ok(serde_json::to_string_pretty(&report)?);
    }

    // ── Text report ───────────────────────────────────────────────────────
    let mut out = Vec::<String>::new();
    const W: usize = 72;

    out.push(format!("╔{}╗", "═".repeat(W)));
    out.push(format!("║{:<W$}║", "  WhoIsSharp — Smart Money Headless Scan"));
    out.push(format!("║{:<W$}║", format!("  {}", ts)));
    out.push(format!("╚{}╝", "═".repeat(W)));
    out.push(format!("Markets scanned: {}  |  Flagged wallets: {}  |  Elapsed: {:.1}s",
        result.markets_scanned, result.wallets.len(), scan_start.elapsed().as_secs_f32()));
    out.push(format!("Score threshold: {:.0}/100  |  Distribution: {} wallets sampled",
        min_suspicion, dist.len()));
    out.push("─".repeat(W));

    if result.wallets.is_empty() {
        out.push(format!("No wallets met the suspicion threshold of {:.0}.", min_suspicion));
    } else {
        out.push(format!("{:<6}  {:<3}  {:<3}  {:<22}  {:>4}  {:>4}  {:>5}  {:>8}  {}",
            "Score", "Pct", "Ldr", "Pseudonym", "Mkt", "Flg", "WinR%", "Vol$", "Address"));
        out.push("─".repeat(W));
        for (i, w) in result.wallets.iter().enumerate() {
            let addr = &w.wallet[..w.wallet.len().min(14)];
            let name: String = w.pseudonym.chars().take(22).collect();
            let leader_tag = if w.leader_score > 0 { format!("×{}", w.leader_score) } else { "   ".to_string() };
            out.push(format!("{:>5.0}/100  {:>2.0}%  {:<3}  {:<22}  {:>4}  {:>4}  {:>5.1}  {:>8.0}  {}…",
                w.avg_suspicion, w.suspicion_pct, leader_tag, name,
                w.markets_total, w.markets_flagged,
                w.global_win_rate * 100.0, w.total_vol, addr));

            // Flagged market list (indented, truncated)
            if !w.flagged_markets.is_empty() {
                let mkt_list: String = w.flagged_markets.iter()
                    .map(|m| m.chars().take(35).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(" | ");
                out.push(format!("  #{:<2}  Flagged in: {}", i + 1, mkt_list));
            }
            // Leader annotation
            if w.leader_score > 0 {
                out.push(format!("       ★ LEADER: entered {} market(s) before other suspicious wallets", w.leader_score));
            }
        }
        out.push(String::new());
        out.push("Columns: Score=avg_suspicion Pct=percentile_in_scan Ldr=leader_count".to_string());
    }

    // ── Temporal coordination clusters ────────────────────────────────────
    if !result.temporal_clusters.is_empty() {
        out.push(String::new());
        out.push(format!("╔{}╗", "═".repeat(W)));
        out.push(format!("║{:<W$}║", "  TEMPORAL COORDINATION CLUSTERS"));
        out.push(format!("║{:<W$}║", "  ≥ 2 wallets entered the same market within 24h — strong coordination signal"));
        out.push(format!("╚{}╝", "═".repeat(W)));
        for (i, cluster) in result.temporal_clusters.iter().enumerate() {
            out.push(format!("[{}] {}  (spread: {:.1}h  |  {} wallets)",
                i + 1, cluster.market_title, cluster.spread_hours, cluster.entries.len()));
            out.push(format!("    ConditionId: {}", cluster.condition_id));
            for (idx, (wallet, pseudonym, ts_entry)) in cluster.entries.iter().enumerate() {
                let dt = chrono::DateTime::<Utc>::from_timestamp(*ts_entry, 0)
                    .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "?".to_string());
                let role = if idx == 0 { "LEADER".to_string() } else { format!("  +{:.1}h", (ts_entry - cluster.entries[0].2) as f64 / 3600.0) };
                out.push(format!("    {:<6}  {} ({})  entered: {}", role, pseudonym, &wallet[..wallet.len().min(14)], dt));
            }
            out.push(String::new());
        }
    }

    // ── Per-wallet deep dive ──────────────────────────────────────────────
    let to_dive: Vec<_> = result.wallets.iter().take(deep_dive_n).collect();
    if !to_dive.is_empty() {
        eprintln!("[scan] Running deep-dive on top {} wallets…", to_dive.len());
        out.push(format!("╔{}╗", "═".repeat(W)));
        out.push(format!("║{:<W$}║", format!("  DEEP DIVE: TOP {} WALLETS (full analyze_wallet profile)", to_dive.len())));
        out.push(format!("║{:<W$}║", "  Note: score below excludes per-market volume context (S4=0 in bulk mode)"));
        out.push(format!("╚{}╝", "═".repeat(W)));
        for (i, w) in to_dive.iter().enumerate() {
            let pct = percentile_rank(&dist, w.avg_suspicion);
            out.push("─".repeat(W));
            out.push(format!("Rank #{} | {:.0}/100 (p{:.0}) | Leader score: {} | {}",
                i + 1, w.avg_suspicion, pct, w.leader_score, w.pseudonym));
            let args = serde_json::json!({ "wallet": w.wallet, "limit": 250 });
            let profile_out = dispatch(clients, "analyze_wallet", &args).await;
            out.push(profile_out.text);
        }
    }

    let elapsed = scan_start.elapsed().as_secs_f32();
    out.push(String::new());
    out.push(format!("╔{}╗", "═".repeat(W)));
    out.push(format!("║{:<W$}║", format!("  SCAN COMPLETE  |  total time: {:.1}s", elapsed)));
    out.push(format!("║{:<W$}║", "  Tip: --scan-json for machine-readable output | diff with yesterday to spot new suspects"));
    out.push(format!("╚{}╝", "═".repeat(W)));

    eprintln!("[scan] Done in {:.1}s.", elapsed);
    Ok(out.join("\n"))
}

/// Fetch smart money data for a Polymarket market and return structured results.
/// Intended for the TUI Smart Money tab; the AI tool uses `find_smart_money`.
///
/// `market_volume` — the market's daily volume from the Gamma API, used to
/// compute size-anomaly impact scores (wallet position / market volume).
///
/// `coord_threshold` — Jaccard similarity threshold for wallet coordination
/// detection.  Wallet pairs sharing ≥ this fraction of traded markets are
/// flagged as possibly coordinated.  Default 0.35.
pub async fn smart_money_for_market(
    clients:          &MarketClients,
    market_id:        &str,
    top_n:            usize,
    market_volume:    Option<f64>,
    market_category:  Option<&str>,
    coord_threshold:  f64,
) -> anyhow::Result<SmartMoneyResult> {
    use std::collections::HashMap;
    use futures_util::future::join_all;

    let market_trades = clients
        .polymarket
        .fetch_market_trades(market_id, 200)
        .await?;

    if market_trades.is_empty() {
        return Ok(SmartMoneyResult {
            market_title: market_id.to_string(),
            wallets:      Vec::new(),
            coord_pairs:  Vec::new(),
        });
    }

    let market_title = market_trades[0].market_title.clone();

    // Rank wallets by buy-side size in this market
    let mut wallet_agg: HashMap<String, (f64, String)> = HashMap::new();
    for t in &market_trades {
        if t.side == "BUY" {
            let entry = wallet_agg.entry(t.wallet.clone()).or_insert((0.0, t.pseudonym.clone()));
            entry.0 += t.size;
        }
    }
    let mut ranked: Vec<(String, f64, String)> = wallet_agg
        .into_iter()
        .map(|(w, (s, p))| (w, s, p))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(top_n.min(10));

    // Fetch TRADE and REDEEM histories concurrently for every wallet.
    // REDEEMs must be fetched separately because the data-api requires type=.
    let trade_hists: Vec<_> = join_all(
        ranked.iter().map(|(w, _, _)| clients.polymarket.fetch_user_trades(w, clients.history_limit))
    ).await;
    let redeem_hists: Vec<_> = join_all(
        ranked.iter().map(|(w, _, _)| clients.polymarket.fetch_user_redeems(w, clients.history_limit))
    ).await;

    let profiles: Vec<WalletProfile> = ranked
        .iter()
        .zip(trade_hists.iter().zip(redeem_hists.iter()))
        .filter_map(|((wallet, market_size, pseudonym), (trades_res, redeems_res))| {
            let mut history = trades_res.as_ref().ok()?.clone();
            if let Ok(redeems) = redeems_res {
                history.extend(redeems.iter().cloned());
            }
            Some(build_wallet_profile(wallet.clone(), pseudonym.clone(), *market_size, &history))
        })
        .collect();

    // Coordination detection (pairwise Jaccard over traded market sets)
    let mut coord_pairs = Vec::new();
    for i in 0..profiles.len() {
        for j in (i + 1)..profiles.len() {
            let sim = jaccard(&profiles[i].market_set, &profiles[j].market_set);
            if sim >= coord_threshold {
                coord_pairs.push((profiles[i].pseudonym.clone(), profiles[j].pseudonym.clone(), sim));
            }
        }
    }

    // Niche market flag: volume < $50k
    let is_niche = market_volume.map(|v| v < 50_000.0).unwrap_or(false);
    let cat_mult = market_insider_risk(market_category, &market_title);
    let is_spec  = is_speculation_market(market_category, &market_title);

    // Convert to public structs using the improved scoring formula
    let wallets = profiles.iter().map(|p| {
        let volume_impact = match market_volume {
            Some(vol) if vol > 0.0 => p.market_size / vol,
            _ => 0.0,
        };
        let (suspicion, signal_scores) = compute_suspicion(p, volume_impact, is_niche, is_spec, cat_mult);
        let flagged = suspicion >= 50.0;
        let stat_lower_bound = if p.n_positions >= 5 {
            wilson_lower_bound(p.n_wins, p.n_positions, 1.96)
        } else { f64::NAN };
        SmartMoneyWallet {
            wallet:           p.wallet.clone(),
            pseudonym:        p.pseudonym.clone(),
            market_size:      p.market_size,
            n_positions:      p.n_positions,
            n_wins:           p.n_wins,
            win_rate:         p.win_rate,
            alpha_score:      p.alpha_score,
            total_vol:        p.total_vol,
            suspicion,
            flagged,
            is_fresh:         p.is_fresh,
            wallet_age_days:  p.wallet_age_days,
            volume_impact,
            stat_lower_bound,
            informed_sizing:  p.informed_sizing_ratio,
            profit_roi:       p.profit_roi,
            sell_precision:   p.sell_precision,
            signal_scores,
        }
    }).collect();

    Ok(SmartMoneyResult { market_title, wallets, coord_pairs })
}

// ─── Smart money / account analysis ──────────────────────────────────────────

/// Wilson score lower bound — the conservative 95%-CI lower estimate of the
/// true win probability.  Uses the Agresti-Coull approximation.
///
/// z = 1.96 for 95 % CI, 1.645 for 90 % CI.
fn wilson_lower_bound(wins: usize, n: usize, z: f64) -> f64 {
    if n == 0 { return 0.0; }
    let z2     = z * z;
    let n_     = n as f64 + z2;
    let p_tilde = (wins as f64 + z2 / 2.0) / n_;
    (p_tilde - z * (p_tilde * (1.0 - p_tilde) / n_).sqrt()).max(0.0)
}

/// Per-wallet analytics bundle, built from merged TRADE + REDEEM history.
struct WalletProfile {
    wallet:          String,
    pseudonym:       String,
    /// Dollar size in the queried market specifically.
    market_size:     f64,
    /// Distinct markets traded (proxy for experience).
    n_positions:     usize,
    /// REDEEM events (each = a winning payout).
    n_wins:          usize,
    /// n_wins / n_positions — only meaningful when n_positions ≥ MIN_POSITIONS.
    win_rate:        f64,
    /// Mean BUY price on positions that later hit REDEEM (lower = earlier entry).
    alpha_score:     f64,
    /// Total buy-side dollar volume across history.
    total_vol:       f64,
    /// Full set of conditionIds traded (for coordination detection).
    market_set:      std::collections::HashSet<String>,
    /// Heuristic: wallet has ≤10 lifetime trades and all are within 7 days.
    is_fresh:        bool,
    /// Days since oldest observed activity (None if history is empty).
    wallet_age_days: Option<f64>,
    /// Recency-weighted win rate (90-day half-life; emphasises recent positions).
    win_rate_weighted: f64,
    /// Fraction of wins coming from above-median-sized positions.
    /// 0.5 = random; > 0.65 = informed sizing pattern.
    informed_sizing_ratio: f64,
    /// Largest single-market buy-side position / total_vol.
    /// High concentration = all-in on one bet, consistent with private knowledge.
    concentration:   f64,
    /// ROI on winning positions: (payout − cost) / cost = (1 − alpha) / alpha.
    /// Measures quality of early-entry alpha, not just win count.
    profit_roi:      f64,
    /// Average SELL price across SELL events where price > 50¢ (NaN if < 2 such events).
    /// High values (> 70¢) indicate disciplined profit-taking before resolution.
    sell_precision:  f64,
    /// Total number of SELL events in history.
    n_sells:         usize,
    /// Longest consecutive winning streak across chronologically sorted markets.
    /// A streak ≥ 5 is statistically anomalous at random (p < 0.03 for 50% base rate).
    max_win_streak:  usize,
    /// New markets entered per day (n_positions / wallet_age_days).
    /// High values signal an algorithmic or highly active wallet.
    position_velocity: f64,
    /// Fraction of BUY trades placed on the YES outcome (outcome_index == 0).
    /// Strong directional bias (< 0.2 or > 0.8) suggests asymmetric information.
    outcome_yes_fraction: f64,
    /// Markets where avg BUY price was ≤ 90¢ (excludes post-resolution buyers).
    n_quality_positions: usize,
    /// Wins within quality positions only (used by S1 to filter redemption arb).
    n_quality_wins: usize,
}

fn build_wallet_profile(
    wallet:      String,
    pseudonym:   String,
    market_size: f64,
    history:     &[crate::markets::polymarket::PolyTrade],
) -> WalletProfile {
    use std::collections::{HashMap, HashSet};

    const MIN_POSITIONS:    usize = 3;
    const FRESH_MAX_TRADES: usize = 10;
    const FRESH_MAX_DAYS:   f64   = 7.0;

    // ── Market set ────────────────────────────────────────────────────────────
    let market_set: HashSet<String> = history
        .iter()
        .filter(|t| t.trade_type == "TRADE" || t.trade_type.is_empty())
        .map(|t| t.condition_id.clone())
        .collect();
    let n_positions = market_set.len();

    let n_total_trades = history
        .iter()
        .filter(|t| t.trade_type == "TRADE" || t.trade_type.is_empty())
        .count();

    // ── Win set (REDEEM events, restricted to market_set) ────────────────────
    // CRITICAL: Only count REDEEMs for condition_ids that also appear as TRADE
    // events in our history sample.  TRADE and REDEEM are fetched in separate API
    // calls with independent limits; if the limits differ or the wallet is very
    // active, the two sets can be misaligned — e.g. 50 recent REDEEMs from older
    // markets that aren't in the 50 most-recent TRADEs.  Counting those inflates
    // n_wins relative to n_positions and produces spuriously high win rates.
    let all_redeemed: HashSet<&str> = history
        .iter()
        .filter(|t| t.trade_type == "REDEEM")
        .map(|t| t.condition_id.as_str())
        .collect();
    // Intersect with market_set so numerator and denominator cover the same markets.
    let redeemed: HashSet<&str> = all_redeemed
        .iter()
        .copied()
        .filter(|cid| market_set.contains(*cid))
        .collect();
    let n_wins = redeemed.len();

    // ── Per-market buy aggregation (used by several signals below) ───────────
    // For each condition_id: (total_buy_dollar_vol, avg_buy_price_on_wins)
    let mut mkt_buy_vol: HashMap<&str, f64> = HashMap::new();
    for t in history.iter().filter(|t| t.side == "BUY" && t.price > 0.0) {
        *mkt_buy_vol.entry(t.condition_id.as_str()).or_default() += t.size * t.price;
    }

    // ── Alpha score: avg BUY price on positions that later paid out ───────────
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

    // ── Total buy-side dollar volume ─────────────────────────────────────────
    let total_vol: f64 = mkt_buy_vol.values().sum();

    // ── Late-entry filter: exclude markets where avg BUY price > 90¢ ─────────
    // Wallets that buy near-settled markets (avg entry > 90¢) are post-resolution
    // redemption buyers, not informed early traders.  Counting their REDEEMs as
    // "wins" inflates n_wins and corrupts S1/S2.  We compute a parallel set of
    // "quality positions" (avg entry ≤ 90¢) for the win-rate denominator.
    let quality_positions: HashSet<&str> = market_set
        .iter()
        .filter(|cid| {
            let buys: Vec<f64> = history.iter()
                .filter(|t| t.side == "BUY" && t.price > 0.0 && t.condition_id == cid.as_str())
                .map(|t| t.price)
                .collect();
            if buys.is_empty() { return false; }
            let avg = buys.iter().sum::<f64>() / buys.len() as f64;
            avg <= 0.90
        })
        .map(|s| s.as_str())
        .collect();
    let n_quality = quality_positions.len();
    let n_quality_wins = redeemed.iter().filter(|cid| quality_positions.contains(*cid)).count();

    // Cap at 1.0: multi-outcome markets can produce more REDEEMs than traded
    // condition_ids when YES/NO tokens share a condition_id differently.
    let win_rate = if n_positions >= MIN_POSITIONS {
        (n_wins as f64 / n_positions as f64).min(1.0)
    } else { 0.0 };

    // ── Wallet age ───────────────────────────────────────────────────────────
    let now_secs = chrono::Utc::now().timestamp();
    let oldest_ts = history.iter().map(|t| t.timestamp).filter(|&ts| ts > 0).min();
    let wallet_age_days = oldest_ts.map(|ts| (now_secs - ts).max(0) as f64 / 86_400.0);

    let is_fresh = n_total_trades <= FRESH_MAX_TRADES
        && wallet_age_days.map(|d| d <= FRESH_MAX_DAYS).unwrap_or(false);

    // ── Recency-weighted win rate (90-day half-life) ──────────────────────────
    let win_rate_weighted = if n_positions >= MIN_POSITIONS {
        const HALF_LIFE_DAYS: f64 = 90.0;
        let decay = std::f64::consts::LN_2 / (HALF_LIFE_DAYS * 86_400.0);
        let mut mkt_last_ts: HashMap<&str, i64> = HashMap::new();
        for t in history.iter().filter(|t| t.timestamp > 0) {
            let e = mkt_last_ts.entry(t.condition_id.as_str()).or_insert(0);
            *e = (*e).max(t.timestamp);
        }
        let (mut w_pos, mut w_wins) = (0.0f64, 0.0f64);
        for (cid, &ts) in &mkt_last_ts {
            if !market_set.contains(*cid) { continue; }
            let w = (-(now_secs - ts).max(0) as f64 * decay).exp();
            w_pos += w;
            if redeemed.contains(*cid) { w_wins += w; }
        }
        if w_pos > 0.0 { w_wins / w_pos } else { 0.0 }
    } else { 0.0 };

    // ── Informed sizing ratio ─────────────────────────────────────────────────
    // For each market the wallet traded, record (buy_vol, did_win).
    // Then ask: are wins concentrated in the larger-bet half?
    //
    // informed_sizing_ratio = wins_in_top_half / max(1, total_wins)
    // where top_half = markets sorted by buy_vol, upper 50% by count.
    // 0.5 = random; 1.0 = every win came from a large bet.
    let informed_sizing_ratio = {
        let mut mkt_vols: Vec<(f64, bool)> = market_set
            .iter()
            .filter_map(|cid| {
                let vol = *mkt_buy_vol.get(cid.as_str()).unwrap_or(&0.0);
                if vol > 0.0 {
                    Some((vol, redeemed.contains(cid.as_str())))
                } else { None }
            })
            .collect();

        if mkt_vols.len() >= 4 && n_wins >= 2 {
            mkt_vols.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let top_n = (mkt_vols.len() + 1) / 2;
            let wins_in_top: usize = mkt_vols[..top_n].iter().filter(|(_, w)| *w).count();
            wins_in_top as f64 / n_wins as f64
        } else { 0.5 }
    };

    // ── Concentration: largest single-market position / total_vol ────────────
    let concentration = if total_vol > 0.0 {
        mkt_buy_vol.values().cloned().fold(0.0_f64, f64::max) / total_vol
    } else { 0.0 };

    // ── Profit ROI on winning positions ──────────────────────────────────────
    // For each winning market: revenue = Σ(size), cost = Σ(size × price)
    // profit_roi = (revenue − cost) / cost
    //            ≈ (1 − alpha_score) / alpha_score   [for uniform buy prices]
    // Only meaningful when n_wins ≥ 2.
    let profit_roi = if !alpha_score.is_nan() && alpha_score > 0.0 && n_wins >= 2 {
        let (mut rev, mut cost) = (0.0_f64, 0.0_f64);
        for t in history.iter().filter(|t| t.side == "BUY" && t.price > 0.0) {
            if redeemed.contains(t.condition_id.as_str()) {
                rev  += t.size;          // $1 payout per winning share
                cost += t.size * t.price;
            }
        }
        if cost > 0.0 { (rev - cost) / cost } else { f64::NAN }
    } else { f64::NAN };

    // ── Sell-side precision ───────────────────────────────────────────────────
    // High avg SELL price (> 70¢) means the wallet exits positions at near-peak
    // prices, consistent with having foreknowledge of resolution outcomes.
    // Only count sells where price > 50¢ to filter dust / bad-price events.
    // Only count sells on markets where the wallet entered at a reasonable price
    // (avg BUY ≤ 90¢).  Selling at 99¢ after buying at 99¢ is not precision —
    // it's a post-resolution exit.  quality_positions already filters these out.
    let high_sells: Vec<f64> = history.iter()
        .filter(|t| t.side == "SELL" && t.price > 0.5
            && quality_positions.contains(t.condition_id.as_str()))
        .map(|t| t.price)
        .collect();
    let n_sells = history.iter().filter(|t| t.side == "SELL").count();
    let sell_precision = if high_sells.len() >= 2 {
        high_sells.iter().sum::<f64>() / high_sells.len() as f64
    } else { f64::NAN };

    // ── Max consecutive win streak ────────────────────────────────────────────
    // Sort markets by their most-recent trade timestamp (chronological order),
    // then count the longest consecutive run of won markets.
    let max_win_streak = {
        let mut mkt_with_ts: Vec<(i64, bool)> = market_set.iter()
            .map(|cid| {
                let last_ts = history.iter()
                    .filter(|t| &t.condition_id == cid && t.timestamp > 0)
                    .map(|t| t.timestamp)
                    .max()
                    .unwrap_or(0);
                (last_ts, redeemed.contains(cid.as_str()))
            })
            .collect();
        mkt_with_ts.sort_by_key(|e| e.0);
        let mut max_s = 0usize;
        let mut cur_s = 0usize;
        for (_, won) in &mkt_with_ts {
            if *won { cur_s += 1; max_s = max_s.max(cur_s); } else { cur_s = 0; }
        }
        max_s
    };

    // ── Position velocity (markets per day) ───────────────────────────────────
    let position_velocity = wallet_age_days
        .filter(|&d| d >= 1.0)
        .map(|d| n_positions as f64 / d)
        .unwrap_or(0.0);

    // ── Outcome YES/NO bias ───────────────────────────────────────────────────
    // Consistent directional bias (> 80% YES or < 20% YES) may indicate the
    // wallet has asymmetric access to positive or negative resolution information.
    let buy_trades: Vec<_> = history.iter()
        .filter(|t| t.side == "BUY" && (t.trade_type == "TRADE" || t.trade_type.is_empty()))
        .collect();
    let outcome_yes_fraction = if buy_trades.is_empty() {
        f64::NAN
    } else {
        buy_trades.iter().filter(|t| t.outcome_index == 0).count() as f64
        / buy_trades.len() as f64
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
        is_fresh,
        wallet_age_days,
        win_rate_weighted,
        informed_sizing_ratio,
        concentration,
        profit_roi,
        sell_precision,
        n_sells,
        max_win_streak,
        position_velocity,
        outcome_yes_fraction,
        n_quality_positions: n_quality,
        n_quality_wins,
    }
}

/// Jaccard similarity of two market sets — measures trading overlap between
/// two wallets.  High overlap → possible coordination.
fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f64 {
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 { 0.0 } else { inter as f64 / union as f64 }
}

/// Classify a market's insider-trading risk from its category tag and title.
///
/// Returns a multiplier (≥ 1.0) applied to the final suspicion score:
///   1.50 — Politics / Government / Legal / Regulatory
///   1.35 — Company / Finance / Crypto / Macro
///   1.15 — Science / Technology product launches
///   1.00 — Sports / Entertainment / Weather / Gaming (base rate)
///
/// Both `category` (the API field) and `title` (keyword fallback) are checked.
/// Title matching is intentionally conservative: we only upgrade the tier when
/// at least one unambiguous keyword is present so "Will X WIN the championship?"
/// doesn't trigger the politics tier because of the word "will".
pub fn market_insider_risk(category: Option<&str>, title: &str) -> f64 {
    let cat  = category.unwrap_or("").to_lowercase();
    let titl = title.to_lowercase();

    // ── High-risk: Politics, law, regulation, corporate actions ──────────
    let political_cat = ["politics", "political", "election", "government", "legal",
        "law", "policy", "regulation", "geopolitics", "economy", "economics",
        "finance", "financial", "business", "company", "stock", "merger", "ipo",
        "crypto", "bitcoin", "ethereum", "monetary", "macro", "fiscal",
        "federal reserve", "central bank", "tariff", "trade"];
    let political_title = [
        "election", "president", "congress", "senate", "parliament",
        "prime minister", "governor", "supreme court", "indicted", "lawsuit",
        "tariff", "fed rate", "interest rate", "earnings", "revenue", "merger",
        "acquisition", "ipo", "bankruptcy", "indictment", "regulation",
        "sanction", "geopolitical", "gdp", "inflation", "unemployment",
        "bitcoin", "ethereum", "crypto", "defi", "sec ", "fda ", "fbi ",
        "executive order", "legislation", "vote ", "referendum", "impeach",
    ];

    // ── Medium-high: Technology, science (product launches, clinical trials)
    let tech_title = [
        "launch", "release", "announce", "approved by fda", "clinical trial",
        "breakthrough", "patent",
    ];

    // ── Short-term price speculation: no insider info is possible ────────
    // "Will ETH be above $2200 on April 19?" / "Bitcoin Up or Down - 9AM-9:05AM ET"
    // These are hourly/daily price-move bets, not outcome markets with information
    // asymmetry.  Crypto keywords in the title would otherwise trigger ×1.50,
    // inflating scores for pure speculators.  Check this BEFORE political_title.
    //
    // Key patterns: "Up or Down" (intraday binary) and "price above/below" (threshold bets).
    // We intentionally exclude "reach $" / "exceed $" which can be long-horizon bets
    // where macro/regulatory insider info IS relevant (e.g. "Bitcoin reach $100k by 2025?").
    let speculation_title = [
        "up or down",  // classic Polymarket intraday binary format
        "above $",     // "ETH be above $2,200 on April X" — dollar-threshold bet
        "below $",     // "will price be below $1,800"
        "dip to $",    // always short-term directional speculation
        "drop to $",
    ];
    if speculation_title.iter().any(|k| titl.contains(k)) {
        return 1.00;
    }

    // ── Sports / Gaming: explicit downgrade keywords (base rate) ──────────
    let sports_title = [
        "game ", "match", "o/u", "over/under", "kills", "goals", "points",
        "championship", "tournament", "grand slam", "open golf", "heritage",
        "masters golf", "nba", "nfl", "nhl", "mlb", "premier league",
        "world cup", "playoffs", "season", "league", "score ", "vs.",
        "round ", "quarter ", "set ", "inning", "wicket",
    ];

    // ── Sports / Entertainment category → always base rate ───────────────
    let sports_cat = ["sports", "entertainment", "gaming", "esports", "music", "tv", "film",
        "awards", "celebrity", "weather"];
    if sports_cat.iter().any(|k| cat.contains(k)) {
        return 1.00;
    }

    // ── Title speculation patterns override everything else ────────────────
    // Even if the category says "Crypto", a title like "ETH above $2,200 on April 19?"
    // is a pure price-threshold bet where insider information is impossible.
    // This check must happen BEFORE the political_cat check below.
    if speculation_title.iter().any(|k| titl.contains(k)) {
        return 1.00;
    }

    // ── Explicit political/financial/crypto category ───────────────────────
    if political_cat.iter().any(|k| cat.contains(k)) {
        return 1.50;
    }

    // ── Title keyword matching (fallback when no explicit category) ───────────
    // Politics/corporate tier
    if political_title.iter().any(|k| titl.contains(k)) {
        return 1.50;
    }
    // Sports/gaming — explicitly base rate
    if sports_title.iter().any(|k| titl.contains(k)) {
        return 1.00;
    }
    // Tech/science — modest boost
    if tech_title.iter().any(|k| titl.contains(k)) {
        return 1.15;
    }
    // Unknown / ambiguous — no adjustment
    1.00
}

/// Returns true when the market title matches short-term price-speculation patterns
/// (intraday binary bets, daily price-threshold bets).  These markets are inherently
/// low-volume and niche, so the niche multiplier must NOT apply — the market's small
/// size is a structural feature, not a signal of unusual informed interest.
fn is_speculation_market(category: Option<&str>, title: &str) -> bool {
    let cat  = category.unwrap_or("").to_lowercase();
    let titl = title.to_lowercase();
    let sports_cat = ["sports", "entertainment", "gaming", "esports", "music", "tv", "film",
        "awards", "celebrity", "weather"];
    if sports_cat.iter().any(|k| cat.contains(k)) { return false; }
    let speculation_title = ["up or down", "above $", "below $", "dip to $", "drop to $"];
    speculation_title.iter().any(|k| titl.contains(k))
}

/// Quant-grade six-signal suspicion score (0–100), production-calibrated.
///
/// ┌──────────────────────────────────────────────────────────────────────────┐
/// │  S1  Statistical edge      0.25  Wilson LB ≥ 55%, gate: n ≥ 10         │
/// │  S2  Early-entry alpha     0.19  Avg win entry < 45¢, confidence-scaled │
/// │  S3  Informed sizing       0.15  Wins in upper-half, gate: 65%, n ≥ 6   │
/// │  S4  Fresh concentrated    0.15  New wallet × volume impact ≥ 3%        │
/// │  S5  Recency acceleration  0.13  Edge improving over time                │
/// │  S6  Sell precision        0.13  Exits at high prices (> 70¢)           │
/// ├──────────────────────────────────────────────────────────────────────────┤
/// │  Multi-signal bonus ×1.25/×1.50/×1.75  (2/3/4+ signals > 0.15)        │
/// │  Niche market multiplier  ×1.50  (daily vol < $50k)                     │
/// │  Category risk multiplier ×1.00–×1.50  (politics/company > sports)     │
/// └──────────────────────────────────────────────────────────────────────────┘
///
/// Signal gate changes from v1 (reduces false positives on small samples):
///   S1: n ≥ 10 (was 5), LB threshold 55% (was 50%)
///   S2: confidence-weighted by min(1, n_wins/5) — penalises n_wins < 5
///   S3: threshold 65% (was 60%), n ≥ 6 (was 4)
///   S4: volume_impact gate 3% fresh / 8% established (was 1% / 5%)
///
/// Returns (composite_score, [s1..s6]) so callers can display per-signal
/// breakdowns without re-computing.
fn compute_suspicion(p: &WalletProfile, volume_impact: f64, is_niche: bool, is_speculation: bool, category_mult: f64) -> (f64, [f64; 6]) {

    // ─── S1: Statistical significance of win rate (quality-filtered) ────
    // Uses n_quality_positions / n_quality_wins which exclude markets where
    // the wallet's avg BUY price was > 90¢ — those are post-resolution redemption
    // buyers, not informed early traders, and would otherwise inflate the win rate.
    // Gate: n_quality ≥ 10, Wilson LB threshold 55%, score 0→1 at LB 55%→77%.
    let s1 = if p.n_quality_positions >= 10 {
        let lb = wilson_lower_bound(p.n_quality_wins, p.n_quality_positions, 1.96);
        ((lb - 0.55).max(0.0) / 0.22).min(1.0)
    } else { 0.0 };

    // ─── S2: Early-entry alpha with confidence weighting + win-rate gate ────
    // alpha_score = avg BUY price on winning positions (0–1).
    // confidence_mult penalises wallets with only 2–4 winning positions.
    //
    // NEW: quality_win_rate gate (≥ 35%) — a wallet that buys at 5¢ and wins
    // only 23% of the time is playing lottery tickets at market odds, NOT
    // demonstrating informed early entry.  Informed traders are cheap AND right
    // at an above-random rate.  Without this gate, any speculative strategy
    // that occasionally wins big at low prices scores maximum S2.
    let quality_win_rate = if p.n_quality_positions > 0 {
        p.n_quality_wins as f64 / p.n_quality_positions as f64
    } else { 0.0 };
    let s2 = if !p.alpha_score.is_nan() && p.n_wins >= 2 && p.alpha_score < 0.45
               && quality_win_rate >= 0.40 {
        let raw = (0.45 - p.alpha_score) / 0.45;
        let confidence_mult = (p.n_wins as f64 / 5.0).min(1.0);
        raw * confidence_mult
    } else { 0.0 };

    // ─── S3: Informed sizing (tightened) ─────────────────────────────────
    // Threshold raised to 65% (was 60%) and gate to n ≥ 6 (was 4) to reduce
    // false positives from wallets with few positions.
    let s3 = if p.informed_sizing_ratio > 0.65 && p.n_positions >= 6 {
        ((p.informed_sizing_ratio - 0.65) / 0.30).min(1.0)
    } else { 0.0 };

    // ─── S4: Fresh concentrated bet (tightened thresholds) ───────────────
    // Fresh wallet gate raised to 3% of market volume (was 1%) to avoid
    // flagging tiny positions in small markets as suspicious.
    // Established wallet gate raised to 8% (was 5%).
    let s4 = if p.is_fresh && volume_impact > 0.03 {
        let conc_mult = (p.concentration / 0.50).min(2.0);
        ((volume_impact / 0.04).min(3.0) / 3.0 * conc_mult).min(1.0)
    } else if volume_impact > 0.08 {
        ((volume_impact - 0.08) / 0.12).min(0.5)
    } else { 0.0 };

    // ─── S5: Recency acceleration (edge improving over time) ──────────────
    let s5 = if p.win_rate_weighted > p.win_rate + 0.10 && p.win_rate > 0.40 {
        ((p.win_rate_weighted - p.win_rate - 0.10) / 0.25).min(1.0)
    } else { 0.0 };

    // ─── S6: Sell-side precision (exits at high prices) ───────────────────
    // Informed traders exit positions near peak prices before bad news.
    // Score: 0 at ≤ 70¢, 1.0 at 95¢. Gate: n_sells ≥ 2, avg sell > 70¢.
    let s6 = if !p.sell_precision.is_nan() && p.n_sells >= 2 && p.sell_precision > 0.70 {
        ((p.sell_precision - 0.70) / 0.25).min(1.0)
    } else { 0.0 };

    // ─── Multi-signal bonus ───────────────────────────────────────────────
    let signals = [s1, s2, s3, s4, s5, s6];
    let n_triggered = signals.iter().filter(|&&s| s > 0.15).count();
    let multi_bonus: f64 = match n_triggered {
        0 | 1 => 1.00,
        2     => 1.25,
        3     => 1.50,
        _     => 1.75,
    };
    // Speculation markets (intraday price bets) are structurally low-volume/niche,
    // so niche_mult must not apply — the small size is expected, not suspicious.
    let niche_mult: f64 = if is_niche && !is_speculation { 1.50 } else { 1.0 };

    // Category multiplier only applies when the wallet has demonstrated real
    // winning ability in quality markets (n_quality_wins ≥ 3).  A speculative
    // wallet with no track record should not get boosted just because the market
    // happens to mention "bitcoin" or "election" in its title.
    let effective_cat_mult = if p.n_quality_wins >= 3 { category_mult } else { 1.0 };

    let base = s1*0.25 + s2*0.19 + s3*0.15 + s4*0.15 + s5*0.13 + s6*0.13;
    let score = (base * multi_bonus * niche_mult * effective_cat_mult * 100.0).min(100.0);

    (score, signals)
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

    let market_id      = args["market_id"].as_str().unwrap_or("");
    let top_n          = args["top_n"].as_u64().unwrap_or(5).min(10) as usize;
    let history_len    = args["history_trades"].as_u64().unwrap_or(100).min(200) as u32;
    let coord_threshold = args["coord_threshold"].as_f64().unwrap_or(0.35).clamp(0.05, 0.95);

    if market_id.is_empty() {
        return Ok(ToolOutput::err(
            "Required: market_id (Polymarket conditionId). \
             Use list_markets or search_markets to find a conditionId.",
        ));
    }

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
    report.push(format!("Market: {}", market_title));

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

    // ── 3. Fetch TRADE + REDEEM histories CONCURRENTLY ────────────────────
    let trade_hists: Vec<_> = join_all(
        ranked.iter().map(|(w, _, _)| clients.polymarket.fetch_user_trades(w, history_len))
    ).await;
    let redeem_hists: Vec<_> = join_all(
        ranked.iter().map(|(w, _, _)| clients.polymarket.fetch_user_redeems(w, history_len))
    ).await;

    // Build profiles — merge TRADE + REDEEM before handing to build_wallet_profile
    let profiles: Vec<WalletProfile> = ranked
        .iter()
        .zip(trade_hists.iter().zip(redeem_hists.iter()))
        .filter_map(|((wallet, market_size, pseudonym), (trades_res, redeems_res))| {
            let mut history = trades_res.as_ref().ok()?.clone();
            if let Ok(redeems) = redeems_res {
                history.extend(redeems.iter().cloned());
            }
            Some(build_wallet_profile(wallet.clone(), pseudonym.clone(), *market_size, &history))
        })
        .collect();

    // Also look up market volume + category for size-anomaly and insider-risk scoring
    let market_meta = clients
        .polymarket
        .fetch_market_by_condition_id(market_id)
        .await
        .ok()
        .flatten();
    let market_volume:   Option<f64>   = market_meta.as_ref().and_then(|m| m.volume);
    let market_category: Option<String> = market_meta.as_ref().and_then(|m| m.category.clone());
    let is_niche   = market_volume.map(|v| v < 50_000.0).unwrap_or(false);
    let cat_mult   = market_insider_risk(market_category.as_deref(), market_title);
    let is_spec    = is_speculation_market(market_category.as_deref(), market_title);
    let risk_tier  = if cat_mult >= 1.45 { format!("×{:.2} POLITICS/CORPORATE (high insider risk)", cat_mult) }
        else if cat_mult >= 1.25 { format!("×{:.2} FINANCE/MACRO (elevated insider risk)", cat_mult) }
        else if cat_mult >= 1.10 { format!("×{:.2} TECH/SCIENCE (modest insider risk)", cat_mult) }
        else { format!("×{:.2} SPORTS/ENTERTAINMENT (base rate)", cat_mult) };
    report.push(format!("Category risk tier: {}\n", risk_tier));

    // ── 4. Summary table ───────────────────────────────────────────────────
    report.push(format!(
        "{:<22} {:>8} {:>7} {:>6} {:>9} {:>10} {:>6} {:>9}",
        "Name", "Pos($)", "Mkts", "Wins", "WinRate", "AlphaEntry", "Vol%", "Suspicion"
    ));
    report.push("─".repeat(87));

    let mut flagged: Vec<(&WalletProfile, f64)> = Vec::new();

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

        let volume_impact = match market_volume {
            Some(vol) if vol > 0.0 => p.market_size / vol,
            _ => 0.0,
        };
        let vol_pct = if volume_impact > 0.0 {
            format!("{:.1}%", volume_impact * 100.0)
        } else {
            "—".to_string()
        };

        let (suspicion, sigs) = compute_suspicion(p, volume_impact, is_niche, is_spec, cat_mult);
        let stat_lb = if p.n_positions >= 5 {
            format!("LB:{:.0}%", wilson_lower_bound(p.n_wins, p.n_positions, 1.96) * 100.0)
        } else { "LB:n/a".to_string() };
        let fresh_flag = if p.is_fresh { "N " } else { "  " };

        report.push(format!(
            "{}{:<22} {:>8.0} {:>7} {:>6} {:>8.1}% {:>10} {:>6} {:>8.0}/100  [{}  sz:{:.0}  α:{:.0}  fresh:{:.0}  acc:{:.0}]",
            fresh_flag,
            name,
            p.market_size,
            p.n_positions,
            p.n_wins,
            p.win_rate * 100.0,
            alpha_str,
            vol_pct,
            suspicion,
            stat_lb,
            sigs[2] * 100.0,
            sigs[1] * 100.0,
            sigs[3] * 100.0,
            sigs[4] * 100.0,
        ));

        if suspicion >= 50.0 {
            flagged.push((p, suspicion));
        }
    }

    // ── 5. Coordination detection (pairwise Jaccard) ───────────────────────
    let mut coord_pairs: Vec<(String, String, f64)> = Vec::new();
    for i in 0..profiles.len() {
        for j in (i + 1)..profiles.len() {
            let sim = jaccard(&profiles[i].market_set, &profiles[j].market_set);
            if sim >= coord_threshold {
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
        report.push("\nNo accounts reached the flagging threshold (suspicion ≥ 50).".to_string());
    } else {
        report.push("\n⚠  FLAGGED ACCOUNTS".to_string());
        report.push("─".repeat(87));
        for (p, suspicion) in &flagged {
            report.push(format!("\n  {} ({}…)", p.pseudonym, &p.wallet[..p.wallet.len().min(10)]));
            report.push(format!("    Suspicion score:  {:.0}/100", suspicion));
            if p.is_fresh {
                let age_str = p.wallet_age_days
                    .map(|d| format!("{:.1} days old", d))
                    .unwrap_or_else(|| "unknown age".to_string());
                report.push(format!("    ⚠ Fresh wallet: only {} lifetime trades, {} — high insider signal",
                    p.market_set.len(), age_str));
            }
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

    // ── Fetch TRADE + REDEEM concurrently (REDEEM = winning payouts) ───────
    let (trades_res, redeems_res) = tokio::join!(
        clients.polymarket.fetch_user_trades(wallet, history_len),
        clients.polymarket.fetch_user_redeems(wallet, history_len),
    );
    let mut history = trades_res.context("Failed to fetch wallet trade history")?;
    if let Ok(redeems) = redeems_res {
        history.extend(redeems);
    }
    // Sort newest-first for display
    history.sort_unstable_by(|a, b| b.timestamp.cmp(&a.timestamp));

    if history.is_empty() {
        return Ok(ToolOutput::ok(format!(
            "=== WALLET PROFILE: {} ===\n\nNo trade history found.",
            wallet
        )));
    }

    let pseudonym = history.iter().find(|t| !t.pseudonym.is_empty())
        .map(|t| t.pseudonym.clone())
        .unwrap_or_else(|| wallet[..wallet.len().min(10)].to_string());

    let profile = build_wallet_profile(wallet.to_string(), pseudonym, 0.0, &history);

    let mut report = Vec::new();
    report.push(format!("=== WALLET PROFILE: {} ===", profile.pseudonym));
    report.push(format!("Address: {}", profile.wallet));

    let age_str = profile.wallet_age_days
        .map(|d| if d >= 365.0 { format!("{:.1}y", d / 365.0) } else { format!("{:.0}d", d) })
        .unwrap_or_else(|| "unknown".to_string());
    let fresh_note = if profile.is_fresh { "  ⚠ FRESH WALLET" } else { "" };
    report.push(format!("Wallet age:  {}{}",  age_str, fresh_note));
    report.push(format!("Confidence:  {} ({} positions in history)",
        confidence_label(profile.n_positions), profile.n_positions));

    // ── Performance summary ────────────────────────────────────────────────
    report.push("\n--- Performance Summary ---".to_string());
    report.push(format!("Markets traded (n):    {}", profile.n_positions));
    report.push(format!("Winning payouts:       {}", profile.n_wins));

    // Win rate with sample-size caveat
    let win_rate_display = if profile.n_positions < 5 {
        format!("{:.1}%  (n={}, LOW SAMPLE — treat with caution)",
            profile.win_rate * 100.0, profile.n_positions)
    } else if profile.n_positions < 15 {
        format!("{:.1}%  (n={}, moderate sample)",
            profile.win_rate * 100.0, profile.n_positions)
    } else {
        format!("{:.1}%  (n={}, robust sample)",
            profile.win_rate * 100.0, profile.n_positions)
    };
    report.push(format!("Raw win rate:          {}", win_rate_display));
    if profile.win_rate_weighted > 0.0 {
        let delta = profile.win_rate_weighted - profile.win_rate;
        let trend = if delta > 0.05 { "  → improving recently" }
                    else if delta < -0.05 { "  → declining recently" }
                    else { "" };
        report.push(format!("Recency-weighted (90d half-life):  {:.1}%{}",
            profile.win_rate_weighted * 100.0, trend));
    }
    report.push(format!("Total buy-side volume: ${:.0}", profile.total_vol));

    if !profile.alpha_score.is_nan() {
        let advantage = 50.0 - profile.alpha_score * 100.0;
        report.push(format!(
            "Alpha entry:           {:.1}¢  ({:+.1}¢ ahead of 50¢ fair-coin baseline)",
            profile.alpha_score * 100.0, advantage,
        ));
        let label = if advantage > 20.0 {
            "Very strong — entries consistently well before price moves"
        } else if advantage > 10.0 {
            "Moderate — buys at a discount on winning positions"
        } else if advantage > 0.0 {
            "Weak — slight early-entry advantage"
        } else {
            "None — buys late on winning positions (reactive trader)"
        };
        report.push(format!("Alpha quality:         {}", label));
    }

    // ── Timing-to-resolution for winning positions ─────────────────────────
    {
        use std::collections::HashMap;

        // Collect earliest BUY timestamp per market
        let mut first_buy: HashMap<&str, (i64, f64)> = HashMap::new(); // cid → (ts, price)
        for t in history.iter().filter(|t| t.side == "BUY" && t.timestamp > 0) {
            let e = first_buy.entry(t.condition_id.as_str()).or_insert((i64::MAX, t.price));
            if t.timestamp < e.0 { *e = (t.timestamp, t.price); }
        }
        // REDEEM timestamp per market
        let mut redeem_ts: HashMap<&str, i64> = HashMap::new();
        for t in history.iter().filter(|t| t.trade_type == "REDEEM" && t.timestamp > 0) {
            redeem_ts.insert(t.condition_id.as_str(), t.timestamp);
        }
        // Markets that have both a first BUY and a REDEEM
        let mut wins_timing: Vec<(String, f64, f64)> = first_buy.iter()
            .filter_map(|(cid, &(buy_ts, buy_price))| {
                let rdm_ts = redeem_ts.get(cid)?;
                let days_held = (rdm_ts - buy_ts).max(0) as f64 / 86_400.0;
                let title = history.iter().find(|t| t.condition_id == *cid)
                    .map(|t| t.market_title.clone())
                    .unwrap_or_else(|| cid.to_string());
                Some((title, buy_price * 100.0, days_held))
            })
            .collect();
        wins_timing.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

        if !wins_timing.is_empty() {
            let avg_days = wins_timing.iter().map(|e| e.2).sum::<f64>() / wins_timing.len() as f64;
            let avg_entry = wins_timing.iter().map(|e| e.1).sum::<f64>() / wins_timing.len() as f64;
            report.push(format!("\n--- Winning Position Timing ({} positions) ---", wins_timing.len()));
            report.push(format!("Avg hold before redemption: {:.0} days  |  Avg first-buy price: {:.1}¢",
                avg_days, avg_entry));
            report.push(format!("{:<6} {:>8}  {}", "Days", "Entry¢", "Market"));
            report.push("─".repeat(70));
            for (title, entry_p, days) in wins_timing.iter().take(10) {
                let n = title.chars().count().min(52);
                let t_trunc: String = title.chars().take(n).collect();
                report.push(format!("{:>5.0}d {:>8.1}  {}", days, entry_p, t_trunc));
            }
            if wins_timing.len() > 10 {
                report.push(format!("  … and {} more", wins_timing.len() - 10));
            }
        }
    }

    // ── Recent activity (newest first, trades + redeems) ──────────────────
    report.push("\n--- Recent Activity (newest first, TRADE + REDEEM) ---".to_string());
    report.push(format!("{:<6}  {:<8} {:<8} {:>7} {:>6}  {}", "Date", "Type", "Side", "Size", "Price¢", "Market"));
    report.push("─".repeat(80));

    for t in history.iter().take(25) {
        let date_str = chrono::DateTime::<chrono::Utc>::from_timestamp(t.timestamp, 0)
            .map(|d| d.format("%m-%d").to_string())
            .unwrap_or_else(|| "?".to_string());
        let n = t.market_title.chars().count().min(35);
        let title_trunc: String = t.market_title.chars().take(n).collect();
        report.push(format!(
            "{:<6}  {:<8} {:<8} {:>7.1} {:>6.1}  {}",
            date_str, t.trade_type, t.side, t.size, t.price * 100.0, title_trunc,
        ));
    }
    if history.len() > 25 {
        report.push(format!("  … and {} more events", history.len() - 25));
    }

    // ── Top markets by buy-side dollar exposure ────────────────────────────
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
            let n = title.chars().count().min(55);
            let t_trunc: String = title.chars().take(n).collect();
            report.push(format!("  ${:>8.0}  {}", vol, t_trunc));
        }
    }

    // ── Suspicion assessment (quant five-signal model) ────────────────────
    report.push("\n--- Suspicion Assessment ---".to_string());
    let (suspicion, sigs) = compute_suspicion(&profile, 0.0, false, false, 1.0);
    let stat_lb = if profile.n_quality_positions >= 5 {
        format!("{:.1}%  (n={} quality positions)",
            wilson_lower_bound(profile.n_quality_wins, profile.n_quality_positions, 1.96) * 100.0,
            profile.n_quality_positions)
    } else {
        format!("n/a ({} quality positions < 5; {}/{} total excluded as late-entry)",
            profile.n_quality_positions,
            profile.n_positions - profile.n_quality_positions,
            profile.n_positions)
    };
    let roi_str = if profile.profit_roi.is_nan() { "n/a".to_string() }
                  else { format!("{:.0}%", profile.profit_roi * 100.0) };
    report.push(format!("Composite score:     {:.0}/100", suspicion));
    report.push(format!("  S1 Statistical edge   {:.0}/100  (Wilson LB 95% CI: {})", sigs[0] * 100.0, stat_lb));
    report.push(format!("  S2 Early-entry alpha  {:.0}/100  (avg entry on wins: {})",
        sigs[1] * 100.0,
        if profile.alpha_score.is_nan() { "n/a".to_string() } else { format!("{:.1}¢", profile.alpha_score * 100.0) }));
    report.push(format!("  S3 Informed sizing    {:.0}/100  (wins in top-half by size: {:.0}%)",
        sigs[2] * 100.0, profile.informed_sizing_ratio * 100.0));
    report.push(format!("  S4 Fresh concentrated {:.0}/100  (fresh={}, concentration={:.0}%)",
        sigs[3] * 100.0, profile.is_fresh, profile.concentration * 100.0));
    report.push(format!("  S5 Recency accel.     {:.0}/100  (raw {:.1}%  →  recency-wtd {:.1}%)",
        sigs[4] * 100.0, profile.win_rate * 100.0, profile.win_rate_weighted * 100.0));
    report.push(format!("  S6 Sell precision     {:.0}/100  (avg sell price on exits > 50¢: {})",
        sigs[5] * 100.0,
        if profile.sell_precision.is_nan() { "n/a".to_string() }
        else { format!("{:.1}¢  ({} sell events)", profile.sell_precision * 100.0, profile.n_sells) }));
    report.push(format!("  Profit ROI on wins:   {}  (no market-vol context available)", roi_str));
    report.push("  Note: category_mult=1.0 here (standalone); find_smart_money applies market risk tier.".to_string());
    report.push(String::new());
    report.push("--- Supplementary Metrics ---".to_string());
    report.push(format!("Max win streak:        {} consecutive wins ({})",
        profile.max_win_streak,
        if profile.max_win_streak >= 5 { "⚠ statistically unlikely at random" }
        else if profile.max_win_streak >= 3 { "moderate" }
        else { "typical" }));
    let vel_str = if profile.position_velocity < 0.001 { "< 0.01".to_string() }
                  else { format!("{:.2}", profile.position_velocity) };
    report.push(format!("Position velocity:     {}/day  ({})",
        vel_str,
        if profile.position_velocity > 1.0 { "high — multiple new markets per day" }
        else if profile.position_velocity > 0.3 { "moderate" }
        else { "low — infrequent trading" }));
    if !profile.outcome_yes_fraction.is_nan() {
        let bias_label = if profile.outcome_yes_fraction > 0.80 { "⚠ strong YES bias" }
                        else if profile.outcome_yes_fraction < 0.20 { "⚠ strong NO bias" }
                        else { "balanced" };
        report.push(format!("YES-outcome bias:      {:.0}% of BUY trades on YES  ({})",
            profile.outcome_yes_fraction * 100.0, bias_label));
    }
    report.push(format!("Sample confidence:     {}", confidence_label(profile.n_positions)));
    // Boost verdict if supplementary metrics raise independent red flags.
    let supp_flags = {
        let mut f = Vec::new();
        if profile.max_win_streak >= 7 { f.push(format!("{}-win streak", profile.max_win_streak)); }
        if !profile.outcome_yes_fraction.is_nan()
            && (profile.outcome_yes_fraction > 0.85 || profile.outcome_yes_fraction < 0.15) {
            f.push(format!("{:.0}% directional bias", profile.outcome_yes_fraction * 100.0));
        }
        f
    };
    let adjusted_suspicion = if supp_flags.is_empty() { suspicion }
                              else { (suspicion + supp_flags.len() as f64 * 10.0).min(100.0) };

    let verdict = if adjusted_suspicion > 70.0 {
        "HIGH — multiple strong insider indicators present"
    } else if adjusted_suspicion > 45.0 {
        "MODERATE — some indicators; monitor closely"
    } else {
        "LOW — no strong signals at this stage"
    };
    report.push(format!("Verdict: {}", verdict));
    if !supp_flags.is_empty() {
        report.push(format!("  → Supplementary flags: {}", supp_flags.join(", ")));
        report.push(format!("  → Adjusted suspicion (incl. supplementary): {:.0}/100", adjusted_suspicion));
    }
    report.push("\nNote: scores are probabilistic proxies, not evidence of wrongdoing.".to_string());
    report.push("Use find_smart_money on a specific market to include volume-impact (S4) context.".to_string());

    Ok(ToolOutput::ok(report.join("\n")))
}

// ─── Market-wide smart money scanner ─────────────────────────────────────────

/// Light-weight per-market smart-money check used by scan_smart_money.
/// Fetches top `top_n` wallets with 50-trade histories for speed.
/// Returns `(market_title, condition_id, max_suspicion, top_wallet_name, top_wallet_addr)`.
async fn quick_market_scan(
    clients:         &MarketClients,
    market_id:       &str,
    market_title:    &str,
    market_category: Option<&str>,
    market_volume:   Option<f64>,
    top_n:           usize,
) -> (String, String, f64, String, String) {
    use std::collections::HashMap;
    use futures_util::future::join_all;

    let fallback = (market_title.to_string(), market_id.to_string(), 0.0, String::new(), String::new());

    let Ok(trades) = clients.polymarket.fetch_market_trades(market_id, 100).await else {
        return fallback;
    };
    if trades.is_empty() { return fallback; }

    // Top wallets by buy-side size
    let mut agg: HashMap<String, (f64, String)> = HashMap::new();
    for t in &trades {
        if t.side == "BUY" {
            let e = agg.entry(t.wallet.clone()).or_insert((0.0, t.pseudonym.clone()));
            e.0 += t.size;
        }
    }
    let mut ranked: Vec<(String, f64, String)> = agg
        .into_iter().map(|(w, (s, p))| (w, s, p)).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(top_n);

    if ranked.is_empty() { return fallback; }

    // Fetch TRADE + REDEEM histories concurrently (200 each for accuracy)
    let trade_futs  = join_all(ranked.iter().map(|(w, _, _)| clients.polymarket.fetch_user_trades(w, clients.history_limit)));
    let redeem_futs = join_all(ranked.iter().map(|(w, _, _)| clients.polymarket.fetch_user_redeems(w, clients.history_limit)));
    let (trade_hists, redeem_hists) = tokio::join!(trade_futs, redeem_futs);

    let is_niche = market_volume.map(|v| v < 50_000.0).unwrap_or(false);
    let cat_mult  = market_insider_risk(market_category, market_title);
    let is_spec   = is_speculation_market(market_category, market_title);
    let mut best = (0.0f64, String::new(), String::new());

    for (i, (wallet, market_size, pseudonym)) in ranked.iter().enumerate() {
        let mut history = trade_hists[i].as_ref().ok().cloned().unwrap_or_default();
        if let Ok(r) = &redeem_hists[i] { history.extend(r.iter().cloned()); }
        let profile = build_wallet_profile(wallet.clone(), pseudonym.clone(), *market_size, &history);
        let vol_impact = match market_volume {
            Some(v) if v > 0.0 => market_size / v,
            _ => 0.0,
        };
        let (suspicion, _) = compute_suspicion(&profile, vol_impact, is_niche, is_spec, cat_mult);
        if suspicion > best.0 {
            best = (suspicion, profile.pseudonym.clone(), wallet.clone());
        }
    }

    (market_title.to_string(), market_id.to_string(), best.0, best.1, best.2)
}

/// Scan a batch of Polymarket markets and return those with elevated smart-money
/// suspicion scores, sorted highest-first.  Faster than calling find_smart_money
/// repeatedly because it uses shallow histories (50 trades/wallet) and returns
/// only summary rows.  Follow up with find_smart_money or analyze_wallet on the
/// flagged markets/wallets for full detail.
async fn scan_smart_money(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    use futures_util::future::join_all;

    let limit         = args["limit"].as_u64().unwrap_or(20).min(30) as u32;
    let top_n         = args["top_n"].as_u64().unwrap_or(3).min(5) as usize;
    let min_suspicion = args["min_suspicion"].as_f64().unwrap_or(40.0);
    let category      = args["category"].as_str();

    let markets = clients.polymarket.fetch_markets(limit, None, category).await
        .context("Failed to fetch markets for scan")?;

    if markets.is_empty() {
        return Ok(ToolOutput::ok("No active Polymarket markets found.".to_string()));
    }

    // Run all market scans concurrently (shallow, fast)
    let scans = join_all(markets.iter().map(|m| {
        let cid   = m.id.clone();
        let title = m.title.clone();
        let cat   = m.category.clone();
        let vol   = m.volume;
        async move { quick_market_scan(clients, &cid, &title, cat.as_deref(), vol, top_n).await }
    })).await;

    // Filter and sort
    let mut flagged: Vec<_> = scans.into_iter()
        .filter(|(_, _, susp, _, _)| *susp >= min_suspicion)
        .collect();
    flagged.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    if flagged.is_empty() {
        return Ok(ToolOutput::ok(format!(
            "No markets met the minimum suspicion threshold of {:.0} out of {} scanned.",
            min_suspicion, markets.len()
        )));
    }

    let mut report = Vec::new();
    report.push(format!(
        "=== SMART MONEY SCAN: {} markets scanned, {} flagged (≥{:.0} suspicion) ===\n",
        markets.len(), flagged.len(), min_suspicion
    ));
    report.push(format!("{:<8}  {:<20}  {}  {}",
        "Score", "Top Wallet", "ConditionId", "Market"));
    report.push("─".repeat(100));

    for (title, cid, susp, wallet_name, wallet_addr) in &flagged {
        let wname = if wallet_name.is_empty() { "—".to_string() } else {
            let n = wallet_name.chars().count().min(20);
            wallet_name.chars().take(n).collect()
        };
        let mkt_short: String = title.chars().take(52).collect();
        report.push(format!("{:>6.0}/100  {:<20}  {}…  {}",
            susp, wname, &cid[..cid.len().min(18)], mkt_short));
        if !wallet_addr.is_empty() {
            report.push(format!("             wallet: {}  → call analyze_wallet for detail", wallet_addr));
        }
    }

    report.push(format!(
        "\nNext steps:\n\
         • find_smart_money market_id=<conditionId>  — full analysis for a specific market\n\
         • analyze_wallet wallet=<address>            — deep-dive on a specific wallet\n\
         • get_wallet_positions wallet=<address>      — current open positions for a wallet"
    ));

    Ok(ToolOutput::ok(report.join("\n")))
}

// ─── Current open positions for a wallet ─────────────────────────────────────

/// Derive open (unresolved) positions from a wallet's TRADE + REDEEM history.
/// A position is "open" when the wallet has net positive shares in a market
/// and has not yet received a REDEEM event for it.
async fn get_wallet_positions(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    let wallet      = args["wallet"].as_str().unwrap_or("").trim();
    let history_len = args["limit"].as_u64().unwrap_or(300).min(500) as u32;

    if wallet.is_empty() {
        return Ok(ToolOutput::err(
            "Required: wallet (Polymarket proxy wallet address). \
             Obtain from find_smart_money or scan_smart_money."
        ));
    }

    let (trades_res, redeems_res) = tokio::join!(
        clients.polymarket.fetch_user_trades(wallet, history_len),
        clients.polymarket.fetch_user_redeems(wallet, history_len),
    );
    let mut history = trades_res.context("Failed to fetch wallet trades")?;
    if let Ok(r) = redeems_res { history.extend(r); }

    if history.is_empty() {
        return Ok(ToolOutput::ok(format!(
            "=== OPEN POSITIONS: {} ===\n\nNo trade history found.", wallet
        )));
    }

    let pseudonym = history.iter().find(|t| !t.pseudonym.is_empty())
        .map(|t| t.pseudonym.clone())
        .unwrap_or_else(|| wallet[..wallet.len().min(10)].to_string());

    use std::collections::HashMap;

    #[derive(Default)]
    struct Pos {
        title:    String,
        net_yes:  f64,   // YES shares held (bought – sold)
        net_no:   f64,   // NO shares held
        yes_cost: f64,   // net dollars spent on YES
        no_cost:  f64,
        last_ts:  i64,
        resolved: bool,
    }

    let mut positions: HashMap<String, Pos> = HashMap::new();

    for t in &history {
        let e = positions.entry(t.condition_id.clone()).or_default();
        if t.market_title != e.title && !t.market_title.is_empty() {
            e.title = t.market_title.clone();
        }
        e.last_ts = e.last_ts.max(t.timestamp);

        if t.trade_type == "REDEEM" {
            e.resolved = true;
            continue;
        }

        let is_yes = t.outcome_index == 0;
        let dollar_val = t.size * t.price;
        match t.side.as_str() {
            "BUY"  => { if is_yes { e.net_yes += t.size; e.yes_cost += dollar_val; }
                         else      { e.net_no  += t.size; e.no_cost  += dollar_val; } }
            "SELL" => { if is_yes { e.net_yes -= t.size; e.yes_cost -= dollar_val; }
                         else      { e.net_no  -= t.size; e.no_cost  -= dollar_val; } }
            _ => {}
        }
    }

    // Open = unresolved with net positive shares
    let mut open: Vec<(String, Pos)> = positions.into_iter()
        .filter(|(_, p)| !p.resolved && (p.net_yes > 0.5 || p.net_no > 0.5))
        .collect();
    // Sort by cost (largest first)
    open.sort_by(|a, b| {
        let ca = if a.1.net_yes >= a.1.net_no { a.1.yes_cost } else { a.1.no_cost };
        let cb = if b.1.net_yes >= b.1.net_no { b.1.yes_cost } else { b.1.no_cost };
        cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut report = Vec::new();
    report.push(format!("=== OPEN POSITIONS: {} ===", pseudonym));
    report.push(format!("Address: {}", wallet));
    report.push(format!("{} open position(s) out of {} markets in history\n", open.len(), positions_total(&history)));

    if open.is_empty() {
        report.push("No open positions detected — all positions appear resolved or flat.".to_string());
        report.push("(History may be truncated; increase limit for a deeper check)".to_string());
    } else {
        report.push(format!("{:<5}  {:>9}  {:>9}  {:>9}  {:>11}  {}",
            "Side", "Shares", "Cost $", "AvgPx¢", "LastActivity", "Market"));
        report.push("─".repeat(92));

        let now = chrono::Utc::now().timestamp();
        let mut total_cost = 0.0f64;

        for (_cid, p) in &open {
            let (side, shares, cost) = if p.net_yes >= p.net_no {
                ("YES", p.net_yes, p.yes_cost)
            } else {
                ("NO", p.net_no, p.no_cost)
            };
            let avg_px = if shares > 0.0 { cost / shares * 100.0 } else { 0.0 };
            let age_days = (now - p.last_ts).max(0) as f64 / 86_400.0;
            let last_str = if age_days < 1.0 {
                format!("{:.0}h ago", age_days * 24.0)
            } else {
                format!("{:.0}d ago", age_days)
            };
            let mkt: String = p.title.chars().take(42).collect();
            report.push(format!("{:<5}  {:>9.1}  {:>9.2}  {:>9.1}  {:>11}  {}",
                side, shares, cost, avg_px, last_str, mkt));
            total_cost += cost;
        }
        report.push(format!("\nTotal open exposure:  ${:.2}", total_cost));
        report.push(format!("kelly_size tip: use each position's avg_px as market_price to size against your view."));
    }

    Ok(ToolOutput::ok(report.join("\n")))
}

fn positions_total(history: &[crate::markets::polymarket::PolyTrade]) -> usize {
    use std::collections::HashSet;
    history.iter().map(|t| t.condition_id.as_str()).collect::<HashSet<_>>().len()
}

// ─── News search ─────────────────────────────────────────────────────────────

/// Search newsdata.io for articles matching `query`.
async fn search_news(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    let query  = args["query"].as_str().unwrap_or("").trim();
    let limit  = args["limit"].as_u64().unwrap_or(8).min(10) as u8;

    let Some(news) = &clients.news else {
        return Ok(ToolOutput::err(
            "News API not configured. Set the NEWSDATA_API_KEY environment variable."
        ));
    };

    if query.is_empty() {
        return Ok(ToolOutput::err("Required: query (search terms)"));
    }

    let articles = news.fetch_latest(query, limit).await
        .context("newsdata.io search failed")?;

    if articles.is_empty() {
        return Ok(ToolOutput::ok(format!("No recent news found for query: '{}'", query)));
    }

    Ok(ToolOutput::ok(format_news_articles(query, &articles)))
}

/// Fetch news contextually relevant to a specific prediction market.
///
/// Looks up the market title, extracts key terms automatically (same
/// stop-word removal as the UI's [0] key), then calls newsdata.io.
async fn get_market_news(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
    let market_id = args["market_id"].as_str().unwrap_or("").trim();
    let platform  = args["platform"].as_str().unwrap_or("polymarket");
    let limit     = args["limit"].as_u64().unwrap_or(8).min(10) as u8;

    if market_id.is_empty() {
        return Ok(ToolOutput::err("Required: market_id"));
    }

    let Some(news) = &clients.news else {
        return Ok(ToolOutput::err(
            "News API not configured. Set the NEWSDATA_API_KEY environment variable."
        ));
    };

    // Resolve the market title.
    let markets = match platform {
        "polymarket" => clients.polymarket.fetch_markets(200, None, None).await
            .context("Failed to fetch Polymarket markets")?,
        "kalshi" => clients.kalshi.fetch_markets(200, None).await
            .context("Failed to fetch Kalshi markets")?,
        _ => return Ok(ToolOutput::err(format!("Unknown platform: {}", platform))),
    };

    let market = markets.iter()
        .find(|m| m.id.eq_ignore_ascii_case(market_id))
        .or_else(|| markets.iter().find(|m| m.title.to_lowercase().contains(&market_id.to_lowercase())));

    let Some(m) = market else {
        return Ok(ToolOutput::err(format!("Market '{}' not found on {}", market_id, platform)));
    };

    let articles = news.fetch_for_market(&m.title, limit).await
        .context("newsdata.io fetch failed")?;

    if articles.is_empty() {
        return Ok(ToolOutput::ok(format!(
            "No recent news found for market: '{}'\nQuery terms extracted: (none — title may be too generic; try search_news with custom terms)",
            m.title
        )));
    }

    // Prefix with the market title and extracted query so the LLM has full context.
    let header = format!("=== NEWS for market: '{}' ===", m.title);
    Ok(ToolOutput::ok(format!("{}\n{}", header, format_news_articles(&m.title, &articles))))
}

fn format_news_articles(label: &str, articles: &[crate::news::NewsArticle]) -> String {
    let mut out = vec![format!("=== NEWS: '{}' ({} results) ===\n", label, articles.len())];
    for a in articles {
        let sentiment = match a.sentiment.as_deref() {
            Some("positive") => " [+positive]",
            Some("negative") => " [-negative]",
            Some("neutral")  => " [~neutral]",
            _                => "",
        };
        out.push(format!("• {} [{}{}]  —  {}",
            a.title, a.source_name, sentiment, a.age_label()));
        if !a.description.is_empty() {
            let desc: String = a.description.chars().take(200).collect();
            out.push(format!("  {}", desc));
        }
        if let Some(kw) = &a.keywords {
            if !kw.is_empty() {
                out.push(format!("  Keywords: {}", kw.join(", ")));
            }
        }
        out.push(format!("  {}\n", a.link));
    }
    out.join("\n")
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
                        "description": "For Polymarket: the CLOB token_id shown by get_market (long decimal, NOT the 0x conditionId). For Kalshi: market ticker."
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
            description: "Deep smart-money analysis for ONE Polymarket market. Fetches recent \
                trades, profiles the top traders by buy-side size (TRADE + REDEEM histories \
                fetched concurrently), computes recency-weighted win rate, alpha-entry score, \
                volume-anomaly, and a composite suspicion score (0–100). Also detects coordinated \
                wallet pairs via Jaccard market-overlap. Use after scan_smart_money has flagged a \
                market, or when you already have a conditionId of interest. \
                Recommended workflow: scan_smart_money → find_smart_money → analyze_wallet.".into(),
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
                    },
                    "coord_threshold": {
                        "type": "number",
                        "description": "Jaccard similarity threshold (0–1) for flagging wallet \
                            pairs as coordinated — wallets sharing at least this fraction of \
                            their traded markets are highlighted. Default 0.35. Lower to surface \
                            more pairs; raise to reduce noise."
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
            description: "Deep profile of a specific Polymarket wallet. Fetches TRADE + REDEEM \
                histories concurrently to correctly compute win rate (REDEEMs = winning payouts), \
                recency-weighted win rate (90-day half-life), alpha-entry score, timing-to-resolution \
                on winning positions (how many days before redemption they first bought), and a \
                unified suspicion score using the same formula as find_smart_money. \
                Also shows recent activity with dates and top markets by buy-side exposure. \
                Use after find_smart_money or scan_smart_money to investigate a flagged wallet. \
                Wallet address comes from those tools' output.".into(),
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
            name: "scan_smart_money".into(),
            description: "Scan multiple Polymarket markets simultaneously for elevated smart-money \
                activity. Uses shallow per-wallet histories (50 trades) for speed, so it can \
                process 20–30 markets in one call and rank them by maximum suspicion score. \
                Use this as your morning book-scan: run it first to find markets worth deep-diving, \
                then call find_smart_money on flagged conditionIds. \
                Recommended first step when you don't have a specific market in mind.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Number of active markets to scan (1–30). Default 20."
                    },
                    "top_n": {
                        "type": "integer",
                        "description": "Wallets to profile per market (1–5). Default 3."
                    },
                    "min_suspicion": {
                        "type": "number",
                        "description": "Minimum suspicion score (0–100) to include in results. Default 40."
                    },
                    "category": {
                        "type": "string",
                        "description": "Optionally restrict to a topic category (e.g. 'politics', 'crypto')."
                    }
                },
                "required": []
            }),
        },
        ToolDefinition {
            name: "get_wallet_positions".into(),
            description: "Derive the current OPEN positions for a specific Polymarket wallet by \
                replaying its TRADE + REDEEM history. A position is open when net shares \
                (bought minus sold) are positive and no REDEEM (winning payout) has occurred. \
                Returns: side (YES/NO), share count, cost basis, average entry price, and \
                days since last activity for each open market. \
                Use this to answer 'what is this wallet currently betting on?' after \
                find_smart_money or scan_smart_money surfaces a suspicious wallet.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "wallet": {
                        "type": "string",
                        "description": "Polymarket proxy wallet address (hex, e.g. '0xabc…'). \
                            Obtain from find_smart_money or scan_smart_money."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max trade events to pull from history. Default 300, max 500."
                    }
                },
                "required": ["wallet"]
            }),
        },
        ToolDefinition {
            name: "get_market_news".into(),
            description: "Fetch recent news articles relevant to a specific prediction market. \
                Automatically extracts the most informative terms from the market title and \
                queries newsdata.io. ALWAYS call this immediately after get_market before \
                forming any probability estimate — news context is essential for calibrated \
                predictions. Returns titles, sources, publication age, sentiment, and keywords. \
                Requires NEWSDATA_API_KEY to be configured.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "market_id": {
                        "type": "string",
                        "description": "The market's condition ID (or ticker for Kalshi)."
                    },
                    "platform": {
                        "type": "string",
                        "enum": ["polymarket", "kalshi"],
                        "description": "Which platform the market belongs to. Default: polymarket."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Number of articles to return (1–10). Default 8."
                    }
                },
                "required": ["market_id"]
            }),
        },
        ToolDefinition {
            name: "search_news".into(),
            description: "Search for recent news articles by custom query terms. Use when you want \
                to investigate a specific angle not captured by get_market_news — e.g. a related \
                entity, a second search with refined terms, or cross-checking a specific claim. \
                Returns titles, sources, publication age, sentiment labels, keywords, and descriptions. \
                Requires NEWSDATA_API_KEY to be configured.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search terms (3-5 key words work best, e.g. 'Trump tariffs China' or 'Fed rate decision')."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Number of articles to return (1–10). Default 8."
                    }
                },
                "required": ["query"]
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

/// Tool definitions used exclusively in the Too-Smart LLM scan agent loop.
/// Includes the regular analytical tools PLUS `flag_too_smart_wallet` so the
/// LLM can register its confirmed suspects in a structured way.
pub fn too_smart_llm_definitions() -> Vec<ToolDefinition> {
    let mut defs = all_definitions();
    defs.push(ToolDefinition {
        name: "flag_too_smart_wallet".into(),
        description: "Register a wallet you have identified as a 'too smart' informed trader. \
            Call this once per suspect you are confident in. Be selective — only flag wallets \
            with clear multi-signal evidence. The result will be shown to the user in the \
            Too-Smart LLM tab with your reasoning.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "wallet": {
                    "type": "string",
                    "description": "Full Polymarket proxy wallet address (hex, e.g. '0xabc…')."
                },
                "pseudonym": {
                    "type": "string",
                    "description": "Display name / pseudonym from the scan data."
                },
                "rank": {
                    "type": "integer",
                    "description": "Your confidence ranking for this wallet (1 = most suspicious overall)."
                },
                "reasoning": {
                    "type": "string",
                    "description": "Your analytical reasoning in 2–4 sentences explaining why this \
                        wallet is suspicious — cite specific statistics and patterns."
                },
                "key_signals": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "2–4 specific signal strings (e.g. 'Wilson LB 72% at n=12', \
                        'appeared in 4/5 markets', 'avg entry 31¢ on wins')."
                }
            },
            "required": ["wallet", "pseudonym", "rank", "reasoning", "key_signals"]
        }),
    });
    defs
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

    // ── market_insider_risk ───────────────────────────────────────────────────

    #[test]
    fn risk_politics_keyword_in_title() {
        let r = market_insider_risk(None, "Will Donald Trump win the 2024 election?");
        assert!((r - 1.50).abs() < 1e-9, "election title should be 1.50, got {}", r);
    }

    #[test]
    fn risk_politics_category_field() {
        let r = market_insider_risk(Some("politics"), "Any market title");
        assert!((r - 1.50).abs() < 1e-9, "politics category should be 1.50, got {}", r);
    }

    #[test]
    fn risk_company_bitcoin() {
        let r = market_insider_risk(None, "Will Bitcoin exceed $100k by end of 2025?");
        assert!((r - 1.50).abs() < 1e-9, "bitcoin title should be 1.50, got {}", r);
    }

    #[test]
    fn risk_fed_rate_decision() {
        let r = market_insider_risk(None, "Will the Fed cut interest rate in June?");
        assert!((r - 1.50).abs() < 1e-9, "fed rate title should be 1.50, got {}", r);
    }

    #[test]
    fn risk_sports_golf_base_rate() {
        let r = market_insider_risk(None, "Will Ryan Fox win the 2026 RBC Heritage?");
        assert!((r - 1.00).abs() < 1e-9, "golf tournament title should be 1.00, got {}", r);
    }

    #[test]
    fn risk_esports_kills_base_rate() {
        let r = market_insider_risk(None, "Total Kills Over/Under 28.5 in Game 2?");
        assert!((r - 1.00).abs() < 1e-9, "esports o/u title should be 1.00, got {}", r);
    }

    #[test]
    fn risk_soccer_over_under_base_rate() {
        let r = market_insider_risk(None, "AS Roma vs. Atalanta BC: O/U 3.5");
        assert!((r - 1.00).abs() < 1e-9, "soccer o/u should be 1.00, got {}", r);
    }

    #[test]
    fn risk_sports_category_overrides_any_title() {
        // Even if title could match something, explicit "sports" category → base rate
        let r = market_insider_risk(Some("sports"), "Bitcoin price above $100k?");
        assert!((r - 1.00).abs() < 1e-9,
            "explicit sports category should stay 1.00 even with bitcoin title, got {}", r);
    }

    #[test]
    fn risk_multiplier_bounded_above_one() {
        let r = market_insider_risk(None, "completely ambiguous market");
        assert!(r >= 1.0, "risk multiplier should never drop below 1.0, got {}", r);
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
