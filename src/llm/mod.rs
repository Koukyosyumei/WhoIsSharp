//! LLM backend abstraction.
//!
//! All backends translate between the universal types defined here and their
//! own wire formats.  `agent.rs` only depends on this module.

pub mod anthropic;
pub mod gemini;
pub mod openai;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

// ─── Global request timeout ──────────────────────────────────────────────────

static REQUEST_TIMEOUT_SECS: AtomicU64 = AtomicU64::new(120);

pub fn get_timeout_secs() -> u64 {
    REQUEST_TIMEOUT_SECS.load(Ordering::Relaxed)
}

pub fn set_timeout_secs(secs: u64) {
    REQUEST_TIMEOUT_SECS.store(secs, Ordering::Relaxed);
}

// ─── Universal message types ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageContent {
    Text(String),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: MessageRole,
    pub content: Vec<MessageContent>,
}

impl LlmMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        LlmMessage {
            role: MessageRole::User,
            content: vec![MessageContent::Text(text.into())],
        }
    }

    pub fn tool_results(results: Vec<ToolResult>) -> Self {
        LlmMessage {
            role: MessageRole::User,
            content: results.into_iter().map(MessageContent::ToolResult).collect(),
        }
    }

    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.content
            .iter()
            .filter_map(|c| match c {
                MessageContent::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .collect()
    }

    pub fn texts(&self) -> Vec<&str> {
        self.content
            .iter()
            .filter_map(|c| match c {
                MessageContent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn estimated_chars(&self) -> usize {
        self.content
            .iter()
            .map(|c| match c {
                MessageContent::Text(t) => t.len(),
                MessageContent::ToolCall(tc) => tc.name.len() + tc.args.to_string().len() + 16,
                MessageContent::ToolResult(tr) => tr.content.len() + 16,
            })
            .sum()
    }

    pub fn is_tool_result_message(&self) -> bool {
        !self.content.is_empty()
            && self
                .content
                .iter()
                .all(|c| matches!(c, MessageContent::ToolResult(_)))
    }
}

// ─── Universal tool definition ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema (lowercase types: "object", "string", "integer", "boolean", "array").
    pub parameters: serde_json::Value,
}

// ─── Backend trait ────────────────────────────────────────────────────────────

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn generate(
        &self,
        system: &str,
        history: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> Result<LlmMessage>;

    /// Streaming variant — sends text chunks via `chunk_tx` as they arrive.
    /// Default implementation falls back to non-streaming `generate`.
    async fn generate_streaming(
        &self,
        system: &str,
        history: &[LlmMessage],
        tools: &[ToolDefinition],
        _chunk_tx: &tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Result<LlmMessage> {
        self.generate(system, history, tools).await
    }

    fn display_name(&self) -> String;
}
