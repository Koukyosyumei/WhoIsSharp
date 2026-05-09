# WhoIsSharp

> The intelligence layer for professional prediction traders.

<p align="center">
  <a href="https://github.com/Koukyosyumei/WhoIsSharp/" target="_blank">
      <img src="./assets/Who%20is%20Sharp.png" alt="Logo" height="126">
  </a>
</p>

![example workflow](https://github.com/Koukyosyumei/WhoIsSharp/actions/workflows/test.yaml/badge.svg)
![Apache-2.0](https://img.shields.io/github/license/Koukyosyumei/WhoIsSharp)

A high-performance terminal for institutional-grade analysis of [Polymarket](https://polymarket.com) and [Kalshi](https://kalshi.com). Real-time microstructure, smart-money profiling, and an embedded AI analyst — all keyboard-driven.

<p align="center">
  <a href="https://github.com/Koukyosyumei/WhoIsSharp/" target="_blank">
      <img src="./assets/main.gif" alt="WhoIsSharp Terminal" width="90%">
  </a>
</p>

---

## Quickstart

```bash
cargo install --git https://github.com/Koukyosyumei/WhoIsSharp whoissharp
```

**No API key required** — drive the AI with your existing Claude Code or Codex login:

```bash
whoissharp --backend claude-code   # uses your `claude` CLI subscription
whoissharp --backend codex         # uses your `codex login`
```

Both spawn the local CLI per turn, wired via MCP to WhoIsSharp's full toolset (orderbooks, smart-money scans, Kelly sizing, news, …). Run `whoissharp` with no flags for a pure data dashboard.

<details>
<summary>Other backends (require API keys)</summary>

```bash
ANTHROPIC_API_KEY=sk-ant-…  whoissharp --backend anthropic
OPENAI_API_KEY=sk-…         whoissharp --backend openai
                            whoissharp --backend ollama --model llama3.2
GOOGLE_APPLICATION_CREDENTIALS=/path/to/key.json \
GOOGLE_PROJECT_ID=my-project whoissharp --backend gemini
```

Override the model on any backend with `--model <id>` or `WHOISSHARP_MODEL=<id>`.

</details>

---

## Capabilities

**Signal engine** — Cross-platform arbitrage (ARB), informed-flow detection (INSDR), momentum (MOMT), and thin-liquidity alerts (THIN).

**Smart-money profiling** — Polymarket wallet ranking by win rate, alpha-entry score, and Jaccard coordination clusters.

**Microstructure** — Live orderbooks, bid/ask spread in bps, imbalance ratios, Roll's spread, Amihud illiquidity, Kyle's λ.

**Portfolio & risk** — Wallet import, VaR/CVaR, stress tests, Kelly sizing (single and correlation-adjusted).

**News & macro** — Sentiment-tagged article fetch via newsdata.io, FRED macro snapshot.

### AI analysis framework

The embedded analyst executes a fixed six-step workflow per market:

1. **Fundamental prior** — base-rate probability independent of price.
2. **Market signal** — implied odds vs. fair value.
3. **Price action** — trend, MA7/MA20, volume confirmation.
4. **Microstructure** — orderbook walls and depth.
5. **Flow check** — smart-money and insider signals.
6. **Optimal position** — Kelly-sized recommendation.

---

## Backends

| Backend | Flag | Auth |
|---|---|---|
| Claude Code (headless) | `--backend claude-code` | existing `claude` login |
| Codex (headless) | `--backend codex` | existing `codex login` |
| Anthropic | `--backend anthropic` | `ANTHROPIC_API_KEY` |
| OpenAI | `--backend openai` | `OPENAI_API_KEY` |
| Gemini / Vertex AI | `--backend gemini` | `GOOGLE_APPLICATION_CREDENTIALS` + `GOOGLE_PROJECT_ID` |
| Ollama (local) | `--backend ollama` | — |
| None (data only) | _(default)_ | — |

Optional: set `NEWSDATA_API_KEY` for the News tab and `FRED_API_KEY` for the macro snapshot.

---

## Key bindings

| Key | Action |
|---|---|
| `1`–`9`, `0` | Switch tab |
| `Tab` / `Shift+Tab` | Cycle tabs |
| `j` / `k` | Navigate |
| `Enter` | Select market / send chat |
| `^` | Refresh |
| `@` | Pre-fill AI analysis prompt |
| `?` | Help overlay |
| `Ctrl+C` | Quit |

Slash commands (`/refresh`, `/platform`, `/chart`, `/sort`, `/watchlist`, `/alert`, `/add`, `/kelly`, `/risk`, `/wallet <0x…>`, `/export`, …) — press `?` in the app for the full reference.

---

## MCP server

WhoIsSharp can also run as a standalone [MCP](https://modelcontextprotocol.io) server, exposing every tool to Claude Desktop, Claude Code, or any other MCP host:

```bash
claude mcp add whoissharp -- whoissharp --mcp
```

For Claude Desktop, add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "whoissharp": {
      "command": "whoissharp",
      "args": ["--mcp"],
      "env": {
        "NEWSDATA_API_KEY": "…",
        "FRED_API_KEY":     "…"
      }
    }
  }
}
```

<details>
<summary>Tools exposed via MCP</summary>

| Tool | Description |
|---|---|
| `list_markets` | List markets from Polymarket / Kalshi |
| `get_market` | Full details for a specific market |
| `get_orderbook` | Live bid/ask depth |
| `get_price_history` | Historical YES prices with sparkline |
| `get_events` | Event categories |
| `search_markets` | Keyword search across both platforms |
| `analyze_insider` | Detect informed-flow signals on one market |
| `find_smart_money` | Rank top wallets by edge score |
| `analyze_wallet` | Full profile for a single wallet |
| `scan_smart_money` | Bulk suspicious-wallet scan |
| `get_wallet_positions` | Current open positions for a wallet |
| `kelly_size` | Single-bet Kelly Criterion sizing |
| `kelly_correlated` | Multi-bet Kelly under pairwise correlations |
| `binary_greeks` | Δ, Θ, Vega for a prediction-market position |
| `market_microstructure` | Roll's spread, Amihud illiquidity, Kyle's λ |
| `test_cointegration` | Engle-Granger cointegration test for a PM/KL pair |
| `get_market_news` | News contextualised to a market |
| `search_news` | Fetch news articles by free-text query |
| `get_portfolio` | Open positions with P&L |
| `get_portfolio_risk` | VaR / CVaR / stress tests |
| `get_watchlist` | Watchlist with alert thresholds |
| `get_signals` | Run the full signal engine (ARB/INSDR/MOMT/THIN) |
| `get_macro` | FRED macro snapshot |

</details>

---

## License

Apache 2.0
