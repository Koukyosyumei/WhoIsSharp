//! Async agent loop.
//!
//! Drives the LLM backend in a tool-calling loop and emits `AppEvent`s so the
//! TUI can update in real time without blocking the main thread.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::llm::{LlmBackend, LlmMessage, MessageContent, ToolResult};
use crate::signals::{self, Signal};
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
    RefreshStarted,
    RefreshDone,
    RefreshError(String),
}

// ─── System prompt ────────────────────────────────────────────────────────────

const SYSTEM_PROMPT: &str = "\
You are WhoIsSharp, a professional-grade AI prediction market analyst embedded in an \
interactive terminal dashboard. You have access to live data from Polymarket and Kalshi, \
plus a suite of analytical tools for insider detection and position sizing.

ANALYTICAL WORKFLOW
1. Always fetch fresh data before drawing conclusions.
2. Express probabilities in both decimal and percent (e.g. '0.72 / 72% YES').
3. Note price movements, volume spikes, spread anomalies, and cross-platform divergences.
4. When you spot an unusual market, chain your tools: market data → insider signal → \
   smart money → wallet profile → Kelly sizing.
5. Be concise — the user is in a terminal, not reading a report. Bullets over prose.

TOOL REFERENCE
Market data (both platforms):
  list_markets        — browse markets by platform, category, or keyword
  get_market          — full detail for one market (price, volume, liquidity)
  get_orderbook       — live bid/ask depth
  get_price_history   — historical YES-price chart with ASCII sparkline
  get_events          — event categories and groupings
  search_markets      — keyword search across Polymarket + Kalshi

Insider / smart-money analysis (Polymarket only):
  analyze_insider     — price velocity + orderbook imbalance for ONE market
  find_smart_money    — rank top wallets in a market by win rate, alpha-entry score,
                        and wallet coordination (concurrent fetch, Jaccard clustering)
  analyze_wallet      — deep profile of ONE wallet: history, alpha score, top markets,
                        composite suspicion score (0–100)

Position sizing:
  kelly_size          — Kelly / half-Kelly bet size given your edge vs market price;
                        returns dollar amount, share count, and expected value

INSIDER SIGNAL INTERPRETATION
  • Alpha entry score: average BUY price on winning trades. Below 35¢ = they bought \
    before consensus; strong informed-flow signal.
  • Coordination: Jaccard market-overlap ≥ 35% between two wallets → possible coordinated \
    positioning. Investigate wallet funding sources.
  • Vol/Liq ratio > 15× at extreme price (>75% or <25%) = INSDR signal on the Signals tab.
  • Always cross-reference against public news timelines before drawing conclusions.

KELLY SIZING RULES (professional standard)
  • Negative Kelly = no edge on that side; consider the opposite side.
  • Always recommend HALF-Kelly as the default; full Kelly is too aggressive.
  • Cap any single position at 5–10% of bankroll regardless of Kelly output.
  • Kelly requires accurate probability estimates — err on the conservative side.

KELLY AUTO-FILL RULES — CRITICAL
  When the user asks to 'size a bet', 'run Kelly', or 'calculate Kelly' on a market \
  that has already been discussed in this conversation, you MUST call kelly_size \
  immediately with values inferred from context. Do NOT ask the user for parameters \
  you can derive yourself:
  • market_price: use the YES price from the most recently fetched market data. \
    If not in context, call get_market first, then call kelly_size.
  • your_probability: derive from your analysis. If smart money is bearish (high \
    suspicion scores, strong alpha buying NO side), shade your estimate below the \
    market price. If bullish signals, shade above. When uncertain, use the market \
    price ± 0.05 and note the assumption.
  • bankroll: default to 1000 unless the user stated a specific amount.
  • side: infer from context ('yes' if you think the market underprices YES, \
    'no' if overpriced). State your assumption explicitly.
  NEVER ask the user for market_price — you have tools to fetch it. \
  NEVER refuse to run Kelly because parameters are missing — make reasonable \
  assumptions, state them clearly, then call the tool.";

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

    // Compute signals synchronously before emitting events
    let computed_signals = signals::compute_signals(&all);

    let _ = event_tx.send(AppEvent::MarketsLoaded(all));
    let _ = event_tx.send(AppEvent::SignalsComputed(computed_signals));
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
            let id = market.token_id.as_deref().unwrap_or(&market.id);
            clients
                .polymarket
                .fetch_price_history(id, interval.polymarket_fidelity(), start_ts, now)
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
            let _ = event_tx.send(AppEvent::RefreshError(format!("Price history: {}", e)));
        }
    }
}

pub async fn refresh_smart_money(
    clients:   Arc<MarketClients>,
    market_id: String,
    event_tx:  mpsc::UnboundedSender<AppEvent>,
) {
    let _ = event_tx.send(AppEvent::SmartMoneyLoading);

    match tools::smart_money_for_market(&clients, &market_id, 8).await {
        Ok(result) => {
            let _ = event_tx.send(AppEvent::SmartMoneyLoaded { market_id, result });
        }
        Err(e) => {
            let _ = event_tx.send(AppEvent::RefreshError(format!("Smart money: {}", e)));
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
