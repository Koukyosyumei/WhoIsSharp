# WhoIsSharp

> AI-powered prediction market terminal for professional traders.

<p align="center">
  <a href="https://github.com/Koukyosyumei/WhoIsSharp/" target="_blank">
      <img src="./assets/Who%20is%20Sharp.png" alt="Logo" height="126">
  </a>
</p>

![example workflow](https://github.com/Koukyosyumei/WhoIsSharp/actions/workflows/test.yaml/badge.svg)
![Apache-2.0](https://img.shields.io/github/license/Koukyosyumei/WhoIsSharp?color=blue)

WhoIsSharp is a Bloomberg-style terminal for [Polymarket](https://polymarket.com) and [Kalshi](https://kalshi.com) that embeds a full AI analyst. It detects cross-platform arbitrage, spots informed flow before it moves the market, and sizes positions with the Kelly criterion.

---

## Features

**Live market data**
- Real-time orderbook depth, bid/ask spread, and imbalance ratio
- Price history charts with OHLC stats, MA7/MA20, and volume overlay
- Time & Sales tape (Polymarket) with trade-by-trade flow

**Signal engine** — fires automatically on every refresh

| Signal | Trigger |
|--------|---------|
| `ARB` | Cross-platform price gap > 2.5pp on matched markets |
| `INSDR` | Vol/liquidity ratio > 15× at an extreme price (>75% or <25%) |
| `MOMT` | Price moved ≥ 4pp intraday — possible catalyst |
| `VOL` | Volume spike vs market average |
| `50/50` | Price within 5pp of 50% — maximum uncertainty |
| `THIN` | Very low liquidity, high adverse-selection risk |

**News feed** (Tab 0)
- Per-market news from [newsdata.io](https://newsdata.io) (free tier: 200 req/day)
- Sentiment badges: `+` positive · `-` negative · `~` neutral
- The AI automatically fetches news before any probability estimate via `get_market_news`
- Set `NEWSDATA_API_KEY` to enable (get a free key at https://newsdata.io)

**Cross-platform pairs** (Tab 9)
- Jaccard keyword matching always on; LLM semantic matching on demand
- Net arbitrage after estimated transaction fees (2% per leg)
- Resolution-risk assessment — flags when markets may resolve differently despite similar titles

**AI analyst** — structured 5-layer analysis framework
1. Fundamental Prior — base-rate reasoning independent of market price
2. Market Signal — implied mispricing direction and magnitude
3. Price Action — trend, moving averages, volume confirmation
4. Microstructure — spread, orderbook depth, imbalance
5. Informed-Flow Check — chains into `analyze_insider` → `find_smart_money` → `analyze_wallet`

Full dashboard context (prices, candles, orderbook, signals, portfolio) is injected into every LLM message automatically.

**Smart Money** (Tab 7)
- Ranks top wallets by win rate and alpha-entry score
- Detects coordinated positioning via Jaccard market-overlap clustering
- Wallet profiles: composite suspicion score 0–100

**Portfolio**
- Mark-to-market P&L, take-profit/stop-loss alerts
- Kelly / half-Kelly position sizing with one command
- Category exposure map for correlation risk
- Session persistence + Markdown report export

---

## Quickstart

```bash
# No AI — live data dashboard only
cargo run --release

# With Claude (recommended)
ANTHROPIC_API_KEY=sk-ant-... cargo run --release -- --backend anthropic

# With OpenAI
OPENAI_API_KEY=sk-... cargo run --release -- --backend openai

# Local model via Ollama
cargo run --release -- --backend ollama --model llama3.2

# Gemini / Vertex AI
GOOGLE_APPLICATION_CREDENTIALS=/path/to/key.json \
GOOGLE_PROJECT_ID=my-project \
cargo run --release -- --backend gemini
```

No config file needed. Environment variables are the only required setup.

---

## Install

**Requirements:** Rust 1.75+ (stable), internet connection.

```bash
git clone https://github.com/yourname/whoissharp
cd whoissharp
cargo build --release
./target/release/whoissharp --backend anthropic
```

---

## Key bindings

**Navigation**

| Key | Action |
|-----|--------|
| `1`–`9` | Switch tabs directly |
| `0` | Open News tab for selected market |
| `Tab` / `Shift+Tab` | Cycle tabs |
| `j` / `k` | Navigate list / scroll |
| `Enter` | Select market (loads chart + book) / send chat |
| `Ctrl+C` | Quit (or cancel any active input mode) |

**Direct shortcuts**

| Key | Action |
|-----|--------|
| `^` | Refresh market data |
| `@` | Pre-fill AI analysis prompt for selected market |
| `?` | Toggle help overlay |
| `[` / `]` | Lower / raise threshold (SmartMoney & Pairs tabs) |

**Slash commands** — press `/`, type a command, press `Enter`

| Command | Action |
|---------|--------|
| `/refresh` or `/r` | Refresh markets + chart + orderbook |
| `/platform` or `/p` | Cycle platform filter (All → PM → KL) |
| `/chart` or `/c` | Cycle chart interval (1h → 6h → 1d → 1w → 1m) |
| `/sort` or `/s` | Cycle sort mode (~50% → Vol → End date → A-Z) |
| `/watchlist` or `/w` | Toggle watchlist for selected market |
| `/wf` | Toggle watchlist-only filter |
| `/alert` or `/e` | Edit price alert thresholds |
| `/add` or `/n` | Add portfolio position (multi-step) |
| `/targets` or `/t` | Set take-profit / stop-loss |
| `/delete` or `/d` | Delete selected position |
| `/dismiss` or `/x` | Dismiss signal for this session |
| `/analyze` or `/a` | Pre-fill AI analysis prompt |
| `/kelly` or `/k` | Open Kelly position-size calculator |
| `/risk` or `/v` | Toggle risk/exposure view (Portfolio tab) |
| `/pairs` or `/l` | Re-run LLM pair matching (Pairs tab) |
| `/lower` / `/raise` | Adjust threshold (SmartMoney / Pairs tab) |
| `/wallet <0x…>` | Import Polymarket wallet positions into portfolio |
| `/wallet sync` | Re-sync all registered wallet addresses |
| `/wallet analyze` or `/wa` | Ask AI to analyse registered wallet(s) |
| `/export` or `/csv` | Export current tab to CSV |
| `/report` or `/m` | Export Markdown research report |
| `/help` or `/?` | Toggle help overlay |
| `/<search term>` | Unrecognised input → filter market list |

| Special | Action |
|---------|--------|
| `!note <text>` | Append timestamped note to research log (no AI call) |

---

## Backends

| Backend | Env vars | Flag |
|---------|----------|------|
| Anthropic Claude | `ANTHROPIC_API_KEY` | `--backend anthropic` |
| Google Gemini | `GOOGLE_APPLICATION_CREDENTIALS` + `GOOGLE_PROJECT_ID` | `--backend gemini` |
| OpenAI | `OPENAI_API_KEY` | `--backend openai` |
| Ollama (local) | — | `--backend ollama --model llama3.2` |
| None (data only) | — | _(default)_ |

**Optional: news feed**

Set `NEWSDATA_API_KEY` to enable Tab 0 and the AI's `get_market_news` tool.
Get a free key (200 req/day) at [https://newsdata.io](https://newsdata.io).

Override any model: `--model claude-opus-4-6` or `WHOISSHARP_MODEL=<id>`.

---

## License

Apache 2.0
