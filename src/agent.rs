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
    RefreshStarted,
    RefreshDone,
    RefreshError(String),
}

// ─── System prompt ────────────────────────────────────────────────────────────

const SYSTEM_PROMPT: &str = "\
You are WhoIsSharp, an AI-powered prediction market analyst embedded in an interactive \
terminal dashboard. You have access to live data from Polymarket and Kalshi.

Use your tools to:
- Fetch and analyse current market prices and probabilities
- Examine orderbook depth and liquidity
- Review historical price trends
- Compare similar markets across platforms
- Search for markets by topic

When answering questions:
1. Always fetch fresh data before drawing conclusions
2. Express probabilities clearly (e.g. '72% YES' or '0.72 implied probability')
3. Note significant price movements, volume spikes, or unusual spreads
4. When comparing across platforms, highlight divergences — they may indicate arbitrage
5. Be concise: the user is looking at a terminal dashboard, not a report

Tools available: list_markets, get_market, get_orderbook, get_price_history, \
get_events, search_markets.";

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
            clients
                .kalshi
                .fetch_candlesticks(&market.id, interval.kalshi_period_interval(), start_ts, now)
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
