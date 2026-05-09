//! Claude Code (headless) backend.
//!
//! Spawns `claude -p` as a subprocess, wired to our own binary running in
//! `--mcp` mode so the model can call all WhoIsSharp tools (list_markets,
//! get_orderbook, …) via MCP.  Uses the user's existing Claude Code login —
//! no API key required.
//!
//! Each invocation:
//!   1. Serialise the conversation history into a transcript that's appended
//!      to the system prompt.
//!   2. Pass the latest user message as the prompt argument.
//!   3. Stream `--output-format stream-json` events: text deltas → chunk_tx,
//!      tool-use / tool-result blocks → inline traces in the streamed text.
//!   4. Return a final assistant message containing the full text (no tool
//!      calls — Claude already executed them internally).

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

use super::{LlmBackend, LlmMessage, MessageContent, MessageRole, ToolDefinition};

const MCP_TOOL_PREFIX: &str = "mcp__whoissharp__";

pub struct ClaudeHeadlessBackend {
    /// Optional model alias (e.g. "sonnet", "opus", or full model id).
    model_id:    Option<String>,
    /// Path to the running whoissharp binary — used as the MCP server command.
    binary_path: PathBuf,
}

impl ClaudeHeadlessBackend {
    pub fn new(model_id: Option<String>) -> Result<Self> {
        let binary_path = std::env::current_exe()
            .context("Failed to resolve current executable path for MCP server")?;
        Ok(Self { model_id, binary_path })
    }

    fn mcp_config_json(&self) -> String {
        json!({
            "mcpServers": {
                "whoissharp": {
                    "command": self.binary_path.to_string_lossy(),
                    "args":    ["--mcp"],
                }
            }
        })
        .to_string()
    }
}

/// Render history (excluding the latest user message) as a transcript that
/// can be appended to the system prompt.  Tool-call / tool-result entries
/// are summarised — Claude managed its own tools last turn, so the raw
/// arguments aren't useful, but the text exchange matters for continuity.
fn format_transcript(history: &[LlmMessage]) -> String {
    if history.len() <= 1 {
        return String::new();
    }
    let mut out = String::from(
        "\n\n=== PRIOR CONVERSATION ===\n\
         The following is the conversation so far. The latest user message \
         is delivered separately as the prompt.\n",
    );
    let last_idx = history.len() - 1;
    for (i, msg) in history.iter().enumerate() {
        if i == last_idx {
            break;
        }
        let texts = msg.texts();
        if texts.is_empty() {
            continue;
        }
        let role = match msg.role {
            MessageRole::User      => "USER",
            MessageRole::Assistant => "ASSISTANT",
        };
        out.push_str(&format!("\n{}: {}\n", role, texts.join("\n")));
    }
    out
}

/// Extract the most recent user-text message — that's the prompt argument.
fn latest_user_text(history: &[LlmMessage]) -> Result<String> {
    history
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User && !m.texts().is_empty())
        .map(|m| m.texts().join("\n"))
        .ok_or_else(|| anyhow!("No user message found in history"))
}

/// Build the `--allowedTools` list: every `mcp__whoissharp__<tool>` from
/// the WhoIsSharp tool catalogue.  Restricting to MCP tools keeps Claude
/// from reaching for Bash / Edit / Write inside the trading workflow.
fn allowed_tools(tools: &[ToolDefinition]) -> Vec<String> {
    tools
        .iter()
        .map(|t| format!("{}{}", MCP_TOOL_PREFIX, t.name))
        .collect()
}

pub struct CliBackendConfig {
    pub binary:        &'static str,
    pub display_label: &'static str,
}

pub const CLAUDE_CLI: CliBackendConfig = CliBackendConfig {
    binary:        "claude",
    display_label: "Claude Code (headless)",
};

#[async_trait]
impl LlmBackend for ClaudeHeadlessBackend {
    async fn generate(
        &self,
        system:  &str,
        history: &[LlmMessage],
        tools:   &[ToolDefinition],
    ) -> Result<LlmMessage> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        self.generate_streaming(system, history, tools, &tx).await
    }

    async fn generate_streaming(
        &self,
        system:   &str,
        history:  &[LlmMessage],
        tools:    &[ToolDefinition],
        chunk_tx: &UnboundedSender<String>,
    ) -> Result<LlmMessage> {
        let user_msg     = latest_user_text(history)?;
        let transcript   = format_transcript(history);
        let full_system  = format!("{system}{transcript}");
        let mcp_config   = self.mcp_config_json();
        let allowed      = allowed_tools(tools);

        let mut cmd = Command::new(CLAUDE_CLI.binary);
        cmd.arg("-p").arg(&user_msg)
            .arg("--output-format").arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--verbose")
            .arg("--strict-mcp-config")
            .arg("--mcp-config").arg(&mcp_config)
            .arg("--system-prompt").arg(&full_system)
            .arg("--permission-mode").arg("bypassPermissions")
            .arg("--no-session-persistence");

        if !allowed.is_empty() {
            cmd.arg("--allowedTools").args(&allowed);
        }
        if let Some(model) = &self.model_id {
            cmd.arg("--model").arg(model);
        }

        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "Failed to spawn `{}` — is Claude Code installed and on PATH? \
                 (https://claude.com/claude-code)",
                CLAUDE_CLI.binary
            )
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("claude subprocess produced no stdout"))?;
        let stderr = child.stderr.take();

        // Drain stderr into a buffer so it's available if the process fails.
        let stderr_handle = stderr.map(|s| {
            tokio::spawn(async move {
                let mut buf = String::new();
                let mut reader = BufReader::new(s);
                use tokio::io::AsyncReadExt;
                let _ = reader.read_to_string(&mut buf).await;
                buf
            })
        });

        let mut reader = BufReader::new(stdout).lines();
        let mut final_text = String::new();
        let mut saw_tool_use = false;

        while let Ok(Some(line)) = reader.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed) else { continue; };

            match val.get("type").and_then(|v| v.as_str()) {
                // Streaming partial text deltas (Anthropic SDK passthrough).
                Some("stream_event") => {
                    let event = val.get("event").unwrap_or(&Value::Null);
                    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if event_type == "content_block_delta" {
                        if let Some(delta_text) = event
                            .pointer("/delta/text")
                            .and_then(|v| v.as_str())
                        {
                            let _ = chunk_tx.send(delta_text.to_string());
                            final_text.push_str(delta_text);
                        }
                    } else if event_type == "content_block_start" {
                        // Surface tool-use traces inline so the chat shows
                        // what Claude is doing, not just a long silent gap.
                        if let Some(block) = event.get("content_block") {
                            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                                let raw = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                let tool = raw.strip_prefix(MCP_TOOL_PREFIX).unwrap_or(raw);
                                let trace = format!("\n→ {tool}\n");
                                let _ = chunk_tx.send(trace.clone());
                                final_text.push_str(&trace);
                                saw_tool_use = true;
                            }
                        }
                    }
                }

                // Final result event — fall back to its `result` field if we
                // somehow missed all the partial deltas.
                Some("result") => {
                    if final_text.is_empty() {
                        if let Some(text) = val.get("result").and_then(|v| v.as_str()) {
                            let _ = chunk_tx.send(text.to_string());
                            final_text = text.to_string();
                        }
                    }
                    if let Some(err) = val.get("subtype").and_then(|v| v.as_str()) {
                        if err != "success" && final_text.is_empty() {
                            return Err(anyhow!("Claude headless returned: {}", err));
                        }
                    }
                    break;
                }

                _ => {}
            }
        }

        let status = child.wait().await.context("Failed to wait on claude subprocess")?;
        if !status.success() && final_text.is_empty() {
            let stderr_text = match stderr_handle {
                Some(h) => h.await.unwrap_or_default(),
                None    => String::new(),
            };
            return Err(anyhow!(
                "claude exited with status {}: {}",
                status,
                stderr_text.trim()
            ));
        }

        let _ = saw_tool_use; // reserved for future telemetry

        Ok(LlmMessage {
            role:    MessageRole::Assistant,
            content: vec![MessageContent::Text(final_text)],
        })
    }

    fn display_name(&self) -> String {
        match &self.model_id {
            Some(m) => format!("{m} ({})", CLAUDE_CLI.display_label),
            None    => CLAUDE_CLI.display_label.to_string(),
        }
    }
}
