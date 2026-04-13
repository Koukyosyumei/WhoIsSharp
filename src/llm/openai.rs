//! OpenAI-compatible backend.
//!
//! Works with:
//!   - OpenAI API          (https://api.openai.com/v1)
//!   - Ollama              (http://localhost:11434/v1, no key needed)
//!   - Any OpenAI-compatible server (LM Studio, vLLM, etc.)
//!
//! Env vars: OPENAI_API_KEY, OPENAI_BASE_URL (optional)

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{LlmBackend, LlmMessage, MessageContent, MessageRole, ToolCall, ToolDefinition};

fn to_oai_messages(system: &str, history: &[LlmMessage]) -> Vec<serde_json::Value> {
    let mut msgs = vec![json!({ "role": "system", "content": system })];

    for msg in history {
        match msg.role {
            MessageRole::User => {
                let texts = msg.texts();
                if !texts.is_empty() {
                    msgs.push(json!({ "role": "user", "content": texts.join("\n") }));
                }
                for c in &msg.content {
                    if let MessageContent::ToolResult(tr) = c {
                        msgs.push(json!({
                            "role":         "tool",
                            "tool_call_id": tr.call_id,
                            "content":      tr.content,
                        }));
                    }
                }
            }
            MessageRole::Assistant => {
                let tool_calls = msg.tool_calls();
                let texts = msg.texts();
                let content_str: Option<String> =
                    if texts.is_empty() { None } else { Some(texts.join("\n")) };

                if tool_calls.is_empty() {
                    msgs.push(json!({
                        "role":    "assistant",
                        "content": content_str.unwrap_or_default(),
                    }));
                } else {
                    let tc_json: Vec<serde_json::Value> = tool_calls
                        .iter()
                        .map(|tc| json!({
                            "id":   tc.id,
                            "type": "function",
                            "function": {
                                "name":      tc.name,
                                "arguments": tc.args.to_string(),
                            }
                        }))
                        .collect();
                    msgs.push(json!({
                        "role":       "assistant",
                        "content":    content_str,
                        "tool_calls": tc_json,
                    }));
                }
            }
        }
    }
    msgs
}

fn to_oai_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| json!({
            "type": "function",
            "function": {
                "name":        t.name,
                "description": t.description,
                "parameters":  t.parameters,
            }
        }))
        .collect()
}

#[derive(Deserialize, Debug)]
struct OAResponse {
    choices: Vec<OAChoice>,
}

#[derive(Deserialize, Debug)]
struct OAChoice {
    message: OAMessage,
}

#[derive(Deserialize, Debug)]
struct OAMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OAToolCall>,
}

#[derive(Deserialize, Debug)]
struct OAToolCall {
    id:       String,
    function: OAFunctionCall,
}

#[derive(Deserialize, Debug)]
struct OAFunctionCall {
    name:      String,
    arguments: String,
}

pub struct OpenAiBackend {
    http:     reqwest::Client,
    api_key:  String,
    base_url: String,
    model_id: String,
}

impl OpenAiBackend {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        OpenAiBackend {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            api_key:  api_key.into(),
            base_url: base_url.into(),
            model_id: model_id.into(),
        }
    }
}

#[async_trait]
impl LlmBackend for OpenAiBackend {
    async fn generate(
        &self,
        system: &str,
        history: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> Result<LlmMessage> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let body = json!({
            "model":       self.model_id,
            "messages":    to_oai_messages(system, history),
            "tools":       to_oai_tools(tools),
            "tool_choice": "auto",
        });

        let mut req = self
            .http
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(crate::llm::get_timeout_secs()));

        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let resp = req.send().await.context("OpenAI API request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI API error {}: {}", status, body);
        }

        let oar: OAResponse =
            resp.json().await.context("Failed to parse OpenAI response")?;

        let msg = oar
            .choices
            .into_iter()
            .next()
            .context("OpenAI returned no choices")?
            .message;

        let mut content = Vec::new();
        if let Some(text) = msg.content {
            if !text.is_empty() {
                content.push(MessageContent::Text(text));
            }
        }
        for tc in msg.tool_calls {
            let args: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);
            content.push(MessageContent::ToolCall(ToolCall {
                id:   tc.id,
                name: tc.function.name,
                args,
            }));
        }

        Ok(LlmMessage { role: MessageRole::Assistant, content })
    }

    fn display_name(&self) -> String {
        if self.base_url.contains("localhost") || self.base_url.contains("127.0.0.1") {
            format!("{} (Ollama · {})", self.model_id, self.base_url)
        } else {
            format!("{} (OpenAI)", self.model_id)
        }
    }
}
