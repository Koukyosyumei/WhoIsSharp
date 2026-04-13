//! Anthropic Claude backend.
//!
//! Endpoint : https://api.anthropic.com/v1/messages
//! Auth     : x-api-key header
//! Env var  : ANTHROPIC_API_KEY

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{LlmBackend, LlmMessage, MessageContent, MessageRole, ToolCall, ToolDefinition};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 8192;

fn to_anthropic_messages(history: &[LlmMessage]) -> Vec<serde_json::Value> {
    history
        .iter()
        .map(|msg| {
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
            };
            let content: Vec<serde_json::Value> = msg
                .content
                .iter()
                .map(|c| match c {
                    MessageContent::Text(t) => json!({ "type": "text", "text": t }),
                    MessageContent::ToolCall(tc) => json!({
                        "type": "tool_use",
                        "id":    tc.id,
                        "name":  tc.name,
                        "input": tc.args,
                    }),
                    MessageContent::ToolResult(tr) => json!({
                        "type":        "tool_result",
                        "tool_use_id": tr.call_id,
                        "content":     tr.content,
                    }),
                })
                .collect();
            json!({ "role": role, "content": content })
        })
        .collect()
}

fn to_anthropic_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| json!({
            "name":         t.name,
            "description":  t.description,
            "input_schema": t.parameters,
        }))
        .collect()
}

#[derive(Deserialize, Debug)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
    stop_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String, input: serde_json::Value },
    #[serde(other)]
    Unknown,
}

pub struct AnthropicBackend {
    http:     reqwest::Client,
    api_key:  String,
    model_id: String,
}

impl AnthropicBackend {
    pub fn new(api_key: impl Into<String>, model_id: impl Into<String>) -> Self {
        AnthropicBackend {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            api_key:  api_key.into(),
            model_id: model_id.into(),
        }
    }
}

#[async_trait]
impl LlmBackend for AnthropicBackend {
    async fn generate(
        &self,
        system: &str,
        history: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> Result<LlmMessage> {
        let body = json!({
            "model":    self.model_id,
            "max_tokens": MAX_TOKENS,
            "system":   system,
            "messages": to_anthropic_messages(history),
            "tools":    to_anthropic_tools(tools),
        });

        let resp = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .timeout(std::time::Duration::from_secs(crate::llm::get_timeout_secs()))
            .send()
            .await
            .context("Anthropic API request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error {}: {}", status, body);
        }

        let ar: AnthropicResponse =
            resp.json().await.context("Failed to parse Anthropic response")?;

        if let Some(reason) = &ar.stop_reason {
            match reason.as_str() {
                "tool_use" | "end_turn" | "max_tokens" => {}
                "error" => anyhow::bail!(
                    "Anthropic returned stop_reason: error — request may have been refused."
                ),
                other => anyhow::bail!(
                    "Anthropic stopped with unexpected stop_reason: {}. Try again.", other
                ),
            }
        }

        let content: Vec<MessageContent> = ar
            .content
            .into_iter()
            .filter_map(|block| match block {
                AnthropicContentBlock::Text { text } => Some(MessageContent::Text(text)),
                AnthropicContentBlock::ToolUse { id, name, input } => {
                    Some(MessageContent::ToolCall(ToolCall { id, name, args: input }))
                }
                AnthropicContentBlock::Unknown => None,
            })
            .collect();

        Ok(LlmMessage { role: MessageRole::Assistant, content })
    }

    fn display_name(&self) -> String {
        format!("{} (Anthropic)", self.model_id)
    }
}
