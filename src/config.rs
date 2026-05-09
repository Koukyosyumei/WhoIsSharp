//! Configuration: backend selection and credential loading from environment.

use anyhow::{Context, Result};
use std::path::PathBuf;

// ─── Backend kind ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum BackendKind {
    /// No LLM — market data dashboard only (default).
    None,
    Anthropic,
    Gemini,
    OpenAi,
    Ollama,
    /// Claude Code CLI in headless mode (`claude -p`). Uses the user's
    /// existing Claude Code login — no API key required.
    ClaudeHeadless,
    /// OpenAI Codex CLI in headless mode (`codex exec`). Uses the user's
    /// existing `codex login` — no API key required.
    CodexHeadless,
}

impl std::str::FromStr for BackendKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "none" | "no" | ""                                    => Ok(BackendKind::None),
            "anthropic" | "claude"                                => Ok(BackendKind::Anthropic),
            "gemini"    | "google"                                => Ok(BackendKind::Gemini),
            "openai"    | "gpt"                                   => Ok(BackendKind::OpenAi),
            "ollama"                                              => Ok(BackendKind::Ollama),
            "claude-code" | "claude-headless" | "claude-cli"      => Ok(BackendKind::ClaudeHeadless),
            "codex" | "codex-headless" | "codex-cli"              => Ok(BackendKind::CodexHeadless),
            _ => anyhow::bail!(
                "Unknown backend '{}'. Choose: none, anthropic, gemini, openai, ollama, \
                 claude-code, codex", s
            ),
        }
    }
}

// ─── Per-backend config ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum BackendConfig {
    /// No LLM backend — TUI runs as a pure market-data dashboard.
    None,
    Anthropic {
        api_key:  String,
        model_id: String,
    },
    /// Vertex AI — service-account JSON → RS256 JWT → OAuth2 access token.
    Gemini {
        credentials_path: PathBuf,
        project_id:       String,
        location:         String,
        model_id:         String,
    },
    OpenAi {
        api_key:  String,
        base_url: String,
        model_id: String,
    },
    Ollama {
        base_url: String,
        model_id: String,
    },
    /// Spawns `claude -p` per turn; no API key, uses Claude Code subscription.
    ClaudeHeadless {
        model_id: Option<String>,
    },
    /// Spawns `codex exec` per turn; no API key, uses Codex login.
    CodexHeadless {
        model_id: Option<String>,
    },
}

impl BackendConfig {
    /// Resolve credentials from CLI overrides first, then environment variables.
    pub fn load(
        kind:                BackendKind,
        model_override:      Option<&str>,
        // Gemini-specific
        credentials_override: Option<PathBuf>,
        project_override:    Option<&str>,
        location_override:   Option<&str>,
        // OpenAI / Anthropic
        api_key_override:    Option<&str>,
        base_url_override:   Option<&str>,
    ) -> Result<Self> {
        match kind {
            BackendKind::None => Ok(BackendConfig::None),

            BackendKind::Anthropic => {
                let api_key = api_key_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                    .context(
                        "Anthropic backend: no API key found.\n\
                         Set ANTHROPIC_API_KEY=sk-ant-...\n\
                         or pass --api-key sk-ant-...",
                    )?;
                let model_id = model_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("WHOISSHARP_MODEL").ok())
                    .unwrap_or_else(|| "claude-sonnet-4-6".to_string());
                Ok(BackendConfig::Anthropic { api_key, model_id })
            }

            BackendKind::Gemini => {
                let credentials_path = credentials_override
                    .or_else(|| {
                        std::env::var("GOOGLE_APPLICATION_CREDENTIALS").ok().map(PathBuf::from)
                    })
                    .context(
                        "Gemini backend: no service-account credentials found.\n\
                         Set GOOGLE_APPLICATION_CREDENTIALS=/path/to/key.json\n\
                         or pass --credentials /path/to/key.json",
                    )?;
                let project_id = project_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("GOOGLE_PROJECT_ID").ok())
                    .context(
                        "Gemini backend: no GCP project ID found.\n\
                         Set GOOGLE_PROJECT_ID=your-project-id\n\
                         or pass --project your-project-id",
                    )?;
                let location = location_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("GOOGLE_LOCATION").ok())
                    .unwrap_or_else(|| "us-central1".to_string());
                let model_id = model_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("WHOISSHARP_MODEL").ok())
                    .unwrap_or_else(|| "gemini-2.5-flash".to_string());
                Ok(BackendConfig::Gemini { credentials_path, project_id, location, model_id })
            }

            BackendKind::OpenAi => {
                let api_key = api_key_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                    .context(
                        "OpenAI backend: no API key found.\n\
                         Set OPENAI_API_KEY=sk-...\n\
                         or pass --api-key sk-...",
                    )?;
                let base_url = base_url_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
                    .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
                let model_id = model_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("WHOISSHARP_MODEL").ok())
                    .unwrap_or_else(|| "gpt-4o-mini".to_string());
                Ok(BackendConfig::OpenAi { api_key, base_url, model_id })
            }

            BackendKind::Ollama => {
                let base_url = base_url_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("OLLAMA_BASE_URL").ok())
                    .unwrap_or_else(|| "http://localhost:11434/v1".to_string());
                let model_id = model_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("WHOISSHARP_MODEL").ok())
                    .unwrap_or_else(|| "llama3.2".to_string());
                Ok(BackendConfig::Ollama { base_url, model_id })
            }

            BackendKind::ClaudeHeadless => {
                let model_id = model_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("WHOISSHARP_MODEL").ok());
                Ok(BackendConfig::ClaudeHeadless { model_id })
            }

            BackendKind::CodexHeadless => {
                let model_id = model_override
                    .map(|s| s.to_string())
                    .or_else(|| std::env::var("WHOISSHARP_MODEL").ok());
                Ok(BackendConfig::CodexHeadless { model_id })
            }
        }
    }
}
