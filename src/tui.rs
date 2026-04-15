//! Terminal UI — Bloomberg-style prediction market dashboard.
//!
//! Layout:
//!   ┌─ header (1 line: title + backend + time) ──────────────────────────────┐
//!   │ tab bar (1 line)                                                        │
//!   ├─ content (fills remaining height) ────────────────────────────────────┤
//!   │ status bar (1 line)                                                     │
//!   │ input box (1 line)                                                      │
//!   └────────────────────────────────────────────────────────────────────────┘
//!
//! Tabs: [1] Signals  [2] Markets  [3] Chart  [4] Book  [5] Portfolio  [6] Chat

use std::io;
use std::sync::Arc;

use crossterm::{
    event::{Event, EventStream, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Chart, Clear, Dataset, GraphType,
        List, ListItem, ListState, Paragraph, Tabs, Wrap,
    },
    Frame, Terminal,
};
use tokio::sync::mpsc;

use std::collections::{HashMap, HashSet};

use crate::agent::{self, AppEvent};
use crate::llm::{LlmBackend, LlmMessage};
use crate::news::NewsArticle;
use crate::markets::{ChartInterval, Market, Orderbook, Platform};
use crate::markets::polymarket::PolyTrade;
use crate::portfolio::{self, Portfolio, Position, Session, Side, WatchEntry};
use crate::signals::{self, Signal, SignalKind};
use crate::tools::{MarketClients, SmartMoneyWallet};

// ─── Tabs ────────────────────────────────────────────────────────────────────

const TAB_NAMES: &[&str] = &["Signals", "Markets", "Chart", "Book", "Portfolio", "Chat", "SmartMoney", "Trades", "Pairs", "News"];

const TIPS: &[&str] = &[
    "Press / to open the command bar — type a command and press Enter",
    "Try /help to see all available commands",
    "Press 1-9 or Tab/Shift+Tab to switch tabs",
    "Press j/k or arrow keys to navigate the market list",
    "Press Enter on a market to load its chart and order book",
    "Press ^ or type /refresh to refresh market data now",
    "Type /watchlist to add/remove the selected market from your watchlist",
    "Type /platform to cycle the filter: All → PM → KL",
    "Type /chart to cycle chart interval: 1h → 6h → 1d → 1w → 1m",
    "Type a search term after / to filter markets by keyword",
    "Type /analyze to pre-fill an AI analysis prompt for the selected market",
    "Type /sort to cycle market sort: ~50% → Volume → End date → A-Z",
    "Type /kelly to open the Kelly position-size calculator",
    "In Chat (tab 6), type a question and press Enter to ask the AI",
    "Press Ctrl+C to quit (saves session automatically)",
    "Type /wf to focus on starred markets only",
    "Type /dismiss on a signal to hide it for the current session",
    "Type /risk in Portfolio to toggle the risk/exposure view",
];


#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Signals    = 0,
    Markets    = 1,
    Chart      = 2,
    Orderbook  = 3,
    Portfolio  = 4,
    Chat       = 5,
    SmartMoney = 6,
    Trades     = 7,
    Pairs      = 8,
    News       = 9,
}

impl Tab {
    fn from_index(n: usize) -> Option<Self> {
        match n {
            0 => Some(Tab::Signals),
            1 => Some(Tab::Markets),
            2 => Some(Tab::Chart),
            3 => Some(Tab::Orderbook),
            4 => Some(Tab::Portfolio),
            5 => Some(Tab::Chat),
            6 => Some(Tab::SmartMoney),
            7 => Some(Tab::Trades),
            8 => Some(Tab::Pairs),
            9 => Some(Tab::News),
            _ => None,
        }
    }
    fn next(self) -> Self {
        Tab::from_index((self as usize + 1) % TAB_NAMES.len()).unwrap()
    }
    fn prev(self) -> Self {
        Tab::from_index((self as usize + TAB_NAMES.len() - 1) % TAB_NAMES.len()).unwrap()
    }
}

// ─── Chat message ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum ChatMsg {
    User(String),
    Assistant(String),
    ToolCall { name: String, args: String },
    ToolResult { name: String, preview: String },
    Error(String),
}

// ─── Platform filter ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlatformFilter { All, Polymarket, Kalshi }

impl PlatformFilter {
    pub fn matches(&self, p: &Platform) -> bool {
        match self {
            PlatformFilter::All        => true,
            PlatformFilter::Polymarket => p == &Platform::Polymarket,
            PlatformFilter::Kalshi     => p == &Platform::Kalshi,
        }
    }
    pub fn label(&self) -> &str {
        match self {
            PlatformFilter::All        => "ALL",
            PlatformFilter::Polymarket => "PM",
            PlatformFilter::Kalshi     => "KL",
        }
    }
}

// ─── Market sort ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarketSort {
    /// Closest to 50% first (default — most uncertain / tradeable).
    YesPrice,
    /// Highest volume first.
    Volume,
    /// Earliest end date first (market calendar view).
    EndDate,
    /// Alphabetical by title.
    Name,
}

impl MarketSort {
    pub fn label(&self) -> &str {
        match self {
            MarketSort::YesPrice => "~50%",
            MarketSort::Volume   => "Vol",
            MarketSort::EndDate  => "End",
            MarketSort::Name     => "A-Z",
        }
    }
    pub fn next(self) -> Self {
        match self {
            MarketSort::YesPrice => MarketSort::Volume,
            MarketSort::Volume   => MarketSort::EndDate,
            MarketSort::EndDate  => MarketSort::Name,
            MarketSort::Name     => MarketSort::YesPrice,
        }
    }
}

// ─── Alert edit step ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AlertEditStep { #[default] Above, Below }

// ─── Kelly calculator step ────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum KellyStep {
    #[default]
    MyProb,    // user types their probability estimate
    Bankroll,  // user types their bankroll in dollars
    Result,    // displaying the recommendation
}

// ─── App state ───────────────────────────────────────────────────────────────

pub struct App {
    // Data
    pub markets:           Vec<Market>,
    pub signals:           Vec<Signal>,
    pub portfolio:         Portfolio,
    pub orderbook:         Option<Orderbook>,
    pub chart_data:        Vec<(f64, f64)>,
    pub chart_min:         f64,
    pub chart_max:         f64,

    // Navigation
    pub active_tab:        Tab,
    pub market_list:       ListState,
    pub signal_list:       ListState,
    pub portfolio_list:    ListState,
    pub chat_scroll:       u16,
    pub book_scroll:       u16,

    // Filter / search
    pub platform_filter:   PlatformFilter,
    pub search:            String,      // active market filter (persists after command bar closes)
    pub command_input:     String,      // text being typed in the command bar (not a live filter)
    pub search_mode:       bool,
    pub chart_interval:    ChartInterval,

    // Chat
    pub chat_msgs:         Vec<ChatMsg>,
    pub input:             String,
    pub sent_history:      Vec<String>,
    pub sent_cursor:       Option<usize>,

    // Portfolio add-position mode
    pub pos_input_mode:    bool,
    pub pos_input_step:    PosInputStep,
    pub pos_draft:         PosDraft,

    // Smart Money tab
    pub sm_wallets:         Vec<SmartMoneyWallet>,
    pub sm_market_title:    String,
    pub sm_coord_pairs:     Vec<(String, String, f64)>,
    pub sm_loading:         bool,
    pub sm_list:            ListState,
    // Wallet drill-down detail (Enter on a wallet row)
    pub sm_detail:          Option<crate::tools::WalletDetail>,
    pub sm_detail_loading:  bool,
    pub sm_detail_scroll:   u16,

    // Time & Sales tab
    pub trades_data:      Vec<PolyTrade>,
    pub trades_list:      ListState,

    // Chart: full candle data for volume overlay
    pub chart_candles:    Vec<crate::markets::Candle>,

    // Market sort
    pub market_sort:      MarketSort,

    // Watchlist
    pub watchlist:        Vec<WatchEntry>,
    pub watchlist_only:   bool,
    pub watch_alerts:     Vec<String>,  // recent alert messages

    // Alert threshold editor
    pub alert_edit_mode:  bool,
    pub alert_edit_step:  AlertEditStep,
    pub alert_edit_mkt:   String,   // market_id being edited

    // Help overlay
    pub show_help:        bool,

    // Auto-refresh state
    pub refresh_secs:     u64,
    pub next_refresh_at:  Option<std::time::Instant>,

    // Status
    pub status:            String,
    pub is_loading:        bool,
    pub backend_name:      String,
    pub last_updated:      Option<chrono::DateTime<chrono::Local>>,

    // Selected market ID (for chart / orderbook loading)
    pub selected_market_id: Option<String>,

    // Price velocity: previous YES prices from the last refresh cycle (for momentum signals)
    pub prev_prices:      HashMap<String, f64>,

    // Dismissed signal IDs (persisted in session; `x` key)
    pub dismissed_signals: HashSet<String>,

    // Session (chat persistence)
    pub session:           Session,

    // Stop/take-profit target input mode for portfolio positions
    pub target_input_mode: bool,
    pub target_input_step: TargetInputStep,
    pub target_pos_id:     String,   // position being edited

    // Adjustable thresholds (user-configurable in TUI, also settable by AI tools)
    /// Jaccard market-overlap threshold for wallet coordination detection (0–1).
    pub coord_threshold:         f64,
    /// Jaccard word-set threshold for Pairs tab cross-platform matching (0–1).
    pub pairs_jaccard_threshold: f64,

    // Cross-platform pairs (Pairs tab)
    pub pairs:             Vec<crate::pairs::MarketPair>,
    pub pairs_cursor:      usize,
    pub pairs_loading:     bool,

    // Portfolio risk view toggle (v key in Portfolio tab)
    pub show_risk_view:    bool,

    // Startup loading spinner (cycles 0–9)
    pub spinner_tick:      u8,

    // Rotating tips index (cycles through TIPS on each auto-refresh)
    pub tip_index:         usize,

    // Kelly position sizer modal (k key)
    pub kelly_mode:        bool,
    pub kelly_step:        KellyStep,
    pub kelly_input:       String,
    pub kelly_my_prob:     Option<f64>,
    pub kelly_bankroll:    f64,    // remembered across calls

    // Registered Polymarket wallet addresses for portfolio sync
    pub wallet_addresses:  Vec<String>,

    // News tab
    pub news_articles:   Vec<NewsArticle>,
    pub news_list:       ListState,
    /// market_id whose news is currently displayed (None = no news loaded yet)
    pub news_market_id:  Option<String>,
    pub news_loading:    bool,
    /// Index of the article expanded in the detail panel (right side)
    pub news_detail_idx: Option<usize>,
    /// Last error message from the news fetch (shown inside the tab)
    pub news_error:      Option<String>,
}

// Position add-flow state machine
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub enum PosInputStep {
    #[default]
    EntryPrice,
    Shares,
    Side,
    Note,
}

// Stop / take-profit target input state machine
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub enum TargetInputStep {
    #[default]
    TakeProfit,
    StopLoss,
}

#[derive(Default, Clone, Debug)]
pub struct PosDraft {
    pub market_id:   String,
    pub title:       String,
    pub platform:    Option<Platform>,
    pub entry_price: Option<f64>,
    pub shares:      Option<f64>,
    pub side:        Option<Side>,
}

impl App {
    pub fn new(backend_name: String) -> Self {
        App {
            markets:           Vec::new(),
            signals:           Vec::new(),
            portfolio:         portfolio::load_portfolio(),
            orderbook:         None,
            chart_data:        Vec::new(),
            chart_min:         0.0,
            chart_max:         100.0,
            active_tab:        Tab::Signals,
            market_list:       ListState::default(),
            signal_list:       ListState::default(),
            portfolio_list:    ListState::default(),
            chat_scroll:       0,
            book_scroll:       0,
            platform_filter:   PlatformFilter::All,
            search:            String::new(),
            command_input:     String::new(),
            search_mode:       false,
            chart_interval:    ChartInterval::OneWeek,
            chat_msgs:         Vec::new(),
            input:             String::new(),
            sent_history:      Vec::new(),
            sent_cursor:       None,
            pos_input_mode:    false,
            pos_input_step:    PosInputStep::default(),
            pos_draft:         PosDraft::default(),
            sm_wallets:         Vec::new(),
            sm_market_title:    String::new(),
            sm_coord_pairs:     Vec::new(),
            sm_loading:         false,
            sm_list:            ListState::default(),
            sm_detail:          None,
            sm_detail_loading:  false,
            sm_detail_scroll:   0,
            trades_data:       Vec::new(),
            trades_list:       ListState::default(),
            chart_candles:     Vec::new(),
            market_sort:       MarketSort::YesPrice,
            watchlist:         portfolio::load_watchlist(),
            watchlist_only:    false,
            watch_alerts:      Vec::new(),
            alert_edit_mode:   false,
            alert_edit_step:   AlertEditStep::default(),
            alert_edit_mkt:    String::new(),
            show_help:         false,
            refresh_secs:      60,
            next_refresh_at:   None,
            status:            "Loading market data…".to_string(),
            is_loading:        true,
            backend_name,
            last_updated:      None,
            selected_market_id: None,
            prev_prices:        HashMap::new(),
            dismissed_signals:  HashSet::new(),
            session:            Session {
                started_at: chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string(),
                messages:   Vec::new(),
                notes:      Vec::new(),
            },
            target_input_mode: false,
            target_input_step: TargetInputStep::default(),
            target_pos_id:     String::new(),
            coord_threshold:         0.35,
            pairs_jaccard_threshold: 0.35,
            pairs:             Vec::new(),
            pairs_cursor:      0,
            pairs_loading:     false,
            show_risk_view:    false,
            spinner_tick:      0,
            tip_index:         0,
            kelly_mode:        false,
            kelly_step:        KellyStep::default(),
            kelly_input:       String::new(),
            kelly_my_prob:     None,
            kelly_bankroll:    10_000.0,
            wallet_addresses:  portfolio::load_wallets(),
            news_articles:     Vec::new(),
            news_list:         ListState::default(),
            news_market_id:    None,
            news_loading:      false,
            news_detail_idx:   None,
            news_error:        None,
        }
    }

    // ── Filtered markets ──────────────────────────────────────────────────────

    pub fn filtered_markets(&self) -> Vec<&Market> {
        let mut v: Vec<&Market> = self.markets
            .iter()
            .filter(|m| {
                self.platform_filter.matches(&m.platform)
                    && (!self.watchlist_only || self.watchlist.iter().any(|w| w.market_id == m.id))
                    && (self.search.is_empty()
                        || m.title.to_lowercase().contains(&self.search.to_lowercase()))
            })
            .collect();

        match self.market_sort {
            MarketSort::YesPrice => {
                // Default: closest to 50% first (already sorted this way from refresh)
            }
            MarketSort::Volume => {
                v.sort_by(|a, b| {
                    b.volume.unwrap_or(0.0)
                        .partial_cmp(&a.volume.unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            MarketSort::EndDate => {
                v.sort_by(|a, b| {
                    a.end_date.as_deref().unwrap_or("9999-99-99")
                        .cmp(b.end_date.as_deref().unwrap_or("9999-99-99"))
                });
            }
            MarketSort::Name => {
                v.sort_by(|a, b| a.title.cmp(&b.title));
            }
        }
        v
    }

    pub fn is_watched(&self, market_id: &str) -> bool {
        self.watchlist.iter().any(|w| w.market_id == market_id)
    }

    pub fn toggle_watchlist(&mut self, market: &Market) {
        if self.is_watched(&market.id) {
            self.watchlist.retain(|w| w.market_id != market.id);
            self.status = format!("Removed '{}' from watchlist", &market.title[..market.title.len().min(40)]);
        } else {
            self.watchlist.push(WatchEntry::new(market.id.clone(), market.title.clone()));
            self.status = format!("Added '{}' to watchlist  [★ {} watched]", &market.title[..market.title.len().min(30)], self.watchlist.len());
        }
        let _ = portfolio::save_watchlist(&self.watchlist);
    }

    /// Check watched markets against live prices and collect alert messages.
    pub fn check_watch_alerts(&mut self) {
        self.watch_alerts.clear();
        for entry in &self.watchlist {
            if let Some(m) = self.markets.iter().find(|m| m.id == entry.market_id) {
                if entry.alert_above < 1.0 && m.yes_price > entry.alert_above {
                    self.watch_alerts.push(format!(
                        "⚡ {} crossed ABOVE {:.0}¢ (now {:.0}¢)",
                        &m.title[..m.title.len().min(30)],
                        entry.alert_above * 100.0,
                        m.yes_price * 100.0,
                    ));
                }
                if entry.alert_below > 0.0 && m.yes_price < entry.alert_below {
                    self.watch_alerts.push(format!(
                        "⚡ {} dropped BELOW {:.0}¢ (now {:.0}¢)",
                        &m.title[..m.title.len().min(30)],
                        entry.alert_below * 100.0,
                        m.yes_price * 100.0,
                    ));
                }
            }
        }
    }

    /// Check portfolio positions against stop-loss / take-profit thresholds.
    /// Fires alerts into `watch_alerts` alongside watchlist alerts.
    pub fn check_position_alerts(&mut self) {
        for pos in &self.portfolio.positions {
            let mark = match pos.mark_price {
                Some(m) => m,
                None    => continue,
            };
            let title_short = &pos.title[..pos.title.len().min(28)];
            if let Some(tp) = pos.take_profit {
                if mark >= tp {
                    self.watch_alerts.push(format!(
                        "🎯 {} {} hit TAKE-PROFIT @ {:.0}¢ (mark {:.0}¢)",
                        pos.side.label(), title_short,
                        tp * 100.0, mark * 100.0,
                    ));
                }
            }
            if let Some(sl) = pos.stop_loss {
                if mark <= sl {
                    self.watch_alerts.push(format!(
                        "🛑 {} {} hit STOP-LOSS @ {:.0}¢ (mark {:.0}¢)",
                        pos.side.label(), title_short,
                        sl * 100.0, mark * 100.0,
                    ));
                }
            }
        }
    }

    pub fn selected_market(&self) -> Option<&Market> {
        let filtered = self.filtered_markets();
        let idx = self.market_list.selected()?;
        filtered.get(idx).copied()
    }

    pub fn selected_signal(&self) -> Option<&Signal> {
        let idx = self.signal_list.selected()?;
        self.signals.get(idx)
    }

    // ── List navigation ───────────────────────────────────────────────────────

    pub fn list_down(&mut self) {
        match self.active_tab {
            Tab::Signals => {
                let len = self.signals.len();
                if len == 0 { return; }
                let i = self.signal_list.selected().map(|i| (i + 1) % len).unwrap_or(0);
                self.signal_list.select(Some(i));
            }
            Tab::Markets => {
                let len = self.filtered_markets().len();
                if len == 0 { return; }
                let i = self.market_list.selected().map(|i| (i + 1) % len).unwrap_or(0);
                self.market_list.select(Some(i));
            }
            Tab::Portfolio => {
                let len = self.portfolio.positions.len();
                if len == 0 { return; }
                let i = self.portfolio_list.selected().map(|i| (i + 1) % len).unwrap_or(0);
                self.portfolio_list.select(Some(i));
            }
            Tab::Chat       => { self.chat_scroll = self.chat_scroll.saturating_sub(1); } // j = scroll toward bottom
            Tab::Orderbook  => { self.book_scroll = self.book_scroll.saturating_add(1); }
            Tab::SmartMoney => {
                if self.sm_detail.is_some() || self.sm_detail_loading {
                    // In detail view: scroll down through trade history
                    self.sm_detail_scroll = self.sm_detail_scroll.saturating_add(1);
                } else {
                    // In list view: navigate wallets
                    let len = self.sm_wallets.len() + 2;
                    if len <= 2 { return; }
                    let i = self.sm_list.selected().map(|i| {
                        if i + 1 >= len { 2 } else { i + 1 }
                    }).unwrap_or(2);
                    self.sm_list.select(Some(i));
                }
            }
            Tab::Trades => {
                let len = self.trades_data.len();
                if len == 0 { return; }
                let i = self.trades_list.selected().map(|i| (i + 1) % len).unwrap_or(0);
                self.trades_list.select(Some(i));
            }
            Tab::Pairs => {
                if !self.pairs.is_empty() {
                    self.pairs_cursor = (self.pairs_cursor + 1) % self.pairs.len();
                }
            }
            Tab::News => {
                let len = self.news_articles.len();
                if len == 0 { return; }
                let i = self.news_list.selected().map(|i| (i + 1) % len).unwrap_or(0);
                self.news_list.select(Some(i));
                self.news_detail_idx = Some(i);
            }
            _ => {}
        }
    }

    pub fn list_up(&mut self) {
        match self.active_tab {
            Tab::Signals => {
                let len = self.signals.len();
                if len == 0 { return; }
                let i = self.signal_list.selected()
                    .map(|i| if i == 0 { len - 1 } else { i - 1 })
                    .unwrap_or(0);
                self.signal_list.select(Some(i));
            }
            Tab::Markets => {
                let len = self.filtered_markets().len();
                if len == 0 { return; }
                let i = self.market_list.selected()
                    .map(|i| if i == 0 { len - 1 } else { i - 1 })
                    .unwrap_or(0);
                self.market_list.select(Some(i));
            }
            Tab::Portfolio => {
                let len = self.portfolio.positions.len();
                if len == 0 { return; }
                let i = self.portfolio_list.selected()
                    .map(|i| if i == 0 { len - 1 } else { i - 1 })
                    .unwrap_or(0);
                self.portfolio_list.select(Some(i));
            }
            Tab::Chat       => { self.chat_scroll = self.chat_scroll.saturating_add(1); } // k = scroll toward top
            Tab::Orderbook  => { self.book_scroll = self.book_scroll.saturating_sub(1); }
            Tab::SmartMoney => {
                if self.sm_detail.is_some() || self.sm_detail_loading {
                    // In detail view: scroll up through trade history
                    self.sm_detail_scroll = self.sm_detail_scroll.saturating_sub(1);
                } else {
                    // In list view: navigate wallets
                    let len = self.sm_wallets.len() + 2;
                    if len <= 2 { return; }
                    let i = self.sm_list.selected()
                        .map(|i| if i <= 2 { len - 1 } else { i - 1 })
                        .unwrap_or(2);
                    self.sm_list.select(Some(i));
                }
            }
            Tab::Trades => {
                let len = self.trades_data.len();
                if len == 0 { return; }
                let i = self.trades_list.selected()
                    .map(|i| if i == 0 { len - 1 } else { i - 1 })
                    .unwrap_or(0);
                self.trades_list.select(Some(i));
            }
            Tab::Pairs => {
                if !self.pairs.is_empty() {
                    self.pairs_cursor = if self.pairs_cursor == 0 {
                        self.pairs.len() - 1
                    } else {
                        self.pairs_cursor - 1
                    };
                }
            }
            Tab::News => {
                let len = self.news_articles.len();
                if len == 0 { return; }
                let i = self.news_list.selected()
                    .map(|i| if i == 0 { len - 1 } else { i - 1 })
                    .unwrap_or(0);
                self.news_list.select(Some(i));
                self.news_detail_idx = Some(i);
            }
            _ => {}
        }
    }

    // ── Input history (↑/↓ in chat input) ────────────────────────────────────

    pub fn history_up(&mut self) {
        if self.sent_history.is_empty() { return; }
        let new_cursor = match self.sent_cursor {
            None    => Some(self.sent_history.len() - 1),
            Some(0) => Some(0),
            Some(i) => Some(i - 1),
        };
        self.sent_cursor = new_cursor;
        if let Some(i) = self.sent_cursor {
            self.input = self.sent_history[i].clone();
        }
    }

    pub fn history_down(&mut self) {
        if self.sent_history.is_empty() { return; }
        let new_cursor = self.sent_cursor
            .and_then(|i| if i + 1 >= self.sent_history.len() { None } else { Some(i + 1) });
        self.sent_cursor = new_cursor;
        self.input = new_cursor
            .map(|i| self.sent_history[i].clone())
            .unwrap_or_default();
    }

    // ── Portfolio helpers ─────────────────────────────────────────────────────

    pub fn delete_selected_position(&mut self) {
        if let Some(idx) = self.portfolio_list.selected() {
            if let Some(pos) = self.portfolio.positions.get(idx) {
                let id = pos.id.clone();
                self.portfolio.remove(&id);
                let _ = portfolio::save_portfolio(&self.portfolio);
                if self.portfolio.positions.is_empty() {
                    self.portfolio_list.select(None);
                } else {
                    let new_idx = idx.min(self.portfolio.positions.len() - 1);
                    self.portfolio_list.select(Some(new_idx));
                }
                self.status = "Position deleted.".to_string();
            }
        }
    }

    pub fn update_portfolio_marks(&mut self) {
        let pairs: Vec<(Platform, String, f64)> = self.markets
            .iter()
            .map(|m| (m.platform.clone(), m.id.clone(), m.yes_price))
            .collect();
        self.portfolio.update_marks(pairs.into_iter());
    }

    pub fn start_add_position(&mut self) {
        let market_info = self.selected_market().map(|m| {
            (m.id.clone(), m.title.clone(), m.platform.clone(), m.yes_price)
        });
        if let Some((id, title, platform, yes_price)) = market_info {
            let pct = yes_price * 100.0;
            let status = format!(
                "Add position: {} [{:.1}¢] — Enter entry price (¢):", title, pct
            );
            self.pos_draft = PosDraft {
                market_id: id,
                title,
                platform:  Some(platform),
                entry_price: None,
                shares:    None,
                side:      None,
            };
            self.pos_input_mode = true;
            self.pos_input_step = PosInputStep::EntryPrice;
            self.input.clear();
            self.status = status;
        } else {
            self.status = "Select a market first.".to_string();
        }
    }

    pub fn advance_pos_input(&mut self) -> bool {
        let val = self.input.trim().to_string();
        self.input.clear();

        match self.pos_input_step {
            PosInputStep::EntryPrice => {
                if let Ok(p) = val.parse::<f64>() {
                    self.pos_draft.entry_price = Some(p / 100.0);
                    self.pos_input_step = PosInputStep::Shares;
                    self.status = "Enter number of shares:".to_string();
                    false
                } else {
                    self.status = "Invalid price. Enter entry price in cents (e.g. 55):".to_string();
                    false
                }
            }
            PosInputStep::Shares => {
                if let Ok(s) = val.parse::<f64>() {
                    self.pos_draft.shares = Some(s);
                    self.pos_input_step = PosInputStep::Side;
                    self.status = "Enter side (yes / no):".to_string();
                    false
                } else {
                    self.status = "Invalid shares. Enter a number (e.g. 100):".to_string();
                    false
                }
            }
            PosInputStep::Side => {
                let side = Side::from_str(&val);
                self.pos_draft.side = Some(side);
                self.pos_input_step = PosInputStep::Note;
                self.status = "Optional note/thesis (or Enter to skip):".to_string();
                false
            }
            PosInputStep::Note => {
                // Commit position
                let note = if val.is_empty() { None } else { Some(val) };
                if let (Some(plat), Some(price), Some(shares), Some(side)) = (
                    self.pos_draft.platform.clone(),
                    self.pos_draft.entry_price,
                    self.pos_draft.shares,
                    self.pos_draft.side.clone(),
                ) {
                    let pos = Position::new(
                        plat,
                        self.pos_draft.market_id.clone(),
                        self.pos_draft.title.clone(),
                        price,
                        shares,
                        side,
                        note,
                    );
                    self.portfolio.add(pos);
                    let _ = portfolio::save_portfolio(&self.portfolio);
                    let new_idx = self.portfolio.positions.len().saturating_sub(1);
                    self.portfolio_list.select(Some(new_idx));
                    self.status = "Position added.".to_string();
                }
                self.pos_input_mode = false;
                self.pos_input_step = PosInputStep::EntryPrice;
                self.pos_draft = PosDraft::default();
                true // done
            }
        }
    }
}

// ─── Rendering ───────────────────────────────────────────────────────────────

fn render(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // tab bar
            Constraint::Min(0),    // content
            Constraint::Length(1), // status
            Constraint::Length(1), // input
        ])
        .split(area);

    render_header(f, chunks[0], app);
    render_tabs(f, chunks[1], app);
    match app.active_tab {
        Tab::Signals    => render_signals(f, chunks[2], app),
        Tab::Markets    => render_markets(f, chunks[2], app),
        Tab::Chart      => render_chart(f, chunks[2], app),
        Tab::Orderbook  => render_orderbook(f, chunks[2], app),
        Tab::Portfolio  => render_portfolio(f, chunks[2], app),
        Tab::Chat       => render_chat(f, chunks[2], app),
        Tab::SmartMoney => render_smart_money(f, chunks[2], app),
        Tab::Trades     => render_trades(f, chunks[2], app),
        Tab::Pairs      => render_pairs(f, chunks[2], app),
        Tab::News       => render_news(f, chunks[2], app),
    }
    render_status(f, chunks[3], app);
    render_input(f, chunks[4], app);

    // Startup loading overlay — shown until first market data arrives
    if app.is_loading && app.markets.is_empty() {
        render_loading_overlay(f, area, app);
    }

    // Kelly modal renders on top of content, below help
    if app.kelly_mode {
        render_kelly_modal(f, area, app);
    }

    // Help overlay renders on top of everything
    if app.show_help {
        render_help_overlay(f, area);
    }
}

// ── Header ────────────────────────────────────────────────────────────────────

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let now = chrono::Local::now().format("%H:%M:%S").to_string();
    let updated = app.last_updated
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "never".to_string());
    let loading = if app.is_loading { " ⟳" } else { "" };

    let total_pnl = app.portfolio.total_pnl();
    let pnl_color = if total_pnl >= 0.0 { Color::Green } else { Color::Red };
    let pnl_str = format!("  PnL: {:+.2}$", total_pnl);

    let line = Line::from(vec![
        Span::styled(" WhoIsSharp ", Style::default().fg(Color::Black).bg(Color::Cyan).bold()),
        Span::raw(" "),
        Span::styled(&app.backend_name, Style::default().fg(Color::Yellow)),
        Span::raw("  │  "),
        Span::styled("PM", Style::default().fg(Color::Green)),
        Span::raw(" + "),
        Span::styled("KL", Style::default().fg(Color::Blue)),
        Span::raw(format!("{}  │  updated: {}  │  ", loading, updated)),
        Span::styled(pnl_str, Style::default().fg(pnl_color).bold()),
        Span::raw("  │  "),
        Span::styled(now, Style::default().fg(Color::White)),
    ]);

    f.render_widget(Paragraph::new(line).style(Style::default().bg(Color::DarkGray)), area);
}

// ── Tab bar ───────────────────────────────────────────────────────────────────

fn render_tabs(f: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line> = TAB_NAMES
        .iter()
        .enumerate()
        .map(|(i, name)| {
            // Tab indices 0-8 → keys 1-9; index 9 (News) → key 0
            let key = if i < 9 { (i + 1).to_string() } else { "0".to_string() };
            Line::from(format!(" [{}] {} ", key, name))
        })
        .collect();

    let tabs = Tabs::new(titles)
        .select(app.active_tab as usize)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(Style::default().fg(Color::Cyan).bold())
        .divider(symbols::DOT);

    f.render_widget(tabs, area);
}

// ── Signals tab ───────────────────────────────────────────────────────────────

fn render_signals(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    render_signal_list(f, chunks[0], app);

    // Split right panel: detail on top, quick-guide at bottom
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(11)])
        .split(chunks[1]);

    render_signal_detail(f, right_chunks[0], app);
    render_signal_quickguide(f, right_chunks[1]);
}

fn signal_kind_color(kind: &SignalKind) -> Color {
    match kind {
        SignalKind::Arb          => Color::Magenta,
        SignalKind::InsiderAlert => Color::Red,
        SignalKind::VolSpike     => Color::Yellow,
        SignalKind::NearFifty    => Color::Cyan,
        SignalKind::Thin         => Color::DarkGray,
        SignalKind::Momentum     => Color::Green,
    }
}

fn render_signal_list(f: &mut Frame, area: Rect, app: &App) {
    if app.signals.is_empty() {
        let msg = if app.is_loading {
            "Computing signals…"
        } else {
            "No signals detected. Press 'r' to refresh markets."
        };
        let p = Paragraph::new(msg)
            .block(Block::default().title(" Top Signals ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app.signals.iter().map(|sig| {
        let stars = "★".repeat(sig.stars as usize) + &"☆".repeat(3 - sig.stars as usize);
        let kind_color = signal_kind_color(&sig.kind);
        let price_str = format!("{:.0}¢", sig.price_a * 100.0);

        let title_str = trunc(&sig.title, 35);

        let line = Line::from(vec![
            Span::styled(format!("{} ", stars), Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("[{:5}] ", sig.kind.label()),
                Style::default().fg(kind_color).bold(),
            ),
            Span::styled(sig.platform_a.label(), Style::default().fg(match sig.platform_a {
                Platform::Polymarket => Color::Green,
                Platform::Kalshi     => Color::Blue,
            })),
            Span::raw(" "),
            Span::styled(price_str, Style::default().fg(price_color(sig.price_a))),
            Span::raw("  "),
            Span::raw(title_str),
        ]);
        ListItem::new(line)
    }).collect();

    let count = app.signals.len();
    let title = format!(" Top Signals ({}) ", count);
    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White).bold())
        .highlight_symbol("▶ ");

    let mut state = app.signal_list.clone();
    f.render_stateful_widget(list, area, &mut state);
}

fn render_signal_detail(f: &mut Frame, area: Rect, app: &App) {
    let Some(sig) = app.selected_signal() else {
        let p = Paragraph::new("\n  Select a signal with j/k\n\n  Press Enter to open the primary market.\n  Press @ to ask AI about it.")
            .block(Block::default().title(" Signal Detail ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, area);
        return;
    };

    let kind_color = signal_kind_color(&sig.kind);
    let stars = "★".repeat(sig.stars as usize) + &"☆".repeat(3 - sig.stars as usize);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!(" {} ", sig.kind.label()), Style::default().fg(kind_color).bold().bg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled(stars, Style::default().fg(Color::Yellow)),
            Span::raw(format!("  EV Score: {:.1}", sig.ev_score)),
        ]),
        Line::from(""),
        Line::from(Span::styled(format!(" {}", sig.title), Style::default().fg(Color::White).bold())),
        Line::from(""),
    ];

    // Primary market
    lines.push(Line::from(vec![
        Span::styled(" Primary: ", Style::default().fg(Color::DarkGray)),
        Span::styled(sig.platform_a.label(), Style::default().fg(match sig.platform_a {
            Platform::Polymarket => Color::Green,
            Platform::Kalshi     => Color::Blue,
        })),
        Span::raw(format!("  {:.1}¢  ({})", sig.price_a * 100.0, sig.id_a)),
    ]));

    // Secondary market (arb)
    if let (Some(plat_b), Some(id_b), Some(price_b)) =
        (&sig.platform_b, &sig.id_b, sig.price_b)
    {
        lines.push(Line::from(vec![
            Span::styled(" Secondary:", Style::default().fg(Color::DarkGray)),
            Span::styled(plat_b.label(), Style::default().fg(match plat_b {
                Platform::Polymarket => Color::Green,
                Platform::Kalshi     => Color::Blue,
            })),
            Span::raw(format!("  {:.1}¢  ({})", price_b * 100.0, id_b)),
        ]));

        let gap_color = if sig.gap >= 0.08 { Color::Magenta } else if sig.gap >= 0.04 { Color::Yellow } else { Color::White };
        lines.push(Line::from(vec![
            Span::styled(" Gap:      ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:.1}¢", sig.gap * 100.0), Style::default().fg(gap_color).bold()),
        ]));

        // ── Arb calculator ────────────────────────────────────────────────────
        // price_a = buy price (lower), price_b = sell/NO price (higher)
        // Strategy: buy YES on platform_a + buy NO on platform_b
        let p_a = sig.price_a;                  // lower YES price
        let p_b = price_b;                       // higher YES price
        let no_b = 1.0 - p_b;                   // NO price on platform_b
        let total_cost = p_a + no_b;             // cost per share pair
        let profit_per = p_b - p_a;             // guaranteed profit per share
        let ret_pct = if total_cost > 1e-9 { profit_per / total_cost * 100.0 } else { 0.0 };
        // At $1,000 bankroll
        let bankroll = 1000.0;
        let n_shares = if total_cost > 1e-9 { bankroll / total_cost } else { 0.0 };
        let cost_yes = n_shares * p_a;
        let cost_no  = n_shares * no_b;
        let arb_profit = n_shares * profit_per;

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" ─── ARB CALCULATOR ──────────────────", Style::default().fg(Color::Magenta)),
        ]));
        lines.push(Line::from(vec![
            Span::raw(format!(" Buy YES on {} @ {:.1}¢  +  Buy NO on {} @ {:.1}¢",
                sig.platform_a.label(), p_a * 100.0,
                plat_b.label(), no_b * 100.0)),
        ]));
        lines.push(Line::from(vec![
            Span::raw(format!(" Cost/share: {:.1}¢   Payout/share: 100¢   Profit/share: {:.1}¢",
                total_cost * 100.0, profit_per * 100.0)),
        ]));
        lines.push(Line::from(vec![
            Span::raw(" Guaranteed return:  "),
            Span::styled(format!("{:.2}%", ret_pct), Style::default().fg(gap_color).bold()),
        ]));
        lines.push(Line::from(vec![
            Span::styled(format!(" At $1,000 bankroll:", ), Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::from(vec![
            Span::raw(format!("   YES @ {}: ${:.0}   NO @ {}: ${:.0}   Profit: ",
                sig.platform_a.label(), cost_yes, plat_b.label(), cost_no)),
            Span::styled(format!("${:.0}", arb_profit), Style::default().fg(gap_color).bold()),
        ]));
    } else {
        let gap_label = match sig.kind {
            SignalKind::NearFifty    => "Distance from 50%",
            SignalKind::VolSpike     => "Vol × avg",
            SignalKind::Thin         => "Liquidity ($)",
            SignalKind::Arb          => "Gap",
            SignalKind::InsiderAlert => "Vol/Liq ratio",
            SignalKind::Momentum     => "Δ Price",
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {:>10}: ", gap_label), Style::default().fg(Color::DarkGray)),
            Span::raw(match sig.kind {
                SignalKind::NearFifty    => format!("{:.1}¢", sig.gap * 100.0),
                SignalKind::VolSpike     => format!("{:.1}×", sig.gap),
                SignalKind::Thin         => format!("${:.0}K", sig.gap / 1000.0),
                SignalKind::Arb          => format!("{:.1}¢", sig.gap * 100.0),
                SignalKind::InsiderAlert => format!("{:.0}×", sig.gap),
                SignalKind::Momentum     => format!("{:+.1}pp", sig.gap * 100.0),
            }),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" Action:", Style::default().fg(Color::DarkGray))));
    for chunk in textwrap(&sig.action, (area.width as usize).saturating_sub(4)) {
        lines.push(Line::from(Span::styled(
            format!("  {}", chunk),
            Style::default().fg(Color::Cyan),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  [Enter] open market  [@] ask AI  [/add] add position  [/dismiss] dismiss",
        Style::default().fg(Color::DarkGray),
    )));

    let p = Paragraph::new(lines)
        .block(Block::default().title(" Signal Detail ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_signal_quickguide(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(vec![
            Span::styled("  [ARB]   ", Style::default().fg(Color::Magenta).bold()),
            Span::styled("Cross-platform price gap — guaranteed profit if gap holds", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  [NEAR50]", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" Price near 50¢ — high uncertainty, good two-sided entry", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  [VOLSPK]", Style::default().fg(Color::Yellow).bold()),
            Span::styled(" Volume spike — unusual activity vs. average", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  [INSDR] ", Style::default().fg(Color::Red).bold()),
            Span::styled(" Insider alert — vol/liquidity ratio exceeds threshold", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  [THIN]  ", Style::default().fg(Color::DarkGray).bold()),
            Span::styled(" Thin liquidity — price may move on small orders", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  [MOMNTM]", Style::default().fg(Color::Green).bold()),
            Span::styled(" Momentum — significant price drift since last refresh", Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Enter", Style::default().fg(Color::Cyan)),
            Span::raw(" open market   "),
            Span::styled("@", Style::default().fg(Color::Cyan)),
            Span::raw(" AI analysis   "),
            Span::styled("/add", Style::default().fg(Color::Cyan)),
            Span::raw(" add position   "),
            Span::styled("/dismiss", Style::default().fg(Color::Cyan)),
            Span::raw(" dismiss"),
        ]),
    ];

    let p = Paragraph::new(lines)
        .block(Block::default().title(" Signal Types ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    f.render_widget(p, area);
}

// ── Markets tab ───────────────────────────────────────────────────────────────

fn render_markets(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);

    render_market_list(f, chunks[0], app);
    render_market_detail(f, chunks[1], app);
}

fn render_market_list(f: &mut Frame, area: Rect, app: &App) {
    let filter_label = if app.watchlist_only {
        format!("★ {}", app.platform_filter.label())
    } else {
        app.platform_filter.label().to_string()
    };
    let search_label = if app.search.is_empty() {
        String::new()
    } else {
        format!(" /{}", app.search)
    };

    let title = format!(" Markets [{}]{} ", filter_label, search_label);

    let filtered = app.filtered_markets();
    let items: Vec<ListItem> = filtered
        .iter()
        .map(|m| {
            let platform_color = match m.platform {
                Platform::Polymarket => Color::Green,
                Platform::Kalshi     => Color::Blue,
            };
            let pct = m.yes_price * 100.0;
            let pct_color = price_color(m.yes_price);
            let vol = format_volume(m.volume);

            let title_str = trunc(&m.title, 28);
            let watch_star = if app.is_watched(&m.id) { "★" } else { " " };

            // Price velocity indicator vs previous snapshot
            let (vel_str, vel_color) = if let Some(&prev) = app.prev_prices.get(&m.id) {
                let delta = m.yes_price - prev;
                if delta.abs() >= 0.01 {
                    let sign = if delta > 0.0 { "▲" } else { "▼" };
                    (format!("{}{:.1}pp", sign, delta.abs() * 100.0), if delta > 0.0 { Color::Green } else { Color::Red })
                } else {
                    ("  —  ".to_string(), Color::DarkGray)
                }
            } else {
                ("     ".to_string(), Color::DarkGray)
            };

            let line = Line::from(vec![
                Span::styled(watch_star, Style::default().fg(Color::Yellow)),
                Span::styled(m.platform.label(), Style::default().fg(platform_color)),
                Span::raw(" "),
                Span::styled(format!("{:5.1}%", pct), Style::default().fg(pct_color).bold()),
                Span::raw(format!(" {:>7} ", vol)),
                Span::styled(format!("{:<7}", vel_str), Style::default().fg(vel_color)),
                Span::raw(title_str),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White).bold())
        .highlight_symbol("▶ ");

    let mut state = app.market_list.clone();
    f.render_stateful_widget(list, area, &mut state);
}

fn render_market_detail(f: &mut Frame, area: Rect, app: &App) {
    let Some(m) = app.selected_market() else {
        let p = Paragraph::new("Select a market with j/k or ↑/↓")
            .block(Block::default().title(" Detail ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, area);
        return;
    };

    let pct_color = price_color(m.yes_price);
    let vol = format_volume(m.volume);
    let liq = m.liquidity
        .map(|v| format!("${:.0}K", v / 1_000.0))
        .unwrap_or_else(|| "N/A".into());

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(format!(" {} ", m.title), Style::default().fg(Color::White).bold())),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!(" {} ", m.platform.name()), Style::default().fg(match m.platform {
                Platform::Polymarket => Color::Green,
                Platform::Kalshi     => Color::Blue,
            })),
            Span::raw(" │ "),
            Span::raw(m.status.as_str()),
            Span::raw(" │ "),
            Span::raw(m.category.as_deref().unwrap_or("Uncategorized")),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw(" YES  "),
            Span::styled(format!("{:.1}¢", m.yes_price * 100.0), Style::default().fg(pct_color).bold()),
            Span::raw(format!("  ({:.1}%)", m.yes_price * 100.0)),
        ]),
        Line::from(vec![
            Span::raw(" NO   "),
            Span::styled(
                format!("{:.1}¢", m.no_price * 100.0),
                Style::default().fg(Color::Red).bold(),
            ),
            Span::raw(format!("  ({:.1}%)", m.no_price * 100.0)),
        ]),
        Line::from(""),
        Line::from(format!(" Volume:    {}", vol)),
        Line::from(format!(" Liquidity: {}", liq)),
        Line::from(format!(" Ends:      {}", m.end_date.as_deref().unwrap_or("N/A"))),
        Line::from(format!(" ID:        {}", m.id)),
        Line::from(""),
    ];

    // ── Time value / theta block ──────────────────────────────────────────────
    if let Some((days, daily_sig, annual_vol)) = market_time_value(m) {
        lines.push(Line::from(vec![
            Span::styled(" ─── TIME VALUE ─────────────────────", Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Days left:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}", days), Style::default().fg(Color::White).bold()),
            Span::raw("   "),
            Span::styled("Daily σ: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:.2}pp/day", daily_sig), Style::default().fg(Color::Yellow).bold()),
            Span::raw("   "),
            Span::styled("Ann vol: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:.1}%", annual_vol), Style::default().fg(Color::White)),
        ]));
        // Theta: edge decay per day assuming constant position
        // Also show "break-even move needed per day"
        lines.push(Line::from(vec![
            Span::styled(" B/E daily:  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{:.2}pp move needed/day to change outcome", daily_sig)),
        ]));
        lines.push(Line::from(""));
    }

    lines.extend([
        Line::from(Span::styled(
            "  [Enter] load chart/book  [/kelly] Kelly sizer  [/add] add position  [@] ask AI",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
    ]);

    if let Some(desc) = &m.description {
        if !desc.is_empty() {
            lines.push(Line::from(Span::styled(" Description:", Style::default().fg(Color::DarkGray))));
            for chunk in textwrap(desc, (area.width as usize).saturating_sub(4)) {
                lines.push(Line::from(format!("  {}", chunk)));
            }
        }
    }

    let title = format!(" {} ", m.platform.label());
    let p = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .wrap(Wrap { trim: true });

    f.render_widget(p, area);
}

// ── Chart tab ─────────────────────────────────────────────────────────────────

fn render_chart(f: &mut Frame, area: Rect, app: &App) {
    if app.chart_data.is_empty() {
        let msg = if app.selected_market_id.is_some() {
            "Loading price history…"
        } else {
            "Select a market in the Markets tab, then press Enter to load its chart."
        };
        let p = Paragraph::new(msg)
            .block(Block::default().title(" Chart ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, area);
        return;
    }

    // Determine if we have volume data (Kalshi only)
    let has_volume = app.chart_candles.iter().any(|c| c.volume.is_some());

    // Split vertically: price chart on top, volume bar on bottom (if available)
    let (chart_area, vol_area_opt) = if has_volume && area.height > 8 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    let x_min = app.chart_data.first().map(|(x, _)| *x).unwrap_or(0.0);
    let x_max = app.chart_data.last().map(|(x, _)| *x).unwrap_or(1.0);
    let y_min = (app.chart_min - 2.0).max(0.0);
    let y_max = (app.chart_max + 2.0).min(100.0);

    let title_str = app.selected_market_id
        .as_ref()
        .and_then(|id| app.markets.iter().find(|m| &m.id == id))
        .map(|m| format!(" {} [{}] ", m.title, app.chart_interval.label()))
        .unwrap_or_else(|| format!(" Chart [{}] ", app.chart_interval.label()));

    let fmt_ts = |ts: f64| {
        let dt = chrono::DateTime::from_timestamp(ts as i64, 0)
            .map(|d| d.with_timezone(&chrono::Local).format("%m/%d %H:%M").to_string())
            .unwrap_or_default();
        Span::raw(dt)
    };
    let x_labels = vec![fmt_ts(x_min), fmt_ts((x_min + x_max) / 2.0), fmt_ts(x_max)];

    let y_labels: Vec<Span> = (0..=4)
        .map(|i| {
            let v = y_min + (y_max - y_min) * i as f64 / 4.0;
            Span::raw(format!("{:.0}%", v))
        })
        .collect();

    let dataset = Dataset::default()
        .name("YES Price")
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Green))
        .data(&app.chart_data);

    let chart = Chart::new(vec![dataset])
        .block(
            Block::default()
                .title(title_str)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([x_min, x_max])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([y_min, y_max])
                .labels(y_labels),
        );

    f.render_widget(chart, chart_area);

    // ── OHLC stats for the most-recent candle ─────────────────────────────────
    // Show under the chart when the area is tall enough and candle data exists
    if !app.chart_candles.is_empty() && area.height > 12 {
        // The vol_area_opt already reserves space; we add a 1-line OHLC row
        // before the volume bar by overwriting the bottom of chart_area.
        // We use the last candle as "current".
        let c = app.chart_candles.last().unwrap();
        let prev_close = app.chart_candles
            .iter().rev().nth(1).map(|p| p.close).unwrap_or(c.open);
        let day_chg  = c.close - prev_close;
        let chg_col  = if day_chg >= 0.0 { Color::Green } else { Color::Red };
        let chg_sign = if day_chg >= 0.0 { "+" } else { "" };

        let ohlc_line = Line::from(vec![
            Span::styled(" OHLC  ", Style::default().fg(Color::DarkGray)),
            Span::styled("O ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{:.1}¢  ", c.open * 100.0)),
            Span::styled("H ", Style::default().fg(Color::Green)),
            Span::raw(format!("{:.1}¢  ", c.high * 100.0)),
            Span::styled("L ", Style::default().fg(Color::Red)),
            Span::raw(format!("{:.1}¢  ", c.low  * 100.0)),
            Span::styled("C ", Style::default().fg(Color::White)),
            Span::styled(format!("{:.1}¢  ", c.close * 100.0), Style::default().fg(Color::White).bold()),
            Span::styled(
                format!("{}{:.1}pp", chg_sign, day_chg * 100.0),
                Style::default().fg(chg_col).bold(),
            ),
            if let Some(vol) = c.volume {
                Span::styled(format!("   Vol {}", format_volume(Some(vol))), Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]);

        // Draw one row above the volume area (or above the bottom border)
        let ohlc_rect = Rect {
            x:      chart_area.x + 1,
            y:      chart_area.y + chart_area.height - 2,
            width:  chart_area.width.saturating_sub(2),
            height: 1,
        };
        f.render_widget(Paragraph::new(ohlc_line), ohlc_rect);
    }

    // Volume overlay (Kalshi only — Polymarket price history has no volume)
    if let Some(vol_area) = vol_area_opt {
        let max_vol = app.chart_candles.iter()
            .filter_map(|c| c.volume)
            .fold(f64::NEG_INFINITY, f64::max);

        if max_vol > 0.0 {
            let inner_w = vol_area.width.saturating_sub(2) as usize;
            let n = app.chart_candles.len();
            let step = if n > 0 { (n as f64 / inner_w as f64).max(1.0) } else { 1.0 };
            let vol_bar: String = (0..inner_w)
                .map(|i| {
                    let idx = ((i as f64 * step) as usize).min(n.saturating_sub(1));
                    let pct = app.chart_candles.get(idx)
                        .and_then(|c| c.volume)
                        .map(|v| v / max_vol)
                        .unwrap_or(0.0);
                    match (pct * 8.0) as u8 {
                        0 => ' ', 1 => '▁', 2 => '▂', 3 => '▃',
                        4 => '▄', 5 => '▅', 6 => '▆', 7 => '▇', _ => '█',
                    }
                })
                .collect();

            let vol_p = Paragraph::new(Line::from(Span::styled(vol_bar, Style::default().fg(Color::DarkGray))))
                .block(Block::default()
                    .title(" Volume ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)));
            f.render_widget(vol_p, vol_area);
        }
    }
}

// ── Orderbook tab ─────────────────────────────────────────────────────────────

fn render_orderbook(f: &mut Frame, area: Rect, app: &App) {
    let Some(book) = &app.orderbook else {
        let msg = if app.selected_market_id.is_some() {
            "Loading order book…"
        } else {
            "Select a market in the Markets tab, then press Enter."
        };
        let p = Paragraph::new(msg)
            .block(Block::default().title(" Order Book ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, area);
        return;
    };

    let title = app.selected_market_id
        .as_ref()
        .and_then(|id| app.markets.iter().find(|m| &m.id == id))
        .map(|m| format!(" Order Book — {} ", trunc(&m.title, 50)))
        .unwrap_or_else(|| " Order Book ".to_string());

    // ── Aggregate stats ───────────────────────────────────────────────────────
    let total_bid_sz: f64 = book.bids.iter().map(|b| b.size).sum();
    let total_ask_sz: f64 = book.asks.iter().map(|a| a.size).sum();
    let imbalance = if total_bid_sz + total_ask_sz > 1e-9 {
        (total_bid_sz - total_ask_sz) / (total_bid_sz + total_ask_sz)
    } else { 0.0 };
    let imb_color = if imbalance > 0.1 { Color::Green } else if imbalance < -0.1 { Color::Red } else { Color::Yellow };

    let spread_pct = book.spread().unwrap_or(0.0) * 100.0;
    let mid_pct    = book.mid().unwrap_or(0.0) * 100.0;

    // Best bid / ask in cents
    let best_bid = book.bids.first().map(|b| b.price * 100.0).unwrap_or(0.0);
    let best_ask = book.asks.first().map(|a| a.price * 100.0).unwrap_or(0.0);

    // ── Depth histogram (max 12 chars wide per side) ──────────────────────────
    let max_sz = book.bids.iter().chain(book.asks.iter())
        .map(|l| l.size)
        .fold(0.0_f64, f64::max)
        .max(1.0);
    const BAR_W: usize = 12;
    let size_bar = |sz: f64| -> String {
        let filled = ((sz / max_sz) * BAR_W as f64).round() as usize;
        "█".repeat(filled.min(BAR_W))
    };

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(" Mid: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:.1}¢", mid_pct), Style::default().fg(Color::White).bold()),
            Span::styled("  Spread: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:.1}¢  ({:.0}bps)", spread_pct, spread_pct * 10.0),
                Style::default().fg(Color::Yellow)),
            Span::styled("  Bid: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:.1}¢", best_bid), Style::default().fg(Color::Green)),
            Span::styled("  Ask: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:.1}¢", best_ask), Style::default().fg(Color::Red)),
        ]),
        Line::from(vec![
            Span::styled(" Imbalance: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:+.2}  (bid {:.0}  ask {:.0})", imbalance, total_bid_sz, total_ask_sz),
                Style::default().fg(imb_color).bold(),
            ),
            Span::styled(
                if imbalance > 0.15 { "  ← buy pressure" }
                else if imbalance < -0.15 { "  ← sell pressure" }
                else { "  ← balanced" },
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {:>12}  {:>8}  {:>7}  │  {:>7}  {:>8}  {:<12}",
                    "DEPTH", "SIZE", "BID¢", "ASK¢", "SIZE", "DEPTH"),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(Span::styled("─".repeat(70), Style::default().fg(Color::DarkGray))),
    ];

    let depth = book.bids.len().max(book.asks.len()).min(20);
    let mut bid_total = 0.0f64;
    let mut ask_total = 0.0f64;

    let bids: Vec<(f64, f64, f64)> = book.bids.iter().take(depth).map(|b| {
        bid_total += b.size;
        (b.price * 100.0, b.size, bid_total)
    }).collect();
    let asks: Vec<(f64, f64, f64)> = book.asks.iter().take(depth).map(|a| {
        ask_total += a.size;
        (a.price * 100.0, a.size, ask_total)
    }).collect();

    for i in 0..depth {
        let bid_part = bids.get(i).map(|(p, s, t)| {
            let bar = size_bar(*s);
            (
                Span::styled(format!("{:>12}", bar), Style::default().fg(Color::Green)),
                Span::styled(format!("  {:>8.0}", s), Style::default().fg(Color::White)),
                Span::styled(format!("  {:>7.1}", p), Style::default().fg(Color::Green).bold()),
                *t,
            )
        });
        let ask_part = asks.get(i).map(|(p, s, t)| {
            let bar = size_bar(*s);
            (
                Span::styled(format!("{:>7.1}", p), Style::default().fg(Color::Red).bold()),
                Span::styled(format!("  {:>8.0}", s), Style::default().fg(Color::White)),
                Span::styled(format!("  {:<12}", bar), Style::default().fg(Color::Red)),
                *t,
            )
        });

        // Show cumulative total as DarkGray at the far edges
        let _ = bid_part.as_ref().map(|(.., t)| t);
        let _ = ask_part.as_ref().map(|(.., t)| t);

        let mut spans = Vec::new();
        match bid_part {
            Some((bar, size, price, _)) => { spans.push(bar); spans.push(size); spans.push(price); }
            None => { spans.push(Span::raw(" ".repeat(30))); }
        }
        spans.push(Span::styled("  │  ", Style::default().fg(Color::DarkGray)));
        match ask_part {
            Some((price, size, bar, _)) => { spans.push(price); spans.push(size); spans.push(bar); }
            None => {}
        }
        lines.push(Line::from(spans));
    }

    let p = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .scroll((app.book_scroll, 0));

    f.render_widget(p, area);
}

// ── Portfolio tab ─────────────────────────────────────────────────────────────

fn render_portfolio(f: &mut Frame, area: Rect, app: &App) {
    if app.show_risk_view {
        render_portfolio_risk(f, area, app);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(0)])
        .split(area);

    render_portfolio_summary(f, chunks[0], app);
    render_portfolio_positions(f, chunks[1], app);
}

// ─── Portfolio risk view ──────────────────────────────────────────────────────

fn render_portfolio_risk(f: &mut Frame, area: Rect, app: &App) {
    use crate::risk;

    if app.portfolio.positions.is_empty() {
        let p = Paragraph::new("No positions to analyse. Add positions from the Markets tab (/add).")
            .block(Block::default().title(" Portfolio Risk Analysis ").borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)));
        f.render_widget(p, area);
        return;
    }

    let risk = risk::compute(&app.portfolio, &app.markets);

    let outer_block = Block::default()
        .title(" Portfolio Risk Analysis  [v] or [/risk] to toggle ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = outer_block.inner(area);
    f.render_widget(outer_block, area);

    // Split vertically: summary row | histogram | scenario table | position breakdown
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // key metrics
            Constraint::Length(10), // histogram
            Constraint::Min(0),     // scenario stress + per-position EV
        ])
        .split(inner);

    // ── Key metrics ───────────────────────────────────────────────────────────
    let ep_color = if risk.expected_pnl >= 0.0 { Color::Green } else { Color::Red };
    let pp_color = if risk.prob_profit >= 0.5   { Color::Green } else { Color::Red };

    let metric_lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  E[P&L]: "),
            Span::styled(format!("{:+.2}", risk.expected_pnl), Style::default().fg(ep_color).bold()),
            Span::raw("   σ: "),
            Span::styled(format!("{:.2}", risk.std_dev), Style::default().fg(Color::White).bold()),
            Span::raw("   P(profit): "),
            Span::styled(format!("{:.0}%", risk.prob_profit * 100.0), Style::default().fg(pp_color).bold()),
            Span::raw("   Best: "),
            Span::styled(format!("{:+.2}", risk.best_case), Style::default().fg(Color::Green)),
            Span::raw("   Worst: "),
            Span::styled(format!("{:+.2}", risk.worst_case), Style::default().fg(Color::Red)),
        ]),
    ];
    f.render_widget(Paragraph::new(metric_lines), rows[0]);

    // ── Histogram ─────────────────────────────────────────────────────────────
    let hist_block = Block::default()
        .title(" P&L Distribution  (normal approx., independent positions) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let hist_inner = hist_block.inner(rows[1]);
    f.render_widget(hist_block, rows[1]);

    if risk.histogram.is_empty() {
        f.render_widget(
            Paragraph::new("  (need ≥ 2 positions with non-zero σ for distribution)"),
            hist_inner,
        );
    } else {
        // Render as vertical bars inside the inner area.
        // Height available: hist_inner.height rows; width ÷ HIST_BUCKETS cols per bar.
        let h = hist_inner.height as usize;
        let bar_w = (hist_inner.width as usize / risk::HIST_BUCKETS).max(1);

        // Draw rows top→bottom. Row 0 = top (tallest only); row h-2 = bottom bar row.
        // Row h-1 = axis + labels.
        let bar_rows = h.saturating_sub(2);

        let mut lines: Vec<Line> = Vec::new();
        for row in 0..bar_rows {
            // threshold: what fraction of max height this row represents
            let threshold = 1.0 - (row as f64 + 0.5) / bar_rows as f64;
            // Colour: green right of zero, red left, white at zero
            let zero_bucket = risk.histogram.iter()
                .position(|(mid, _)| *mid >= 0.0)
                .unwrap_or(risk::HIST_BUCKETS);

            let spans: Vec<Span> = risk.histogram.iter().enumerate()
                .map(|(i, (_, height))| {
                    let filled = *height >= threshold;
                    let text = if filled {
                        "█".repeat(bar_w)
                    } else {
                        " ".repeat(bar_w)
                    };
                    let color = if !filled { Color::Reset }
                                else if i < zero_bucket { Color::Red }
                                else { Color::Green };
                    Span::styled(text, Style::default().fg(color))
                })
                .collect();
            lines.push(Line::from(spans));
        }

        // Axis line
        lines.push(Line::from(Span::styled(
            "─".repeat(hist_inner.width as usize),
            Style::default().fg(Color::DarkGray),
        )));

        // Label line: worst / zero / best
        let total_w = hist_inner.width as usize;
        let worst_label = format!("{:+.0}", risk.worst_case);
        let best_label  = format!("{:+.0}", risk.best_case);
        let zero_label  = "$0";
        let zero_pos    = (risk::HIST_BUCKETS * bar_w)
            .saturating_mul(
                risk.histogram.iter().position(|(m, _)| *m >= 0.0).unwrap_or(0)
            )
            / risk::HIST_BUCKETS.max(1);

        let mut label_row = " ".repeat(total_w);
        // Write worst label at start
        if worst_label.len() <= total_w {
            label_row.replace_range(0..worst_label.len(), &worst_label);
        }
        // Write zero label near zero position
        let z_start = zero_pos.min(total_w.saturating_sub(zero_label.len()));
        label_row.replace_range(z_start..z_start + zero_label.len().min(total_w - z_start), zero_label);
        // Write best label at end
        let b_start = total_w.saturating_sub(best_label.len());
        label_row.replace_range(b_start.., &best_label[..best_label.len().min(total_w - b_start)]);

        lines.push(Line::from(Span::styled(label_row, Style::default().fg(Color::DarkGray))));

        let para = Paragraph::new(lines);
        f.render_widget(para, hist_inner);
    }

    // ── Scenario stress + per-position EV ────────────────────────────────────
    let bottom_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[2]);

    // Left: category stress tests
    let stress_block = Block::default()
        .title(" Category Stress Tests ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let stress_inner = stress_block.inner(bottom_cols[0]);
    f.render_widget(stress_block, bottom_cols[0]);

    if risk.category_stress.is_empty() {
        f.render_widget(Paragraph::new("  No positions."), stress_inner);
    } else {
        let max_abs = risk.category_stress.iter()
            .map(|s| s.stressed_pnl.abs())
            .fold(0.0f64, f64::max)
            .max(1.0);

        let bar_max_w = (stress_inner.width as usize).saturating_sub(32);

        let mut stress_lines = vec![Line::from(vec![
            Span::styled("  Category             Stress P&L", Style::default().fg(Color::DarkGray)),
        ])];

        for s in &risk.category_stress {
            let pnl_color = if s.stressed_pnl >= 0.0 { Color::Green } else { Color::Red };
            let bar_len = ((s.stressed_pnl.abs() / max_abs) * bar_max_w as f64) as usize;
            let bar     = "█".repeat(bar_len);
            let conc_pct = s.concentration * 100.0;
            stress_lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:.<18} {:>3}pos {:>3.0}%  ",
                        trunc(&s.category, 16), s.n_positions, conc_pct),
                    Style::default().fg(Color::Gray),
                ),
                Span::styled(
                    format!("{:+.0}", s.stressed_pnl),
                    Style::default().fg(pnl_color).bold(),
                ),
            ]));
            stress_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(bar, Style::default().fg(pnl_color)),
            ]));
        }
        stress_lines.push(Line::from(""));
        stress_lines.push(Line::from(vec![
            Span::styled(
                "  All positions in category resolve against you.",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        f.render_widget(Paragraph::new(stress_lines), stress_inner);
    }

    // Right: per-position EV breakdown
    let ev_block = Block::default()
        .title(" Per-Position Expected Value ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let ev_inner = ev_block.inner(bottom_cols[1]);
    f.render_widget(ev_block, bottom_cols[1]);

    let mut ev_lines = vec![Line::from(vec![
        Span::styled(
            "  Title                         P(win)  E[P&L]  Win    Lose  ",
            Style::default().fg(Color::DarkGray),
        ),
    ])];
    for pr in &risk.positions {
        let ev_color = if pr.expected_pnl >= 0.0 { Color::Green } else { Color::Red };
        let wp_color = if pr.win_prob >= 0.5 { Color::Green } else { Color::Red };
        ev_lines.push(Line::from(vec![
            Span::styled(
                format!("  {:.<30} ", trunc(&pr.title, 28)),
                Style::default().fg(Color::Gray),
            ),
            Span::styled(format!("{:>5.0}%  ", pr.win_prob * 100.0), Style::default().fg(wp_color)),
            Span::styled(format!("{:>+7.2}  ", pr.expected_pnl), Style::default().fg(ev_color)),
            Span::styled(format!("{:>+6.2}  ", pr.win_pnl),  Style::default().fg(Color::Green)),
            Span::styled(format!("{:>+6.2}", pr.lose_pnl), Style::default().fg(Color::Red)),
        ]));
    }
    f.render_widget(Paragraph::new(ev_lines), ev_inner);
}

fn render_portfolio_summary(f: &mut Frame, area: Rect, app: &App) {
    let total_cost  = app.portfolio.total_cost();
    let total_value = app.portfolio.total_value();
    let total_pnl   = app.portfolio.total_pnl();
    let pnl_color   = if total_pnl >= 0.0 { Color::Green } else { Color::Red };

    // ── Risk metrics ──────────────────────────────────────────────────────────
    let positions = &app.portfolio.positions;
    let n = positions.len();
    let winning = positions.iter().filter(|p| p.pnl() > 0.0).count();
    let win_rate = if n > 0 { winning as f64 / n as f64 * 100.0 } else { 0.0 };

    let concentration = if total_cost > 1e-9 {
        positions.iter()
            .map(|p| p.cost() / total_cost * 100.0)
            .fold(f64::NEG_INFINITY, f64::max)
    } else { 0.0 };

    let pm_cost: f64 = positions.iter()
        .filter(|p| p.platform == Platform::Polymarket)
        .map(|p| p.cost()).sum();
    let kl_cost: f64 = positions.iter()
        .filter(|p| p.platform == Platform::Kalshi)
        .map(|p| p.cost()).sum();

    let best_pnl  = positions.iter().map(|p| p.pnl()).fold(f64::NEG_INFINITY, f64::max);
    let worst_pnl = positions.iter().map(|p| p.pnl()).fold(f64::INFINITY,     f64::min);
    let best_color  = if best_pnl  >= 0.0 { Color::Green } else { Color::Red };
    let worst_color = if worst_pnl >= 0.0 { Color::Green } else { Color::Red };

    // ── Category exposure (correlation proxy) ─────────────────────────────────
    // Group cost by category keyword to show topic concentration risk.
    let cat_exposure: Vec<(String, f64)> = {
        let mut map: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        for pos in positions {
            let cat = app.markets.iter()
                .find(|m| m.id == pos.market_id)
                .and_then(|m| m.category.clone())
                .unwrap_or_else(|| "Other".to_string());
            *map.entry(cat).or_insert(0.0) += pos.cost();
        }
        let mut v: Vec<_> = map.into_iter().collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.truncate(4);
        v
    };
    let exposure_str = if cat_exposure.is_empty() {
        "—".to_string()
    } else {
        cat_exposure.iter()
            .map(|(cat, cost)| format!("{}: ${:.0}", cat, cost))
            .collect::<Vec<_>>()
            .join("  ")
    };

    // Count positions with stop/target set
    let with_targets = positions.iter()
        .filter(|p| p.take_profit.is_some() || p.stop_loss.is_some())
        .count();

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Cost basis:   "),
            Span::styled(format!("${:.2}", total_cost), Style::default().fg(Color::White).bold()),
            Span::raw("   Mark value: "),
            Span::styled(format!("${:.2}", total_value), Style::default().fg(Color::White).bold()),
            Span::raw("   Unrealised PnL: "),
            Span::styled(format!("{:+.2}$", total_pnl), Style::default().fg(pnl_color).bold()),
        ]),
        Line::from(vec![
            Span::raw("  Positions: "),
            Span::styled(format!("{}", n), Style::default().fg(Color::White)),
            Span::raw(format!("   Win rate: {:.0}%", win_rate)),
            Span::raw(format!("   Top conc.: {:.0}%", concentration)),
            Span::raw(format!("   PM: ${:.0}  KL: ${:.0}", pm_cost, kl_cost)),
        ]),
        Line::from(vec![
            Span::raw("  Best: "),
            Span::styled(if n > 0 { format!("{:+.2}$", best_pnl)  } else { "—".into() }, Style::default().fg(best_color)),
            Span::raw("   Worst: "),
            Span::styled(if n > 0 { format!("{:+.2}$", worst_pnl) } else { "—".into() }, Style::default().fg(worst_color)),
            Span::raw(format!("   Targets set: {}/{}", with_targets, n)),
        ]),
        Line::from(vec![
            Span::styled("  Exposure: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&exposure_str[..exposure_str.len().min(70)]),
        ]),
        Line::from(vec![
            Span::styled(
                "  [/add] Add  [/targets] Set target  [/delete] Delete  [Enter] Load chart",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];

    let p = Paragraph::new(lines)
        .block(Block::default().title(" Portfolio Summary ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    f.render_widget(p, area);
}

fn render_portfolio_positions(f: &mut Frame, area: Rect, app: &App) {
    if app.portfolio.positions.is_empty() {
        let msg = if app.pos_input_mode {
            "Adding position — follow the prompts in the status bar."
        } else {
            "No positions. Navigate to Markets tab, select a market, type /add to add a position."
        };
        let p = Paragraph::new(msg)
            .block(Block::default().title(" Positions ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app.portfolio.positions.iter().map(|pos| {
        let pnl = pos.pnl();
        let pnl_pct = pos.pnl_pct();
        let pnl_color = if pnl >= 0.0 { Color::Green } else { Color::Red };
        let platform_color = match pos.platform {
            Platform::Polymarket => Color::Green,
            Platform::Kalshi     => Color::Blue,
        };
        let mark = pos.mark_price.unwrap_or(pos.entry_price) * 100.0;
        let title_str = trunc(&pos.title, 28);

        // Stop / take-profit indicators
        let target_str = match (pos.take_profit, pos.stop_loss) {
            (Some(tp), Some(sl)) => format!(" TP:{:.0}¢ SL:{:.0}¢", tp*100.0, sl*100.0),
            (Some(tp), None)     => format!(" TP:{:.0}¢", tp*100.0),
            (None,     Some(sl)) => format!(" SL:{:.0}¢", sl*100.0),
            (None,     None)     => String::new(),
        };

        // Alert state indicators
        let alert = if let Some(mark_p) = pos.mark_price {
            if pos.take_profit.map(|tp| mark_p >= tp).unwrap_or(false) { " 🎯" }
            else if pos.stop_loss.map(|sl| mark_p <= sl).unwrap_or(false) { " 🛑" }
            else { "" }
        } else { "" };

        // Wallet-synced badge: show truncated wallet address in dim cyan.
        let wallet_badge: Option<String> = pos.note.as_deref()
            .and_then(|n| n.strip_prefix("wallet:"))
            .map(|addr| format!(" [{}]", short_wallet(addr)));

        let mut spans = vec![
            Span::styled(pos.platform.label(), Style::default().fg(platform_color)),
            Span::raw(" "),
            Span::styled(pos.side.label(), Style::default().fg(Color::White).bold()),
            Span::raw(format!(" {:>6.1}¢→{:>6.1}¢  ", pos.entry_price * 100.0, mark)),
            Span::styled(
                format!("{:+.2}$ ({:+.1}%)", pnl, pnl_pct),
                Style::default().fg(pnl_color).bold(),
            ),
            Span::styled(target_str, Style::default().fg(Color::DarkGray)),
            Span::styled(alert, Style::default().fg(Color::Yellow)),
            Span::raw("  "),
            Span::raw(title_str),
        ];
        if let Some(badge) = wallet_badge {
            spans.push(Span::styled(badge, Style::default().fg(Color::Cyan).dim()));
        }
        let line = Line::from(spans);
        ListItem::new(line)
    }).collect();

    let list = List::new(items)
        .block(Block::default()
            .title(format!(" Positions ({}) ", app.portfolio.positions.len()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White).bold())
        .highlight_symbol("▶ ");

    let mut state = app.portfolio_list.clone();
    f.render_stateful_widget(list, area, &mut state);
}

// ── Chat tab ──────────────────────────────────────────────────────────────────

fn render_chat(f: &mut Frame, area: Rect, app: &App) {
    // Pre-wrap all text at the known inner width so line counts are exact.
    // This avoids the Paragraph Wrap widget whose visual-line count we can't
    // predict, which caused auto-scroll to cut off the last visible rows.
    let inner_width = (area.width as usize).saturating_sub(4); // 2 border + 2 indent

    let mut lines: Vec<Line> = Vec::new();

    if app.chat_msgs.is_empty() && !app.is_loading {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  WhoIsSharp AI Assistant",
            Style::default().fg(Color::Cyan).bold(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Ask anything about prediction markets. Try:",
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(""));
        for prompt in &[
            "\"Which markets have the highest uncertainty right now?\"",
            "\"Analyze the order book for [market name]\"",
            "\"What's the price history trend for [market]?\"",
            "\"Search for markets related to elections\"",
            "\"List the top markets by volume\"",
        ] {
            lines.push(Line::from(vec![
                Span::styled("  › ", Style::default().fg(Color::Yellow)),
                Span::styled(*prompt, Style::default().fg(Color::White)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Press @ on any market to pre-fill an analysis prompt.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    for msg in &app.chat_msgs {
        match msg {
            ChatMsg::User(text) => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    " You",
                    Style::default().fg(Color::Cyan).bold(),
                )));
                for raw_line in text.lines() {
                    let wrapped = textwrap(raw_line, inner_width.saturating_sub(2));
                    if wrapped.is_empty() {
                        lines.push(Line::from(""));
                    } else {
                        for w in wrapped {
                            lines.push(Line::from(format!("  {}", w)));
                        }
                    }
                }
            }
            ChatMsg::Assistant(text) => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    " AI",
                    Style::default().fg(Color::Green).bold(),
                )));
                for raw_line in text.lines() {
                    let wrapped = textwrap(raw_line, inner_width.saturating_sub(2));
                    if wrapped.is_empty() {
                        lines.push(Line::from(""));
                    } else {
                        for w in wrapped {
                            lines.push(Line::from(format!("  {}", w)));
                        }
                    }
                }
            }
            ChatMsg::ToolCall { name, args } => {
                let max_args = inner_width.saturating_sub(name.len() + 8);
                let preview = if args.chars().count() > max_args {
                    let end = args.char_indices().nth(max_args).map(|(i,_)| i).unwrap_or(args.len());
                    format!("{}…", &args[..end])
                } else {
                    args.clone()
                };
                lines.push(Line::from(vec![
                    Span::styled("  ⟳ ", Style::default().fg(Color::Yellow)),
                    Span::styled(name, Style::default().fg(Color::Yellow)),
                    Span::styled(format!("({})", preview), Style::default().fg(Color::DarkGray)),
                ]));
            }
            ChatMsg::ToolResult { name, preview } => {
                let max_p = inner_width.saturating_sub(name.len() + 4);
                let p = if preview.chars().count() > max_p {
                    let end = preview.char_indices().nth(max_p).map(|(i,_)| i).unwrap_or(preview.len());
                    format!("{}…", &preview[..end])
                } else {
                    preview.clone()
                };
                lines.push(Line::from(vec![
                    Span::styled("  ✓ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(name, Style::default().fg(Color::DarkGray)),
                    Span::raw(format!(": {}", p)),
                ]));
            }
            ChatMsg::Error(e) => {
                for wrapped in textwrap(e, inner_width.saturating_sub(10)) {
                    lines.push(Line::from(Span::styled(
                        format!("  Error: {}", wrapped),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
        }
    }

    if app.is_loading {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " ⟳  Thinking…",
            Style::default().fg(Color::Yellow),
        )));
    }

    // chat_scroll = 0 → pinned to bottom (newest messages).
    // k/↑ increments chat_scroll (scroll up to older content).
    // j/↓ decrements chat_scroll (scroll back down to newer content).
    let total_lines = lines.len() as u16;
    let visible_height = area.height.saturating_sub(2);
    let bottom_offset = total_lines.saturating_sub(visible_height);
    let effective_scroll = bottom_offset.saturating_sub(app.chat_scroll);

    let p = Paragraph::new(lines)
        .block(Block::default().title(" Chat ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .scroll((effective_scroll, 0));

    f.render_widget(p, area);
}

// ── Time & Sales tab ──────────────────────────────────────────────────────────

// ─── Pairs tab ───────────────────────────────────────────────────────────────

fn render_pairs(f: &mut Frame, area: Rect, app: &App) {
    use crate::pairs::{MatchType, ResolutionRisk};

    let matcher_label = if app.pairs_loading {
        " ⟳ matching… "
    } else if app.pairs.iter().any(|p| p.llm_matched) {
        " LLM "
    } else {
        " Jaccard "
    };
    let arb_count = app.pairs.iter().filter(|p| p.net_gap > 0.0).count();
    let header_title = format!(
        " Cross-Platform Pairs  {}  [{} pairs  {} arb]  match≥{:.0}%  [ ] adjust ",
        matcher_label,
        app.pairs.len(),
        arb_count,
        app.pairs_jaccard_threshold * 100.0,
    );

    if app.pairs.is_empty() {
        let msg = if app.pairs_loading {
            "Matching markets with LLM — please wait…"
        } else {
            "No matching pairs found. Type /pairs to run LLM matching, or press ^ to refresh."
        };
        let block = Block::default()
            .title(header_title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let para = Paragraph::new(msg)
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(para, area);
        return;
    }

    // Split: left list (40%) | right detail (60%)
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    // ── Left: pairs list ──────────────────────────────────────────────────────
    let list_block = Block::default()
        .title(header_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let items: Vec<ListItem> = app.pairs.iter().enumerate().map(|(i, p)| {
        let star_str = match p.stars {
            3 => "★★★",
            2 => "★★☆",
            1 => "★☆☆",
            _ => "☆☆☆",
        };
        let net_color = if p.net_gap > 0.02 { Color::Green }
                        else if p.net_gap > 0.0 { Color::Yellow }
                        else { Color::Red };
        let match_char = match p.match_type {
            MatchType::Identical     => "≡",
            MatchType::NearIdentical => "≈",
            MatchType::Related       => "~",
            MatchType::Different     => "?",
        };
        let llm_tag = if p.llm_matched { "⚡" } else { "" };

        let line = Line::from(vec![
            Span::styled(
                format!("{} {} ", star_str, match_char),
                Style::default().fg(if i == app.pairs_cursor { Color::Cyan } else { Color::DarkGray }),
            ),
            Span::styled(
                trunc(&p.pm_market.title, 24),
                Style::default().fg(if i == app.pairs_cursor { Color::White } else { Color::Gray }),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{:+.1}pp{}", p.net_gap * 100.0, llm_tag),
                Style::default().fg(net_color),
            ),
        ]);
        ListItem::new(line)
    }).collect();

    let mut list_state = ListState::default();
    list_state.select(Some(app.pairs_cursor));
    f.render_stateful_widget(
        List::new(items)
            .block(list_block)
            .highlight_style(Style::default().bg(Color::DarkGray)),
        cols[0],
        &mut list_state,
    );

    // ── Right: detail panel ───────────────────────────────────────────────────
    let Some(pair) = app.pairs.get(app.pairs_cursor) else { return; };

    let detail_block = Block::default()
        .title(format!(" {} ", trunc(&pair.pm_market.title, 60)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = detail_block.inner(cols[1]);
    f.render_widget(detail_block, cols[1]);

    // Layout inside detail: two price rows then analysis rows
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // PM + KL price blocks side-by-side
            Constraint::Length(1), // gap row
            Constraint::Min(0),    // analysis text
        ])
        .split(inner);

    // Price columns
    let price_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);

    let pm_price_color = if pair.buy_yes_on == crate::markets::Platform::Polymarket {
        Color::Green
    } else {
        Color::Red
    };
    let kl_price_color = if pair.buy_yes_on == crate::markets::Platform::Kalshi {
        Color::Green
    } else {
        Color::Red
    };

    let fmt_money = |v: f64| -> String {
        if v >= 1_000_000.0 { format!("${:.1}M", v / 1_000_000.0) }
        else if v >= 1_000.0 { format!("${:.0}K", v / 1_000.0) }
        else { format!("${:.0}", v) }
    };

    let pm_text = vec![
        Line::from(vec![
            Span::raw("POLYMARKET  "),
            Span::styled(
                format!("YES {:.1}%", pair.pm_market.yes_price * 100.0),
                Style::default().fg(pm_price_color).bold(),
            ),
        ]),
        Line::from(format!(
            "Vol: {}   Liq: {}",
            pair.pm_market.volume.map(|v| fmt_money(v)).unwrap_or_else(|| "n/a".into()),
            pair.pm_market.liquidity.map(|l| fmt_money(l)).unwrap_or_else(|| "n/a".into()),
        )),
        Line::from(trunc(&pair.pm_market.title, 36)),
    ];
    f.render_widget(Paragraph::new(pm_text), price_cols[0]);

    let kl_text = vec![
        Line::from(vec![
            Span::raw("KALSHI      "),
            Span::styled(
                format!("YES {:.1}%", pair.kl_market.yes_price * 100.0),
                Style::default().fg(kl_price_color).bold(),
            ),
        ]),
        Line::from(format!(
            "Vol: {}   Liq: {}",
            pair.kl_market.volume.map(|v| fmt_money(v)).unwrap_or_else(|| "n/a".into()),
            pair.kl_market.liquidity.map(|l| fmt_money(l)).unwrap_or_else(|| "n/a".into()),
        )),
        Line::from(trunc(&pair.kl_market.title, 36)),
    ];
    f.render_widget(Paragraph::new(kl_text), price_cols[1]);

    // Gap separator line
    let gap_color = if pair.net_gap > 0.02 { Color::Green }
                    else if pair.net_gap > 0.0 { Color::Yellow }
                    else { Color::Red };
    let stars_str = match pair.stars {
        3 => "★★★",
        2 => "★★☆",
        1 => "★☆☆",
        _ => "☆☆☆",
    };
    let gap_line = Line::from(vec![
        Span::styled(
            format!(
                "  {}  Gross gap: {:.1}pp  │  Net (after {}%+{}% fees): ",
                stars_str,
                pair.gross_gap * 100.0,
                (crate::pairs::PM_TAKER_FEE * 100.0) as u32,
                (crate::pairs::KL_TAKER_FEE * 100.0) as u32,
            ),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{:+.2}pp", pair.net_gap * 100.0),
            Style::default().fg(gap_color).bold(),
        ),
    ]);
    f.render_widget(Paragraph::new(gap_line), rows[1]);

    // Analysis text
    let match_type_str = pair.match_type.label();
    let match_color = match pair.match_type {
        MatchType::Identical     => Color::Green,
        MatchType::NearIdentical => Color::Yellow,
        MatchType::Related       => Color::Magenta,
        MatchType::Different     => Color::DarkGray,
    };
    let risk_color = match pair.res_risk {
        ResolutionRisk::Low    => Color::Green,
        ResolutionRisk::Medium => Color::Yellow,
        ResolutionRisk::High   => Color::Red,
    };
    let llm_or_jaccard = if pair.llm_matched {
        format!("LLM  (confidence {:.0}%)", pair.similarity * 100.0)
    } else {
        format!("Jaccard  (score {:.2})", pair.similarity)
    };

    let analysis_lines = vec![
        Line::from(vec![
            Span::raw("  Match type : "),
            Span::styled(match_type_str, Style::default().fg(match_color).bold()),
        ]),
        Line::from(vec![
            Span::raw("  Res. risk  : "),
            Span::styled(pair.res_risk.label(), Style::default().fg(risk_color).bold()),
            Span::styled(
                format!("  — {}", pair.res_risk_note),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Strategy   : "),
            Span::styled(pair.direction_label(), Style::default().fg(Color::Cyan).bold()),
        ]),
        Line::from(vec![
            Span::raw("  Est. profit: "),
            Span::styled(
                if pair.capturable_usd > 0.0 {
                    format!("~${:.0} at max liquidity", pair.capturable_usd)
                } else {
                    "None (negative net gap)".to_string()
                },
                Style::default().fg(if pair.capturable_usd > 0.0 { Color::Green } else { Color::DarkGray }),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Matched by : "),
            Span::styled(llm_or_jaccard, Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  [/pairs] re-match with LLM   [Enter] open PM market   [j/k] navigate   [[ ]] adjust threshold",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];
    f.render_widget(Paragraph::new(analysis_lines), rows[2]);
}

fn render_trades(f: &mut Frame, area: Rect, app: &App) {
    let title = app.selected_market_id
        .as_ref()
        .and_then(|id| app.markets.iter().find(|m| &m.id == id))
        .map(|m| format!(" Time & Sales — {} ", trunc(&m.title, 50)))
        .unwrap_or_else(|| " Time & Sales ".to_string());

    if app.trades_data.is_empty() {
        let msg = if app.selected_market_id.is_some() {
            "Loading trades… (Polymarket only; Kalshi trade tape not available)"
        } else {
            "Select a Polymarket market and press Enter to load the trade tape."
        };
        let p = Paragraph::new(msg)
            .block(Block::default().title(title).borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, area);
        return;
    }

    let header = Line::from(vec![
        Span::styled(
            format!("  {:<20} {:>6} {:>5} {:>8} {:>8}  {}",
                "Trader", "Type", "Side", "Price", "Size", "Market"),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let sep = Line::from(Span::styled("  ".to_string() + &"─".repeat(70), Style::default().fg(Color::DarkGray)));

    let mut items: Vec<ListItem> = vec![
        ListItem::new(header),
        ListItem::new(sep),
    ];

    for t in &app.trades_data {
        let side_color = if t.side == "BUY" { Color::Green } else if t.side == "SELL" { Color::Red } else { Color::DarkGray };
        let type_color = if t.trade_type == "REDEEM" { Color::Magenta } else { Color::White };
        let name = trunc(&t.pseudonym, 20);
        let market_short = trunc(&t.market_title, 30);

        let line = Line::from(vec![
            Span::raw(format!("  {:<20}", name)),
            Span::styled(format!(" {:>6}", t.trade_type), Style::default().fg(type_color)),
            Span::styled(format!(" {:>5}", if t.side.is_empty() { "—" } else { &t.side }), Style::default().fg(side_color)),
            Span::styled(format!(" {:>8.1}¢", t.price * 100.0), Style::default().fg(price_color(t.price))),
            Span::raw(format!(" {:>8.0}", t.size)),
            Span::raw(format!("  {}", market_short)),
        ]);
        items.push(ListItem::new(line));
    }

    let count = app.trades_data.len();
    let full_title = format!("{} ({} trades) ", title.trim_end(), count);
    let list = List::new(items)
        .block(Block::default().title(full_title).borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
        .highlight_symbol("▶ ");

    let mut state = app.trades_list.clone();
    f.render_stateful_widget(list, area, &mut state);
}

// ── Smart Money tab ───────────────────────────────────────────────────────────

fn render_smart_money(f: &mut Frame, area: Rect, app: &App) {
    if app.sm_loading {
        let p = Paragraph::new("\n  Fetching top traders…  (this may take a few seconds)")
            .block(Block::default().title(" Smart Money ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, area);
        return;
    }

    if app.sm_wallets.is_empty() {
        let msg = if app.selected_market_id.is_some() {
            "No trade history found for this market, or market is on Kalshi (Polymarket only).\nSelect a Polymarket market and press Enter to load Smart Money data."
        } else {
            "Select a Polymarket market in the Markets tab, then press Enter to load Smart Money data."
        };
        let p = Paragraph::new(msg)
            .block(Block::default().title(" Smart Money ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, area);
        return;
    }

    // When a wallet is selected (or loading), split horizontally: list left, detail right.
    let show_detail = app.sm_detail.is_some() || app.sm_detail_loading;
    let (list_area, detail_area_opt) = if show_detail {
        let horiz = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
            .split(area);
        (horiz[0], Some(horiz[1]))
    } else {
        (area, None)
    };

    // ── Vertical split of left side: wallet table + coordination panel + legend
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(app.sm_coord_pairs.len().max(1) as u16 + 3),
            Constraint::Length(13),
        ])
        .split(list_area);

    // ── Top traders table ──────────────────────────────────────────────────
    let title = format!(" Smart Money — {} ({} traders)  coord≥{:.0}%  [/] adjust ",
        app.sm_market_title, app.sm_wallets.len(), app.coord_threshold * 100.0);

    let header = Line::from(vec![
        Span::styled(
            format!("  {:<22} {:>8} {:>6} {:>5} {:>9} {:>10} {:>6} {:>9}",
                "Name", "Pos($)", "Mkts", "Wins", "WinRate", "AlphaEntry", "Vol%", "Suspicion"),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let mut items: Vec<ListItem> = vec![
        ListItem::new(header),
        ListItem::new(Line::from(Span::styled("  ".to_string() + &"─".repeat(84), Style::default().fg(Color::DarkGray)))),
    ];

    for w in &app.sm_wallets {
        let name = trunc(&w.pseudonym, 22);
        let alpha_str = if w.alpha_score.is_nan() {
            "  n/a    ".to_string()
        } else {
            format!("{:>8.1}¢", w.alpha_score * 100.0)
        };

        let suspicion_color = if w.flagged && w.suspicion > 70.0 {
            Color::Red
        } else if w.flagged {
            Color::Yellow
        } else {
            Color::White
        };

        let vol_pct_str = if w.volume_impact > 0.0 {
            format!("{:.1}%", w.volume_impact * 100.0)
        } else {
            "—".to_string()
        };

        let flags = format!("{}{} ",
            if w.is_fresh { "N" } else { " " },
            if w.flagged  { "⚠" } else { " " },
        );

        let line = Line::from(vec![
            Span::styled(flags, Style::default().fg(Color::Yellow)),
            Span::styled(format!("{:<22}", name), Style::default().fg(suspicion_color).bold()),
            Span::raw(format!(" {:>8.0}", w.market_size)),
            Span::raw(format!(" {:>6}", w.n_positions)),
            Span::raw(format!(" {:>5}", w.n_wins)),
            Span::styled(format!(" {:>8.1}%", w.win_rate * 100.0), Style::default().fg(
                if w.win_rate >= 0.7 { Color::Red }
                else if w.win_rate >= 0.55 { Color::Yellow }
                else { Color::White }
            )),
            Span::raw(format!(" {:>10}", alpha_str)),
            Span::styled(format!(" {:>6}", vol_pct_str), Style::default().fg(
                if w.volume_impact > 0.05 { Color::Red }
                else if w.volume_impact > 0.02 { Color::Yellow }
                else { Color::DarkGray }
            )),
            Span::styled(format!(" {:>8.0}/100", w.suspicion), Style::default().fg(suspicion_color)),
        ]);
        items.push(ListItem::new(line));
    }

    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
        .highlight_symbol("▶ ");

    let mut state = app.sm_list.clone();
    f.render_stateful_widget(list, left_chunks[0], &mut state);

    // ── Coordination panel ─────────────────────────────────────────────────
    let coord_title = if app.sm_coord_pairs.is_empty() {
        format!(" Coordination  (none detected, threshold {:.0}%) ", app.coord_threshold * 100.0)
    } else {
        format!(" Coordination  ({} pair(s) ≥{:.0}% overlap) ", app.sm_coord_pairs.len(), app.coord_threshold * 100.0)
    };

    let mut coord_lines: Vec<Line> = vec![Line::from("")];
    if app.sm_coord_pairs.is_empty() {
        coord_lines.push(Line::from(Span::styled(
            "  No wallet pairs share ≥35% of traded markets.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (a, b, sim) in &app.sm_coord_pairs {
            coord_lines.push(Line::from(vec![
                Span::styled("  ⚠ ", Style::default().fg(Color::Yellow)),
                Span::styled(trunc(a, 18), Style::default().fg(Color::Yellow)),
                Span::raw("  ↔  "),
                Span::styled(trunc(b, 18), Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("  ({:.0}% market overlap)", sim * 100.0),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    coord_lines.push(Line::from(Span::styled(
        "  Press 'a' to ask AI for a deep-dive  |  [a] ask AI",
        Style::default().fg(Color::DarkGray),
    )));

    let coord_p = Paragraph::new(coord_lines)
        .block(Block::default().title(coord_title).borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
    f.render_widget(coord_p, left_chunks[1]);

    // ── Legend / quick-reference panel ────────────────────────────────────
    render_sm_legend(f, left_chunks[2]);

    // ── Wallet detail panel (right side) ───────────────────────────────────
    if let Some(detail_area) = detail_area_opt {
        render_sm_wallet_detail(f, detail_area, app);
    }
}

/// Notation / usage legend pinned to the bottom-left of the Smart Money tab.
fn render_sm_legend(f: &mut Frame, area: Rect) {
    let dg = Style::default().fg(Color::DarkGray);
    let hi = Style::default().fg(Color::White);
    let yw = Style::default().fg(Color::Yellow);

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  N ", yw),
            Span::styled("fresh wallet (≤10 lifetime trades, all within 7d)  ", dg),
            Span::styled("⚠ ", yw),
            Span::styled("flagged (suspicion ≥ threshold)", dg),
        ]),
        Line::from(vec![
            Span::styled("  WinRate ", dg),
            Span::styled("redeems / total positions  ", hi),
            Span::styled("AlphaEntry ", dg),
            Span::styled("avg entry price delta on winning trades", hi),
        ]),
        Line::from(vec![
            Span::styled("  Vol%    ", dg),
            Span::styled("wallet position / market daily volume  ", hi),
            Span::styled("Suspicion ", dg),
            Span::styled("0–100 composite score", hi),
        ]),
        Line::from(vec![
            Span::styled("  Suspicion = ", dg),
            Span::styled("fresh×0.4 + vol_anomaly×0.35 + win_rate×0.25", hi),
            Span::styled("  (×1.2/1.3/1.5 for 2/3 signals / niche market)", dg),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Enter ", yw), Span::styled("wallet drill-down  ", dg),
            Span::styled("Esc ", yw),    Span::styled("back to list  ", dg),
            Span::styled("[ ] ", yw),    Span::styled("adjust coord threshold  ", dg),
            Span::styled("^ ", yw),      Span::styled("refresh", dg),
        ]),
        Line::from(vec![
            Span::styled("  j/k ", yw),  Span::styled("navigate / scroll  ", dg),
            Span::styled("@ ", yw),      Span::styled("pre-fill AI prompt for selected market", dg),
        ]),
    ];

    let p = Paragraph::new(lines)
        .block(Block::default().title(" Legend & Keys ").borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)));
    f.render_widget(p, area);
}

/// Render the wallet detail panel (right side of Smart Money split view).
fn render_sm_wallet_detail(f: &mut Frame, area: Rect, app: &App) {
    use chrono::{DateTime, Utc};

    if app.sm_detail_loading {
        let p = Paragraph::new("\n  Fetching wallet history…")
            .block(Block::default().title(" Wallet Detail ").borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, area);
        return;
    }

    let Some(detail) = &app.sm_detail else { return };

    // ── Stats header (fixed height) ────────────────────────────────────────
    let stats_height: u16 = 10;
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(stats_height), Constraint::Min(0)])
        .split(area);

    let age_str = if detail.is_fresh {
        "fresh / new wallet".to_string()
    } else {
        detail.wallet_age_days
            .map(|d| if d >= 365.0 { format!("{:.1}y old", d / 365.0) } else { format!("{:.0}d old", d) })
            .unwrap_or_else(|| "age unknown".to_string())
    };
    let alpha_str = if detail.alpha_score.is_nan() {
        "n/a".to_string()
    } else {
        format!("{:.1}¢", detail.alpha_score * 100.0)
    };

    let stats_lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Wallet  ", Style::default().fg(Color::DarkGray)),
            Span::styled(trunc(&detail.wallet, 42), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled("  Name    ", Style::default().fg(Color::DarkGray)),
            Span::styled(&detail.pseudonym as &str, Style::default().fg(Color::White).bold()),
            Span::styled(format!("  ({})", age_str), Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(vec![
            Span::styled("  Trades  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{} trades  ", detail.recent_trades.len())),
            Span::styled("Positions  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{}", detail.n_positions)),
        ]),
        Line::from(vec![
            Span::styled("  Wins    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{}/{}", detail.n_wins, detail.n_positions)),
            Span::styled("  Win Rate  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.1}%", detail.win_rate * 100.0),
                Style::default().fg(if detail.win_rate >= 0.7 { Color::Red } else if detail.win_rate >= 0.55 { Color::Yellow } else { Color::White }),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Vol $   ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("${:.0}", detail.total_vol)),
            Span::styled("  Alpha   ", Style::default().fg(Color::DarkGray)),
            Span::raw(alpha_str),
        ]),
        Line::from(vec![
            Span::styled("  Top markets  ", Style::default().fg(Color::DarkGray)),
            Span::raw(
                detail.top_markets.iter().take(3)
                    .map(|(t, v)| format!("{} (${:.0})", trunc(t, 20), v))
                    .collect::<Vec<_>>().join("  ·  ")
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  j/k scroll  ·  Esc to return",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let stats_p = Paragraph::new(stats_lines)
        .block(Block::default()
            .title(format!(" {} ", detail.pseudonym))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(stats_p, vert[0]);

    // ── Trade history (scrollable) ─────────────────────────────────────────
    let trade_header = Line::from(vec![
        Span::styled(
            format!("  {:<12} {:<6} {:<7} {:>8} {:>7}  {}", "Date", "Type", "Side", "Size", "Price", "Market"),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let mut trade_lines: Vec<Line> = vec![trade_header,
        Line::from(Span::styled("  ".to_string() + &"─".repeat(70), Style::default().fg(Color::DarkGray)))
    ];

    for trade in &detail.recent_trades {
        let dt = DateTime::<Utc>::from_timestamp(trade.timestamp, 0)
            .map(|d| d.format("%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".to_string());
        let type_col = if trade.trade_type == "REDEEM" {
            Span::styled(format!("{:<6}", "RDEEM"), Style::default().fg(Color::Green))
        } else {
            Span::raw(format!("{:<6}", "TRADE"))
        };
        let side_col = match trade.side.as_str() {
            "BUY"  => Span::styled(format!("{:<7}", "BUY"), Style::default().fg(Color::Green)),
            "SELL" => Span::styled(format!("{:<7}", "SELL"), Style::default().fg(Color::Red)),
            _      => Span::styled(format!("{:<7}", "—"), Style::default().fg(Color::DarkGray)),
        };

        trade_lines.push(Line::from(vec![
            Span::raw(format!("  {:<12} ", dt)),
            type_col,
            Span::raw(" "),
            side_col,
            Span::raw(format!(" {:>8.1}", trade.size)),
            Span::styled(
                format!(" {:>6.1}¢  ", trade.price * 100.0),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(trunc(&trade.market_title, 30)),
        ]));
    }

    if detail.recent_trades.is_empty() {
        trade_lines.push(Line::from(Span::styled(
            "  No recent trades found.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let trade_p = Paragraph::new(trade_lines)
        .block(Block::default().title(" Trade History ").borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)))
        .scroll((app.sm_detail_scroll, 0));
    f.render_widget(trade_p, vert[1]);
}

// ── Help overlay ─────────────────────────────────────────────────────────────

fn centered_rect(pct_x: u16, pct_y: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vert[1])[1]
}

// ── Theta / time-value helper ─────────────────────────────────────────────────

/// Returns `(days_left, daily_sigma_pp, annual_vol_pct)` for a market.
///
/// Uses the Brownian-bridge approximation for a binary that settles at {0,1}:
///   daily σ = sqrt(p*(1-p) / T)  in probability-point units
///   annualised vol = sqrt(p*(1-p) / T * 365) * 100 as a percentage
fn market_time_value(market: &Market) -> Option<(i64, f64, f64)> {
    let end_str = market.end_date.as_deref()?;
    // Accept "YYYY-MM-DDTHH:MM:SSZ", "YYYY-MM-DD HH:MM:SS", and "YYYY-MM-DD"
    let end_date = chrono::NaiveDate::parse_from_str(end_str, "%Y-%m-%dT%H:%M:%SZ")
        .or_else(|_| chrono::NaiveDate::parse_from_str(end_str, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| chrono::NaiveDate::parse_from_str(end_str, "%Y-%m-%d"))
        .ok()?;
    let today = chrono::Local::now().date_naive();
    let days  = (end_date - today).num_days();
    if days <= 0 { return None; }

    let p = market.yes_price.clamp(0.001, 0.999);
    let variance   = p * (1.0 - p);
    let daily_sig  = (variance / days as f64).sqrt() * 100.0; // pp per day
    let annual_vol = (variance / days as f64 * 365.0_f64).sqrt() * 100.0;
    Some((days, daily_sig, annual_vol))
}

// ── Kelly position-sizer modal ────────────────────────────────────────────────

fn render_kelly_modal(f: &mut Frame, area: Rect, app: &App) {
    let Some(market) = app.selected_market() else { return };

    let popup = centered_rect(56, 60, area);
    f.render_widget(Clear, popup);

    let p_mkt  = market.yes_price;
    let title_short: String = market.title.chars().take(52).collect();

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Market: ", Style::default().fg(Color::DarkGray)),
            Span::styled(title_short, Style::default().fg(Color::White).bold()),
        ]),
        Line::from(vec![
            Span::styled("  YES price: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:.1}¢  ({:.1}%)", p_mkt * 100.0, p_mkt * 100.0),
                Style::default().fg(Color::Green).bold()),
        ]),
        Line::from(""),
    ];

    match app.kelly_step {
        KellyStep::MyProb => {
            lines.push(Line::from(vec![
                Span::styled("  Your probability estimate: ", Style::default().fg(Color::Yellow)),
                Span::styled(format!("{}%█", app.kelly_input), Style::default().fg(Color::White).bold()),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("  Enter 0–100, then press Enter", Style::default().fg(Color::DarkGray))));
        }

        KellyStep::Bankroll => {
            let p_mine = app.kelly_my_prob.unwrap_or(p_mkt);
            let edge = p_mine - p_mkt;
            let edge_color = if edge > 0.0 { Color::Green } else if edge < 0.0 { Color::Red } else { Color::DarkGray };
            lines.push(Line::from(vec![
                Span::styled("  Your estimate:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{:.1}%", p_mine * 100.0), Style::default().fg(Color::White).bold()),
                Span::raw("   Edge: "),
                Span::styled(format!("{:+.1}pp", edge * 100.0), Style::default().fg(edge_color).bold()),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  Your bankroll ($): ", Style::default().fg(Color::Yellow)),
                Span::styled(format!("{}█", app.kelly_input), Style::default().fg(Color::White).bold()),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("  Enter bankroll in dollars, then press Enter", Style::default().fg(Color::DarkGray))));
        }

        KellyStep::Result => {
            let p_mine    = app.kelly_my_prob.unwrap_or(p_mkt);
            let bankroll  = app.kelly_bankroll;
            let edge      = p_mine - p_mkt;
            let edge_color = if edge > 0.0 { Color::Green } else { Color::Red };

            // Kelly fractions
            let (kelly_frac, side_label) = if edge > 0.0 {
                ((p_mine - p_mkt) / (1.0 - p_mkt), "YES")   // buy YES
            } else {
                ((p_mkt - p_mine) / p_mkt, "NO ")             // buy NO
            };

            let kelly_frac  = kelly_frac.max(0.0);
            let full_kelly  = kelly_frac * bankroll;
            let half_kelly  = full_kelly * 0.5;
            let qtr_kelly   = full_kelly * 0.25;

            // EV per dollar invested
            let ev_per_dollar = edge.abs(); // = |p_mine - p_mkt|
            // Implied daily σ / theta
            let time_info = market_time_value(market);

            lines.push(Line::from(vec![
                Span::styled("  Your estimate:  ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{:.1}%", p_mine * 100.0), Style::default().fg(Color::White).bold()),
                Span::raw("   "),
                Span::styled(format!("Edge {:+.1}pp", edge * 100.0), Style::default().fg(edge_color).bold()),
                Span::raw(format!("   Side: {}", side_label)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  EV per $1 invested: ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{:+.3}", ev_per_dollar), Style::default().fg(edge_color).bold()),
                Span::raw(format!("   Bankroll: ${:.0}", bankroll)),
            ]));

            if let Some((days, daily_sig, annual_vol)) = time_info {
                lines.push(Line::from(vec![
                    Span::styled("  Days to resolution: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{}", days), Style::default().fg(Color::White)),
                    Span::raw(format!("   Daily σ: {:.2}pp   Ann vol: {:.1}%", daily_sig, annual_vol)),
                ]));
            }

            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  ─────────── KELLY SIZING ───────────",
                Style::default().fg(Color::Cyan),
            )));
            lines.push(Line::from(""));

            let fmt_kelly = |label: &str, dollars: f64, frac: f64| {
                Line::from(vec![
                    Span::styled(format!("  {:<16}", label), Style::default().fg(Color::Yellow).bold()),
                    Span::styled(format!("${:>9.0}", dollars), Style::default().fg(Color::White).bold()),
                    Span::styled(format!("  ({:.2}%)", frac * 100.0), Style::default().fg(Color::DarkGray)),
                ])
            };

            lines.push(fmt_kelly("Full Kelly:", full_kelly, kelly_frac));
            lines.push(fmt_kelly("Half Kelly:", half_kelly, kelly_frac * 0.5));
            lines.push(fmt_kelly("Quarter Kelly:", qtr_kelly, kelly_frac * 0.25));

            if kelly_frac <= 0.0 {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  No edge — Kelly says: do not bet.",
                    Style::default().fg(Color::Red).bold(),
                )));
            }

            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [/add] Add position   [/kelly] Recalculate   [Esc] Close",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let border_color = match app.kelly_step {
        KellyStep::MyProb | KellyStep::Bankroll => Color::Yellow,
        KellyStep::Result                        => Color::Cyan,
    };

    let p = Paragraph::new(lines)
        .block(
            Block::default()
                .title("  Kelly Position Sizer  ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(p, popup);
}

fn render_loading_overlay(f: &mut Frame, area: Rect, app: &App) {
    // Braille spinner frames
    const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let frame = SPINNER[(app.spinner_tick as usize) % SPINNER.len()];

    // Indeterminate "knight-rider" progress bar (20 cells wide)
    const BAR_WIDTH: usize = 20;
    let pos = (app.spinner_tick as usize / 2) % (BAR_WIDTH * 2);
    // bounce: 0..BAR_WIDTH forward, BAR_WIDTH..BAR_WIDTH*2 backward
    let head = if pos < BAR_WIDTH { pos } else { BAR_WIDTH * 2 - 1 - pos };
    let bar: String = (0..BAR_WIDTH)
        .map(|i| if i == head { '█' } else if i.abs_diff(head) <= 1 { '▓' } else if i.abs_diff(head) <= 2 { '░' } else { '·' })
        .collect();

    let step_lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {}  Connecting to Polymarket & Kalshi…", frame),
                Style::default().fg(Color::Cyan).bold(),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("[{}]", bar), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {}", app.status),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  Fetching live orderbooks, price history, and computing signals…",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  This typically takes 5–15 seconds on first launch.",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
    ];

    // Use a fixed-size centered popup (50% wide, auto height)
    let popup = centered_rect(54, 40, area);
    f.render_widget(Clear, popup);

    let p = Paragraph::new(step_lines)
        .block(
            Block::default()
                .title("  WhoIsSharp — Loading  ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(p, popup);
}

fn render_help_overlay(f: &mut Frame, area: Rect) {
    let popup = centered_rect(66, 92, area);
    f.render_widget(Clear, popup);

    let h = |s: &'static str| Line::from(vec![Span::styled(s, Style::default().fg(Color::Cyan).bold())]);
    let kv = |k: &'static str, v: &'static str| Line::from(vec![
        Span::styled(format!("  {:<24}", k), Style::default().fg(Color::Yellow)),
        Span::raw(v),
    ]);

    let lines = vec![
        Line::from(""),
        h(" Navigation"),
        kv("1–9 / Tab / Shift+Tab", "Switch tabs directly"),
        kv("j / ↓  ·  k / ↑", "Navigate list / scroll"),
        kv("Enter", "Select market → load chart + book + trades"),
        kv("Ctrl+C", "Quit (saves session automatically)"),
        Line::from(""),
        h(" Slash commands  —  press / then type and Enter"),
        kv("/refresh  or  /r  or  ^", "Refresh markets + chart + orderbook"),
        kv("/platform  or  /p", "Cycle platform filter  ALL → PM → KL"),
        kv("/chart  or  /c", "Cycle chart interval  1h → 6h → 1d → 1w → 1m"),
        kv("/sort  or  /s", "Cycle sort  ~50% → Volume → End Date → A-Z"),
        kv("/watchlist  or  /w", "Toggle watchlist for selected market  (★)"),
        kv("/wf", "Toggle watchlist-only filter"),
        kv("/alert  or  /e", "Edit price alert thresholds (above / below)"),
        kv("/add  or  /n", "Add position for selected market"),
        kv("/wallet <0x…>", "Import Polymarket wallet positions  (sync on each call)"),
        kv("/wallet sync", "Re-sync all registered wallet addresses"),
        kv("/wallet analyze  or  /wa", "Ask AI to analyse registered wallet(s)"),
        Line::from(""),
        h(" News tab  —  key [0]"),
        kv("0", "Open news feed for selected market  (requires NEWSDATA_API_KEY)"),
        kv("/refresh  (on News tab)", "Re-fetch news for selected market"),
        kv("/targets  or  /t", "Set take-profit / stop-loss"),
        kv("/delete  or  /d", "Delete selected position  (Portfolio tab)"),
        kv("/dismiss  or  /x", "Dismiss selected signal (hidden until restart)"),
        kv("/analyze  or  /a  or  @", "Pre-fill AI analysis prompt"),
        kv("/kelly  or  /k", "Open Kelly position-size calculator"),
        kv("/risk  or  /v", "Toggle risk/exposure view  (Portfolio tab)"),
        kv("/pairs  or  /l", "LLM re-match  (Pairs tab)"),
        kv("/lower  /  /raise", "Adjust threshold  (also: [ / ] keys)"),
        kv("/export  or  /csv", "Export current tab to CSV"),
        kv("/report  or  /m", "Export Markdown report for selected market"),
        kv("/help  or  /?  or  ?", "Toggle this help overlay"),
        Line::from(""),
        h(" Search / filter"),
        kv("/", "Open command bar — unrecognised terms search markets"),
        kv("Esc", "Close command bar / clear filter"),
        Line::from(""),
        h(" Chat / AI"),
        kv("Enter", "Send chat message"),
        kv("↑ / ↓", "Scroll input history"),
        kv("!note <text>", "Append timestamped note to research log"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "  Press /help or Esc to close               WhoIsSharp v0.1.0",
            Style::default().fg(Color::DarkGray),
        )]),
    ];

    let p = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" WhoIsSharp — Key Bindings ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(p, popup);
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let filter   = app.platform_filter.label();
    let interval = app.chart_interval.label();

    // Refresh countdown
    let refresh_str = match app.next_refresh_at {
        Some(t) => {
            let secs = t.saturating_duration_since(std::time::Instant::now()).as_secs();
            format!(" ↺{}s ", secs)
        }
        None => " ↺off ".to_string(),
    };

    // Watchlist indicator
    let wl_str = if app.watchlist.is_empty() {
        String::new()
    } else {
        format!(" ★{} ", app.watchlist.len())
    };
    let wl_color = if app.watchlist_only { Color::Yellow } else { Color::DarkGray };

    // Alert or status text; when idle show a rotating tip
    let (status_text, status_color) = if !app.watch_alerts.is_empty() {
        (app.watch_alerts.join("  "), Color::Yellow)
    } else if app.status == "Ready" || app.status.is_empty() {
        let tip = TIPS[app.tip_index % TIPS.len()];
        (format!("Tip: {}", tip), Color::DarkGray)
    } else {
        (app.status.clone(), Color::White)
    };

    let line = Line::from(vec![
        Span::styled(
            if app.is_loading { " ⟳ Loading " } else { " ● Ready   " },
            Style::default().fg(if app.is_loading { Color::Yellow } else { Color::Green }),
        ),
        Span::raw(format!(" {}  Chart:{}  ", filter, interval)),
        Span::styled(refresh_str, Style::default().fg(Color::DarkGray)),
        Span::styled(wl_str, Style::default().fg(wl_color)),
        Span::styled("│", Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(status_text, Style::default().fg(status_color)),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::DarkGray)),
        area,
    );
}

// ── Input box ────────────────────────────────────────────────────────────────

fn render_input(f: &mut Frame, area: Rect, app: &App) {
    let (prompt, content): (&str, &str) = if app.search_mode {
        ("/ ", &app.command_input)
    } else if app.pos_input_mode {
        ("pos> ", &app.input)
    } else {
        ("> ", &app.input)
    };
    let line = Line::from(vec![
        Span::styled(prompt, Style::default().fg(Color::Cyan)),
        Span::raw(content),
    ]);
    let p = Paragraph::new(line);
    f.render_widget(p, area);

    // Show cursor
    let x = area.x + prompt.len() as u16 + content.len() as u16;
    if x < area.x + area.width {
        f.set_cursor_position((x, area.y));
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn price_color(p: f64) -> Color {
    if p >= 0.7      { Color::Green }
    else if p >= 0.5 { Color::LightGreen }
    else if p >= 0.3 { Color::Yellow }
    else if p >= 0.15 { Color::LightRed }
    else             { Color::Red }
}

fn format_volume(v: Option<f64>) -> String {
    match v {
        None => String::new(),
        Some(v) if v >= 1_000_000.0 => format!("${:.1}M", v / 1_000_000.0),
        Some(v) if v >= 1_000.0     => format!("${:.0}K", v / 1_000.0),
        Some(v)                     => format!("${:.0}", v),
    }
}

/// Shorten a wallet address for display: `0x1234…abcd`.
fn short_wallet(addr: &str) -> String {
    let a = addr.trim();
    if a.len() <= 12 { return a.to_string(); }
    format!("{}…{}", &a[..6], &a[a.len() - 4..])
}

/// Truncate `s` to at most `max_chars` Unicode scalar values, appending `…` if cut.
fn trunc(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let mut out = String::with_capacity(max_chars + 3);
    let mut count = 0;
    while let Some(c) = chars.next() {
        if count == max_chars {
            out.push('…');
            return out;
        }
        out.push(c);
        count += 1;
    }
    out
}

fn textwrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 { return vec![s.to_string()]; }
    let mut lines = Vec::new();
    let mut line  = String::new();
    for word in s.split_whitespace() {
        if line.is_empty() {
            line.push_str(word);
        } else if line.len() + 1 + word.len() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            lines.push(std::mem::take(&mut line));
            line.push_str(word);
        }
    }
    if !line.is_empty() { lines.push(line); }
    lines
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── trunc ─────────────────────────────────────────────────────────────────

    #[test]
    fn trunc_short_string_unchanged() {
        assert_eq!(trunc("hello", 10), "hello");
    }

    #[test]
    fn trunc_at_exact_length_unchanged() {
        assert_eq!(trunc("hello", 5), "hello");
    }

    #[test]
    fn trunc_long_string_ellipsis() {
        assert_eq!(trunc("hello world", 5), "hello…");
    }

    #[test]
    fn trunc_unicode_char_boundary_safe() {
        // "café" = 4 chars but 5 bytes; slicing at byte 4 would split 'é'
        assert_eq!(trunc("café extra", 4), "café…");
    }

    #[test]
    fn trunc_multibyte_emoji() {
        // Each emoji is 1 char (but 4 bytes)
        assert_eq!(trunc("🚀🎯🎪 overflow", 3), "🚀🎯🎪…");
    }

    #[test]
    fn trunc_empty_string() {
        assert_eq!(trunc("", 5), "");
    }

    // ── textwrap ──────────────────────────────────────────────────────────────

    #[test]
    fn textwrap_fits_on_one_line() {
        assert_eq!(textwrap("hello world", 20), vec!["hello world"]);
    }

    #[test]
    fn textwrap_wraps_at_word_boundary() {
        let lines = textwrap("one two three four five", 10);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(line.len() <= 10, "line too long: {:?}", line);
        }
        // All words should be present
        let rejoined = lines.join(" ");
        assert!(rejoined.contains("one") && rejoined.contains("five"));
    }

    #[test]
    fn textwrap_zero_width_returns_original() {
        assert_eq!(textwrap("hello world", 0), vec!["hello world"]);
    }

    #[test]
    fn textwrap_empty_input() {
        assert!(textwrap("", 20).is_empty());
    }

    // ── format_volume ─────────────────────────────────────────────────────────

    #[test]
    fn format_volume_none() {
        assert_eq!(format_volume(None), "");
    }

    #[test]
    fn format_volume_millions() {
        assert_eq!(format_volume(Some(2_500_000.0)), "$2.5M");
    }

    #[test]
    fn format_volume_millions_round() {
        assert_eq!(format_volume(Some(1_000_000.0)), "$1.0M");
    }

    #[test]
    fn format_volume_thousands() {
        assert_eq!(format_volume(Some(12_345.0)), "$12K");
    }

    #[test]
    fn format_volume_sub_thousand() {
        assert_eq!(format_volume(Some(500.0)), "$500");
    }

    // ── price_color ───────────────────────────────────────────────────────────

    #[test]
    fn price_color_all_ranges() {
        // Just confirm no panic across the full [0, 1] range
        for i in 0..=100 {
            let _ = price_color(i as f64 / 100.0);
        }
    }

    #[test]
    fn price_color_green_for_high() {
        assert_eq!(price_color(0.75), Color::Green);
    }

    #[test]
    fn price_color_red_for_low() {
        assert_eq!(price_color(0.10), Color::Red);
    }

    // ── PlatformFilter ────────────────────────────────────────────────────────

    #[test]
    fn platform_filter_all_matches_everything() {
        assert!(PlatformFilter::All.matches(&Platform::Polymarket));
        assert!(PlatformFilter::All.matches(&Platform::Kalshi));
    }

    #[test]
    fn platform_filter_polymarket_only() {
        assert!( PlatformFilter::Polymarket.matches(&Platform::Polymarket));
        assert!(!PlatformFilter::Polymarket.matches(&Platform::Kalshi));
    }

    #[test]
    fn platform_filter_kalshi_only() {
        assert!( PlatformFilter::Kalshi.matches(&Platform::Kalshi));
        assert!(!PlatformFilter::Kalshi.matches(&Platform::Polymarket));
    }

    // ── Tab cycle ─────────────────────────────────────────────────────────────

    #[test]
    fn tab_next_cycles_forward() {
        assert_eq!(Tab::Signals.next(),     Tab::Markets);
        assert_eq!(Tab::Markets.next(),     Tab::Chart);
        assert_eq!(Tab::Chart.next(),       Tab::Orderbook);
        assert_eq!(Tab::Orderbook.next(),   Tab::Portfolio);
        assert_eq!(Tab::Portfolio.next(),   Tab::Chat);
        assert_eq!(Tab::Chat.next(),        Tab::SmartMoney);
        assert_eq!(Tab::SmartMoney.next(),  Tab::Trades);
        assert_eq!(Tab::Trades.next(),      Tab::Pairs);
        assert_eq!(Tab::Pairs.next(),       Tab::News);
        assert_eq!(Tab::News.next(),        Tab::Signals); // wraps
    }

    #[test]
    fn tab_prev_cycles_backward() {
        assert_eq!(Tab::Signals.prev(),     Tab::News); // wraps
        assert_eq!(Tab::Markets.prev(),     Tab::Signals);
        assert_eq!(Tab::Chat.prev(),        Tab::Portfolio);
        assert_eq!(Tab::SmartMoney.prev(),  Tab::Chat);
        assert_eq!(Tab::Trades.prev(),      Tab::SmartMoney);
        assert_eq!(Tab::Pairs.prev(),       Tab::Trades);
        assert_eq!(Tab::News.prev(),        Tab::Pairs);
    }
}

// ─── Main TUI loop ────────────────────────────────────────────────────────────

pub async fn run_tui(
    backend:      Option<Arc<dyn LlmBackend>>,
    clients:      Arc<MarketClients>,
    backend_name: String,
    refresh_secs: u64,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend_term = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend_term)?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AppEvent>();
    let mut app = App::new(backend_name);
    app.refresh_secs = refresh_secs;
    let mut llm_history: Vec<LlmMessage> = Vec::new();

    // Kick off initial market data refresh
    {
        let clients_clone = clients.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move { agent::refresh_markets(clients_clone, tx).await });
    }

    // Auto-refresh ticker (fires every refresh_secs; disabled when refresh_secs == 0)
    let mut refresh_ticker: Option<tokio::time::Interval> = if refresh_secs > 0 {
        let mut iv = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(refresh_secs),
            std::time::Duration::from_secs(refresh_secs),
        );
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        app.next_refresh_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(refresh_secs));
        Some(iv)
    } else {
        None
    };

    // WebSocket cancel switch — dropped when switching to a new market's orderbook
    let mut ws_cancel: Option<tokio::sync::oneshot::Sender<()>> = None;

    // Spinner ticker — 80 ms interval, only active during initial market load
    let mut spinner_iv = tokio::time::interval(std::time::Duration::from_millis(80));
    spinner_iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut term_events = EventStream::new();

    loop {
        terminal.draw(|f| render(f, &app))?;

        tokio::select! {
            // ── Startup spinner tick (only while markets haven't loaded yet) ──
            _ = spinner_iv.tick(), if app.markets.is_empty() && app.is_loading => {
                app.spinner_tick = app.spinner_tick.wrapping_add(1);
            }

            // ── Auto-refresh tick ──────────────────────────────────────────────
            _ = async {
                match &mut refresh_ticker {
                    Some(iv) => iv.tick().await,
                    None     => { std::future::pending::<tokio::time::Instant>().await }
                }
            } => {
                app.tip_index = app.tip_index.wrapping_add(1);
                let clients_c = clients.clone();
                let tx = event_tx.clone();
                tokio::spawn(async move { agent::refresh_markets(clients_c, tx).await });
                // Refresh chart + orderbook for whichever market is selected
                if app.selected_market_id.is_some() {
                    trigger_chart_load(&app, &clients, &event_tx).await;
                    trigger_orderbook_load(&app, &clients, &event_tx).await;
                }
                if app.refresh_secs > 0 {
                    app.next_refresh_at = Some(
                        std::time::Instant::now()
                            + std::time::Duration::from_secs(app.refresh_secs)
                    );
                }
            }

            Some(ev) = event_rx.recv() => {
                match ev {
                    AppEvent::MarketsLoaded(markets) => {
                        // Snapshot current prices for momentum detection before overwriting
                        let prev: HashMap<String, f64> = app.markets.iter()
                            .map(|m| (m.id.clone(), m.yes_price))
                            .collect();

                        app.markets = markets;
                        if app.market_list.selected().is_none() && !app.markets.is_empty() {
                            app.market_list.select(Some(0));
                        }
                        app.update_portfolio_marks();
                        app.check_position_alerts();
                        app.check_watch_alerts();

                        // Recompute signals with velocity and dismissed state
                        let sigs = signals::compute_signals(
                            &app.markets,
                            &prev,
                            &app.dismissed_signals,
                        );
                        app.prev_prices = prev;
                        app.signals = sigs;
                        if app.signal_list.selected().is_none() && !app.signals.is_empty() {
                            app.signal_list.select(Some(0));
                        }

                        // Always compute Jaccard pairs immediately on market load.
                        // LLM-enhanced matching happens when user visits the Pairs tab.
                        let jaccard = crate::pairs::jaccard_pairs(&app.markets, Some(app.pairs_jaccard_threshold));
                        let arb_count = jaccard.iter().filter(|p| p.net_gap > 0.0).count();
                        if !jaccard.is_empty() {
                            app.pairs = jaccard;
                            app.pairs_cursor = 0;
                        }
                        if arb_count > 0 {
                            app.status = format!("{} arb pair(s) detected (tab 9)", arb_count);
                        }
                    }
                    AppEvent::EventsLoaded(_) => {}  // Events tab removed; ignore
                    AppEvent::SignalsComputed(_) => {} // Now computed inline in MarketsLoaded
                    AppEvent::PriceHistoryLoaded { market_id, candles } => {
                        if Some(&market_id) == app.selected_market_id.as_ref() {
                            app.chart_data = candles
                                .iter()
                                .map(|c| (c.ts as f64, c.close * 100.0))
                                .collect();
                            app.chart_min = candles.iter().map(|c| c.low * 100.0).fold(f64::INFINITY, f64::min);
                            app.chart_max = candles.iter().map(|c| c.high * 100.0).fold(f64::NEG_INFINITY, f64::max);
                            app.chart_candles = candles;
                        }
                    }
                    AppEvent::TradesLoaded { market_id, trades } => {
                        if Some(&market_id) == app.selected_market_id.as_ref() {
                            app.trades_data = trades;
                            if !app.trades_data.is_empty() {
                                app.trades_list.select(Some(0));
                            }
                            app.status = format!("Loaded {} trades", app.trades_data.len());
                        }
                    }
                    AppEvent::OrderbookLoaded { market_id, orderbook } => {
                        if Some(&market_id) == app.selected_market_id.as_ref() {
                            app.orderbook = Some(orderbook);
                        }
                    }
                    AppEvent::SmartMoneyLoading => {
                        app.sm_loading = true;
                        app.sm_wallets.clear();
                        app.sm_coord_pairs.clear();
                        app.status = "Loading smart money data…".to_string();
                    }
                    AppEvent::SmartMoneyLoaded { market_id, result } => {
                        app.sm_loading = false;
                        if Some(&market_id) == app.selected_market_id.as_ref() {
                            app.sm_market_title = result.market_title;
                            app.sm_wallets = result.wallets;
                            app.sm_coord_pairs = result.coord_pairs;
                            app.sm_detail = None;
                            if !app.sm_wallets.is_empty() {
                                app.sm_list.select(Some(2)); // skip header rows
                            }
                            let flagged = app.sm_wallets.iter().filter(|w| w.flagged).count();
                            app.status = format!(
                                "Smart money: {} traders, {} flagged — Enter to drill into a wallet",
                                app.sm_wallets.len(), flagged
                            );
                        }
                    }
                    AppEvent::WalletDetailLoading => {
                        app.sm_detail_loading = true;
                        app.sm_detail = None;
                        app.sm_detail_scroll = 0;
                        app.status = "Loading wallet detail…".to_string();
                    }
                    AppEvent::WalletDetailLoaded(detail) => {
                        app.sm_detail_loading = false;
                        app.status = format!("Wallet: {} — {} trades", detail.pseudonym, detail.recent_trades.len());
                        app.sm_detail = Some(detail);
                    }
                    AppEvent::RefreshStarted => {
                        app.is_loading = true;
                        app.status = "Refreshing…".to_string();
                    }
                    AppEvent::RefreshDone => {
                        app.is_loading = false;
                        app.last_updated = Some(chrono::Local::now());
                        app.status = format!(
                            "{} markets  {} signals",
                            app.markets.len(),
                            app.signals.len()
                        );
                    }
                    AppEvent::RefreshError(e) => {
                        app.status = format!("Error: {}", e);
                    }
                    AppEvent::AgentThinking => {
                        app.is_loading = true;
                        app.status = "AI thinking…".to_string();
                    }
                    AppEvent::AgentToolCall { name, display_args } => {
                        app.chat_msgs.push(ChatMsg::ToolCall {
                            name:  name.clone(),
                            args:  display_args.clone(),
                        });
                        app.status = format!("Calling {}…", name);
                    }
                    AppEvent::AgentToolResult { name, output } => {
                        let preview: String = output.lines().take(2).collect::<Vec<_>>().join(" | ");
                        app.chat_msgs.push(ChatMsg::ToolResult { name, preview });
                    }
                    AppEvent::AgentText(text) => {
                        if let Some(ChatMsg::Assistant(existing)) = app.chat_msgs.last_mut() {
                            if text.len() > existing.len() {
                                *existing = text;
                            }
                        } else {
                            app.chat_msgs.push(ChatMsg::Assistant(text));
                        }
                    }
                    AppEvent::AgentTextChunk(chunk) => {
                        if let Some(ChatMsg::Assistant(existing)) = app.chat_msgs.last_mut() {
                            existing.push_str(&chunk);
                        } else {
                            app.chat_msgs.push(ChatMsg::Assistant(chunk));
                        }
                    }
                    AppEvent::AgentDone => {
                        app.is_loading = false;
                        app.status = "Ready".to_string();
                        // Record the final assistant message into session log
                        if let Some(ChatMsg::Assistant(text)) = app.chat_msgs.last() {
                            app.session.messages.push(portfolio::SessionMessage {
                                role: "assistant".into(),
                                content: text.clone(),
                            });
                        }
                    }
                    AppEvent::AgentError(e) => {
                        app.is_loading = false;
                        app.chat_msgs.push(ChatMsg::Error(e.clone()));
                        app.status = format!("Error: {}", e);
                    }
                    AppEvent::HistoryUpdated(hist) => {
                        llm_history = hist;
                    }
                    AppEvent::PairsMatching => {
                        app.pairs_loading = true;
                        app.status = "Matching pairs with LLM…".to_string();
                    }
                    AppEvent::PairsLoaded(pairs) => {
                        app.pairs_loading = false;
                        app.pairs = pairs;
                        app.pairs_cursor = 0;
                        let arb_count = app.pairs.iter().filter(|p| p.net_gap > 0.0).count();
                        app.status = format!(
                            "{} pairs found  ({} profitable after fees)",
                            app.pairs.len(), arb_count
                        );
                    }
                    AppEvent::NewsLoaded { market_id, articles } => {
                        app.news_loading = false;
                        app.news_error = None;
                        app.news_market_id = market_id;
                        app.news_articles = articles;
                        app.news_list.select(if app.news_articles.is_empty() { None } else { Some(0) });
                        app.news_detail_idx = if app.news_articles.is_empty() { None } else { Some(0) };
                        app.status = format!("{} news article(s) loaded", app.news_articles.len());
                    }
                    AppEvent::NewsError(e) => {
                        app.news_loading = false;
                        app.news_error = Some(e.clone());
                        app.status = format!("News error: {}", e);
                    }
                    AppEvent::WalletImportStarted { wallet } => {
                        let short = short_wallet(&wallet);
                        app.status = format!("Syncing wallet {}…", short);
                        app.is_loading = true;
                    }
                    AppEvent::WalletImportDone { wallet, imported, skipped } => {
                        let short = short_wallet(&wallet);
                        app.is_loading = false;
                        portfolio::save_portfolio(&app.portfolio).ok();
                        app.status = format!(
                            "Wallet {} synced — {} position(s) imported, {} skipped",
                            short, imported, skipped
                        );
                    }
                    AppEvent::WalletImportError { wallet, error } => {
                        let short = short_wallet(&wallet);
                        app.is_loading = false;
                        app.status = format!("Wallet {} import failed: {}", short, error);
                    }
                    AppEvent::WalletPositionsReady(positions) => {
                        for pos in positions {
                            app.portfolio.add(pos);
                        }
                        // update_marks will pick up mark prices on next market refresh
                    }
                }
            }

            Some(Ok(ev)) = term_events.next() => {
                if let Event::Key(key) = ev {
                    let prev_market = app.selected_market_id.clone();

                    if handle_key(&mut app, key, &backend, &clients, &event_tx, &mut llm_history).await {
                        break;
                    }

                    // Start/restart Polymarket WS orderbook stream when a new market is selected
                    if app.selected_market_id != prev_market {
                        drop(ws_cancel.take()); // signal old WS task to exit
                        if let Some(id) = &app.selected_market_id {
                            if let Some(market) = app.markets.iter().find(|m| &m.id == id) {
                                if market.platform == Platform::Polymarket {
                                    if let Some(token_id) = &market.token_id {
                                        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                                        ws_cancel = Some(cancel_tx);
                                        let token  = token_id.clone();
                                        let mkt_id = id.clone();
                                        let tx     = event_tx.clone();
                                        tokio::spawn(async move {
                                            agent::stream_polymarket_orderbook(token, mkt_id, tx, cancel_rx).await;
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Save session on clean exit
    if !app.session.messages.is_empty() || !app.session.notes.is_empty() {
        let _ = portfolio::save_session(&app.session);
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

// ─── News tab ────────────────────────────────────────────────────────────────

fn render_news(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::style::Modifier;

    // Split into left list (40%) and right detail panel (60%).
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    // ── Left: article list ────────────────────────────────────────────────────

    if app.news_loading {
        let p = Paragraph::new("Loading news…")
            .block(Block::default().title(" News ").borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, panes[0]);
        return;
    }

    if app.news_articles.is_empty() {
        let msg = if let Some(ref err) = app.news_error {
            format!("Error: {}", err)
        } else if app.selected_market_id.is_none() {
            "Select a market (Enter), then press 0 to load news.".to_string()
        } else {
            "No news loaded. Press /refresh or 0 to fetch.".to_string()
        };
        let color = if app.news_error.is_some() { Color::Red } else { Color::DarkGray };
        let p = Paragraph::new(msg)
            .wrap(Wrap { trim: false })
            .block(Block::default().title(" News ").borders(Borders::ALL)
                .border_style(Style::default().fg(color)));
        f.render_widget(p, area);
        return;
    }

    let market_label = app.news_market_id.as_deref()
        .and_then(|id| app.markets.iter().find(|m| m.id == id))
        .map(|m| format!(" News — {} ", trunc(&m.title, 30)))
        .unwrap_or_else(|| " News ".to_string());

    let items: Vec<ListItem> = app.news_articles.iter().enumerate().map(|(i, a)| {
        let selected = app.news_detail_idx == Some(i);
        let sentiment_color = match a.sentiment.as_deref() {
            Some("positive") => Color::Green,
            Some("negative") => Color::Red,
            _                => Color::DarkGray,
        };
        let badge = a.sentiment_char();
        let age   = a.age_label();
        let title: String = a.title.chars().take(48).collect();

        let line = Line::from(vec![
            Span::styled(
                format!("{} ", badge),
                Style::default().fg(sentiment_color).bold(),
            ),
            Span::styled(
                title,
                if selected {
                    Style::default().fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                },
            ),
            Span::raw("  "),
            Span::styled(
                format!("{:>4}", age),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        ListItem::new(line)
    }).collect();

    let list = List::new(items)
        .block(Block::default().title(market_label).borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)))
        .highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▶ ");

    let mut state = app.news_list.clone();
    f.render_stateful_widget(list, panes[0], &mut state);

    // ── Right: article detail ─────────────────────────────────────────────────

    let detail_block = Block::default()
        .title(" Article ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let Some(idx) = app.news_detail_idx else {
        f.render_widget(detail_block, panes[1]);
        return;
    };
    let Some(a) = app.news_articles.get(idx) else {
        f.render_widget(detail_block, panes[1]);
        return;
    };

    let sentiment_str = match a.sentiment.as_deref() {
        Some("positive") => " [+positive]",
        Some("negative") => " [-negative]",
        Some("neutral")  => " [~neutral]",
        _                => "",
    };
    let sentiment_color = match a.sentiment.as_deref() {
        Some("positive") => Color::Green,
        Some("negative") => Color::Red,
        _                => Color::DarkGray,
    };

    let mut lines: Vec<Line> = Vec::new();

    // Title
    lines.push(Line::from(Span::styled(
        a.title.clone(),
        Style::default().fg(Color::White).bold().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    // Meta row: source · age · sentiment
    lines.push(Line::from(vec![
        Span::styled(a.source_name.clone(), Style::default().fg(Color::Cyan)),
        Span::raw("  ·  "),
        Span::styled(a.age_label(), Style::default().fg(Color::DarkGray)),
        Span::styled(sentiment_str, Style::default().fg(sentiment_color)),
    ]));
    lines.push(Line::from(""));

    // Description (word-wrapped at panel width)
    let panel_w = panes[1].width.saturating_sub(4) as usize;
    if panel_w > 0 && !a.description.is_empty() {
        for word_line in wrap_text(&a.description, panel_w) {
            lines.push(Line::from(Span::raw(word_line)));
        }
        lines.push(Line::from(""));
    }

    // Keywords
    if let Some(kws) = &a.keywords {
        if !kws.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("Keywords: ", Style::default().fg(Color::DarkGray)),
                Span::raw(kws.join(", ")),
            ]));
            lines.push(Line::from(""));
        }
    }

    // Link (dim, at bottom)
    lines.push(Line::from(Span::styled(
        a.link.clone(),
        Style::default().fg(Color::DarkGray),
    )));

    let p = Paragraph::new(lines)
        .block(detail_block)
        .wrap(Wrap { trim: true });
    f.render_widget(p, panes[1]);
}

/// Naïve word-wrapper: splits `text` into lines of at most `width` chars.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current.clone());
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

// ─── Key handling ─────────────────────────────────────────────────────────────

enum SlashCmd { Handled, NotACommand, Quit }

/// Pre-fill the chat input with a wallet analysis prompt for all registered wallets.
///
/// The LLM is directed to call `get_wallet_positions` (live snapshot from the
/// data-api) and `analyze_wallet` (deep trade-history profile) for each wallet,
/// then synthesise the findings.
fn wallet_analyze_prompt(app: &mut App) {
    if app.wallet_addresses.is_empty() {
        app.status = "No wallets registered. Use /wallet <0x…> first.".to_string();
        return;
    }

    // Build a wallet list string: "0xabc…  /  0xdef…"
    let wallet_list = app.wallet_addresses.join("  /  ");
    let count = app.wallet_addresses.len();
    let plural = if count == 1 { "wallet" } else { "wallets" };

    app.input = format!(
        "Analyze my Polymarket {plural} ({count} registered).\n\
         Wallet address{es}: {wallet_list}\n\n\
         For each wallet:\n\
         1. Call `get_wallet_positions` to see the current open positions (live snapshot).\n\
         2. Call `analyze_wallet` for the full trade-history profile \
            (win rate, alpha-entry score, timing signals, suspicion score).\n\
         3. For any markets with meaningful exposure, call `get_price_history` to \
            show how the position has moved since entry.\n\n\
         Then give me:\n\
         • A concise portfolio summary (total exposure, P&L direction, biggest bets).\n\
         • Win-rate and edge assessment — am I statistically an alpha trader or noise?\n\
         • Top 2-3 actionable insights or risk flags (e.g. over-concentrated, \
           position approaching stop-loss territory, a market about to resolve).",
        plural = plural,
        count = count,
        es = if count == 1 { "" } else { "es" },
        wallet_list = wallet_list,
    );
    // Switch to Chat tab so the user sees the pre-filled prompt.
    app.active_tab = Tab::Chat;
    app.status = "Wallet analysis prompt ready — press Enter to send.".to_string();
}

/// Return (title, id, platform) for the market the user wants to analyze.
///
/// Prefers `selected_market_id` (the market explicitly loaded via Enter) over
/// the list-selection index, which can drift when the search filter changes
/// while the user is typing a command.
fn analyze_target(app: &App) -> Option<(String, String, String)> {
    type AppTab = Tab;
    // Signals tab: use the highlighted signal's primary market
    if app.active_tab == AppTab::Signals {
        return app.selected_signal().map(|s| (
            s.title.clone(),
            s.id_a.clone(),
            s.platform_a.name().to_lowercase(),
        ));
    }
    // Any other tab: prefer the explicitly loaded market (selected_market_id),
    // fall back to the highlighted row in the market list.
    if let Some(ref id) = app.selected_market_id {
        if let Some(m) = app.markets.iter().find(|m| &m.id == id) {
            return Some((m.title.clone(), m.id.clone(), m.platform.name().to_lowercase()));
        }
    }
    app.selected_market().map(|m| (
        m.title.clone(),
        m.id.clone(),
        m.platform.name().to_lowercase(),
    ))
}

/// Dispatch a slash command typed in the command bar.
///
/// `raw` is the text the user typed after pressing `/` (without the leading slash).
/// Returns `SlashCmd::Handled` if the command was recognised and executed,
/// `SlashCmd::Quit` if the user wants to exit, or `SlashCmd::NotACommand` if the
/// text should be treated as a search/filter term instead.
async fn dispatch_slash_command(
    raw: &str,
    app: &mut App,
    backend: &Option<Arc<dyn LlmBackend>>,
    clients: &Arc<MarketClients>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) -> SlashCmd {
    type AppTab = Tab;
    let cmd = raw.trim().to_lowercase();
    let cmd_word = cmd.split_whitespace().next().unwrap_or("");
    match cmd_word {
        // ── quit ─────────────────────────────────────────────────────────────
        "q" | "quit" => SlashCmd::Quit,

        // ── refresh ──────────────────────────────────────────────────────────
        "r" | "refresh" => {
            let clients_c = clients.clone();
            let tx = event_tx.clone();
            tokio::spawn(async move { agent::refresh_markets(clients_c, tx).await });
            if app.selected_market_id.is_some() {
                trigger_chart_load(app, clients, event_tx).await;
                trigger_orderbook_load(app, clients, event_tx).await;
            }
            if app.active_tab == AppTab::News {
                // Force a re-fetch (clear market_id so the stale-check lets it through)
                app.news_market_id = None;
                trigger_news_load(app, clients, event_tx);
            }
            app.status = "Refreshing…".to_string();
            SlashCmd::Handled
        }

        // ── platform filter ───────────────────────────────────────────────────
        "p" | "platform" | "filter" => {
            app.platform_filter = match app.platform_filter {
                PlatformFilter::All        => PlatformFilter::Polymarket,
                PlatformFilter::Polymarket => PlatformFilter::Kalshi,
                PlatformFilter::Kalshi     => PlatformFilter::All,
            };
            app.market_list.select(Some(0));
            app.status = format!("Filter: {}", app.platform_filter.label());
            SlashCmd::Handled
        }

        // ── chart interval ────────────────────────────────────────────────────
        "c" | "chart" | "interval" => {
            app.chart_interval = app.chart_interval.next();
            app.chart_data.clear();
            app.status = format!("Chart interval: {}", app.chart_interval.label());
            trigger_chart_load(app, clients, event_tx).await;
            SlashCmd::Handled
        }

        // ── sort ──────────────────────────────────────────────────────────────
        "s" | "sort" => {
            app.market_sort = app.market_sort.next();
            app.market_list.select(Some(0));
            app.status = format!("Sort: {}", app.market_sort.label());
            SlashCmd::Handled
        }

        // ── dismiss signal ────────────────────────────────────────────────────
        "x" | "dismiss" => {
            if let Some(sig) = app.selected_signal() {
                let id    = sig.id_a.clone();
                let title = trunc(&sig.title, 40);
                app.dismissed_signals.insert(id);
                if let Some(idx) = app.signal_list.selected() {
                    let new_len = app.signals.len().saturating_sub(1);
                    app.signal_list.select(if new_len == 0 { None } else { Some(idx.min(new_len - 1)) });
                }
                app.signals = signals::compute_signals(
                    &app.markets, &app.prev_prices, &app.dismissed_signals
                );
                app.status = format!("Dismissed: {}", title);
            } else {
                app.status = "No signal selected.".to_string();
            }
            SlashCmd::Handled
        }

        // ── watchlist toggle ──────────────────────────────────────────────────
        "w" | "watchlist" => {
            let market_info = match app.active_tab {
                AppTab::Markets => app.selected_market().map(|m| m.clone()),
                AppTab::Signals => app.selected_signal()
                    .and_then(|s| app.markets.iter().find(|m| m.id == s.id_a))
                    .cloned(),
                _ => app.selected_market().map(|m| m.clone()),
            };
            if let Some(m) = market_info {
                app.toggle_watchlist(&m);
            } else {
                app.status = "Select a market first.".to_string();
            }
            SlashCmd::Handled
        }

        // ── watchlist-only filter ─────────────────────────────────────────────
        "wf" => {
            app.watchlist_only = !app.watchlist_only;
            app.market_list.select(Some(0));
            app.status = if app.watchlist_only {
                format!("Watchlist filter ON  ({} markets)", app.watchlist.len())
            } else {
                "Watchlist filter OFF".to_string()
            };
            SlashCmd::Handled
        }

        // ── alert threshold editor ────────────────────────────────────────────
        "e" | "alert" => {
            let mkt = app.selected_market().map(|m| (m.id.clone(), m.title.clone()));
            if let Some((id, title)) = mkt {
                if app.is_watched(&id) {
                    app.alert_edit_mode = true;
                    app.alert_edit_step = AlertEditStep::default();
                    app.alert_edit_mkt  = id;
                    app.input.clear();
                    app.status = format!(
                        "Alert for '{}': enter ABOVE threshold in ¢ (or Enter for none):",
                        trunc(&title, 30)
                    );
                } else {
                    app.status = "Market not watched — use /watchlist to add first.".to_string();
                }
            } else {
                app.status = "Select a market first.".to_string();
            }
            SlashCmd::Handled
        }

        // ── add position ──────────────────────────────────────────────────────
        "n" | "add" | "position" => {
            if app.active_tab == AppTab::Signals {
                if let Some(sig) = app.selected_signal() {
                    let id    = sig.id_a.clone();
                    let plat  = sig.platform_a.clone();
                    let title = sig.title.clone();
                    let price = sig.price_a;
                    if app.markets.iter().any(|m| m.id == id && m.platform == plat) {
                        let prev_tab = app.active_tab;
                        app.active_tab = AppTab::Markets;
                        app.pos_draft = PosDraft {
                            market_id: id,
                            title,
                            platform: Some(plat),
                            entry_price: None,
                            shares: None,
                            side: None,
                        };
                        app.pos_input_mode = true;
                        app.pos_input_step = PosInputStep::EntryPrice;
                        app.input.clear();
                        app.active_tab = prev_tab;
                        app.status = format!("Add position [{:.1}¢] — Enter entry price (¢):", price * 100.0);
                    } else {
                        app.start_add_position();
                    }
                }
            } else {
                app.start_add_position();
            }
            SlashCmd::Handled
        }

        // ── wallet portfolio import ───────────────────────────────────────────
        "wallet" | "w2" => {
            // Split off the address argument: `/wallet <0x...>`, `/wallet sync`, `/wallet analyze`
            let arg = cmd.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim().to_string();
            if arg.is_empty() {
                // No argument — list registered wallets
                if app.wallet_addresses.is_empty() {
                    app.status = "No wallets registered. Usage: /wallet <0x…address>".to_string();
                } else {
                    app.status = format!(
                        "Registered wallets: {}  (use /wallet analyze to ask AI)",
                        app.wallet_addresses.iter().map(|w| short_wallet(w)).collect::<Vec<_>>().join(", ")
                    );
                }
            } else if arg == "sync" {
                // Re-sync all registered wallets
                let wallets = app.wallet_addresses.clone();
                if wallets.is_empty() {
                    app.status = "No wallets registered. Usage: /wallet <0x…address>".to_string();
                } else {
                    for w in wallets {
                        trigger_wallet_import(app, w, clients, event_tx);
                    }
                }
            } else if arg == "analyze" || arg == "a" {
                // Pre-fill chat with an AI wallet analysis prompt
                wallet_analyze_prompt(app);
            } else {
                // Register + import the given address
                trigger_wallet_import(app, arg, clients, event_tx);
            }
            SlashCmd::Handled
        }

        // ── AI wallet analysis (alias) ────────────────────────────────────────
        "wa" => {
            wallet_analyze_prompt(app);
            SlashCmd::Handled
        }

        // ── set stop / take-profit targets ────────────────────────────────────
        "t" | "targets" => {
            if let Some(idx) = app.portfolio_list.selected() {
                if let Some(pos) = app.portfolio.positions.get(idx) {
                    let mark = pos.mark_price.unwrap_or(pos.entry_price) * 100.0;
                    app.target_pos_id     = pos.id.clone();
                    app.target_input_mode = true;
                    app.target_input_step = TargetInputStep::TakeProfit;
                    app.input.clear();
                    app.status = format!(
                        "Set take-profit for '{}' (mark {:.1}¢): enter ¢ or Enter to skip",
                        trunc(&pos.title, 30), mark
                    );
                } else {
                    app.status = "Select a position first.".to_string();
                }
            } else {
                app.status = "Select a position first.".to_string();
            }
            SlashCmd::Handled
        }

        // ── delete position ───────────────────────────────────────────────────
        "d" | "delete" => {
            app.delete_selected_position();
            SlashCmd::Handled
        }

        // ── CSV export ────────────────────────────────────────────────────────
        "export" | "csv" => {
            app.status = export_current_tab(app);
            SlashCmd::Handled
        }

        // ── Markdown report ───────────────────────────────────────────────────
        "m" | "report" | "md" => {
            app.status = export_markdown_report(app);
            SlashCmd::Handled
        }

        // ── AI analyze ────────────────────────────────────────────────────────
        "a" | "analyze" => {
            let info = analyze_target(app);
            if let Some((title, id, plat)) = info {
                app.input = format!("Analyze the market: '{}' (platform: {}, id: {})", title, plat, id);
            } else {
                app.status = "Select a market first.".to_string();
            }
            SlashCmd::Handled
        }

        // ── help overlay ──────────────────────────────────────────────────────
        "?" | "help" | "h" => {
            app.show_help = !app.show_help;
            SlashCmd::Handled
        }

        // ── Kelly position sizer ──────────────────────────────────────────────
        "k" | "kelly" => {
            if app.kelly_mode {
                app.kelly_step    = KellyStep::MyProb;
                app.kelly_input   = String::new();
                app.kelly_my_prob = None;
            } else {
                app.kelly_mode    = true;
                app.kelly_step    = KellyStep::MyProb;
                app.kelly_input   = String::new();
                app.kelly_my_prob = None;
            }
            SlashCmd::Handled
        }

        // ── LLM pairs re-match ────────────────────────────────────────────────
        "l" | "pairs" => {
            trigger_llm_pairs(app, backend, event_tx).await;
            SlashCmd::Handled
        }

        // ── risk view toggle ──────────────────────────────────────────────────
        "v" | "risk" => {
            app.show_risk_view = !app.show_risk_view;
            app.status = if app.show_risk_view {
                "Risk view — E[P&L], σ, scenario analysis  (v or /risk to toggle back)".to_string()
            } else {
                "Positions view  (v or /risk for risk analysis)".to_string()
            };
            SlashCmd::Handled
        }

        // ── threshold lower ───────────────────────────────────────────────────
        "lower" => {
            match app.active_tab {
                AppTab::SmartMoney => {
                    app.coord_threshold = (app.coord_threshold - 0.05).max(0.05);
                    app.status = format!(
                        "Coord threshold → {:.0}%  (re-load Smart Money to apply)",
                        app.coord_threshold * 100.0,
                    );
                }
                AppTab::Pairs => {
                    app.pairs_jaccard_threshold = (app.pairs_jaccard_threshold - 0.05).max(0.05);
                    app.status = format!(
                        "Pairs Jaccard threshold → {:.0}%  (use /pairs to re-match)",
                        app.pairs_jaccard_threshold * 100.0,
                    );
                }
                _ => {
                    app.status = "Switch to SmartMoney or Pairs tab to use /lower".to_string();
                }
            }
            SlashCmd::Handled
        }

        // ── threshold raise ───────────────────────────────────────────────────
        "raise" => {
            match app.active_tab {
                AppTab::SmartMoney => {
                    app.coord_threshold = (app.coord_threshold + 0.05).min(0.95);
                    app.status = format!(
                        "Coord threshold → {:.0}%  (re-load Smart Money to apply)",
                        app.coord_threshold * 100.0,
                    );
                }
                AppTab::Pairs => {
                    app.pairs_jaccard_threshold = (app.pairs_jaccard_threshold + 0.05).min(0.95);
                    app.status = format!(
                        "Pairs Jaccard threshold → {:.0}%  (use /pairs to re-match)",
                        app.pairs_jaccard_threshold * 100.0,
                    );
                }
                _ => {
                    app.status = "Switch to SmartMoney or Pairs tab to use /raise".to_string();
                }
            }
            SlashCmd::Handled
        }

        _ => SlashCmd::NotACommand,
    }
}

/// Returns `true` if the user requested to quit.
async fn handle_key(
    app:         &mut App,
    key:         crossterm::event::KeyEvent,
    backend:     &Option<Arc<dyn LlmBackend>>,
    clients:     &Arc<MarketClients>,
    event_tx:    &mpsc::UnboundedSender<AppEvent>,
    llm_history: &mut Vec<LlmMessage>,
) -> bool {
    use crossterm::event::KeyCode as KC;
    type AppTab = Tab;

    // Ctrl+C always quits (or cancels any active input mode)
    if key.modifiers == KeyModifiers::CONTROL && key.code == KC::Char('c') {
        let any_mode = !app.input.is_empty()
            || app.search_mode
            || app.pos_input_mode
            || app.alert_edit_mode
            || app.target_input_mode
            || app.kelly_mode;
        if any_mode {
            app.input.clear();
            app.search.clear();
            app.command_input.clear();
            app.sent_cursor = None;
            app.search_mode       = false;
            app.pos_input_mode    = false;
            app.alert_edit_mode   = false;
            app.target_input_mode = false;
            app.kelly_mode        = false;
            app.pos_input_step    = PosInputStep::EntryPrice;
            app.alert_edit_step   = AlertEditStep::default();
            app.pos_draft         = PosDraft::default();
            app.status = "Cancelled.".to_string();
            return false;
        }
        return true;
    }

    // ── Alert threshold edit flow ─────────────────────────────────────────────
    if app.alert_edit_mode {
        match key.code {
            KC::Esc => {
                app.alert_edit_mode = false;
                app.input.clear();
                app.status = "Alert edit cancelled.".to_string();
            }
            KC::Enter => {
                let val_str = app.input.trim().to_string();
                app.input.clear();
                match app.alert_edit_step {
                    AlertEditStep::Above => {
                        // Store above threshold (or 1.0 = no alert)
                        let threshold = if val_str.is_empty() {
                            1.0
                        } else {
                            val_str.parse::<f64>().unwrap_or(1.0) / 100.0
                        };
                        if let Some(entry) = app.watchlist.iter_mut().find(|w| w.market_id == app.alert_edit_mkt) {
                            entry.alert_above = threshold.clamp(0.0, 1.0);
                        }
                        app.alert_edit_step = AlertEditStep::Below;
                        app.status = format!("Alert above set to {:.0}¢. Enter BELOW threshold (¢, or Enter for none):", threshold * 100.0);
                    }
                    AlertEditStep::Below => {
                        let threshold = if val_str.is_empty() {
                            0.0
                        } else {
                            val_str.parse::<f64>().unwrap_or(0.0) / 100.0
                        };
                        if let Some(entry) = app.watchlist.iter_mut().find(|w| w.market_id == app.alert_edit_mkt) {
                            entry.alert_below = threshold.clamp(0.0, 1.0);
                        }
                        let _ = portfolio::save_watchlist(&app.watchlist);
                        app.alert_edit_mode = false;
                        app.alert_edit_step = AlertEditStep::default();
                        app.status = format!("Alert thresholds saved for market.");
                    }
                }
            }
            KC::Backspace => { app.input.pop(); }
            KC::Char(c)   => { app.input.push(c); }
            _ => {}
        }
        return false;
    }

    // ── Position input flow ───────────────────────────────────────────────────
    if app.pos_input_mode {
        match key.code {
            KC::Esc => {
                app.pos_input_mode = false;
                app.input.clear();
                app.pos_draft = PosDraft::default();
                app.status = "Cancelled.".to_string();
            }
            KC::Enter => {
                app.advance_pos_input();
            }
            KC::Backspace => { app.input.pop(); }
            KC::Char(c)   => { app.input.push(c); }
            _ => {}
        }
        return false;
    }

    // ── Stop / take-profit target input flow ──────────────────────────────────
    if app.target_input_mode {
        match key.code {
            KC::Esc => {
                app.target_input_mode = false;
                app.input.clear();
                app.status = "Cancelled.".to_string();
            }
            KC::Enter => {
                let val_str = app.input.trim().to_string();
                app.input.clear();
                match app.target_input_step {
                    TargetInputStep::TakeProfit => {
                        let tp = if val_str.is_empty() { None }
                            else { val_str.parse::<f64>().ok().map(|v| v / 100.0) };
                        if let Some(pos) = app.portfolio.positions.iter_mut()
                                .find(|p| p.id == app.target_pos_id) {
                            pos.take_profit = tp;
                        }
                        app.target_input_step = TargetInputStep::StopLoss;
                        app.status = "Take-profit set. Enter stop-loss price (¢, or Enter to skip):".to_string();
                    }
                    TargetInputStep::StopLoss => {
                        let sl = if val_str.is_empty() { None }
                            else { val_str.parse::<f64>().ok().map(|v| v / 100.0) };
                        if let Some(pos) = app.portfolio.positions.iter_mut()
                                .find(|p| p.id == app.target_pos_id) {
                            pos.stop_loss = sl;
                        }
                        let _ = portfolio::save_portfolio(&app.portfolio);
                        app.target_input_mode = false;
                        app.target_input_step = TargetInputStep::default();
                        app.status = "Stop-loss set. Targets saved.".to_string();
                    }
                }
            }
            KC::Backspace => { app.input.pop(); }
            KC::Char(c)   => { app.input.push(c); }
            _ => {}
        }
        return false;
    }

    // ── Kelly position sizer input flow ──────────────────────────────────────
    if app.kelly_mode {
        match key.code {
            KC::Esc => {
                app.kelly_mode  = false;
                app.kelly_input = String::new();
            }
            KC::Char(c) if c.is_ascii_digit() || c == '.' => {
                app.kelly_input.push(c);
            }
            KC::Backspace => {
                app.kelly_input.pop();
            }
            KC::Enter => {
                match app.kelly_step {
                    KellyStep::MyProb => {
                        if let Ok(v) = app.kelly_input.trim().parse::<f64>() {
                            let prob = (v / 100.0).clamp(0.001, 0.999);
                            app.kelly_my_prob = Some(prob);
                            app.kelly_step    = KellyStep::Bankroll;
                            app.kelly_input   = format!("{:.0}", app.kelly_bankroll);
                        }
                    }
                    KellyStep::Bankroll => {
                        if let Ok(v) = app.kelly_input.trim().parse::<f64>() {
                            if v > 0.0 {
                                app.kelly_bankroll = v;
                                app.kelly_step     = KellyStep::Result;
                                app.kelly_input    = String::new();
                            }
                        }
                    }
                    KellyStep::Result => {
                        // Reset to re-enter a new probability
                        app.kelly_step    = KellyStep::MyProb;
                        app.kelly_input   = String::new();
                        app.kelly_my_prob = None;
                    }
                }
            }
            _ => {}
        }
        return false;
    }

    // ── Command / search mode (activated by /) ────────────────────────────────
    if app.search_mode {
        match key.code {
            KC::Esc => {
                app.search_mode = false;
                app.command_input.clear();
                app.status = "Cancelled.".to_string();
            }
            KC::Enter => {
                let typed = app.command_input.clone();
                app.search_mode = false;
                app.command_input.clear();
                if typed.is_empty() {
                    app.status = "Cancelled.".to_string();
                    return false;
                }
                match dispatch_slash_command(&typed, app, backend, clients, event_tx).await {
                    SlashCmd::Quit    => return true,
                    SlashCmd::Handled => {}
                    SlashCmd::NotACommand => {
                        // Treat as a search/filter term
                        app.search = typed.clone();
                        app.active_tab = AppTab::Markets;
                        app.market_list.select(Some(0));
                        app.status = format!("Filtering: '{}'", typed);
                    }
                }
            }
            KC::Backspace => { app.command_input.pop(); }
            KC::Char(c) => { app.command_input.push(c); }
            _ => {}
        }
        return false;
    }

    // ── Normal mode ───────────────────────────────────────────────────────────
    match key.code {
        // ── Tab switching ─────────────────────────────────────────────────────
        KC::Char('1') if app.input.is_empty() => { app.active_tab = AppTab::Signals; }
        KC::Char('2') if app.input.is_empty() => { app.active_tab = AppTab::Markets; }
        KC::Char('3') if app.input.is_empty() => {
            app.active_tab = AppTab::Chart;
            trigger_chart_load(app, clients, event_tx).await;
        }
        KC::Char('4') if app.input.is_empty() => {
            app.active_tab = AppTab::Orderbook;
            trigger_orderbook_load(app, clients, event_tx).await;
        }
        KC::Char('5') if app.input.is_empty() => { app.active_tab = AppTab::Portfolio; }
        KC::Char('6') if app.input.is_empty() => { app.active_tab = AppTab::Chat; }
        KC::Char('7') if app.input.is_empty() => {
            app.active_tab = AppTab::SmartMoney;
            trigger_smart_money_load(app, clients, event_tx).await;
        }
        KC::Char('8') if app.input.is_empty() => {
            app.active_tab = AppTab::Trades;
            trigger_trades_load(app, clients, event_tx).await;
        }
        KC::Char('9') if app.input.is_empty() => {
            app.active_tab = AppTab::Pairs;
            trigger_llm_pairs(app, backend, event_tx).await;
        }
        KC::Char('0') if app.input.is_empty() => {
            app.active_tab = AppTab::News;
            // Auto-fetch if a market is selected and news isn't already loaded for it
            if app.news_market_id.as_ref() != app.selected_market_id.as_ref() {
                trigger_news_load(app, clients, event_tx);
            }
        }

        KC::Tab => {
            app.active_tab = app.active_tab.next();
            match app.active_tab {
                AppTab::Chart     => { trigger_chart_load(app, clients, event_tx).await; }
                AppTab::Orderbook => { trigger_orderbook_load(app, clients, event_tx).await; }
                AppTab::SmartMoney => { trigger_smart_money_load(app, clients, event_tx).await; }
                AppTab::Trades    => { trigger_trades_load(app, clients, event_tx).await; }
                AppTab::Pairs     => { trigger_llm_pairs(app, backend, event_tx).await; }
                AppTab::News      => { trigger_news_load(app, clients, event_tx); }
                _ => {}
            }
        }
        KC::BackTab => {
            app.active_tab = app.active_tab.prev();
            match app.active_tab {
                AppTab::Chart     => { trigger_chart_load(app, clients, event_tx).await; }
                AppTab::Orderbook => { trigger_orderbook_load(app, clients, event_tx).await; }
                AppTab::SmartMoney => { trigger_smart_money_load(app, clients, event_tx).await; }
                AppTab::Trades    => { trigger_trades_load(app, clients, event_tx).await; }
                AppTab::Pairs     => { trigger_llm_pairs(app, backend, event_tx).await; }
                AppTab::News      => { trigger_news_load(app, clients, event_tx); }
                _ => {}
            }
        }

        // ── Navigation ────────────────────────────────────────────────────────
        KC::Char('j') | KC::Down if app.input.is_empty() => { app.list_down(); }
        KC::Char('k') | KC::Up   if app.input.is_empty() => { app.list_up(); }

        // ── Enter ─────────────────────────────────────────────────────────────
        KC::Enter => {
            if app.active_tab == AppTab::Chat {
                if !app.input.is_empty() {
                    send_chat(app, backend, clients, event_tx, llm_history).await;
                }
            } else if !app.input.is_empty() {
                send_chat(app, backend, clients, event_tx, llm_history).await;
            } else if app.active_tab == AppTab::SmartMoney {
                // Drill into the selected wallet
                if let Some(idx) = app.sm_list.selected() {
                    let wallet_idx = idx.saturating_sub(2); // skip 2 header rows
                    if let Some(w) = app.sm_wallets.get(wallet_idx) {
                        let wallet_addr = w.wallet.clone();
                        let clients_c = clients.clone();
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            agent::refresh_wallet_detail(clients_c, wallet_addr, tx).await;
                        });
                    }
                }
            } else {
                // Load chart + orderbook from selected market (Markets or Signals)
                let market_id = match app.active_tab {
                    AppTab::Markets => {
                        app.selected_market().map(|m| m.id.clone())
                    }
                    AppTab::Signals => {
                        app.selected_signal().map(|s| s.id_a.clone())
                    }
                    AppTab::Portfolio => {
                        app.portfolio_list.selected()
                            .and_then(|i| app.portfolio.positions.get(i))
                            .map(|p| p.market_id.clone())
                    }
                    _ => None,
                };
                if let Some(id) = market_id {
                    app.selected_market_id = Some(id.clone());
                    app.chart_data.clear();
                    app.chart_candles.clear();
                    app.orderbook = None;
                    app.sm_wallets.clear();
                    app.sm_coord_pairs.clear();
                    app.trades_data.clear();
                    app.status = format!("Loading data for {}", id);
                    trigger_chart_load(app, clients, event_tx).await;
                    trigger_orderbook_load(app, clients, event_tx).await;
                    trigger_smart_money_load(app, clients, event_tx).await;
                    trigger_trades_load(app, clients, event_tx).await;
                    app.active_tab = AppTab::Chart;
                }
            }
        }

        // ── Command bar ───────────────────────────────────────────────────────────
        KC::Char('/') if app.input.is_empty() => {
            app.search_mode = true;
            app.command_input.clear();
        }
        // Close wallet detail panel (SmartMoney tab)
        KC::Esc if app.input.is_empty()
            && app.active_tab == AppTab::SmartMoney
            && (app.sm_detail.is_some() || app.sm_detail_loading) =>
        {
            app.sm_detail         = None;
            app.sm_detail_loading = false;
            app.sm_detail_scroll  = 0;
            app.status = "Back to wallet list.".to_string();
        }

        KC::Esc if app.input.is_empty() && !app.search.is_empty() => {
            app.search.clear();
            app.status = "Search cleared".to_string();
        }

        KC::Esc if app.show_help => {
            app.show_help = false;
        }

        // ── Help overlay ──────────────────────────────────────────────────────
        KC::Char('?') if app.input.is_empty() => {
            app.show_help = !app.show_help;
        }

        // ── Risk view toggle (Portfolio tab) ─────────────────────────────────
        KC::Char('v') if app.input.is_empty() && app.active_tab == AppTab::Portfolio => {
            app.show_risk_view = !app.show_risk_view;
            app.status = if app.show_risk_view {
                "Risk view — E[P&L], σ, scenario analysis  (v or /risk to toggle back)".to_string()
            } else {
                "Positions view  (v or /risk for risk analysis)".to_string()
            };
        }

        // ── Refresh shortcut ──────────────────────────────────────────────────
        KC::Char('^') if app.input.is_empty() => {
            let clients_c = clients.clone();
            let tx = event_tx.clone();
            tokio::spawn(async move { agent::refresh_markets(clients_c, tx).await });
            if app.selected_market_id.is_some() {
                trigger_chart_load(app, clients, event_tx).await;
                trigger_orderbook_load(app, clients, event_tx).await;
            }
            app.status = "Refreshing…".to_string();
        }

        // ── AI analyze shortcut ───────────────────────────────────────────────
        KC::Char('@') if app.input.is_empty() => {
            if let Some((title, id, plat)) = analyze_target(app) {
                app.input = format!("Analyze the market: '{}' (platform: {}, id: {})", title, plat, id);
            } else {
                app.status = "Select a market first.".to_string();
            }
        }

        // ── Threshold adjustments ─────────────────────────────────────────────
        KC::Char('[') if app.input.is_empty()
            && matches!(app.active_tab, AppTab::SmartMoney | AppTab::Pairs) =>
        {
            match app.active_tab {
                AppTab::SmartMoney => {
                    app.coord_threshold = (app.coord_threshold - 0.05).max(0.05);
                    app.status = format!(
                        "Coord threshold → {:.0}%  (re-load Smart Money to apply)",
                        app.coord_threshold * 100.0,
                    );
                }
                AppTab::Pairs => {
                    app.pairs_jaccard_threshold = (app.pairs_jaccard_threshold - 0.05).max(0.05);
                    app.status = format!(
                        "Pairs Jaccard threshold → {:.0}%  (use /pairs to re-match)",
                        app.pairs_jaccard_threshold * 100.0,
                    );
                }
                _ => {}
            }
        }
        KC::Char(']') if app.input.is_empty()
            && matches!(app.active_tab, AppTab::SmartMoney | AppTab::Pairs) =>
        {
            match app.active_tab {
                AppTab::SmartMoney => {
                    app.coord_threshold = (app.coord_threshold + 0.05).min(0.95);
                    app.status = format!(
                        "Coord threshold → {:.0}%  (re-load Smart Money to apply)",
                        app.coord_threshold * 100.0,
                    );
                }
                AppTab::Pairs => {
                    app.pairs_jaccard_threshold = (app.pairs_jaccard_threshold + 0.05).min(0.95);
                    app.status = format!(
                        "Pairs Jaccard threshold → {:.0}%  (use /pairs to re-match)",
                        app.pairs_jaccard_threshold * 100.0,
                    );
                }
                _ => {}
            }
        }

        // ── Input editing ─────────────────────────────────────────────────────
        KC::Char(c) => { app.input.push(c); app.sent_cursor = None; }
        KC::Backspace => { app.input.pop(); }

        KC::Up   if !app.input.is_empty() || app.sent_cursor.is_some() => { app.history_up(); }
        KC::Down if app.sent_cursor.is_some() => { app.history_down(); }

        _ => {}
    }

    false
}

/// Build a context block describing current TUI state.
///
/// This is prepended (invisibly to the user) to every LLM message so the model
/// always knows which market is on screen and can answer follow-ups like
/// "further analyze the above" without asking for a market ID.
fn build_context_prefix(app: &App) -> String {
    let mut sections: Vec<String> = Vec::new();

    // ── Selected market details ───────────────────────────────────────────────
    if let Some(ref sel_id) = app.selected_market_id {
        if let Some(m) = app.markets.iter().find(|m| &m.id == sel_id) {
            let plat = match m.platform {
                Platform::Polymarket => "Polymarket",
                Platform::Kalshi    => "Kalshi",
            };
            let yes_pct = m.yes_price * 100.0;
            let no_pct  = (1.0 - m.yes_price) * 100.0;

            let fmt_money = |v: f64| -> String {
                if v >= 1_000_000.0 { format!("${:.2}M", v / 1_000_000.0) }
                else if v >= 1_000.0 { format!("${:.1}K", v / 1_000.0) }
                else { format!("${:.0}", v) }
            };
            let vol_str = m.volume.map(|v| fmt_money(v)).unwrap_or_else(|| "n/a".to_string());
            let liq_str = m.liquidity.map(|l| fmt_money(l)).unwrap_or_else(|| "n/a".to_string());
            let vol_liq_ratio = match (m.volume, m.liquidity) {
                (Some(v), Some(l)) if l > 0.0 => format!("{:.1}×", v / l),
                _ => "n/a".to_string(),
            };

            let mut mkt_lines = vec![
                format!("Title   : {}", m.title),
                format!("Platform: {}  |  Market ID: {}", plat, m.id),
                format!("Price   : YES {yes:.1}%  /  NO {no:.1}%  (implied odds YES {yes_o:.2}:1)",
                    yes = yes_pct, no = no_pct, yes_o = no_pct / yes_pct.max(0.01)),
                format!("Volume  : {}  |  Liquidity: {}  |  Vol/Liq: {}", vol_str, liq_str, vol_liq_ratio),
            ];
            if let Some(ref tok) = m.token_id {
                mkt_lines.push(format!("Token ID (CLOB): {}", tok));
            }
            if let Some(ref end) = m.end_date {
                let end_str = &end[..end.len().min(10)];
                // Days remaining
                if let Ok(end_date) = chrono::NaiveDate::parse_from_str(end_str, "%Y-%m-%d") {
                    let today = chrono::Local::now().date_naive();
                    let days = (end_date - today).num_days();
                    mkt_lines.push(format!("Resolves: {}  ({} days remaining)", end_str, days));
                } else {
                    mkt_lines.push(format!("Resolves: {}", end_str));
                }
            }
            if let Some(ref cat) = m.category {
                mkt_lines.push(format!("Category: {}", cat));
            }
            sections.push(format!("SELECTED MARKET\n{}", mkt_lines.join("\n")));
        }
    }

    // ── Price history (candles) ───────────────────────────────────────────────
    if !app.chart_candles.is_empty() {
        let prices: Vec<f64> = app.chart_candles.iter().map(|c| c.close).collect();
        let n = prices.len();
        let current = prices[n - 1];
        let oldest  = prices[0];

        // Compute simple moving averages
        let ma7 = if n >= 7  { prices[n-7..].iter().sum::<f64>() / 7.0  } else { current };
        let ma20 = if n >= 20 { prices[n-20..].iter().sum::<f64>() / 20.0 } else { current };
        let pct_change = (current - oldest) / oldest.max(0.001) * 100.0;

        // Momentum: last 5 candles vs prior 5 candles
        let recent_avg = if n >= 5  { prices[n-5..].iter().sum::<f64>() / 5.0 } else { current };
        let prior_avg  = if n >= 10 { prices[n-10..n-5].iter().sum::<f64>() / 5.0 } else { oldest };
        let momentum_pp = (recent_avg - prior_avg) * 100.0;

        // High/low range
        let lo = prices.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

        // Volume if available
        let vol_info = {
            let vols: Vec<f64> = app.chart_candles.iter().filter_map(|c| c.volume).collect();
            if vols.len() >= 2 {
                let recent_vol = vols[vols.len()-1];
                let avg_vol = vols.iter().sum::<f64>() / vols.len() as f64;
                format!("  |  Last candle vol: {:.0}  (avg: {:.0}, ratio: {:.1}×)",
                    recent_vol, avg_vol, recent_vol / avg_vol.max(1.0))
            } else {
                String::new()
            }
        };

        let trend_label = if momentum_pp > 2.0 { "↑ UPTREND"
                          } else if momentum_pp < -2.0 { "↓ DOWNTREND"
                          } else { "→ RANGE-BOUND" };

        sections.push(format!(
            "PRICE HISTORY ({n} candles, {interval})\n\
             Current: {cur:.1}%  |  Period Δ: {chg:+.1}%  |  Range: {lo:.1}%–{hi:.1}%\n\
             MA7: {ma7:.1}%  |  MA20: {ma20:.1}%  |  Momentum (5c): {mom:+.1}pp  →  {trend}{vol}",
            n       = n,
            interval = format!("{:?}", app.chart_interval),
            cur     = current * 100.0,
            chg     = pct_change,
            lo      = lo * 100.0,
            hi      = hi * 100.0,
            ma7     = ma7 * 100.0,
            ma20    = ma20 * 100.0,
            mom     = momentum_pp,
            trend   = trend_label,
            vol     = vol_info,
        ));
    }

    // ── Live orderbook (full depth) ───────────────────────────────────────────
    if let Some(ref ob) = app.orderbook {
        if !ob.bids.is_empty() || !ob.asks.is_empty() {
            let total_bid_sz: f64 = ob.bids.iter().map(|b| b.size).sum();
            let total_ask_sz: f64 = ob.asks.iter().map(|a| a.size).sum();
            let total_sz = total_bid_sz + total_ask_sz;
            let imbalance = if total_sz > 0.0 {
                (total_bid_sz - total_ask_sz) / total_sz
            } else { 0.0 };
            let imbalance_label = if imbalance > 0.15 { "BUY PRESSURE" }
                                  else if imbalance < -0.15 { "SELL PRESSURE" }
                                  else { "BALANCED" };

            let spread_pp = match (ob.asks.first(), ob.bids.first()) {
                (Some(ask), Some(bid)) => (ask.price - bid.price) * 100.0,
                _ => 0.0,
            };
            let spread_bps = spread_pp * 100.0;

            let mut ob_lines = vec![
                format!("Best bid: {:.1}%  |  Best ask: {:.1}%  |  Spread: {:.1}pp ({:.0}bps)",
                    ob.bids.first().map(|b| b.price * 100.0).unwrap_or(0.0),
                    ob.asks.first().map(|a| a.price * 100.0).unwrap_or(0.0),
                    spread_pp, spread_bps),
                format!("Total bid size: {:.0}  |  Total ask size: {:.0}  |  Imbalance: {:+.1}%  →  {}",
                    total_bid_sz, total_ask_sz, imbalance * 100.0, imbalance_label),
            ];
            // Top 3 bid/ask levels
            let bid_levels: String = ob.bids.iter().take(3)
                .map(|b| format!("  {:.1}%×{:.0}", b.price*100.0, b.size))
                .collect::<Vec<_>>().join("  |");
            let ask_levels: String = ob.asks.iter().take(3)
                .map(|a| format!("  {:.1}%×{:.0}", a.price*100.0, a.size))
                .collect::<Vec<_>>().join("  |");
            ob_lines.push(format!("Top bids:{}", bid_levels));
            ob_lines.push(format!("Top asks:{}", ask_levels));

            sections.push(format!("LIVE ORDERBOOK\n{}", ob_lines.join("\n")));
        }
    }

    // ── Active signals for this market ────────────────────────────────────────
    if !app.signals.is_empty() {
        let sel_id = app.selected_market_id.as_deref().unwrap_or("");
        let relevant: Vec<String> = app.signals.iter()
            .filter(|s| s.id_a == sel_id || s.id_b.as_deref() == Some(sel_id))
            .map(|s| format!("  [{}] {} — {}", s.kind.label(), s.title, s.action))
            .collect();
        if !relevant.is_empty() {
            sections.push(format!("ACTIVE SIGNALS\n{}", relevant.join("\n")));
        }
    }

    // ── Portfolio position in this market ─────────────────────────────────────
    if let Some(ref sel_id) = app.selected_market_id {
        if let Some(pos) = app.portfolio.positions.iter().find(|p| &p.market_id == sel_id) {
            let current_price = app.markets.iter()
                .find(|m| &m.id == sel_id)
                .map(|m| m.yes_price)
                .unwrap_or(pos.entry_price);
            let pnl = (current_price - pos.entry_price) * pos.shares;
            let mut pos_lines = vec![
                format!("Side: {:?}  |  Shares: {:.0}  |  Entry: {:.1}%  |  Mark: {:.1}%  |  PnL: {:+.2}",
                    pos.side, pos.shares, pos.entry_price * 100.0, current_price * 100.0, pnl),
            ];
            if let Some(tp) = pos.take_profit {
                pos_lines.push(format!("Take-profit: {:.1}%  |  Stop-loss: {}",
                    tp * 100.0,
                    pos.stop_loss.map(|s| format!("{:.1}%", s*100.0)).unwrap_or_else(|| "none".to_string())));
            }
            sections.push(format!("YOUR POSITION\n{}", pos_lines.join("\n")));
        }
    }

    // ── Research notes ────────────────────────────────────────────────────────
    if !app.session.notes.is_empty() {
        let notes_str = app.session.notes.iter()
            .map(|n| format!("  {}", n))
            .collect::<Vec<_>>().join("\n");
        sections.push(format!("RESEARCH NOTES\n{}", notes_str));
    }

    format!(
        "╔═══════════════════════════════════════════════════════╗\n\
         ║  DASHBOARD CONTEXT (live data visible on screen)      ║\n\
         ╚═══════════════════════════════════════════════════════╝\n\
         Use this data directly. Do NOT ask the user to repeat IDs, prices, or figures \
         already shown below. Build your analysis on top of this context.\n\n\
         {}\n\
         ═══════════════════════════════════════════════════════",
        sections.join("\n\n")
    )
}

async fn send_chat(
    app:         &mut App,
    backend:     &Option<Arc<dyn LlmBackend>>,
    clients:     &Arc<MarketClients>,
    event_tx:    &mpsc::UnboundedSender<AppEvent>,
    llm_history: &mut Vec<LlmMessage>,
) {
    let msg = app.input.trim().to_string();
    if msg.is_empty() { return; }

    app.sent_history.push(msg.clone());
    app.input.clear();
    app.sent_cursor = None;
    app.active_tab = Tab::Chat;

    // ── !note shortcut — append to research log without sending to LLM ────────
    if let Some(note_text) = msg.strip_prefix("!note").map(|s| s.trim()) {
        if !note_text.is_empty() {
            let ts  = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
            let note = format!("[{}] {}", ts, note_text);
            app.session.notes.push(note.clone());
            app.chat_msgs.push(ChatMsg::User(msg.clone()));
            app.chat_msgs.push(ChatMsg::Assistant(format!("📝 Note saved: {}", note)));
            app.status = "Note added to research log.".to_string();
            // Persist note to log file immediately
            let log_path = {
                let mut p = dirs_next::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
                p.push(".whoissharp");
                p.push("notes.md");
                p
            };
            if let Some(parent) = log_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let line = format!("- {}\n", note);
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
                let _ = f.write_all(line.as_bytes());
            }
        }
        return;
    }

    let Some(backend_arc) = backend else {
        app.chat_msgs.push(ChatMsg::User(msg));
        app.chat_msgs.push(ChatMsg::Error(
            "No AI backend configured. Run with --backend anthropic (or gemini/openai/ollama) \
             and the appropriate API key.".to_string(),
        ));
        app.status = "No AI backend configured.".to_string();
        return;
    };

    app.chat_msgs.push(ChatMsg::User(msg.clone()));
    app.session.messages.push(portfolio::SessionMessage { role: "user".into(), content: msg.clone() });
    app.chat_scroll = 0; // pin to bottom on new message
    app.is_loading = true;
    app.status = "Sending…".to_string();

    // Inject TUI context as a hidden preamble so the LLM knows which market is
    // on screen without the user having to repeat market IDs.
    let ctx    = build_context_prefix(app);
    let llm_msg = format!("{}\n\nUser: {}", ctx, msg);

    let backend_c  = backend_arc.clone();
    let clients_c  = clients.clone();
    let tx         = event_tx.clone();
    let mut hist   = std::mem::take(llm_history);

    tokio::spawn(async move {
        agent::run_turn(backend_c, clients_c, &mut hist, llm_msg, tx.clone()).await;
        // Return history to the TUI via the event channel so it persists across turns.
        let _ = tx.send(AppEvent::HistoryUpdated(hist));
    });
    // llm_history stays empty until HistoryUpdated arrives; a second message
    // sent before the first turn completes starts a fresh context (rare edge case).
}

/// Spawn a background task that fetches open positions for `wallet` from the
/// Polymarket data-api and upserts them into `app.portfolio`.
///
/// On re-sync the existing positions tagged with this wallet are removed first
/// so the portfolio always reflects the current on-chain state.
fn trigger_wallet_import(
    app:      &mut App,
    wallet:   String,
    clients:  &Arc<MarketClients>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    use crate::markets::Platform;
    use crate::portfolio::{Position, Side};

    // Register the wallet if not already known.
    if !app.wallet_addresses.contains(&wallet) {
        app.wallet_addresses.push(wallet.clone());
        portfolio::save_wallets(&app.wallet_addresses).ok();
    }

    // Remove stale positions previously imported from this wallet so we can
    // replace them with fresh data.
    let tag = format!("wallet:{}", wallet);
    app.portfolio.positions.retain(|p| {
        p.note.as_deref().map(|n| !n.starts_with(&tag)).unwrap_or(true)
    });

    let tx        = event_tx.clone();
    let wallet_c  = wallet.clone();
    let clients_c = clients.clone();

    let _ = tx.send(AppEvent::WalletImportStarted { wallet: wallet.clone() });

    tokio::spawn(async move {
        let positions_raw = match clients_c.polymarket.fetch_wallet_positions(&wallet_c).await {
            Ok(p)  => p,
            Err(e) => {
                let _ = tx.send(AppEvent::WalletImportError {
                    wallet: wallet_c,
                    error:  e.to_string(),
                });
                return;
            }
        };

        let tag = format!("wallet:{}", wallet_c);
        let mut imported = 0usize;
        let mut skipped  = 0usize;
        let mut positions: Vec<Position> = Vec::new();

        for wp in &positions_raw {
            // outcome_index 0 = YES, anything else = NO
            let side = if wp.outcome_index == 0 { Side::Yes } else { Side::No };
            if wp.size < 0.01 {
                skipped += 1;
                continue;
            }
            let mut pos = Position::new(
                Platform::Polymarket,
                wp.condition_id.clone(),
                wp.title.clone(),
                wp.avg_price,
                wp.size,
                side,
                Some(tag.clone()),
            );
            // Set mark price from current_value / size if meaningful.
            if wp.size > 0.0 {
                pos.mark_price = Some((wp.current_value / wp.size).clamp(0.0, 1.0));
            }
            positions.push(pos);
            imported += 1;
        }

        let _ = tx.send(AppEvent::WalletImportDone {
            wallet:   wallet_c,
            imported,
            skipped,
        });

        // Send the new positions via a dedicated event so the TUI can add them.
        let _ = tx.send(AppEvent::WalletPositionsReady(positions));
    });
}

/// Spawn a background task that fetches news for the currently selected market.
/// Does nothing if no market is selected or the news client is unavailable.
fn trigger_news_load(
    app:      &mut App,
    clients:  &Arc<MarketClients>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let Some(market_id) = app.selected_market_id.clone() else {
        app.status = "Select a market first (press Enter on a market row).".to_string();
        return;
    };
    let Some(market) = app.markets.iter().find(|m| m.id == market_id).cloned() else {
        return;
    };
    if app.news_loading {
        return; // already in flight
    }

    app.news_loading = true;
    app.news_error = None;
    app.news_articles.clear();
    app.status = format!("Loading news for '{}'…", trunc(&market.title, 40));

    let tx        = event_tx.clone();
    let clients_c = clients.clone();
    let mid       = market_id.clone();

    tokio::spawn(async move {
        let Some(news) = &clients_c.news else {
            let _ = tx.send(AppEvent::NewsError(
                "Set NEWSDATA_API_KEY to enable the news feed.".to_string()
            ));
            return;
        };
        match news.fetch_for_market(&market.title, 10).await {
            Ok(articles) => {
                let _ = tx.send(AppEvent::NewsLoaded {
                    market_id: Some(mid),
                    articles,
                });
            }
            Err(e) => {
                let _ = tx.send(AppEvent::NewsError(format!("{:#}", e)));
            }
        }
    });
}

async fn trigger_chart_load(
    app:      &App,
    clients:  &Arc<MarketClients>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let Some(id) = &app.selected_market_id else { return };
    let Some(market) = app.markets.iter().find(|m| &m.id == id).cloned() else { return };

    let clients_c = clients.clone();
    let tx        = event_tx.clone();
    let interval  = app.chart_interval;

    tokio::spawn(async move {
        agent::refresh_price_history(clients_c, market, interval, tx).await;
    });
}

async fn trigger_orderbook_load(
    app:      &App,
    clients:  &Arc<MarketClients>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let Some(id) = &app.selected_market_id else { return };
    let Some(market) = app.markets.iter().find(|m| &m.id == id).cloned() else { return };

    let clients_c = clients.clone();
    let tx        = event_tx.clone();

    tokio::spawn(async move {
        agent::refresh_orderbook(clients_c, market, tx).await;
    });
}

async fn trigger_smart_money_load(
    app:      &App,
    clients:  &Arc<MarketClients>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    use crate::markets::Platform;

    let Some(id) = &app.selected_market_id else { return };
    // Smart Money analysis is Polymarket-only
    let Some(market) = app.markets.iter().find(|m| &m.id == id) else { return };
    if market.platform != Platform::Polymarket { return; }

    let clients_c       = clients.clone();
    let tx              = event_tx.clone();
    let market_id       = id.clone();
    let market_volume   = market.volume;
    let coord_threshold = app.coord_threshold;

    tokio::spawn(async move {
        agent::refresh_smart_money(clients_c, market_id, market_volume, coord_threshold, tx).await;
    });
}

async fn trigger_trades_load(
    app:      &App,
    clients:  &Arc<MarketClients>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    use crate::markets::Platform;

    let Some(id) = &app.selected_market_id else { return };
    // Trades tape is Polymarket-only
    let Some(market) = app.markets.iter().find(|m| &m.id == id) else { return };
    if market.platform != Platform::Polymarket { return; }

    let clients_c = clients.clone();
    let tx        = event_tx.clone();
    let market_id = id.clone();

    tokio::spawn(async move {
        agent::refresh_market_trades(clients_c, market_id, tx).await;
    });
}

/// Trigger LLM-enhanced pair matching (if backend available) or Jaccard fallback.
/// Sends `PairsMatching` immediately, then `PairsLoaded` when done.
async fn trigger_llm_pairs(
    app:      &App,
    backend:  &Option<Arc<dyn LlmBackend>>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    if app.markets.is_empty() { return; }
    let _ = event_tx.send(AppEvent::PairsMatching);

    let markets_snap = app.markets.clone();
    let tx = event_tx.clone();
    let jaccard_threshold = app.pairs_jaccard_threshold;

    if let Some(b) = backend.clone() {
        tokio::spawn(async move {
            let pairs = crate::pairs::llm_match_pairs(&markets_snap, &b, Some(jaccard_threshold)).await;
            let _ = tx.send(AppEvent::PairsLoaded(pairs));
        });
    } else {
        // No LLM — use Jaccard immediately (synchronous, fast)
        let pairs = crate::pairs::jaccard_pairs(&markets_snap, Some(jaccard_threshold));
        let _ = event_tx.send(AppEvent::PairsLoaded(pairs));
    }
}

/// Export a Markdown research report for the selected market.
fn export_markdown_report(app: &App) -> String {
    use std::fmt::Write as _;

    let Some(ref sel_id) = app.selected_market_id else {
        return "Select a market first (press Enter in Markets tab).".to_string();
    };
    let Some(m) = app.markets.iter().find(|m| &m.id == sel_id) else {
        return "Market not found in loaded data.".to_string();
    };

    let mut md = String::new();
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();

    let _ = writeln!(md, "# {}", m.title);
    let _ = writeln!(md, "");
    let _ = writeln!(md, "_Generated by WhoIsSharp on {}_", ts);
    let _ = writeln!(md, "");
    let _ = writeln!(md, "## Market Summary");
    let _ = writeln!(md, "");
    let _ = writeln!(md, "| Field | Value |");
    let _ = writeln!(md, "|---|---|");
    let _ = writeln!(md, "| Platform | {} |", m.platform.label());
    let _ = writeln!(md, "| Market ID | `{}` |", m.id);
    let _ = writeln!(md, "| YES Price | {:.1}% |", m.yes_price * 100.0);
    let _ = writeln!(md, "| NO Price | {:.1}% |", m.no_price * 100.0);
    let _ = writeln!(md, "| Volume | {} |", format_volume(m.volume));
    let _ = writeln!(md, "| Liquidity | {} |",
        m.liquidity.map(|l| format_volume(Some(l))).unwrap_or_else(|| "n/a".into()));
    let _ = writeln!(md, "| End Date | {} |",
        m.end_date.as_deref().unwrap_or("n/a"));
    let _ = writeln!(md, "| Category | {} |",
        m.category.as_deref().unwrap_or("n/a"));
    let _ = writeln!(md, "| Status | {} |", m.status);
    if let Some(ref tok) = m.token_id {
        let _ = writeln!(md, "| Token ID | `{}` |", tok);
    }
    let _ = writeln!(md, "");

    // Description
    if let Some(ref desc) = m.description {
        let _ = writeln!(md, "## Description");
        let _ = writeln!(md, "");
        let _ = writeln!(md, "{}", desc);
        let _ = writeln!(md, "");
    }

    // Orderbook snapshot
    if let Some(ref ob) = app.orderbook {
        let _ = writeln!(md, "## Orderbook Snapshot");
        let _ = writeln!(md, "");
        if let Some(mid) = ob.mid() {
            let _ = writeln!(md, "- Mid: {:.1}¢", mid * 100.0);
        }
        if let Some(spread) = ob.spread() {
            let _ = writeln!(md, "- Spread: {:.1}¢", spread * 100.0);
        }
        let total_bid: f64 = ob.bids.iter().map(|b| b.size).sum();
        let total_ask: f64 = ob.asks.iter().map(|a| a.size).sum();
        let imb = if total_bid + total_ask > 0.0 { (total_bid - total_ask) / (total_bid + total_ask) } else { 0.0 };
        let _ = writeln!(md, "- Imbalance: {:+.2} (bid {:.0} / ask {:.0})", imb, total_bid, total_ask);
        let _ = writeln!(md, "");
    }

    // Price history (ASCII sparkline)
    if !app.chart_data.is_empty() {
        let _ = writeln!(md, "## Price History ({})", app.chart_interval.label());
        let _ = writeln!(md, "");
        let blocks = "▁▂▃▄▅▆▇█";
        let min_p = app.chart_data.iter().map(|(_, y)| *y).fold(f64::INFINITY, f64::min);
        let max_p = app.chart_data.iter().map(|(_, y)| *y).fold(f64::NEG_INFINITY, f64::max);
        let range = (max_p - min_p).max(1.0);
        let spark: String = app.chart_data.iter().map(|(_, y)| {
            let idx = (((y - min_p) / range) * 7.0).round() as usize;
            blocks.chars().nth(idx.min(7)).unwrap_or('▁')
        }).collect();
        let _ = writeln!(md, "```");
        let _ = writeln!(md, "{:.1}%  {}  {:.1}%", min_p, spark, max_p);
        let _ = writeln!(md, "```");
        let _ = writeln!(md, "");
    }

    // Signals for this market
    let market_signals: Vec<&Signal> = app.signals.iter()
        .filter(|s| s.id_a == *sel_id || s.id_b.as_deref() == Some(sel_id))
        .collect();
    if !market_signals.is_empty() {
        let _ = writeln!(md, "## Active Signals");
        let _ = writeln!(md, "");
        for sig in &market_signals {
            let _ = writeln!(md, "- **{}** {} — {}", sig.kind.label(),
                "★".repeat(sig.stars as usize), sig.action);
        }
        let _ = writeln!(md, "");
    }

    // Portfolio position if held
    let pos_in_portfolio: Vec<&Position> = app.portfolio.positions.iter()
        .filter(|p| p.market_id == *sel_id).collect();
    if !pos_in_portfolio.is_empty() {
        let _ = writeln!(md, "## Portfolio Position");
        let _ = writeln!(md, "");
        for pos in &pos_in_portfolio {
            let _ = writeln!(md, "- {} {} | Entry: {:.1}¢ | Mark: {:.1}¢ | PnL: {:+.2}$ ({:+.1}%)",
                pos.side.label(), pos.shares,
                pos.entry_price * 100.0,
                pos.mark_price.unwrap_or(pos.entry_price) * 100.0,
                pos.pnl(), pos.pnl_pct());
            if let Some(ref note) = pos.note {
                let _ = writeln!(md, "  - Thesis: {}", note);
            }
            if let Some(tp) = pos.take_profit {
                let _ = writeln!(md, "  - Take-profit: {:.1}¢", tp * 100.0);
            }
            if let Some(sl) = pos.stop_loss {
                let _ = writeln!(md, "  - Stop-loss: {:.1}¢", sl * 100.0);
            }
        }
        let _ = writeln!(md, "");
    }

    // Chat analysis from this session
    let chat_content: Vec<String> = app.chat_msgs.iter().filter_map(|msg| match msg {
        ChatMsg::User(t)      => Some(format!("**You:** {}", t)),
        ChatMsg::Assistant(t) => Some(format!("**AI:** {}", t)),
        _                     => None,
    }).collect();
    if !chat_content.is_empty() {
        let _ = writeln!(md, "## AI Analysis (this session)");
        let _ = writeln!(md, "");
        for line in &chat_content {
            let _ = writeln!(md, "{}", line);
            let _ = writeln!(md, "");
        }
    }

    // Research notes
    if !app.session.notes.is_empty() {
        let _ = writeln!(md, "## Research Notes");
        let _ = writeln!(md, "");
        for note in &app.session.notes {
            let _ = writeln!(md, "- {}", note);
        }
        let _ = writeln!(md, "");
    }

    // Write to file
    let mut dir = dirs_next::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.push(".whoissharp");
    dir.push("reports");
    if std::fs::create_dir_all(&dir).is_err() {
        return "Export failed: cannot create reports directory".to_string();
    }
    let safe_title: String = m.title.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .take(40)
        .collect();
    let filename = format!("{}_{}.md",
        chrono::Local::now().format("%Y%m%d_%H%M%S"),
        safe_title);
    let path = dir.join(&filename);
    match std::fs::write(&path, &md) {
        Ok(_)  => format!("Report saved: ~/.whoissharp/reports/{}", filename),
        Err(e) => format!("Export failed: {}", e),
    }
}

fn export_current_tab(app: &App) -> String {
    use std::fmt::Write as _;

    let mut dir = dirs_next::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.push(".whoissharp");
    dir.push("exports");
    if std::fs::create_dir_all(&dir).is_err() {
        return "Export failed: cannot create exports directory".to_string();
    }

    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");

    let (filename, content) = match app.active_tab {
        Tab::Markets => {
            let mut csv = "Platform,Title,YES%,Volume,Liquidity,EndDate,ID\n".to_string();
            for m in app.filtered_markets() {
                let _ = writeln!(csv, "{},{},{:.1},{},{},{},{}",
                    m.platform.label(),
                    m.title.replace(',', ";"),
                    m.yes_price * 100.0,
                    m.volume.map(|v| format!("{:.0}", v)).unwrap_or_default(),
                    m.liquidity.map(|v| format!("{:.0}", v)).unwrap_or_default(),
                    m.end_date.as_deref().unwrap_or(""),
                    m.id,
                );
            }
            (format!("markets_{}.csv", ts), csv)
        }
        Tab::Signals => {
            let mut csv = "Kind,Stars,Title,Platform,Price,Gap,Action\n".to_string();
            for s in &app.signals {
                let _ = writeln!(csv, "{},{},{},{},{:.1},{:.3},{}",
                    s.kind.label(), s.stars,
                    s.title.replace(',', ";"),
                    s.platform_a.label(),
                    s.price_a * 100.0, s.gap,
                    s.action.replace(',', ";"),
                );
            }
            (format!("signals_{}.csv", ts), csv)
        }
        Tab::Portfolio => {
            let mut csv = "Platform,Title,Side,EntryPrice,Mark,Shares,PnL,PnL%,ID\n".to_string();
            for p in &app.portfolio.positions {
                let _ = writeln!(csv, "{},{},{},{:.2},{:.2},{:.0},{:.2},{:.1},{}",
                    p.platform.label(),
                    p.title.replace(',', ";"),
                    p.side.label(),
                    p.entry_price * 100.0,
                    p.mark_price.unwrap_or(p.entry_price) * 100.0,
                    p.shares, p.pnl(), p.pnl_pct(), p.id,
                );
            }
            (format!("portfolio_{}.csv", ts), csv)
        }
        Tab::Trades => {
            let mut csv = "Trader,Type,Side,Price,Size,Market,ConditionID\n".to_string();
            for t in &app.trades_data {
                let _ = writeln!(csv, "{},{},{},{:.3},{:.0},{},{}",
                    t.pseudonym.replace(',', ";"),
                    t.trade_type, t.side,
                    t.price, t.size,
                    t.market_title.replace(',', ";"),
                    t.condition_id,
                );
            }
            (format!("trades_{}.csv", ts), csv)
        }
        _ => return "Export not available for this tab. Use Markets, Signals, Portfolio, or Trades.".to_string(),
    };

    let path = dir.join(&filename);
    match std::fs::write(&path, &content) {
        Ok(_) => format!("Exported {} rows → {}", content.lines().count() - 1, path.display()),
        Err(e) => format!("Export failed: {}", e),
    }
}
