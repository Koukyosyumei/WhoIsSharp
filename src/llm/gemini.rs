//! Gemini backend via Vertex AI.
//!
//! Authentication: service-account JSON → RS256 JWT → OAuth2 access token.
//! Token is cached in-process and refreshed when < 60 s remain.
//!
//! Env vars:
//!   GOOGLE_APPLICATION_CREDENTIALS — path to service-account key JSON
//!   GOOGLE_PROJECT_ID              — GCP project ID
//!   GOOGLE_LOCATION                — Vertex AI region (default: us-central1)

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::{LlmBackend, LlmMessage, MessageContent, MessageRole, ToolCall, ToolDefinition};

// ─── Service-account ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ServiceAccount {
    client_email: String,
    private_key:  String,
    token_uri:    String,
}

#[derive(Serialize)]
struct JwtClaims {
    iss:   String,
    scope: String,
    aud:   String,
    exp:   i64,
    iat:   i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in:   i64,
}

struct CachedToken {
    access_token: String,
    expires_at:   i64,
}

// ─── Wire types ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
struct GContent {
    role:  String,
    parts: Vec<GPart>,
}

/// `#[serde(untagged)]`: most-specific variant first.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
enum GPart {
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GFunctionCall,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GFunctionResponse,
    },
    Text {
        text: String,
    },
    Unknown(serde_json::Value),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct GFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct GFunctionResponse {
    name:     String,
    response: serde_json::Value,
}

#[derive(Serialize)]
struct GRequest {
    contents:           Vec<GContent>,
    tools:              Vec<GToolSpec>,
    #[serde(rename = "systemInstruction")]
    system_instruction: GSystemInstruction,
    #[serde(rename = "toolConfig")]
    tool_config:        GToolConfig,
    #[serde(rename = "generationConfig")]
    generation_config:  GGenerationConfig,
}

#[derive(Serialize)]
struct GSystemInstruction {
    parts: Vec<GTextPart>,
}

#[derive(Serialize)]
struct GTextPart {
    text: String,
}

#[derive(Serialize)]
struct GToolSpec {
    #[serde(rename = "functionDeclarations")]
    function_declarations: Vec<GFunctionDecl>,
}

#[derive(Serialize)]
struct GFunctionDecl {
    name:        String,
    description: String,
    parameters:  serde_json::Value,
}

#[derive(Serialize)]
struct GToolConfig {
    #[serde(rename = "functionCallingConfig")]
    function_calling_config: GFunctionCallingConfig,
}

#[derive(Serialize)]
struct GFunctionCallingConfig {
    mode: &'static str,
}

#[derive(Serialize)]
struct GGenerationConfig {
    temperature: f32,
}

#[derive(Deserialize, Debug)]
struct GCandidate {
    #[serde(default)]
    content: Option<GContent>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct GPromptFeedback {
    #[serde(rename = "blockReason")]
    block_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct GResponse {
    #[serde(default)]
    candidates:     Vec<GCandidate>,
    #[serde(rename = "promptFeedback")]
    prompt_feedback: Option<GPromptFeedback>,
}

// ─── Schema conversion ────────────────────────────────────────────────────────

/// Gemini requires uppercase JSON Schema type strings ("STRING", "OBJECT", …).
fn uppercase_types(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let new_map: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, val)| {
                    let new_val = if k == "type" {
                        val.as_str()
                            .map(|s| serde_json::Value::String(s.to_uppercase()))
                            .unwrap_or_else(|| uppercase_types(val))
                    } else {
                        uppercase_types(val)
                    };
                    (k.clone(), new_val)
                })
                .collect();
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(uppercase_types).collect())
        }
        other => other.clone(),
    }
}

// ─── Message translation ──────────────────────────────────────────────────────

fn to_gemini_contents(history: &[LlmMessage]) -> Vec<GContent> {
    history
        .iter()
        .map(|msg| {
            let role = match msg.role {
                MessageRole::User      => "user",
                MessageRole::Assistant => "model",
            }
            .to_string();
            let parts = msg
                .content
                .iter()
                .filter_map(|c| match c {
                    MessageContent::Text(t) => Some(GPart::Text { text: t.clone() }),
                    MessageContent::ToolCall(tc) => Some(GPart::FunctionCall {
                        function_call: GFunctionCall {
                            name: tc.name.clone(),
                            args: tc.args.clone(),
                        },
                    }),
                    MessageContent::ToolResult(tr) => Some(GPart::FunctionResponse {
                        function_response: GFunctionResponse {
                            name:     tr.name.clone(),
                            response: serde_json::json!({ "output": tr.content }),
                        },
                    }),
                })
                .collect();
            GContent { role, parts }
        })
        .collect()
}

fn from_gemini_candidate(candidate: GCandidate) -> Result<LlmMessage> {
    if let Some(ref reason) = candidate.finish_reason {
        match reason.as_str() {
            "STOP" | "MAX_TOKENS" => {}
            "SAFETY" => anyhow::bail!(
                "Response blocked by Gemini safety filters (finishReason: SAFETY). \
                 Try rephrasing your request."
            ),
            other => anyhow::bail!(
                "Gemini stopped with unexpected finishReason: {}. Try again.", other
            ),
        }
    }

    let content: Vec<MessageContent> = candidate
        .content
        .map(|c| c.parts)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .filter_map(|(i, part)| match part {
            GPart::Text { text } if !text.is_empty() => Some(MessageContent::Text(text)),
            GPart::FunctionCall { function_call } => {
                Some(MessageContent::ToolCall(ToolCall {
                    id:   format!("{}-{}", function_call.name, i),
                    name: function_call.name,
                    args: function_call.args,
                }))
            }
            _ => None,
        })
        .collect();

    Ok(LlmMessage { role: MessageRole::Assistant, content })
}

// ─── Backend ──────────────────────────────────────────────────────────────────

pub struct GeminiBackend {
    http:        reqwest::Client,
    sa:          ServiceAccount,
    project_id:  String,
    location:    String,
    model_id:    String,
    token_cache: Arc<Mutex<Option<CachedToken>>>,
}

impl GeminiBackend {
    pub fn new(
        credentials_path: &Path,
        project_id: impl Into<String>,
        location:   impl Into<String>,
        model_id:   impl Into<String>,
    ) -> Result<Self> {
        let raw = std::fs::read_to_string(credentials_path)
            .with_context(|| format!("Cannot read '{}'", credentials_path.display()))?;
        let sa: ServiceAccount =
            serde_json::from_str(&raw).context("Cannot parse service-account JSON")?;
        Ok(GeminiBackend {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            sa,
            project_id:  project_id.into(),
            location:    location.into(),
            model_id:    model_id.into(),
            token_cache: Arc::new(Mutex::new(None)),
        })
    }

    async fn access_token(&self) -> Result<String> {
        let mut cache = self.token_cache.lock().await;
        let now = Utc::now().timestamp();
        if let Some(tok) = cache.as_ref() {
            if tok.expires_at > now + 60 {
                return Ok(tok.access_token.clone());
            }
        }
        let claims = JwtClaims {
            iss:   self.sa.client_email.clone(),
            scope: "https://www.googleapis.com/auth/cloud-platform".to_string(),
            aud:   self.sa.token_uri.clone(),
            exp:   now + 3600,
            iat:   now,
        };
        let key = EncodingKey::from_rsa_pem(self.sa.private_key.as_bytes())
            .context("Cannot parse private key from service-account JSON")?;
        let jwt = encode(&Header::new(Algorithm::RS256), &claims, &key)
            .context("JWT signing failed")?;
        let resp: TokenResponse = self
            .http
            .post(&self.sa.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion",  &jwt),
            ])
            .send()
            .await
            .context("Token exchange request failed")?
            .json()
            .await
            .context("Token exchange: unexpected response")?;
        let token = resp.access_token.clone();
        *cache = Some(CachedToken {
            access_token: resp.access_token,
            expires_at:   now + resp.expires_in,
        });
        Ok(token)
    }
}

#[async_trait]
impl LlmBackend for GeminiBackend {
    async fn generate(
        &self,
        system:  &str,
        history: &[LlmMessage],
        tools:   &[ToolDefinition],
    ) -> Result<LlmMessage> {
        let token = self.access_token().await?;
        let url = format!(
            "https://{loc}-aiplatform.googleapis.com/v1/projects/{proj}/locations/{loc}\
             /publishers/google/models/{model}:generateContent",
            loc   = self.location,
            proj  = self.project_id,
            model = self.model_id,
        );

        let function_declarations: Vec<GFunctionDecl> = tools
            .iter()
            .map(|t| GFunctionDecl {
                name:        t.name.clone(),
                description: t.description.clone(),
                parameters:  uppercase_types(&t.parameters),
            })
            .collect();

        let req = GRequest {
            contents: to_gemini_contents(history),
            tools: vec![GToolSpec { function_declarations }],
            system_instruction: GSystemInstruction {
                parts: vec![GTextPart { text: system.to_string() }],
            },
            tool_config: GToolConfig {
                function_calling_config: GFunctionCallingConfig { mode: "AUTO" },
            },
            generation_config: GGenerationConfig { temperature: 0.1 },
        };

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&req)
            .timeout(std::time::Duration::from_secs(crate::llm::get_timeout_secs()))
            .send()
            .await
            .context("Gemini API request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API error {}: {}", status, body);
        }

        let gen_resp: GResponse =
            resp.json().await.context("Failed to parse Gemini response")?;

        if gen_resp.candidates.is_empty() {
            let reason = gen_resp
                .prompt_feedback
                .as_ref()
                .and_then(|pf| pf.block_reason.as_deref())
                .unwrap_or("unknown");
            anyhow::bail!(
                "Request blocked by Gemini (blockReason: {}). Try rephrasing.", reason
            );
        }

        let candidate = gen_resp.candidates.into_iter().next().unwrap();
        from_gemini_candidate(candidate)
    }

    fn display_name(&self) -> String {
        format!("{} (Vertex AI · {})", self.model_id, self.location)
    }
}
