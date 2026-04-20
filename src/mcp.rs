//! MCP (Model Context Protocol) server — JSON-RPC 2.0 over stdio.
//!
//! Run with:  whoissharp --mcp
//! Supports: tools · resources · prompts · progress notifications · ping

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::tools::{self, MarketClients};

// ─── Wire types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id:      Option<Value>,
    method:  String,
    #[serde(default)]
    params:  Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id:      Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result:  Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error:   Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code:    i32,
    message: String,
}

/// A notification has no `id`.
#[derive(Debug, Serialize)]
struct RpcNotification {
    jsonrpc: &'static str,
    method:  String,
    params:  Value,
}

impl RpcResponse {
    fn ok(id: Value, result: Value) -> Self {
        RpcResponse { jsonrpc: "2.0", id, result: Some(result), error: None }
    }
    fn err(id: Value, code: i32, msg: impl Into<String>) -> Self {
        RpcResponse { jsonrpc: "2.0", id, result: None, error: Some(RpcError { code, message: msg.into() }) }
    }
}

// ─── Stdout write channel ─────────────────────────────────────────────────────

type WriteTx = mpsc::UnboundedSender<String>;

fn send_response(tx: &WriteTx, resp: &RpcResponse) {
    if let Ok(s) = serde_json::to_string(resp) { let _ = tx.send(s); }
}

fn send_notification(tx: &WriteTx, notif: &RpcNotification) {
    if let Ok(s) = serde_json::to_string(notif) { let _ = tx.send(s); }
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub async fn run_server(clients: Arc<MarketClients>) -> Result<()> {
    let stdin  = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let mut reader  = BufReader::new(stdin).lines();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // Writer task — serialises all output so concurrent tasks don't interleave.
    tokio::spawn(async move {
        let mut out = stdout;
        while let Some(line) = rx.recv().await {
            let _ = out.write_all(line.as_bytes()).await;
            let _ = out.write_all(b"\n").await;
            let _ = out.flush().await;
        }
    });

    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() { continue; }

        let req: RpcRequest = match serde_json::from_str(&line) {
            Ok(r)  => r,
            Err(e) => {
                let resp = RpcResponse::err(Value::Null, -32700, format!("Parse error: {e}"));
                send_response(&tx, &resp);
                continue;
            }
        };

        // Notifications (no id) — handle silently if needed, never respond.
        if req.id.is_none() {
            continue;
        }

        let id      = req.id.clone().unwrap();
        let method  = req.method.clone();
        let params  = req.params.clone();
        let clients = Arc::clone(&clients);
        let tx2     = tx.clone();

        tokio::spawn(async move {
            let resp = handle(&method, &params, &clients, id, &tx2).await;
            send_response(&tx2, &resp);
        });
    }

    Ok(())
}

// ─── Method dispatch ──────────────────────────────────────────────────────────

async fn handle(
    method:  &str,
    params:  &Value,
    clients: &Arc<MarketClients>,
    id:      Value,
    tx:      &WriteTx,
) -> RpcResponse {
    match method {
        "initialize"               => handle_initialize(id),
        "tools/list"               => handle_tools_list(id),
        "tools/call"               => handle_tools_call(id, params, clients, tx).await,
        "resources/list"           => handle_resources_list(id),
        "resources/read"           => handle_resources_read(id, params, clients).await,
        "prompts/list"             => handle_prompts_list(id),
        "prompts/get"              => handle_prompts_get(id, params),
        "ping"                     => RpcResponse::ok(id, json!({})),
        "notifications/initialized" => RpcResponse::ok(id, json!({})),
        _ => RpcResponse::err(id, -32601, format!("Method not found: {method}")),
    }
}

// ─── initialize ──────────────────────────────────────────────────────────────

fn handle_initialize(id: Value) -> RpcResponse {
    RpcResponse::ok(id, json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools":     {},
            "resources": {},
            "prompts":   {}
        },
        "serverInfo": {
            "name":    "whoissharp",
            "version": env!("CARGO_PKG_VERSION")
        }
    }))
}

// ─── tools/list ──────────────────────────────────────────────────────────────

fn handle_tools_list(id: Value) -> RpcResponse {
    let defs = tools::all_definitions();
    let tools: Vec<Value> = defs
        .into_iter()
        .map(|d| json!({
            "name":        d.name,
            "description": d.description,
            "inputSchema": d.parameters,
        }))
        .collect();
    RpcResponse::ok(id, json!({ "tools": tools }))
}

// ─── tools/call ──────────────────────────────────────────────────────────────

async fn handle_tools_call(
    id:      Value,
    params:  &Value,
    clients: &Arc<MarketClients>,
    tx:      &WriteTx,
) -> RpcResponse {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None    => return RpcResponse::err(id, -32602, "Missing required parameter: name"),
    };

    let args          = params.get("arguments").cloned().unwrap_or(json!({}));
    let progress_token = params.get("_meta")
        .and_then(|m| m.get("progressToken"))
        .cloned();

    // When the client supplies a progressToken, wire a progress channel so
    // long-running tools (e.g. scan_smart_money) can stream notifications.
    let prog_tx = if let Some(token) = progress_token {
        let (ptx, mut prx) = mpsc::unbounded_channel::<(u32, u32)>();
        let notif_tx = tx.clone();
        tokio::spawn(async move {
            while let Some((done, total)) = prx.recv().await {
                let notif = RpcNotification {
                    jsonrpc: "2.0",
                    method:  "notifications/progress".to_string(),
                    params:  json!({
                        "progressToken": token,
                        "progress":      done,
                        "total":         total,
                    }),
                };
                send_notification(&notif_tx, &notif);
            }
        });
        Some(ptx)
    } else {
        None
    };

    let output  = tools::dispatch(clients, &name, &args, prog_tx).await;
    let is_err  = output.text.starts_with("Error:");

    RpcResponse::ok(id, json!({
        "content": [{ "type": "text", "text": output.text }],
        "isError": is_err,
    }))
}

// ─── resources/list ──────────────────────────────────────────────────────────

fn handle_resources_list(id: Value) -> RpcResponse {
    let resources = vec![
        json!({
            "uri":         "market://polymarket/markets",
            "name":        "Polymarket — top markets",
            "description": "Live top-20 Polymarket prediction markets with YES prices, volume, and liquidity.",
            "mimeType":    "text/plain"
        }),
        json!({
            "uri":         "market://kalshi/markets",
            "name":        "Kalshi — top markets",
            "description": "Live top-20 Kalshi prediction markets.",
            "mimeType":    "text/plain"
        }),
        json!({
            "uri":         "portfolio://positions",
            "name":        "Portfolio — open positions",
            "description": "Your locally tracked open positions with entry prices and P&L.",
            "mimeType":    "text/plain"
        }),
        json!({
            "uri":         "portfolio://watchlist",
            "name":        "Portfolio — watchlist",
            "description": "Markets you are watching with price-alert thresholds.",
            "mimeType":    "text/plain"
        }),
        json!({
            "uri":         "portfolio://risk",
            "name":        "Portfolio — risk summary",
            "description": "VaR/CVaR, stress tests, and expected P&L for your open positions.",
            "mimeType":    "text/plain"
        }),
        json!({
            "uri":         "signals://latest",
            "name":        "Trading signals",
            "description": "Current ARB/INSDR/MOMT/THIN/50-50 signals across both platforms.",
            "mimeType":    "text/plain"
        }),
    ];
    RpcResponse::ok(id, json!({ "resources": resources }))
}

// ─── resources/read ──────────────────────────────────────────────────────────

async fn handle_resources_read(
    id:      Value,
    params:  &Value,
    clients: &Arc<MarketClients>,
) -> RpcResponse {
    let uri = match params.get("uri").and_then(|v| v.as_str()) {
        Some(u) => u.to_string(),
        None    => return RpcResponse::err(id, -32602, "Missing required parameter: uri"),
    };

    let text = match uri.as_str() {
        "market://polymarket/markets" => {
            let args = json!({"platform": "polymarket", "limit": 20});
            tools::dispatch(clients, "list_markets", &args, None).await.text
        }
        "market://kalshi/markets" => {
            let args = json!({"platform": "kalshi", "limit": 20});
            tools::dispatch(clients, "list_markets", &args, None).await.text
        }
        "portfolio://positions" => {
            tools::dispatch(clients, "get_portfolio", &json!({}), None).await.text
        }
        "portfolio://watchlist" => {
            tools::dispatch(clients, "get_watchlist", &json!({}), None).await.text
        }
        "portfolio://risk" => {
            tools::dispatch(clients, "get_portfolio_risk", &json!({}), None).await.text
        }
        "signals://latest" => {
            tools::dispatch(clients, "get_signals", &json!({}), None).await.text
        }
        _ => return RpcResponse::err(id, -32602, format!("Unknown resource URI: {uri}")),
    };

    RpcResponse::ok(id, json!({
        "contents": [{
            "uri":      uri,
            "mimeType": "text/plain",
            "text":     text,
        }]
    }))
}

// ─── prompts/list ─────────────────────────────────────────────────────────────

fn handle_prompts_list(id: Value) -> RpcResponse {
    let prompts = vec![
        json!({
            "name":        "full-market-analysis",
            "description": "Complete 6-step analysis workflow for a single market: \
                fundamentals → market signal → price action → microstructure → smart-money flow → Kelly sizing.",
            "arguments": [
                {"name": "market_id",  "description": "Market ID (conditionId for PM, ticker for KL).", "required": true},
                {"name": "platform",   "description": "polymarket or kalshi", "required": true},
                {"name": "your_prob",  "description": "Your estimated YES probability (0-1).", "required": false},
                {"name": "bankroll",   "description": "Bankroll for Kelly sizing in dollars.", "required": false}
            ]
        }),
        json!({
            "name":        "morning-scan",
            "description": "Daily session starter: fetch signals, scan smart money, check macro, \
                review portfolio risk. No arguments required.",
            "arguments": []
        }),
        json!({
            "name":        "arb-hunt",
            "description": "Find arbitrage opportunities: list pairs by Jaccard similarity, \
                test cointegration on the top match, size with correlated Kelly.",
            "arguments": [
                {"name": "min_gap_pct", "description": "Minimum price gap % to consider (default 2).", "required": false}
            ]
        }),
        json!({
            "name":        "wallet-deep-dive",
            "description": "Investigate a specific Polymarket wallet: full profile, open positions, \
                risk-adjusted returns, and comparison to market averages.",
            "arguments": [
                {"name": "wallet", "description": "Polymarket proxy wallet address (0x…).", "required": true}
            ]
        }),
    ];
    RpcResponse::ok(id, json!({ "prompts": prompts }))
}

// ─── prompts/get ──────────────────────────────────────────────────────────────

fn handle_prompts_get(id: Value, params: &Value) -> RpcResponse {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None    => return RpcResponse::err(id, -32602, "Missing required parameter: name"),
    };
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let (description, text) = match name {
        "full-market-analysis" => {
            let market_id = args["market_id"].as_str().unwrap_or("<market_id>");
            let platform  = args["platform"].as_str().unwrap_or("polymarket");
            let your_prob = args["your_prob"].as_f64()
                .map(|p| format!("My estimated probability is {:.0}%.", p * 100.0))
                .unwrap_or_default();
            let bankroll  = args["bankroll"].as_f64()
                .map(|b| format!("Bankroll for sizing: ${:.0}.", b))
                .unwrap_or_default();
            (
                "Full 6-step market analysis workflow.",
                format!(
                    "Please run a complete analysis of market `{market_id}` on {platform}. {your_prob} {bankroll}\n\n\
                     Follow this exact workflow:\n\
                     1. **get_market** — fetch full market details and description.\n\
                     2. **get_market_news** — fetch relevant news; use headlines to calibrate your base-rate.\n\
                     3. **get_price_history** — retrieve 30-day price history; note trend and volatility.\n\
                     4. **binary_greeks** — compute delta/theta/vega given days to resolution.\n\
                     5. **get_orderbook** — analyse bid/ask depth and order imbalance.\n\
                     6. **market_microstructure** — compute Roll's spread, Amihud, Kyle's lambda.\n\
                     7. **find_smart_money** — check whether informed wallets are accumulating.\n\
                     8. **kelly_size** (or **kelly_correlated** if you have related positions) — size the trade.\n\n\
                     Synthesise all findings into a structured report: verdict (YES/NO/PASS), \
                     probability estimate vs market price, edge in cents, and recommended position size.",
                )
            )
        }

        "morning-scan" => (
            "Daily session starter — signals, smart money, macro, portfolio.",
            "Good morning. Please run my daily market intelligence scan in this order:\n\n\
             1. **get_signals** — surface today's ARB, INSDR, MOMT, and THIN alerts. \
                Flag any 3-star signals for immediate follow-up.\n\
             2. **scan_smart_money** (limit: 20) — identify markets with suspicious wallet activity. \
                Note the top 3 wallets with suspicion score ≥ 50.\n\
             3. **get_macro** — pull the FRED snapshot. Flag any macro data that changes \
                my prior on interest-rate or inflation markets.\n\
             4. **get_portfolio** then **get_portfolio_risk** — summarise my open exposure, \
                unrealised P&L, CVaR(95), and any positions near stop-loss.\n\n\
             End with a prioritised action list: which markets deserve analysis today and why."
            .to_string(),
        ),

        "arb-hunt" => {
            let min_gap = args["min_gap_pct"].as_f64().unwrap_or(2.0);
            (
                "Find and size cross-platform arbitrage opportunities.",
                format!(
                    "Please hunt for arbitrage opportunities with a minimum price gap of {min_gap:.0}%.\n\n\
                     Workflow:\n\
                     1. **list_markets** (platform: polymarket, limit: 30) and **list_markets** \
                        (platform: kalshi, limit: 30) — get both market snapshots.\n\
                     2. Identify pairs where |PM_price − KL_price| ≥ {min_gap:.1}%. \
                        List them ranked by gap descending.\n\
                     3. For each candidate pair: call **get_market** on both legs to verify \
                        resolution criteria match.\n\
                     4. For the top pair: call **test_cointegration** to check if spread \
                        mean-reverts (DF stat, half-life).\n\
                     5. If cointegrated: call **get_orderbook** on both legs to assess \
                        executable depth and net gap after fees.\n\
                     6. Size with **kelly_correlated** (set correlation = 1.0 for true arb legs).\n\n\
                     Report: net gap after 2% taker fees, max capturable $ at current depth, \
                     recommended hedge ratio, and half-life estimate.",
                )
            )
        }

        "wallet-deep-dive" => {
            let wallet = args["wallet"].as_str().unwrap_or("<wallet_address>");
            (
                "Deep investigation of a single Polymarket wallet.",
                format!(
                    "Please run a complete deep-dive on Polymarket wallet `{wallet}`.\n\n\
                     Workflow:\n\
                     1. **analyze_wallet** (wallet: {wallet}) — full suspicion profile: \
                        win rate, recency-weighted win rate, alpha-entry score, timing-to-resolution, \
                        suspicion breakdown.\n\
                     2. **get_wallet_positions** (wallet: {wallet}) — current open positions: \
                        sides, sizes, average entry prices.\n\
                     3. For each open position, call **get_market** to get the current YES price \
                        and compare to the wallet's entry price — assess unrealised edge.\n\
                     4. **get_signals** — check if any of the wallet's markets are currently \
                        surfacing INSDR or ARB signals.\n\n\
                     Synthesise: Is this wallet genuinely informed or lucky? \
                     Which of their current positions should I consider mirroring, and at what size? \
                     Use **kelly_size** to suggest a position size for the most compelling one.",
                )
            )
        }

        _ => return RpcResponse::err(id, -32602, format!("Unknown prompt: {name}")),
    };

    RpcResponse::ok(id, json!({
        "description": description,
        "messages": [{
            "role":    "user",
            "content": { "type": "text", "text": text }
        }]
    }))
}
