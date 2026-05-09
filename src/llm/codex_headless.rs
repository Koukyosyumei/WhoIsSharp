//! Codex CLI (headless) backend.
//!
//! Spawns `codex exec --json` as a subprocess, wired to our own binary running
//! in `--mcp` mode so the model can call all WhoIsSharp tools via MCP.  Uses
//! the user's existing `codex login` — no API key required.
//!
//! Codex configures MCP servers via `~/.codex/config.toml`, but `codex` also
//! accepts ad-hoc overrides through repeated `-c key=value` flags, which is
//! what we use so the user doesn't have to edit any config.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

use super::{LlmBackend, LlmMessage, MessageContent, MessageRole, ToolDefinition};

pub struct CodexHeadlessBackend {
    model_id:    Option<String>,
    binary_path: PathBuf,
}

impl CodexHeadlessBackend {
    pub fn new(model_id: Option<String>) -> Result<Self> {
        let binary_path = std::env::current_exe()
            .context("Failed to resolve current executable path for MCP server")?;
        Ok(Self { model_id, binary_path })
    }
}

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

fn latest_user_text(history: &[LlmMessage]) -> Result<String> {
    history
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User && !m.texts().is_empty())
        .map(|m| m.texts().join("\n"))
        .ok_or_else(|| anyhow!("No user message found in history"))
}

#[async_trait]
impl LlmBackend for CodexHeadlessBackend {
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
        _tools:   &[ToolDefinition],
        chunk_tx: &UnboundedSender<String>,
    ) -> Result<LlmMessage> {
        let user_msg    = latest_user_text(history)?;
        let transcript  = format_transcript(history);
        let full_prompt = format!(
            "{system}{transcript}\n\n=== CURRENT REQUEST ===\nUSER: {user_msg}",
        );

        let bin_str = self.binary_path.to_string_lossy().replace('"', "\\\"");

        let mut cmd = Command::new("codex");
        cmd.arg("exec")
            .arg("--json")
            .arg("--skip-git-repo-check")
            .arg("--ephemeral")
            .arg("--dangerously-bypass-approvals-and-sandbox")
            // Wire whoissharp as the only MCP server for this run.
            .arg("-c").arg(format!("mcp_servers.whoissharp.command=\"{bin_str}\""))
            .arg("-c").arg("mcp_servers.whoissharp.args=[\"--mcp\"]");

        if let Some(model) = &self.model_id {
            cmd.arg("--model").arg(model);
        }

        cmd.arg(&full_prompt);

        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let mut child = cmd.spawn().with_context(|| {
            "Failed to spawn `codex` — is the Codex CLI installed and on PATH? \
             (npm i -g @openai/codex)"
                .to_string()
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("codex subprocess produced no stdout"))?;
        let stderr = child.stderr.take();

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
        let mut streamed_text = String::new();
        let mut final_message: Option<String> = None;

        while let Ok(Some(line)) = reader.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed) else { continue; };

            // Codex JSONL events live under the `msg` key (with a `type`
            // discriminator); the exact schema has shifted between releases,
            // so we look for any of the known shapes.
            let msg = val.get("msg").unwrap_or(&val);
            let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match msg_type {
                // Streaming text deltas while the model is generating.
                "agent_message_delta" | "message_delta" => {
                    if let Some(delta) = msg
                        .get("delta")
                        .and_then(|v| v.as_str())
                        .or_else(|| msg.pointer("/delta/text").and_then(|v| v.as_str()))
                    {
                        let _ = chunk_tx.send(delta.to_string());
                        streamed_text.push_str(delta);
                    }
                }

                // Whole assistant message (sent when the run completes, and
                // sometimes mid-stream by older Codex versions).
                "agent_message" | "message" => {
                    if let Some(text) = msg
                        .get("message")
                        .and_then(|v| v.as_str())
                        .or_else(|| msg.get("content").and_then(|v| v.as_str()))
                        .or_else(|| msg.get("text").and_then(|v| v.as_str()))
                    {
                        final_message = Some(text.to_string());
                    }
                }

                // Tool-call traces — surface them inline so the user sees
                // progress instead of a long silent gap.
                "mcp_tool_call_begin" | "tool_call" | "exec_command_begin" => {
                    let tool = msg
                        .pointer("/invocation/tool")
                        .and_then(|v| v.as_str())
                        .or_else(|| msg.get("name").and_then(|v| v.as_str()))
                        .or_else(|| msg.get("tool").and_then(|v| v.as_str()))
                        .unwrap_or("tool");
                    let trace = format!("\n→ {tool}\n");
                    let _ = chunk_tx.send(trace.clone());
                    streamed_text.push_str(&trace);
                }

                "task_complete" | "turn_complete" => {
                    if final_message.is_none() {
                        if let Some(text) = msg
                            .get("last_agent_message")
                            .and_then(|v| v.as_str())
                            .or_else(|| msg.get("message").and_then(|v| v.as_str()))
                        {
                            final_message = Some(text.to_string());
                        }
                    }
                }

                "error" => {
                    let err = msg
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    return Err(anyhow!("codex error: {err}"));
                }

                _ => {}
            }
        }

        let status = child.wait().await.context("Failed to wait on codex subprocess")?;

        let final_text = final_message.unwrap_or_else(|| streamed_text.clone());

        if !status.success() && final_text.is_empty() {
            let stderr_text = match stderr_handle {
                Some(h) => h.await.unwrap_or_default(),
                None    => String::new(),
            };
            return Err(anyhow!(
                "codex exited with status {}: {}",
                status,
                stderr_text.trim()
            ));
        }

        // If we never streamed but did capture a final message, emit it now
        // so the TUI's chat tab still gets the text.
        if streamed_text.is_empty() && !final_text.is_empty() {
            let _ = chunk_tx.send(final_text.clone());
        }

        Ok(LlmMessage {
            role:    MessageRole::Assistant,
            content: vec![MessageContent::Text(final_text)],
        })
    }

    fn display_name(&self) -> String {
        match &self.model_id {
            Some(m) => format!("{m} (Codex CLI headless)"),
            None    => "Codex CLI (headless)".to_string(),
        }
    }
}
