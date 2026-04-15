# WhoIsSharp — Complete Documentation

## Table of Contents

1. [Overview](#overview)
2. [Installation](#installation)
3. [Configuration](#configuration)
4. [UI Layout](#ui-layout)
5. [Tabs Reference](#tabs-reference)
6. [Key Bindings](#key-bindings)
7. [Signal Engine](#signal-engine)
8. [Cross-Platform Pairs](#cross-platform-pairs)
9. [AI Analyst](#ai-analyst)
10. [AI Tools Reference](#ai-tools-reference)
11. [Smart Money Analysis](#smart-money-analysis)
12. [Portfolio Management](#portfolio-management)
13. [Research Workflow](#research-workflow)
14. [Architecture](#architecture)
15. [Adding a New LLM Backend](#adding-a-new-llm-backend)
16. [Adding a New AI Tool](#adding-a-new-ai-tool)
17. [API Endpoints Used](#api-endpoints-used)
18. [Development](#development)

---

## Overview

WhoIsSharp is a terminal application for prediction market analysis. It ingests live data from [Polymarket](https://polymarket.com) and [Kalshi](https://kalshi.com), runs a local signal-detection engine, and optionally embeds an AI analyst that can call live market tools to produce structured, evidence-driven analysis.

**Design philosophy:** The tool is deliberately read-only. It surfaces edge — it does not execute trades. The AI is configured to behave like a quant analyst: it takes a directional view, quantifies its confidence, and sizes positions with Kelly criterion. It never refuses to run an analysis because a parameter is "missing" — it makes reasonable assumptions and states them.

---

## Installation

### Requirements

- Rust 1.75 or newer (stable toolchain)
- Internet connection (Polymarket and Kalshi are public APIs — no account required for read access)

### Build

```bash
git clone https://github.com/yourname/whoissharp
cd whoissharp
cargo build --release
```

The binary is at `target/release/whoissharp`.

### Run

```bash
# Data-only mode (no AI)
./target/release/whoissharp

# Or via cargo (always use --release)
cargo run --release -- --backend anthropic
```

> **Always build and run with `--release`.** The debug build is significantly slower and will feel unresponsive.

---

## Configuration

### CLI flags

```
whoissharp [OPTIONS]

Options:
  -b, --backend <BACKEND>      LLM backend: none, anthropic, gemini, openai, ollama [default: none]
  -m, --model <MODEL>          Model ID override
  -k, --api-key <API_KEY>      API key override (Anthropic / OpenAI)
      --base-url <BASE_URL>    Base URL for OpenAI-compatible servers
      --credentials <PATH>     Path to Google service-account JSON (Gemini)
      --project <PROJECT>      GCP project ID (Gemini / Vertex AI)
      --location <LOCATION>    Vertex AI region [default: us-central1]
  -R, --refresh <SECS>         Auto-refresh interval in seconds; 0 = disabled [default: 60]
  -h, --help                   Print help
  -V, --version                Print version
```

### Environment variables

All credentials can be provided as environment variables instead of CLI flags.

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | API key for Anthropic Claude |
| `OPENAI_API_KEY` | API key for OpenAI |
| `OPENAI_BASE_URL` | Base URL for OpenAI-compatible API (e.g. Ollama) |
| `GOOGLE_APPLICATION_CREDENTIALS` | Path to Google service-account JSON file |
| `GOOGLE_PROJECT_ID` | GCP project ID |
| `GOOGLE_LOCATION` | Vertex AI region (default: `us-central1`) |
| `OLLAMA_BASE_URL` | Ollama server URL (default: `http://localhost:11434`) |
| `WHOISSHARP_MODEL` | Override the model ID for any backend |
| `NEWSDATA_API_KEY` | API key for [newsdata.io](https://newsdata.io) — enables Tab 0 and the AI's `get_market_news` tool (free tier: 200 req/day) |

### Backend setup

#### Anthropic Claude

```bash
export ANTHROPIC_API_KEY=sk-ant-...
cargo run --release -- --backend anthropic
# Or with a specific model:
cargo run --release -- --backend anthropic --model claude-opus-4-6
```

Default model: `claude-sonnet-4-6`.

#### OpenAI

```bash
export OPENAI_API_KEY=sk-...
cargo run --release -- --backend openai
```

Default model: `gpt-4o`.

#### Ollama (local)

Start Ollama with your model first:
```bash
ollama pull llama3.2
ollama serve
```

Then:
```bash
cargo run --release -- --backend ollama --model llama3.2
```

The default Ollama URL is `http://localhost:11434`. Override with `--base-url` or `OLLAMA_BASE_URL`.

#### Google Gemini / Vertex AI

```bash
export GOOGLE_APPLICATION_CREDENTIALS=/path/to/service-account.json
export GOOGLE_PROJECT_ID=my-gcp-project
cargo run --release -- --backend gemini
# Or with explicit flags:
cargo run --release -- \
  --backend gemini \
  --credentials /path/to/key.json \
  --project my-gcp-project \
  --location us-central1
```

Default model: `gemini-2.5-flash`. The backend uses the Vertex AI endpoint with service-account JWT authentication (no OAuth flow required).

---

## UI Layout

```
┌─ header (1 line) ─────────────────────────────────────────────────────────────┐
│  WhoIsSharp v0.1.0  ·  claude-sonnet-4-6  ·  PM + KL              14:23:05  │
├─ tab bar (1 line) ─────────────────────────────────────────────────────────────┤
│  [0]News [1]Signals [2]Markets [3]Chart [4]Book [5]Portfolio [6]Chat [7]SM [8]Trades [9]Pairs
├─ content area (fills remaining height) ───────────────────────────────────────┤
│                                                                                │
│  [tab content]                                                                 │
│                                                                                │
├─ status bar (1 line) ──────────────────────────────────────────────────────────┤
│  ● Ready  ALL  Chart:1w  │  42 markets  6 signals  2 arb pairs                │
└─ input box (1 line) ───────────────────────────────────────────────────────────┘
  > _
```

The input box is always active. Type to send a message to the AI. Prefix with `!note` to log a research note without sending to the AI. Press `/` to open the command bar and type a slash command (e.g. `/refresh`, `/analyze`). Direct shortcuts `^`, `@`, `?`, `[`, `]` fire when the input box is empty.

---

## Tabs Reference

### Tab 0 — News

Per-market news feed powered by [newsdata.io](https://newsdata.io).

**Requires:** `NEWSDATA_API_KEY` environment variable. Get a free key at https://newsdata.io (free tier: 200 requests/day, 5-minute TTL cache so refreshes are conserved).

**Layout:** Two-panel. Left panel lists articles (title, source, age, sentiment badge). Right panel shows the full description and link for the selected article.

**Sentiment badges:**

| Badge | Meaning |
|-------|---------|
| `+` (green) | Positive sentiment |
| `-` (red) | Negative sentiment |
| `~` (gray) | Neutral |
| ` ` | No sentiment data |

**Controls:**
- `0` — open this tab and auto-fetch news for the selected market
- `/refresh` — re-fetch (respects 5-min TTL cache)
- `j`/`k` — scroll article list; detail panel updates live
- `Esc` — return to previous tab

**How news is fetched:** The market title is processed with stop-word removal to extract 3–4 key terms, which are sent to newsdata.io. The same query logic is used by the AI's `get_market_news` tool.

> If no API key is configured, the tab displays sign-up instructions with the newsdata.io URL.

---

### Tab 1 — Signals

The main alert dashboard. Signals are recomputed automatically after every market data refresh.

**Layout:** Two-panel. Left panel shows a scrollable list of signals sorted by priority (stars → EV score). Right panel shows detail for the selected signal.

**Signal detail includes:**
- Signal type, star rating, EV score
- Primary market: platform, ID, YES price, volume, liquidity
- Secondary market (for `ARB`): platform, ID, YES price
- Gap magnitude and actionable hint
- Orderbook imbalance summary if available

**Navigation:** `j`/`k` to move, `Enter` to navigate to the signal's primary market (switches to Chart tab), `@` to pre-fill an AI analysis prompt, `/add` to add a position, `/dismiss` to hide the signal for this session.

---

### Tab 2 — Markets

Full market list from Polymarket and Kalshi combined.

**Layout:** Two-panel. Left panel is the scrollable market list with velocity column. Right panel shows detail for the selected market.

**Market list columns:**
- Platform badge (`PM` / `KL`)
- YES price with color coding (green > 60%, red < 40%, white otherwise)
- Price velocity (▲/▼ with pp change vs previous refresh)
- Volume
- Truncated title

**Controls:**
- `/platform` or `/p` — cycle platform filter: All → PM → KL → All
- `/sort` or `/s` — cycle sort mode: ~50% (closest to 50 first) → Volume → End date → A-Z
- `/watchlist` or `/w` — toggle watchlist for selected market
- `@` — pre-fill AI analysis prompt for selected market
- `Enter` — load chart + orderbook for selected market, switch to Chart tab
- `/` then a search term — filter market list by title
- `Esc` — clear search / cancel command bar

---

### Tab 3 — Chart

Price history chart for the selected market.

**Features:**
- Line chart of YES price over time
- OHLC stats bar at bottom: Open / High / Low / Close / Δpp / volume for the most recent candle
- Chart interval shown in status bar

**Controls:**
- `/chart` or `/c` — cycle chart interval: 1h → 6h → 1d → 1w → 1m
- `^` — refresh chart data

**Chart intervals:**

| Code | Window |
|------|--------|
| `1h` | 1-hour candles |
| `6h` | 6-hour candles |
| `1d` | Daily candles |
| `1w` | Weekly candles |
| `1m` | Monthly candles |

---

### Tab 4 — Book (Orderbook)

Live orderbook for the selected market.

**Features:**
- Bid/ask depth with proportional `█` bars (sized relative to the largest level)
- Best bid / best ask / spread in pp and basis points
- Orderbook imbalance: `(total_bid_size - total_ask_size) / total_size`
  - > +15%: **BUY PRESSURE** (green)
  - < -15%: **SELL PRESSURE** (red)
  - Otherwise: **BALANCED** (white)
- All bid and ask levels shown, scrollable with `j`/`k`

**Interpreting the book:**
- **Tight spread (< 2pp):** Liquid market, low adverse-selection risk. Momentum trades more reliable.
- **Wide spread (> 5pp):** Thin book. Any fill moves the price against you.
- **Large bid imbalance (> +20%):** Bulls defending or accumulating at a level.
- **Large ask imbalance (> +20%):** Distribution; sellers hedging or exiting.

---

### Tab 5 — Portfolio

Position tracking and risk management.

**Summary panel shows:**
- Total positions, total cost basis, total unrealised P&L
- Category exposure map (groups cost by market category, shows % of portfolio in each)
- Number of positions with take-profit and stop-loss set
- Active alerts (positions that have crossed TP or SL threshold)

**Position list shows:**
- Side (YES/NO), entry price → mark price, shares, unrealised P&L
- Take-profit and stop-loss levels with 🎯/🛑 alert icons when triggered

**Controls:**
- `/add` or `/n` — add a new position for the selected market (multi-step input flow)
- `/targets` or `/t` — set take-profit / stop-loss for the selected position (2-step flow)
- `/delete` or `/d` — delete selected position
- `/risk` or `/v` — toggle risk/exposure view

**Adding a position** (`/add`):
1. Enter entry price in cents (e.g., `68` for 68¢)
2. Enter number of shares
3. Enter side: `y` for YES, `n` for NO
4. Optionally enter a research note, or press `Enter` to skip

**Setting targets** (`/targets`):
1. Enter take-profit price in cents, or `Enter` to skip
2. Enter stop-loss price in cents, or `Enter` to skip

---

### Tab 6 — Chat

Interactive AI analyst session.

**Usage:** Type a message in the input box and press `Enter`. The AI has access to all live market tools and the full dashboard context.

**Context injection:** Every message automatically prepends the current dashboard state:
- Selected market title, platform, YES/NO prices, volume, liquidity, vol/liq ratio, days to resolution
- Price history statistics: MA7, MA20, 5-candle momentum, trend label, volume ratio
- Live orderbook: top 3 bid/ask levels, imbalance %, spread in bps
- Any active signals for the selected market
- Your portfolio position in the selected market (entry, mark, P&L, TP/SL)
- Research notes from the current session

This means you can ask "further analyze this market" or "run Kelly" without repeating any IDs or prices.

**`!note` shortcut:** Typing `!note <text>` appends a timestamped note to `~/.whoissharp/notes.md` without sending anything to the AI. Notes are included in the context for future AI messages and in Markdown report exports.

**Session persistence:** All chat messages are saved to `~/.whoissharp/sessions/<timestamp>.json` when you quit.

---

### Tab 7 — Smart Money

Wallet-level flow analysis for Polymarket markets.

**Loads automatically** when you select a Polymarket market and switch to this tab (or press `7`).

**Wallet list columns:**
- Address (truncated)
- Win rate %
- Alpha-entry score (avg BUY price on winning trades — lower = earlier entry)
- Total trades
- Flagged (⚠ if suspicion score is elevated)

**Coordination panel:** Shows wallet pairs with high Jaccard market-overlap (≥ 35%), suggesting possible coordinated positioning.

**Interpreting results:**
- **Alpha entry < 35¢:** The wallet was buying before public consensus formed. Strong informed-flow signal.
- **Coordination Jaccard ≥ 35%:** Two wallets traded many of the same markets together. Investigate funding sources — may be coordinated.
- **Suspicion score 0–100:** Composite of win rate, alpha entry, trade frequency, and coordination. Score ≥ 50 warrants further investigation.

> Smart Money analysis is Polymarket-only. Kalshi does not expose wallet-level trade history via its public API.

---

### Tab 8 — Trades (Time & Sales)

Trade-by-trade tape for the selected Polymarket market.

Shows: timestamp, side (BUY/SELL), price, size, trade ID.

> Polymarket-only. Kalshi trade tape is not available via the public API.

---

### Tab 9 — Pairs

Cross-platform market matching and arbitrage analysis.

**How it works:**

1. **Jaccard matching** (always on): Computed automatically on every market load. Matches markets by word-set overlap on titles, threshold 0.35. Fast, local, no API calls.

2. **LLM matching** (on demand): Triggered when you switch to the Pairs tab (or press `9` or type `/pairs`). Pre-filters candidates with a lower Jaccard threshold (0.15), then sends batches of up to 25 pairs to the LLM for semantic assessment. The LLM returns `match_type`, `res_risk`, `res_risk_note`, and `confidence` for each pair. Falls back to Jaccard results if the LLM call fails.

**Match types:**

| Type | Symbol | Meaning |
|------|--------|---------|
| `IDENTICAL` | `≡` | Same event, same resolution criteria — true arb if gap exists |
| `NEAR-IDENTICAL` | `≈` | Same event, minor wording/timing differences — likely arb |
| `RELATED` | `~` | Related events that could resolve differently |

**Resolution risk:**

| Risk | Color | Meaning |
|------|-------|---------|
| `LOW` | Green | Almost certain to resolve identically |
| `MEDIUM` | Yellow | Some ambiguity in criteria or timing |
| `HIGH` | Red | Meaningful risk of different resolution |

**Net gap calculation:**

```
Arb strategy (when PM price > KL price):
  Buy YES on KL at kl_price
  Buy NO on PM at (1 - pm_price)
  Total outlay = kl_price + (1 - pm_price) = 1 - gross_gap
  Guaranteed payout = $1.00

Fees:
  KL leg: 2% × kl_price
  PM leg: 2% × (1 - pm_price)

Net gap = gross_gap - KL_fee - PM_fee
```

A **positive net gap** means the arb is profitable after transaction costs. **Negative** means fees consume the spread — do not trade.

**Capturable profit:**
```
capturable_usd = max(0, net_gap) × min(pm_liquidity, kl_liquidity)
```
This is an upper bound — real fills will be limited by orderbook depth, not total liquidity.

**Star rating:**

| Stars | Net gap |
|-------|---------|
| ★★★ | ≥ 5pp |
| ★★☆ | ≥ 2pp |
| ★☆☆ | > 0pp |
| ☆☆☆ | ≤ 0pp (not profitable) |

**Controls:**
- `j`/`k` — navigate pairs list
- `/pairs` or `/l` — trigger LLM re-matching
- `[` / `]` — lower / raise Jaccard threshold
- `Enter` — open the Polymarket market in Chart tab

---

## Key Bindings

### Navigation (always available)

| Key | Action |
|-----|--------|
| `1`–`9` | Switch to tab N directly |
| `Tab` | Cycle to next tab |
| `Shift+Tab` | Cycle to previous tab |
| `j` / `↓` | Move selection down / scroll down |
| `k` / `↑` | Move selection up / scroll up |
| `Enter` | Select market (loads chart + book) / send chat message |
| `Ctrl+C` | Quit (or cancel any active input mode) |

### Direct shortcuts (fire when input is empty)

| Key | Action |
|-----|--------|
| `0` | Open News tab and fetch news for selected market |
| `^` | Refresh markets + chart + orderbook |
| `@` | Pre-fill AI analysis prompt for selected market |
| `?` | Toggle help overlay |
| `[` | Lower threshold by 5% (SmartMoney / Pairs tab) |
| `]` | Raise threshold by 5% (SmartMoney / Pairs tab) |

### Slash commands — press `/`, type, press `Enter`

Unrecognised input is used as a market search/filter term.

| Command | Action |
|---------|--------|
| `/refresh` or `/r` | Refresh markets + chart + orderbook |
| `/platform` or `/p` | Cycle platform filter: All → PM → KL → All |
| `/chart` or `/c` | Cycle chart interval: 1h → 6h → 1d → 1w → 1m |
| `/sort` or `/s` | Cycle sort mode: ~50% → Vol → End date → A-Z |
| `/watchlist` or `/w` | Toggle watchlist for selected market |
| `/wf` | Toggle watchlist-only filter |
| `/alert` or `/e` | Edit price alert thresholds (above / below) |
| `/add` or `/n` | Add new position (multi-step) |
| `/targets` or `/t` | Set take-profit / stop-loss for selected position |
| `/delete` or `/d` | Delete selected position (Portfolio tab) |
| `/dismiss` or `/x` | Dismiss selected signal for this session |
| `/analyze` or `/a` | Pre-fill AI analysis prompt for selected market |
| `/kelly` or `/k` | Open Kelly position-size calculator |
| `/risk` or `/v` | Toggle risk/exposure view (Portfolio tab) |
| `/pairs` or `/l` | Re-run LLM pair matching (Pairs tab) |
| `/lower` | Lower threshold by 5% (SmartMoney / Pairs tab) |
| `/raise` | Raise threshold by 5% (SmartMoney / Pairs tab) |
| `/wallet <0x…>` | Register a Polymarket wallet address and import its positions |
| `/wallet sync` | Re-sync all registered wallet addresses |
| `/wallet analyze` or `/wa` | Ask AI to analyse registered wallet(s) |
| `/export` or `/csv` | Export current tab to CSV |
| `/report` or `/m` | Export Markdown research report |
| `/help` or `/?` | Toggle help overlay |

### Special input

| Input | Action |
|-------|--------|
| `!note <text>` | Append timestamped note to `~/.whoissharp/notes.md` (no AI call) |

---

## Signal Engine

Signals are computed in `src/signals.rs` — pure, synchronous, no network calls. They run on every `MarketsLoaded` event.

### Signal types

#### ARB — Cross-platform arbitrage

**Trigger:** A Polymarket market and a Kalshi market have:
- Title similarity (Jaccard word overlap) ≥ 0.38
- Price gap > 2.5pp

**Star rating:**
- ★★★: gap ≥ 8pp
- ★★☆: gap ≥ 4pp
- ★☆☆: gap ≥ 2.5pp

**EV score:** `gap × 100 × ln(min_liquidity + 1)` — liquidity-adjusted.

> This is a lightweight pre-filter. The Pairs tab (Tab 9) does deeper matching with LLM-assessed resolution risk and net-after-fee calculation.

---

#### INSDR — Insider alert

**Trigger:** A market where:
- YES price is extreme: > 75% or < 25%
- Volume / max(liquidity, 1) > 15×

**Interpretation:** A healthy prediction market has vol/liq of 1–5×. Ratios above 15× at an extreme price suggest the market is consuming liquidity rapidly in one direction — consistent with informed flow buying ahead of a news event.

**Star rating:**
- ★★★: ratio ≥ 30×
- ★★☆: ratio ≥ 15×

---

#### MOMT — Momentum

**Trigger:** YES price has moved ≥ 4pp since the previous refresh cycle.

**Direction:** Compares `current_price` vs `prev_prices[market_id]` (snapshot taken before each refresh). Positive delta = price rising, negative = falling.

**Use:** Surface markets where something is happening between refreshes. Check news before trading — this is a symptom signal, not a cause signal.

---

#### VOL — Volume spike

**Trigger:** Market volume is unusually high relative to the cross-market average.

**Use:** High volume without a proportionate price move can indicate informed accumulation (stealth buying). High volume with a large price move confirms directional conviction.

---

#### 50/50 — Near-fifty

**Trigger:** YES price within 5pp of 50%.

**Use:** Maximum uncertainty markets are often the highest-EV opportunities if you have an edge on the fundamental probability. Also surfaces markets where the orderbook is most sensitive to new information.

---

#### THIN — Thin market

**Trigger:** Very low liquidity (threshold set in `signals.rs`).

**Use:** Warning signal. Thin markets have high adverse-selection risk — any fill moves the price. Use with extreme caution for position sizing.

---

### Signal deduplication and limits

- Signals are sorted by stars → EV score descending
- Deduplicated by primary market ID (one signal per market)
- Dismissed signals (user pressed `x`) are filtered out for the session
- Maximum 30 signals shown at once

---

## Cross-Platform Pairs

See [Tab 9 — Pairs](#tab-9--pairs) for the full explanation.

### Matching algorithm

```
1. For each (PM market, KL market) pair:
   a. Compute Jaccard similarity on normalized word sets
      - Lowercase, split on non-alphanumeric
      - Remove stopwords: the, a, an, in, on, by, will, win, ...
      - Remove short words (≤ 2 chars)
   b. If similarity ≥ threshold → candidate pair

2. (LLM mode only) Batch candidates (up to 25) → LLM prompt
   - Returns: match_type, res_risk, res_risk_note, confidence
   - Filters out "different" pairs
   - Falls back to Jaccard on parse failure

3. For each surviving pair:
   - Compute gross_gap = |pm_yes - kl_yes|
   - Compute net_gap = gross_gap - fees (see formula above)
   - Compute capturable_usd = max(0, net_gap) × min_liquidity
   - Assign stars
```

### Fee constants

```
PM_TAKER_FEE = 2%   (applied to notional of the PM leg)
KL_TAKER_FEE = 2%   (applied to notional of the KL leg)
```

These are conservative estimates. Actual fees vary by market and order type. Always verify current fee schedules before trading.

---

## AI Analyst

### Architecture

The AI runs in a background `tokio::spawn`ed task. It communicates with the TUI via an unbounded `mpsc` channel, emitting `AppEvent`s as it works:

```
User message
    → build_context_prefix() — inject dashboard state
    → LLmBackend::generate_streaming() — token stream
    → AppEvent::AgentTextChunk — update chat in real time
    → tool_calls loop — call market data tools
    → AppEvent::AgentToolCall / AgentToolResult — show tool activity
    → AppEvent::AgentDone — final state
    → AppEvent::HistoryUpdated — persist conversation history
```

Conversation history is persisted across turns via the `HistoryUpdated` event. The history is trimmed if it exceeds 80,000 chars by compressing old tool-result messages to summaries.

### Analysis framework

When the user asks to "analyze" a market, the system prompt directs the LLM to work through five layers:

1. **Fundamental Prior** — independent base-rate reasoning. What would an outside analyst estimate the probability at, ignoring the market price?

2. **Market Signal** — compare market price to prior. Is the gap within noise (±5pp) or a potential edge?

3. **Price Action** — fetch `get_price_history`. Interpret trend, MA crossovers, momentum, volume confirmation.

4. **Microstructure** — fetch `get_orderbook`. Analyse spread (bps), imbalance (%), depth stacking.

5. **Informed-Flow Check** — call `analyze_insider`. If suspicion score ≥ 50, chain to `find_smart_money`. If alpha_entry < 35¢ on top wallets, call `analyze_wallet`.

The LLM is instructed to auto-call `kelly_size` if it finds a positive edge, without waiting for the user to ask.

### Context prefix

Every user message is prefixed with a structured block containing:

```
SELECTED MARKET
  Title, Platform, Market ID
  YES/NO prices, implied odds ratio
  Volume, Liquidity, Vol/Liq ratio
  Days remaining to resolution, Category

PRICE HISTORY (N candles, interval)
  Current price, Period Δ%, Range (lo–hi)
  MA7, MA20, 5-candle momentum, trend label
  Last candle volume vs average

LIVE ORDERBOOK
  Best bid/ask, Spread pp and bps
  Total bid/ask depth, Imbalance %, pressure label
  Top 3 bid levels, Top 3 ask levels

ACTIVE SIGNALS
  Any signals firing for this market

YOUR POSITION (if held)
  Side, shares, entry price, mark price, P&L, TP/SL

RESEARCH NOTES
  Session notes from !note commands
```

This means the LLM never needs to ask for prices or IDs visible on screen.

---

## AI Tools Reference

### Market data tools

#### `list_markets`

Browse available markets.

| Parameter | Type | Description |
|-----------|------|-------------|
| `platform` | string | `"polymarket"`, `"kalshi"`, or `"all"` |
| `limit` | integer | Max markets to return (default 20) |
| `category` | string | Optional category filter |

#### `get_market`

Fetch full details for a single market.

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_id` | string | Market ID (conditionId for PM, ticker for KL) |
| `platform` | string | `"polymarket"` or `"kalshi"` |

#### `get_orderbook`

Fetch live bid/ask depth.

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_id` | string | Market ID |
| `platform` | string | `"polymarket"` or `"kalshi"` |

#### `get_price_history`

Historical YES price chart with ASCII sparkline.

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_id` | string | Market ID |
| `platform` | string | `"polymarket"` or `"kalshi"` |
| `interval` | string | `"1h"`, `"6h"`, `"1d"`, `"1w"`, `"1m"` |

#### `get_events`

List event categories from both platforms.

#### `search_markets`

Keyword search across Polymarket and Kalshi.

| Parameter | Type | Description |
|-----------|------|-------------|
| `query` | string | Search term |
| `platform` | string | `"polymarket"`, `"kalshi"`, or `"all"` |
| `limit` | integer | Max results |

---

### Insider / smart-money tools

#### `analyze_insider`

Computes price velocity, vol/liq ratio, and orderbook imbalance for one Polymarket market. Returns a suspicion score and a description of any anomalies.

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_id` | string | Polymarket conditionId |

**When to use:** Before any substantive analysis of a Polymarket market. The AI is instructed to call this automatically.

#### `find_smart_money`

Ranks the top wallets in a Polymarket market by win rate, alpha-entry score, and wallet coordination (concurrent fetching with Jaccard clustering).

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_id` | string | Polymarket conditionId |
| `limit` | integer | Max wallets to return |

**Alpha entry score:** Average BUY price on winning trades. Below 35¢ = the wallet was buying before consensus formed — strong informed-flow signal.

**Coordination:** Jaccard market-overlap ≥ 35% between two wallets → possible coordinated positioning.

#### `analyze_wallet`

Deep profile of one wallet: trade history, alpha score, top markets, composite suspicion score (0–100).

| Parameter | Type | Description |
|-----------|------|-------------|
| `address` | string | Ethereum address |

**Suspicion score components:** win rate, alpha entry, trade frequency, coordination index.

---

### News tools

#### `get_market_news`

Fetches news articles contextually relevant to a specific prediction market. Automatically extracts key terms from the market title (same stop-word logic as Tab 0) and queries newsdata.io.

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_id` | string | Market condition ID or Kalshi ticker |
| `platform` | string | `"polymarket"` or `"kalshi"` (default: `"polymarket"`) |
| `limit` | integer | Articles to return (1–10, default 8) |

**Returns:** Titles, sources, publication age, sentiment labels, keywords, and descriptions.

**When to use:** The AI is instructed to call this automatically immediately after `get_market`, before forming any probability estimate. Requires `NEWSDATA_API_KEY`.

#### `search_news`

Free-form news search by custom query terms. Use when you want to investigate a specific angle beyond what `get_market_news` covers — e.g. a related entity, a follow-up search with refined terms, or cross-checking a specific claim.

| Parameter | Type | Description |
|-----------|------|-------------|
| `query` | string | Search terms (3–5 keywords, e.g. `"Trump tariffs China"`) |
| `limit` | integer | Articles to return (1–10, default 8) |

Requires `NEWSDATA_API_KEY`.

---

### Position sizing

#### `kelly_size`

Computes Kelly and half-Kelly bet sizes given your probability estimate vs the market price.

| Parameter | Type | Description |
|-----------|------|-------------|
| `market_price` | number | Current YES price (0.0–1.0) |
| `your_probability` | number | Your estimated TRUE probability (0.0–1.0) |
| `bankroll` | number | Total bankroll in USD |
| `side` | string | `"yes"` or `"no"` |

**Returns:** Full Kelly %, half-Kelly %, dollar amount, share count, expected value.

**Rules enforced by the AI:**
- Default to half-Kelly; full Kelly is too aggressive for binary outcomes
- Cap any single position at 5–10% of bankroll regardless of Kelly output
- Negative Kelly = no edge on that side; consider the opposite leg
- The AI is instructed to call this automatically when it finds a positive edge

---

## Smart Money Analysis

### How wallet scoring works

WhoIsSharp fetches trade history for the top wallet addresses in a market, then computes:

```
win_rate = (profitable_trades / total_trades)

alpha_entry = mean(entry_price) for winning long trades
    → lower = earlier entry relative to resolution price

coordination_score = max Jaccard overlap with any other wallet
    Jaccard(A, B) = |markets_A ∩ markets_B| / |markets_A ∪ markets_B|

suspicion_score (0–100) = weighted composite of:
    win_rate component   (25%)
    alpha_entry component (35%)
    coordination component (25%)
    trade_frequency component (15%)
```

### Interpretation guide

| Signal | Threshold | Interpretation |
|--------|-----------|----------------|
| Alpha entry | < 35¢ | Wallet was buying before consensus — strong informed signal |
| Alpha entry | < 20¢ | Extreme early entry — likely has pre-public information |
| Win rate | > 65% | Consistently profitable — skilled or informed |
| Coordination | ≥ 35% | Possible coordinated positioning with another wallet |
| Suspicion score | ≥ 50 | Warrants investigation |
| Suspicion score | ≥ 75 | High confidence of informed flow |

### Limitations

- Smart money analysis requires on-chain trade history access via Polymarket's CLOB API
- Kalshi does not expose wallet-level data via its public API
- A single lucky whale ≠ coordinated smart money; always look for multiple confirming wallets
- Always cross-reference against public news timelines before concluding insider activity

---

## Portfolio Management

### Data persistence

Portfolio positions are saved to `~/.whoissharp/portfolio.json` automatically on every change. The file is human-readable JSON.

### Position lifecycle

```
Add (n key) → Mark-to-market (auto, on refresh) → Alert (TP/SL crossed) → Delete (d key)
```

### Position fields

| Field | Description |
|-------|-------------|
| `id` | Unique position ID (hex) |
| `platform` | Polymarket or Kalshi |
| `market_id` | Market identifier |
| `title` | Market title (snapshot at add time) |
| `entry_price` | YES price at entry, 0.0–1.0 |
| `shares` | Number of shares / contracts |
| `side` | YES or NO |
| `opened_at` | UTC timestamp |
| `mark_price` | Current mark (updated from live market data) |
| `note` | Optional research note |
| `take_profit` | Alert threshold — fires when mark ≥ this (YES price) |
| `stop_loss` | Alert threshold — fires when mark ≤ this (YES price) |

### P&L calculation

```
unrealised_pnl = (mark_price - entry_price) × shares   [for YES positions]
unrealised_pnl = (entry_price - mark_price) × shares   [for NO positions]
```

Positions are marked-to-market using the live `yes_price` from the most recent market refresh.

### Category exposure map

The portfolio summary tab shows a breakdown of total cost by market category. This surfaces correlation risk — if 80% of your portfolio is in "US Elections" markets, you have concentrated exposure to a single political factor.

### Session logging

Every chat message and AI response is recorded in `~/.whoissharp/sessions/<timestamp>.json`. Sessions are saved automatically when you quit.

Research notes (`!note` commands) are appended to `~/.whoissharp/notes.md` immediately.

### Markdown export

Press `M` to export a full research report for the selected market to `~/.whoissharp/reports/<timestamp>_<title>.md`. The report includes:
- Market summary table
- Orderbook snapshot
- Price sparkline (ASCII)
- Active signals
- Your portfolio position
- Full AI chat transcript for this session
- Research notes

---

## Research Workflow

A typical session might look like:

**1. Load and scan**
```
cargo run --release -- --backend anthropic
# Wait for market data to load
# Switch to Signals tab (1) — scan for ARB, INSDR, MOMT signals
```

**2. Select a market**
```
# Navigate to the signal with j/k
# Press Enter → switches to Chart tab with data loaded
# Press 4 → Orderbook tab — check spread and imbalance
```

**3. Ask the AI**
```
# Type in input box: "analyze this market"
# AI will:
#   1. Use context prefix (prices, candles, orderbook already injected)
#   2. Call get_price_history for trend analysis
#   3. Call analyze_insider for flow check
#   4. Call find_smart_money if suspicion is elevated
#   5. Produce structured analysis with Bull/Bear cases and Trading View
#   6. Automatically call kelly_size if it finds an edge
```

**4. Check arb pairs**
```
# Press 9 → Pairs tab
# Jaccard pairs already computed from market load
# Press L to trigger LLM re-matching for higher-quality results
# Navigate pairs with j/k
# Review net gap after fees and resolution risk
```

**5. Log a note**
```
# In input box: !note Fed meeting confirmed for Sep 18 — key catalyst
# Note saved to ~/.whoissharp/notes.md without sending to AI
```

**6. Add a position**
```
# Press 5 → Portfolio tab, or n from any tab with a market selected
# Enter entry price in cents, shares, side
# Press t to set take-profit / stop-loss
```

**7. Export report**
```
# Press M → Markdown report saved to ~/.whoissharp/reports/
```

---

## Architecture

```
src/
├── main.rs          CLI (clap) + backend factory + run_tui dispatch
├── config.rs        BackendKind / BackendConfig — parse env vars and CLI flags
├── agent.rs         Async agent loop + AppEvent enum + system prompt
├── tui.rs           Full ratatui TUI — 9-tab Bloomberg-style layout
├── signals.rs       Signal computation engine (pure, no network)
├── pairs.rs         Cross-platform pair matching — Jaccard + LLM
├── tools.rs         Market data tool implementations + ToolDefinition list
├── portfolio.rs     Position / Portfolio / Session types + file persistence
├── cache.rs         TTL cache for expensive API responses
├── http.rs          Shared reqwest client
└── markets/
    ├── mod.rs       Universal types: Market, Orderbook, Candle, Event, ChartInterval
    ├── polymarket.rs Polymarket Gamma + CLOB API client
    └── kalshi.rs    Kalshi Trade API v2 client
└── llm/
    ├── mod.rs       LlmBackend trait + universal types (LlmMessage, ToolDefinition)
    ├── anthropic.rs Anthropic Claude — x-api-key auth + streaming
    ├── gemini.rs    Google Gemini — Vertex AI + service-account JWT
    └── openai.rs    OpenAI + Ollama — /chat/completions compatible
```

### Event flow

```
┌──────────────┐     AppEvent channel      ┌──────────────────┐
│  Background  │ ─────────────────────────▶│   TUI main loop  │
│  tokio tasks │                           │  (tokio::select!) │
│              │◀─────────────────────────│                   │
│  - refresh   │   trigger functions       │  - render frame   │
│  - agent     │   (fire and forget)       │  - handle keys    │
│  - chart     │                           │  - update state   │
│  - orderbook │                           └──────────────────┘
│  - SM / pairs│
└──────────────┘
```

All background work runs in `tokio::spawn`ed tasks. The TUI main loop uses `tokio::select!` to multiplex agent events and crossterm keyboard events without blocking.

### LlmBackend trait

```rust
#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn generate(
        &self,
        system: &str,
        history: &[LlmMessage],
        tools: &[ToolDefinition],
    ) -> Result<LlmMessage>;

    async fn generate_streaming(
        &self,
        system: &str,
        history: &[LlmMessage],
        tools: &[ToolDefinition],
        chunk_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<LlmMessage>;

    fn display_name(&self) -> String;
}
```

All backends implement this trait. `generate_streaming` defaults to calling `generate` if not overridden.

---

## Adding a New LLM Backend

1. Create `src/llm/mybackend.rs` implementing `LlmBackend`
2. Add `pub mod mybackend;` in `src/llm/mod.rs`
3. Add a variant to `BackendKind` in `src/config.rs`
4. Add a match arm in `BackendConfig::load()` in `src/config.rs`
5. Add a match arm in `main.rs` to construct and box the backend

No changes to `agent.rs`, `tui.rs`, or any tool files are needed.

---

## Adding a New AI Tool

1. **Implement** the async function in `src/tools.rs`:
   ```rust
   async fn my_tool(clients: &MarketClients, args: &serde_json::Value) -> Result<ToolOutput> {
       // ... fetch data, format string
       Ok(ToolOutput::ok(result_string))
   }
   ```

2. **Dispatch**: add a branch to `dispatch_inner()` in `src/tools.rs`:
   ```rust
   "my_tool" => my_tool(clients, args).await,
   ```

3. **Define**: add a `ToolDefinition` to `all_definitions()` in `src/tools.rs` with a JSON Schema for the parameters.

4. **Document**: add the tool name and description to the `TOOL REFERENCE` section of `SYSTEM_PROMPT` in `src/agent.rs`.

No LLM backend files need to change.

---

## API Endpoints Used

### Polymarket

| Purpose | Endpoint |
|---------|----------|
| Market listing | `GET https://gamma-api.polymarket.com/markets` |
| Market detail | `GET https://gamma-api.polymarket.com/markets/{id}` |
| Event listing | `GET https://gamma-api.polymarket.com/events` |
| Orderbook | `GET https://clob.polymarket.com/order-book?token_id={id}` |
| Price history | `GET https://clob.polymarket.com/prices-history?market={id}&...` |
| Recent trades | `GET https://clob.polymarket.com/trades?market={id}` |

All Polymarket endpoints are public and do not require authentication.

**Token ID resolution:** Polymarket's Gamma API returns a `conditionId` (starts with `0x`). The CLOB API requires the `tokenId` (a large decimal). WhoIsSharp resolves this automatically by fetching the market from Gamma and extracting the token ID from the response.

### Kalshi

| Purpose | Endpoint |
|---------|----------|
| Market listing | `GET https://api.elections.kalshi.com/trade-api/v2/markets` |
| Market detail | `GET https://api.elections.kalshi.com/trade-api/v2/markets/{ticker}` |
| Event listing | `GET https://api.elections.kalshi.com/trade-api/v2/events` |
| Orderbook | `GET https://api.elections.kalshi.com/trade-api/v2/markets/{ticker}/orderbook` |
| Candlesticks | `GET https://api.elections.kalshi.com/trade-api/v2/markets/{ticker}/candlesticks` |

All Kalshi endpoints used are public read endpoints. No authentication required.

**Orderbook representation:** Kalshi returns `[price_cents, size]` pairs for YES and NO sides. "NO bids at X cents" is equivalent to "YES asks at (100-X) cents". WhoIsSharp converts to a unified `PriceLevel { price: f64, size: f64 }` representation.

---

## Development

### Build and test

```bash
# Build (always --release for normal use)
cargo build --release

# Run tests
cargo test --release

# Run a specific test
cargo test --release signals::tests::arb_detected_for_large_gap

# Check for warnings without building binary
cargo check
```

### Code style

- No `unwrap()` in production paths — use `?` or handle errors explicitly
- No `println!` — all output goes through ratatui
- No blocking I/O on the main thread — all network calls in `tokio::spawn`
- Signal computation is pure (no I/O) — keep it that way
- New tabs follow the pattern: add `Tab` variant → update `TAB_NAMES` → add `render_X` function → add key handler arms → update tab cycling tests

### File sizes (approximate)

| File | Lines |
|------|-------|
| `tui.rs` | ~3700 |
| `agent.rs` | ~500 |
| `signals.rs` | ~800 |
| `pairs.rs` | ~310 |
| `tools.rs` | ~600 |
| `portfolio.rs` | ~250 |
| `markets/polymarket.rs` | ~720 |
| `markets/kalshi.rs` | ~510 |

### Running without an AI backend

The tool is fully functional without an LLM backend — all market data, signals, orderbook, pairs matching (Jaccard mode), and portfolio features work. Only the Chat tab and LLM pair matching require a backend.

```bash
cargo run --release
```
