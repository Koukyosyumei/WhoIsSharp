//! WhoIsSharp — AI-powered prediction market analysis terminal.
//!
//! Usage:
//!   whoissharp                                          # dashboard only (no LLM)
//!   whoissharp --backend anthropic                      # Claude (ANTHROPIC_API_KEY)
//!   whoissharp --backend gemini                         # Vertex AI (GOOGLE_APPLICATION_CREDENTIALS + GOOGLE_PROJECT_ID)
//!   whoissharp --backend openai                         # OpenAI (OPENAI_API_KEY)
//!   whoissharp --backend ollama --model llama3.2        # local Ollama
//!   whoissharp --help                                   # all flags

mod agent;
mod cache;
mod config;
mod http;
mod llm;
mod markets;
mod news;
mod pairs;
mod portfolio;
mod risk;
mod signals;
mod tools;
mod tui;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

use config::{BackendConfig, BackendKind};
use llm::{
    anthropic::AnthropicBackend,
    gemini::GeminiBackend,
    openai::OpenAiBackend,
    LlmBackend,
};
use tools::MarketClients;

// ─── CLI ─────────────────────────────────────────────────────────────────────

/// WhoIsSharp — AI-powered prediction market terminal.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// LLM backend: none, anthropic, gemini, openai, ollama [default: none]
    #[arg(long, short = 'b', default_value = "none")]
    backend: String,

    /// Model ID override (e.g. claude-sonnet-4-6, gemini-2.5-flash, gpt-4o)
    #[arg(long, short = 'm')]
    model: Option<String>,

    /// API key override for Anthropic / OpenAI backends
    #[arg(long, short = 'k')]
    api_key: Option<String>,

    /// Base URL override for OpenAI-compatible servers (e.g. Ollama)
    #[arg(long)]
    base_url: Option<String>,

    /// Path to Google service-account JSON key file (Gemini / Vertex AI)
    #[arg(long)]
    credentials: Option<std::path::PathBuf>,

    /// GCP project ID for Vertex AI (Gemini backend)
    #[arg(long)]
    project: Option<String>,

    /// Vertex AI region (Gemini backend) [default: us-central1]
    #[arg(long)]
    location: Option<String>,

    /// Auto-refresh interval in seconds (0 = disabled) [default: 60]
    #[arg(long, short = 'R', default_value = "60")]
    refresh: u64,

    /// Run a one-shot headless smart-money scan and exit (no TUI).
    /// Scans Polymarket markets, flags suspicious wallets, and prints a report.
    #[arg(long)]
    scan: bool,

    /// Number of markets to scan in headless mode [default: 30]
    #[arg(long, default_value = "30")]
    scan_markets: usize,

    /// Minimum average suspicion score (0–100) to include a wallet [default: 40]
    #[arg(long, default_value = "40.0")]
    scan_threshold: f64,

    /// Number of top flagged wallets to deep-dive with full profile [default: 5]
    #[arg(long, default_value = "5")]
    scan_deep: usize,

    /// Emit JSON instead of human-readable text (useful for piping to jq or scripts)
    #[arg(long)]
    scan_json: bool,
}

// ─── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let kind: BackendKind = cli.backend.parse()?;

    let cfg = BackendConfig::load(
        kind,
        cli.model.as_deref(),
        cli.credentials,
        cli.project.as_deref(),
        cli.location.as_deref(),
        cli.api_key.as_deref(),
        cli.base_url.as_deref(),
    )?;

    let (backend, backend_name): (Option<Arc<dyn LlmBackend>>, String) = match cfg {
        BackendConfig::None => (None, "no AI".to_string()),

        BackendConfig::Anthropic { api_key, model_id } => {
            let b = AnthropicBackend::new(&api_key, &model_id);
            let name = b.display_name();
            (Some(Arc::new(b)), name)
        }

        BackendConfig::Gemini { credentials_path, project_id, location, model_id } => {
            let b = GeminiBackend::new(&credentials_path, &project_id, &location, &model_id)?;
            let name = b.display_name();
            (Some(Arc::new(b)), name)
        }

        BackendConfig::OpenAi { api_key, base_url, model_id } => {
            let b = OpenAiBackend::new(&api_key, &base_url, &model_id);
            let name = b.display_name();
            (Some(Arc::new(b)), name)
        }

        BackendConfig::Ollama { base_url, model_id } => {
            let b = OpenAiBackend::new("", &base_url, &model_id);
            let name = b.display_name();
            (Some(Arc::new(b)), name)
        }
    };

    let newsdata_api_key = std::env::var("NEWSDATA_API_KEY").ok();
    let clients = Arc::new(MarketClients::new(newsdata_api_key));

    if cli.scan {
        let report = tools::headless_scan(
            &clients,
            cli.scan_markets,
            cli.scan_threshold,
            cli.scan_deep,
            cli.scan_json,
        ).await?;
        println!("{}", report);
        return Ok(());
    }

    tui::run_tui(backend, clients, backend_name, cli.refresh).await?;

    Ok(())
}
