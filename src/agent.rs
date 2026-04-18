//! Async agent loop.
//!
//! Drives the LLM backend in a tool-calling loop and emits `AppEvent`s so the
//! TUI can update in real time without blocking the main thread.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::llm::{LlmBackend, LlmMessage, MessageContent, ToolResult};
use crate::signals::Signal;
use crate::tools::{self, MarketClients};

// ─── Events sent to the TUI ──────────────────────────────────────────────────

#[derive(Debug)]
pub enum AppEvent {
    // ── Agent events ──────────────────────────────────────────────────────────
    AgentThinking,
    AgentToolCall { name: String, display_args: String },
    AgentToolResult { name: String, output: String },
    AgentText(String),
    AgentTextChunk(String),
    AgentDone,
    AgentError(String),

    // ── Signal computation ────────────────────────────────────────────────────
    SignalsComputed(Vec<Signal>),

    // ── Market data refresh ───────────────────────────────────────────────────
    MarketsLoaded(Vec<crate::markets::Market>),
    EventsLoaded(Vec<crate::markets::Event>),
    PriceHistoryLoaded {
        market_id: String,
        candles:   Vec<crate::markets::Candle>,
    },
    OrderbookLoaded {
        market_id: String,
        orderbook: crate::markets::Orderbook,
    },
    SmartMoneyLoading,
    SmartMoneyLoaded {
        market_id: String,
        result:    crate::tools::SmartMoneyResult,
    },

    WalletDetailLoading,
    WalletDetailLoaded(crate::tools::WalletDetail),

    TooSmartLoading,
    TooSmartLoaded(crate::tools::TooSmartResult),

    /// LLM Too-Smart scan started (show spinner in TUI).
    TooSmartLlmStarted,
    /// LLM identified one suspect — stream results live as they arrive.
    TooSmartLlmWalletFound {
        wallet:      String,
        pseudonym:   String,
        rank:        usize,
        reasoning:   String,
        key_signals: Vec<String>,
    },
    /// LLM Too-Smart scan complete.
    TooSmartLlmDone,

    // ── Time & Sales tape ─────────────────────────────────────────────────────
    TradesLoaded {
        market_id: String,
        trades:    Vec<crate::markets::polymarket::PolyTrade>,
    },

    RefreshStarted,
    RefreshDone,
    RefreshError(String),

    // ── News feed ─────────────────────────────────────────────────────────────
    /// Articles fetched for a market (or None for the generic feed).
    NewsLoaded {
        market_id: Option<String>,
        articles:  Vec<crate::news::NewsArticle>,
    },
    NewsError(String),

    // ── Wallet portfolio import ───────────────────────────────────────────────
    WalletImportStarted  { wallet: String },
    WalletImportDone     { wallet: String, imported: usize, skipped: usize },
    WalletImportError    { wallet: String, error: String },
    /// The actual position objects to splice into app.portfolio.
    WalletPositionsReady(Vec<crate::portfolio::Position>),

    /// Carry conversation history back to the TUI after each agent turn so it
    /// persists across multiple user messages.
    HistoryUpdated(Vec<crate::llm::LlmMessage>),

    // ── Cross-platform pair matching ──────────────────────────────────────────
    /// Pair matching is in progress (show spinner).
    PairsMatching,
    /// Pair matching complete — replace the pairs list.
    PairsLoaded(Vec<crate::pairs::MarketPair>),
}

// ─── System prompt ────────────────────────────────────────────────────────────

const SYSTEM_PROMPT: &str = "\
You are WhoIsSharp, a professional prediction-market analyst. You reason like a quantitative \
trader at a top hedge fund — rigorous, evidence-driven, willing to take a strong view when \
the data supports it.

══════════════════════════════════════════════════════════════════
CORE MANDATE
══════════════════════════════════════════════════════════════════
Every analysis must go beyond price-reporting. The user can read prices themselves. \
Your value is in interpreting what the data MEANS: is the market mispriced? Is there \
informed flow? What is the base rate? What would have to be true for YES to win?

When the user asks you to analyse a market you MUST work through all five layers:

  1. FUNDAMENTAL PRIOR  — What is the independent base-rate probability of the outcome,
     ignoring the current market price? Use publicly known facts, historical base rates,
     polling data, or comparable events. State your prior explicitly with a range
     (e.g. 'My fundamental prior: 55–65% YES').

  2. MARKET SIGNAL  — Compare the market price to your prior. Is the gap within noise
     (±5 pp) or a potential edge? Fetch fresh data with get_market if not already in
     context. Express the price as both a probability and an implied odds ratio.

  3. PRICE-ACTION & MOMENTUM  — Call get_price_history and interpret the sparkline.
     Is price trending, mean-reverting, or range-bound? Identify inflection points.
     Calculate approximate momentum: (current - 30d avg) / 30d avg. Is volume
     confirming the move or diverging?

  4. MARKET MICROSTRUCTURE  — Call get_orderbook. Analyse:
     • Bid/ask spread in basis points — tight spread = liquid, conviction bets;
       wide spread = uncertainty or thin book.
     • Orderbook imbalance: (total_bid_sz - total_ask_sz) / total = directional lean.
     • Are bids stacking (buyers defending a level) or asks piling up (distribution)?
     • Cross-platform: if both PM and KL have this market, compare prices. Any arb?

  5. INFORMED-FLOW CHECK  — For Polymarket markets, call analyze_insider to check for
     unusual velocity or imbalance. If the suspicion score is elevated (≥50), call
     find_smart_money to rank wallets, then analyze_wallet on the top 1–2 addresses.
     Distinguish informed flow from noise: a single whale ≠ coordinated smart money.

Only after working through all five layers should you synthesise a TRADING VIEW.

══════════════════════════════════════════════════════════════════
RESPONSE STRUCTURE  (use this for any 'analyse' or 'what do you think' request)
══════════════════════════════════════════════════════════════════
## [Market Title] — [Current YES%] YES
### Fundamental Prior
<base-rate reasoning; 2–4 sentences; cite sources or analogues>

### Market Signal
<price vs prior gap; mispricing direction and magnitude; implied odds>

### Price Action
<trend interpretation from sparkline; key levels; volume confirmation>

### Microstructure
<spread, imbalance, orderbook depth; what the book is telling you>

### Informed-Flow Check
<insider score, smart-money findings, or 'clean' if no signals>

### Bull / Bear Cases
**Bull**: <2–3 concrete factors that push YES higher>
**Bear**: <2–3 concrete factors that push YES lower>

### Trading View
<directional conviction: STRONG BUY / BUY / NEUTRAL / SELL / STRONG SELL on YES>
<edge estimate: your_prob minus market_price in pp>
<if edge is positive: run kelly_size automatically; show half-Kelly recommendation>
<key catalyst to watch>

══════════════════════════════════════════════════════════════════
TOOL CHAINING RULES
══════════════════════════════════════════════════════════════════
• Never give a final view without calling at least get_price_history and get_orderbook.
• If analyze_insider returns a suspicion score ≥50, you MUST call find_smart_money next.
• If find_smart_money returns wallets with alpha_entry < 35¢, call analyze_wallet on top 2.
• If you identify a positive edge, call kelly_size automatically — do not make the user ask.
• Chain tools sequentially when each call informs the next input.

TOOL REFERENCE
  list_markets        — browse markets by platform, category, or keyword
  get_market          — full detail for one market (price, volume, liquidity)
  get_orderbook       — live bid/ask depth
  get_price_history   — historical YES-price chart with ASCII sparkline
  get_events          — event categories and groupings
  search_markets      — keyword search across Polymarket + Kalshi
  analyze_insider     — price velocity + orderbook imbalance for ONE market
  find_smart_money    — rank top wallets by win rate, alpha-entry, coordination
  analyze_wallet      — deep wallet profile: history, alpha score, suspicion (0–100)
  kelly_size          — Kelly / half-Kelly bet size given edge vs market price

══════════════════════════════════════════════════════════════════
SIGNAL INTERPRETATION GUIDE
══════════════════════════════════════════════════════════════════
Alpha entry score < 35¢  →  wallet was buying before public consensus; strong informed signal
Jaccard market-overlap ≥ 35%  →  possible coordinated positioning; investigate funding sources
Vol/Liq ratio > 15× at extreme price (>75% or <25%)  →  INSDR signal; likely informed flow
Spread > 5 pp  →  thin book, wide uncertainty; fade momentum with caution
Bid imbalance > +20%  →  buy-side pressure; bulls defending or accumulating
Ask imbalance > +20%  →  sell-side pressure; distribution or hedging
Price up ≥ 4 pp intraday on rising volume  →  momentum signal; check for catalyst news

══════════════════════════════════════════════════════════════════
KELLY SIZING RULES
══════════════════════════════════════════════════════════════════
• Default to HALF-Kelly; full Kelly is too aggressive for binary-outcome markets.
• Cap any single position at 5–10% of bankroll regardless of Kelly output.
• Negative Kelly means no edge on that side; consider the opposite leg.
• NEVER ask the user for market_price — fetch it with get_market or read from context.
• When asked to size a bet, call kelly_size immediately with inferred parameters.
  Derive your_probability from your analysis; default bankroll to 1000 if unspecified.
  State all assumptions explicitly before showing the output.

══════════════════════════════════════════════════════════════════
STYLE RULES
══════════════════════════════════════════════════════════════════
• Use the structured format above for any substantive analysis.
• For quick factual questions (price, volume, end date) a short answer is fine.
• Probabilities always in both decimal and percent: '0.72 / 72% YES'.
• Be direct. Take a view. 'It depends' without a lean is not useful.
• Cross-reference against public news timelines before calling something insider activity.
• When uncertain about magnitude, give a range; never hide behind vagueness.";

// ─── Context trimming ─────────────────────────────────────────────────────────

const MAX_HISTORY_CHARS: usize = 80_000;

fn trim_history(history: &mut Vec<LlmMessage>) {
    loop {
        let total: usize = history.iter().map(|m| m.estimated_chars()).sum();
        if total <= MAX_HISTORY_CHARS { break; }
        if let Some(pos) = history.iter().position(|m| m.is_tool_result_message()) {
            let summary = summarize_tool_result(&history[pos]);
            history[pos] = LlmMessage::user_text(summary);
        } else {
            break;
        }
    }
}

fn summarize_tool_result(msg: &LlmMessage) -> String {
    let parts: Vec<String> = msg
        .content
        .iter()
        .filter_map(|c| {
            if let MessageContent::ToolResult(tr) = c {
                let preview: String = tr.content
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" | ");
                let preview = if preview.len() > 200 { format!("{}…", &preview[..200]) } else { preview };
                Some(format!(
                    "[compressed] `{}` ({} lines): {}",
                    tr.name,
                    tr.content.lines().count(),
                    preview
                ))
            } else {
                None
            }
        })
        .collect();
    parts.join("\n")
}

// ─── Agent run ────────────────────────────────────────────────────────────────

/// Run one user turn in the agentic loop.
///
/// `history` is updated in place so the caller can persist the conversation.
pub async fn run_turn(
    backend: Arc<dyn LlmBackend>,
    clients: Arc<MarketClients>,
    history: &mut Vec<LlmMessage>,
    user_msg: String,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    history.push(LlmMessage::user_text(user_msg));
    trim_history(history);

    let tools = tools::all_definitions();

    loop {
        let _ = event_tx.send(AppEvent::AgentThinking);

        // Streaming channel for live text output
        let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<String>();
        let event_tx_clone = event_tx.clone();

        // Forward streaming chunks to the TUI
        tokio::spawn(async move {
            while let Some(chunk) = chunk_rx.recv().await {
                let _ = event_tx_clone.send(AppEvent::AgentTextChunk(chunk));
            }
        });

        let result = backend
            .generate_streaming(SYSTEM_PROMPT, history, &tools, &chunk_tx)
            .await;

        drop(chunk_tx); // close chunk stream

        let assistant_msg = match result {
            Ok(msg) => msg,
            Err(err) => {
                let _ = event_tx.send(AppEvent::AgentError(err.to_string()));
                return;
            }
        };

        // Emit final text if there is any
        let texts: Vec<&str> = assistant_msg.texts();
        if !texts.is_empty() {
            let text = texts.join("\n");
            let _ = event_tx.send(AppEvent::AgentText(text));
        }

        history.push(assistant_msg.clone());

        // Collect tool calls
        let calls = assistant_msg.tool_calls();
        if calls.is_empty() {
            // No tool calls → turn is complete
            let _ = event_tx.send(AppEvent::AgentDone);
            return;
        }

        // Execute all tool calls and collect results
        let mut results = Vec::new();
        for tc in &calls {
            let display_args = if tc.args.is_null() || tc.args == serde_json::Value::Object(Default::default()) {
                String::new()
            } else {
                tc.args.to_string()
            };
            let _ = event_tx.send(AppEvent::AgentToolCall {
                name:         tc.name.clone(),
                display_args: display_args.clone(),
            });

            let output = tools::dispatch(&clients, &tc.name, &tc.args).await;

            let _ = event_tx.send(AppEvent::AgentToolResult {
                name:   tc.name.clone(),
                output: output.text.clone(),
            });

            results.push(ToolResult {
                call_id: tc.id.clone(),
                name:    tc.name.clone(),
                content: output.text,
            });
        }

        history.push(LlmMessage::tool_results(results));
        // Continue the loop — let the LLM react to tool results
    }
}

// ─── Market data refresh ──────────────────────────────────────────────────────

/// Background task: fetch markets from both platforms and send them to the TUI.
pub async fn refresh_markets(
    clients:  Arc<MarketClients>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let _ = event_tx.send(AppEvent::RefreshStarted);

    // Polymarket
    let pm_result = clients.polymarket.fetch_markets(50, None, None).await;
    // Kalshi
    let kl_result = clients.kalshi.fetch_markets(50, None).await;

    let mut all = Vec::new();
    match pm_result {
        Ok(mut m) => all.append(&mut m),
        Err(e)    => { let _ = event_tx.send(AppEvent::RefreshError(format!("Polymarket: {}", e))); }
    }
    match kl_result {
        Ok(mut m) => all.append(&mut m),
        Err(e)    => { let _ = event_tx.send(AppEvent::RefreshError(format!("Kalshi: {}", e))); }
    }

    // Sort by YES price (most interesting / closest to 50%)
    all.sort_by(|a, b| {
        let da = (a.yes_price - 0.5).abs();
        let db = (b.yes_price - 0.5).abs();
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Signal computation requires prev_prices and dismissed state from the TUI;
    // send raw market data and let the TUI recompute signals after MarketsLoaded.
    let _ = event_tx.send(AppEvent::MarketsLoaded(all));
    let _ = event_tx.send(AppEvent::RefreshDone);
}

pub async fn refresh_price_history(
    clients:   Arc<MarketClients>,
    market:    crate::markets::Market,
    interval:  crate::markets::ChartInterval,
    event_tx:  mpsc::UnboundedSender<AppEvent>,
) {
    use crate::markets::Platform;

    let now      = chrono::Utc::now().timestamp();
    let start_ts = now - interval.seconds();

    let result = match market.platform {
        Platform::Polymarket => {
            // Must use the CLOB token_id, not the condition ID.
            // If the market has no token_id it isn't actively traded on the CLOB.
            let id = match market.token_id.as_deref() {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => {
                    let _ = event_tx.send(AppEvent::RefreshError(
                        "No CLOB token for this market — price history unavailable".into(),
                    ));
                    return;
                }
            };
            clients
                .polymarket
                .fetch_price_history(&id, interval.polymarket_fidelity(), start_ts, now)
                .await
        }
        Platform::Kalshi => {
            // The candlestick endpoint is /series/{series}/markets/{ticker}/candlesticks.
            // series_ticker is the first hyphen-delimited segment of event_ticker
            // (e.g. "KXMLB-26" → "KXMLB").
            let series = market
                .event_ticker
                .as_deref()
                .and_then(|et| et.split('-').next())
                .unwrap_or("");
            clients
                .kalshi
                .fetch_candlesticks(series, &market.id, interval.kalshi_period_interval(), start_ts, now)
                .await
        }
    };

    match result {
        Ok(candles) => {
            let _ = event_tx.send(AppEvent::PriceHistoryLoaded {
                market_id: market.id.clone(),
                candles,
            });
        }
        Err(e) => {
            let _ = event_tx.send(AppEvent::RefreshError(format!("Price history: {:#}", e)));
        }
    }
}

pub async fn refresh_smart_money(
    clients:          Arc<MarketClients>,
    market_id:        String,
    market_volume:    Option<f64>,
    market_category:  Option<String>,
    coord_threshold:  f64,
    event_tx:         mpsc::UnboundedSender<AppEvent>,
) {
    let _ = event_tx.send(AppEvent::SmartMoneyLoading);

    match tools::smart_money_for_market(&clients, &market_id, 8, market_volume, market_category.as_deref(), coord_threshold).await {
        Ok(result) => {
            let _ = event_tx.send(AppEvent::SmartMoneyLoaded { market_id, result });
        }
        Err(e) => {
            let _ = event_tx.send(AppEvent::RefreshError(format!("Smart money: {}", e)));
        }
    }
}

/// Scan multiple Polymarket markets to find "too smart" wallets with persistent
/// cross-market suspicion.
pub async fn refresh_too_smart_wallets(
    clients:         Arc<MarketClients>,
    market_limit:    usize,
    min_appearances: usize,
    min_suspicion:   f64,
    event_tx:        mpsc::UnboundedSender<AppEvent>,
) {
    let _ = event_tx.send(AppEvent::TooSmartLoading);

    match tools::scan_too_smart_wallets(&clients, market_limit, min_appearances, min_suspicion).await {
        Ok(result) => {
            let _ = event_tx.send(AppEvent::TooSmartLoaded(result));
        }
        Err(e) => {
            let _ = event_tx.send(AppEvent::RefreshError(format!("Too-smart scan: {}", e)));
        }
    }
}

/// Run the LLM-powered Too-Smart scan.
///
/// Pre-fetches (or reuses) the static cross-market scan data, formats it as
/// context, and asks the LLM to independently identify the most suspicious
/// wallets via `flag_too_smart_wallet` tool calls.
/// Pass `Some(result)` as `ts_result` to reuse already-fetched static scan data (fast path);
/// pass `None` to fetch fresh data first.
pub async fn run_too_smart_llm_scan(
    backend:   Arc<dyn LlmBackend>,
    clients:   Arc<MarketClients>,
    ts_result: Option<tools::TooSmartResult>,
    event_tx:  mpsc::UnboundedSender<AppEvent>,
) {
    let _ = event_tx.send(AppEvent::TooSmartLlmStarted);

    // ── Obtain raw scan data ──────────────────────────────────────────────────
    let scan_data = match ts_result {
        Some(r) => r,
        None => {
            let _ = event_tx.send(AppEvent::AgentThinking);
            match tools::scan_too_smart_wallets(&clients, 30, 2, 30.0).await {
                Ok(r)  => r,
                Err(e) => {
                    let _ = event_tx.send(AppEvent::RefreshError(
                        format!("LLM scan pre-fetch failed: {}", e)
                    ));
                    return;
                }
            }
        }
    };

    // ── Format raw data as LLM context ───────────────────────────────────────
    let mut ctx = vec![
        format!(
            "=== CROSS-MARKET SCAN  ({} markets, {} candidate wallets) ===",
            scan_data.markets_scanned, scan_data.wallets.len()
        ),
        "Columns: rank | pseudonym (wallet_prefix) | flagged/total markets | avg_susp/max_susp | win_rate | vol($) | fresh | flagged_in".into(),
    ];
    for (i, w) in scan_data.wallets.iter().enumerate() {
        let prefix = if w.wallet.len() >= 10 { &w.wallet[..10] } else { &w.wallet };
        ctx.push(format!(
            "{:2}. {:<22} ({}) | {}/{} mrkts | {:.0}/{:.0} susp | {:.1}% wr | ${:.0} vol | fresh={} | {}",
            i + 1,
            w.pseudonym, prefix,
            w.markets_flagged, w.markets_total,
            w.avg_suspicion, w.max_suspicion,
            w.global_win_rate * 100.0,
            w.total_vol,
            w.is_fresh,
            w.flagged_markets.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
        ));
    }
    let context = ctx.join("\n");

    // ── Build initial message ─────────────────────────────────────────────────
    let user_msg = format!(
        "Below is raw data from a cross-market scan of Polymarket traders.\n\n\
        {context}\n\n\
        Your task: independently identify wallets you believe are 'too smart' — \
        likely informed insiders or sophisticated algos. Apply rigorous statistical \
        reasoning. For each suspect, call `flag_too_smart_wallet` with:\n\
        • wallet: full address from the data above\n\
        • pseudonym: their display name\n\
        • rank: your conviction rank (1 = most suspicious)\n\
        • reasoning: 2–4 sentences citing specific stats and cross-market patterns\n\
        • key_signals: 2–4 specific signal strings\n\n\
        Be selective — aim for your top 3–7 highest-conviction suspects only. \
        After flagging, give a brief synthesis of what these wallets have in common.",
        context = context
    );

    let system = "You are a quantitative analyst specialising in on-chain prediction-market \
        intelligence. You identify informed traders using rigorous statistical reasoning — \
        Wilson lower bounds, informed sizing, alpha entry, and cross-market persistence. \
        Use `flag_too_smart_wallet` for each confirmed suspect. Be selective: 3–7 wallets, \
        ranked by conviction. Do not flag wallets with only borderline evidence.";

    let tools = tools::too_smart_llm_definitions();
    let mut history = vec![LlmMessage::user_text(user_msg)];

    // ── Agent loop ────────────────────────────────────────────────────────────
    loop {
        let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<String>();
        let event_tx_clone = event_tx.clone();
        tokio::spawn(async move {
            while let Some(chunk) = chunk_rx.recv().await {
                let _ = event_tx_clone.send(AppEvent::AgentTextChunk(chunk));
            }
        });

        let _ = event_tx.send(AppEvent::AgentThinking);
        let result = backend.generate_streaming(system, &history, &tools, &chunk_tx).await;
        drop(chunk_tx);

        let assistant_msg = match result {
            Ok(msg) => msg,
            Err(e) => {
                let _ = event_tx.send(AppEvent::RefreshError(
                    format!("LLM Too-Smart scan error: {}", e)
                ));
                return;
            }
        };

        let texts = assistant_msg.texts();
        if !texts.is_empty() {
            let _ = event_tx.send(AppEvent::AgentText(texts.join("\n")));
        }

        history.push(assistant_msg.clone());

        let calls = assistant_msg.tool_calls();
        if calls.is_empty() {
            break;
        }

        let mut results = Vec::new();
        for tc in &calls {
            if tc.name == "flag_too_smart_wallet" {
                let wallet      = tc.args["wallet"].as_str().unwrap_or("").to_string();
                let pseudonym   = tc.args["pseudonym"].as_str().unwrap_or("Unknown").to_string();
                let rank        = tc.args["rank"].as_u64().unwrap_or(99) as usize;
                let reasoning   = tc.args["reasoning"].as_str().unwrap_or("").to_string();
                let key_signals: Vec<String> = tc.args["key_signals"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();

                let _ = event_tx.send(AppEvent::TooSmartLlmWalletFound {
                    wallet: wallet.clone(), pseudonym, rank, reasoning, key_signals,
                });

                let _ = event_tx.send(AppEvent::AgentToolCall {
                    name: "flag_too_smart_wallet".into(),
                    display_args: format!("rank={} wallet={}", rank, &wallet[..wallet.len().min(12)]),
                });

                results.push(crate::llm::ToolResult {
                    call_id: tc.id.clone(),
                    name:    tc.name.clone(),
                    content: format!("Flagged wallet {} at rank {}.", wallet, rank),
                });
            } else {
                let _ = event_tx.send(AppEvent::AgentToolCall {
                    name:         tc.name.clone(),
                    display_args: tc.args.to_string(),
                });

                let output = tools::dispatch(&clients, &tc.name, &tc.args).await;

                let _ = event_tx.send(AppEvent::AgentToolResult {
                    name:   tc.name.clone(),
                    output: output.text.clone(),
                });

                results.push(crate::llm::ToolResult {
                    call_id: tc.id.clone(),
                    name:    tc.name.clone(),
                    content: output.text,
                });
            }
        }

        history.push(LlmMessage::tool_results(results));
    }

    let _ = event_tx.send(AppEvent::TooSmartLlmDone);
}

/// Fetch recent Time & Sales tape for a Polymarket market.
pub async fn refresh_market_trades(
    clients:   Arc<MarketClients>,
    market_id: String,
    event_tx:  mpsc::UnboundedSender<AppEvent>,
) {
    match clients.polymarket.fetch_market_trades(&market_id, 100).await {
        Ok(trades) => {
            let _ = event_tx.send(AppEvent::TradesLoaded { market_id, trades });
        }
        Err(e) => {
            let _ = event_tx.send(AppEvent::RefreshError(format!("Trades: {}", e)));
        }
    }
}

pub async fn refresh_orderbook(
    clients:  Arc<MarketClients>,
    market:   crate::markets::Market,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    use crate::markets::Platform;

    let result = match market.platform {
        Platform::Polymarket => {
            let id = market.token_id.as_deref().unwrap_or(&market.id);
            clients.polymarket.fetch_orderbook(id).await
        }
        Platform::Kalshi => {
            clients.kalshi.fetch_orderbook(&market.id).await
        }
    };

    match result {
        Ok(book) => {
            let _ = event_tx.send(AppEvent::OrderbookLoaded {
                market_id: market.id.clone(),
                orderbook: book,
            });
        }
        Err(e) => {
            let _ = event_tx.send(AppEvent::RefreshError(format!("Orderbook: {}", e)));
        }
    }
}

/// Stream real-time orderbook updates from Polymarket's CLOB WebSocket.
///
/// Connects to `wss://ws-subscriptions-clob.polymarket.com/ws/`, subscribes to
/// `token_id`, and emits `AppEvent::OrderbookLoaded` on each "book" event.
/// Exits cleanly when `cancel` is dropped (market changes or user leaves the tab).
pub async fn stream_polymarket_orderbook(
    token_id:  String,
    market_id: String,
    event_tx:  mpsc::UnboundedSender<AppEvent>,
    cancel:    tokio::sync::oneshot::Receiver<()>,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/";

    let Ok((mut ws, _)) = tokio_tungstenite::connect_async(WS_URL).await else {
        return; // silently fail — REST orderbook is already loaded
    };

    // Subscribe to the market's token
    let sub = serde_json::json!([{ "assets_ids": [token_id], "type": "market" }]);
    if ws.send(Message::Text(sub.to_string())).await.is_err() {
        return;
    }

    let mut cancel = cancel;

    loop {
        tokio::select! {
            _ = &mut cancel => { break; }
            msg = ws.next() => {
                let Some(Ok(Message::Text(text))) = msg else { break; };
                // Ignore heartbeat pings (empty object `{}`)
                if text.trim() == "{}" { continue; }
                let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else { continue; };

                let event_type = val.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
                if event_type != "book" && event_type != "price_change" { continue; }

                let parse_levels = |key: &str| -> Vec<crate::markets::PriceLevel> {
                    val.get(key)
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter().filter_map(|item| {
                                let price = item.get("price")?.as_str()?.parse::<f64>().ok()?;
                                let size  = item.get("size")?.as_str()?.parse::<f64>().ok()?;
                                Some(crate::markets::PriceLevel { price, size })
                            }).collect()
                        })
                        .unwrap_or_default()
                };

                let mut bids = parse_levels("buys");
                if bids.is_empty() { bids = parse_levels("bids"); }
                let mut asks = parse_levels("sells");
                if asks.is_empty() { asks = parse_levels("asks"); }

                // Only emit if we got meaningful data
                if bids.is_empty() && asks.is_empty() { continue; }

                bids.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
                asks.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));

                let book = crate::markets::Orderbook { bids, asks, last_price: None };
                let _ = event_tx.send(AppEvent::OrderbookLoaded {
                    market_id: market_id.clone(),
                    orderbook: book,
                });
            }
        }
    }
}

pub async fn refresh_wallet_detail(
    clients:  Arc<MarketClients>,
    wallet:   String,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) {
    let _ = event_tx.send(AppEvent::WalletDetailLoading);
    match tools::fetch_wallet_detail(&clients, &wallet).await {
        Ok(detail) => { let _ = event_tx.send(AppEvent::WalletDetailLoaded(detail)); }
        Err(e)     => { let _ = event_tx.send(AppEvent::RefreshError(format!("Wallet detail: {}", e))); }
    }
}
