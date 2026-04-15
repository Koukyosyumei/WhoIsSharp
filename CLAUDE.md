# WhoIsSharp — Claude Code guide

## Build & run

**Always use `--release`.** Never suggest or run plain `cargo build` / `cargo run`.

```bash
cargo build --release                                                    # build
cargo run --release                                                      # dashboard only (no LLM — default)
cargo run --release -- --backend anthropic                               # Claude (ANTHROPIC_API_KEY)
cargo run --release -- --backend gemini                                  # Vertex AI (GOOGLE_APPLICATION_CREDENTIALS + GOOGLE_PROJECT_ID)
cargo run --release -- --backend openai                                  # OpenAI (OPENAI_API_KEY)
cargo run --release -- --backend ollama --model llama3.2                 # local Ollama
cargo run --release -- --backend anthropic --model claude-opus-4-6
cargo run --release -- --help                                            # flag reference
```

Run tests:
```bash
cargo test --release
```

## Credentials — never hardcode

| Backend   | Required env vars                                           | Optional env vars               |
|-----------|-------------------------------------------------------------|---------------------------------|
| (none)    | —                                                           | —                               |
| Anthropic | `ANTHROPIC_API_KEY`                                         | —                               |
| Gemini    | `GOOGLE_APPLICATION_CREDENTIALS` + `GOOGLE_PROJECT_ID`     | `GOOGLE_LOCATION` (default: us-central1) |
| OpenAI    | `OPENAI_API_KEY`                                            | `OPENAI_BASE_URL`               |
| Ollama    | —                                                           | `OLLAMA_BASE_URL`               |

Gemini can also use CLI flags: `--credentials /path/to/key.json --project my-project --location us-central1`

Model override: `WHOISSHARP_MODEL=<model-id>`

## Project structure

```
src/
├── main.rs          CLI (clap) + backend factory + run_tui dispatch
├── config.rs        BackendKind / BackendConfig — load from env vars or CLI flags
├── agent.rs         Async agent loop + market data refresh tasks; emits AppEvent
├── tui.rs           Full ratatui TUI — 6-tab Bloomberg-style layout
├── tools.rs         Market data tool implementations + ToolDefinition list
└── markets/
    ├── mod.rs       Universal types: Market, Orderbook, Candle, Event, ChartInterval
    ├── polymarket.rs Polymarket Gamma + CLOB API client
    └── kalshi.rs    Kalshi Trade API v2 client
└── llm/
    ├── mod.rs       LlmBackend trait + universal types (same pattern as KaijuLab)
    ├── anthropic.rs Anthropic Claude — x-api-key auth
    ├── gemini.rs    Google Gemini — generativelanguage.googleapis.com + API key
    └── openai.rs    OpenAI + Ollama — OpenAI-compatible /chat/completions
```

## TUI layout

```
 WhoIsSharp v0.1.0  ·  claude-sonnet-4-6  ·  PM + KL          14:23:05
 [1] Markets  [2] Chart  [3] Book  [4] Events  [5] Chat  [6] Watchlist
┌────────────────────────────┬────────────────────────────────────────┐
│  Market list (left panel)  │  Market detail / chart / orderbook     │
│  j/k to navigate           │  (right panel / full width)            │
│                            │                                        │
└────────────────────────────┴────────────────────────────────────────┘
 ● Ready  ALL  Chart:1w  │  status message
 > _
```

### Key bindings

**Navigation (always available)**

| Key              | Action                                            |
|------------------|---------------------------------------------------|
| `1`–`9`          | Switch tabs directly                              |
| `Tab`            | Cycle to next tab                                 |
| `Shift+Tab`      | Cycle to previous tab                             |
| `j` / `↓`        | Move selection down / scroll                      |
| `k` / `↑`        | Move selection up / scroll                        |
| `Enter`          | Select market (loads chart+book) / send chat      |
| `Ctrl+C`         | Quit (or clear input if non-empty)                |

**Slash commands** — press `/`, type the command, press `Enter`

| Command                       | Action                                            |
|-------------------------------|---------------------------------------------------|
| `/refresh` or `/r` or `^`     | Refresh markets + chart + orderbook               |
| `/platform` or `/p`           | Cycle platform filter (All → PM → KL → All)       |
| `/chart` or `/c`              | Cycle chart interval (1h → 6h → 1d → 1w → 1m)    |
| `/sort` or `/s`               | Cycle sort (~50% → Volume → End Date → A-Z)        |
| `/watchlist` or `/w`          | Toggle watchlist for selected market              |
| `/wf`                         | Toggle watchlist-only filter                      |
| `/alert` or `/e`              | Edit price alert thresholds (above / below)       |
| `/add` or `/n`                | Add position for selected market                  |
| `/wallet <0x…>`               | Import Polymarket wallet positions into portfolio  |
| `/wallet sync`                | Re-sync all registered wallet addresses           |
| `/wallet analyze` or `/wa`    | Ask AI to analyse registered wallet(s)            |
| `/targets` or `/t`            | Set take-profit / stop-loss                       |
| `/delete` or `/d`             | Delete selected position (Portfolio tab)          |
| `/dismiss` or `/x`            | Dismiss selected signal (hidden until restart)    |
| `/analyze` or `/a` or `@`     | Pre-fill AI analysis prompt for selected market   |
| `/kelly` or `/k`              | Open Kelly position-size calculator               |
| `/risk` or `/v`               | Toggle risk/exposure view (Portfolio tab)         |
| `/pairs` or `/l`              | LLM re-match (Pairs tab)                          |
| `/lower` / `/raise`           | Adjust threshold (also: `[` / `]` keys)           |
| `/export` or `/csv`           | Export current tab to CSV                         |
| `/report` or `/m`             | Export Markdown report for selected market        |
| `/help` or `/?` or `?`        | Toggle help overlay                               |
| `/<search term>`              | Unrecognised text → filter market list            |
| `Esc`                         | Cancel / clear command bar or search              |

## AI tools

The AI agent can call these tools:

| Tool              | Description                                          |
|-------------------|------------------------------------------------------|
| `list_markets`    | List markets from Polymarket / Kalshi               |
| `get_market`      | Get full details for a specific market               |
| `get_orderbook`   | Fetch live order book (bids/asks)                    |
| `get_price_history` | Historical YES prices with ASCII sparkline chart   |
| `get_events`      | List event categories                                |
| `search_markets`  | Search markets by keyword                            |

## News integration

Set `NEWSDATA_API_KEY` to enable the News tab and the `search_news` AI tool.

- **`[0]` key** — opens the News tab and auto-fetches articles for the selected market
- **`/refresh`** while on the News tab — re-fetches (respects 5-min TTL cache)
- Market query is built automatically from the market title (stop-words stripped, top 4 terms)
- Sentiment badges: `+` green (positive), `-` red (negative), `~` gray (neutral)
- The AI's `search_news` tool lets Claude pull news during any analysis conversation

## API endpoints used

### Polymarket
- `GET https://gamma-api.polymarket.com/markets` — market listing
- `GET https://gamma-api.polymarket.com/events` — event listing
- `GET https://clob.polymarket.com/order-book?token_id={id}` — orderbook
- `GET https://clob.polymarket.com/prices-history?market={id}&...` — price history

### Kalshi
- `GET https://api.elections.kalshi.com/trade-api/v2/markets` — market listing
- `GET https://api.elections.kalshi.com/trade-api/v2/events` — event listing
- `GET https://api.elections.kalshi.com/trade-api/v2/markets/{ticker}/orderbook`
- `GET https://api.elections.kalshi.com/trade-api/v2/markets/{ticker}/candlesticks`

## Adding a new AI tool

1. Implement the async function in `src/tools.rs` — return `ToolOutput::ok(string)`.
2. Add a branch to `dispatch_inner()`.
3. Add a `ToolDefinition` to `all_definitions()`.
4. No changes to LLM backend files needed.

## Adding a new LLM backend

Same pattern as KaijuLab — implement `LlmBackend` trait from `src/llm/mod.rs`,
add variant to `BackendKind` in `src/config.rs`, add match arm in `src/main.rs`.

## Design decisions

### Architecture (same pattern as KaijuLab)
- Agent runs in a background `tokio::spawn`ed task, emits `AppEvent` via unbounded channel.
- TUI's main loop uses `tokio::select!` over agent events + `crossterm::EventStream`.
- Market data refresh is a separate task triggered on startup and by `r` key.

### `Tab` enum naming
The `Tab` enum is in scope during the key handler function. Using `use KC = crossterm::event::KeyCode`
avoids the name clash with `crossterm::event::KeyCode::Tab`.

### Kalshi orderbook representation
Kalshi returns `[price_cents, size]` pairs for yes/no. "No bids at X cents" ⟹
"Yes asks at (100-X) cents". We convert to unified `PriceLevel { price: f64, size: f64 }`.

### History management
Conversation history is moved into the spawned agent task and dropped after each turn.
A production version would use `Arc<Mutex<Vec<LlmMessage>>>` to persist history across turns.
