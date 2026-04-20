//! MCP (Model Context Protocol) server — JSON-RPC 2.0 over stdio.
//!
//! Run with:  whoissharp --mcp
//! Then point Claude Desktop (or any MCP client) at the binary.
//!
//! Implements:
//!   initialize · tools/list · tools/call · ping
//!   notifications/initialized (silently ignored — no response)

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
    /// Absent on notifications.
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

impl RpcResponse {
    fn ok(id: Value, result: Value) -> Self {
        RpcResponse { jsonrpc: "2.0", id, result: Some(result), error: None }
    }
    fn err(id: Value, code: i32, msg: impl Into<String>) -> Self {
        RpcResponse { jsonrpc: "2.0", id, result: None, error: Some(RpcError { code, message: msg.into() }) }
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub async fn run_server(clients: Arc<MarketClients>) -> Result<()> {
    let stdin  = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let mut reader = BufReader::new(stdin).lines();
    // Serialise all writes through a channel so concurrent tasks don't interleave.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // Writer task
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
                let _ = tx.send(serde_json::to_string(&resp).unwrap_or_default());
                continue;
            }
        };

        // Notifications (no id) — silently ignore.
        if req.id.is_none() {
            continue;
        }

        let id      = req.id.clone().unwrap();
        let method  = req.method.clone();
        let params  = req.params.clone();
        let clients = Arc::clone(&clients);
        let tx2     = tx.clone();

        tokio::spawn(async move {
            let resp = handle(&method, &params, &clients, id).await;
            let _ = tx2.send(serde_json::to_string(&resp).unwrap_or_default());
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
) -> RpcResponse {
    match method {
        "initialize"             => handle_initialize(id, params),
        "tools/list"             => handle_tools_list(id),
        "tools/call"             => handle_tools_call(id, params, clients).await,
        "ping"                   => RpcResponse::ok(id, json!({})),
        "notifications/initialized" => RpcResponse::ok(id, json!({})), // shouldn't have id, but be safe
        _ => RpcResponse::err(id, -32601, format!("Method not found: {method}")),
    }
}

// ─── initialize ──────────────────────────────────────────────────────────────

fn handle_initialize(id: Value, _params: &Value) -> RpcResponse {
    RpcResponse::ok(id, json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
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

async fn handle_tools_call(id: Value, params: &Value, clients: &Arc<MarketClients>) -> RpcResponse {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None    => return RpcResponse::err(id, -32602, "Missing required parameter: name"),
    };

    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let output = tools::dispatch(clients, &name, &args).await;
    let is_err = output.text.starts_with("Error:");

    RpcResponse::ok(id, json!({
        "content": [{ "type": "text", "text": output.text }],
        "isError": is_err,
    }))
}
