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
        Axis, Block, Borders, Chart, Dataset, GraphType,
        List, ListItem, ListState, Paragraph, Tabs, Wrap,
    },
    Frame, Terminal,
};
use tokio::sync::mpsc;

use crate::agent::{self, AppEvent};
use crate::llm::{LlmBackend, LlmMessage};
use crate::markets::{ChartInterval, Market, Orderbook, Platform};
use crate::portfolio::{self, Portfolio, Position, Side};
use crate::signals::{Signal, SignalKind};
use crate::tools::MarketClients;

// ─── Tabs ────────────────────────────────────────────────────────────────────

const TAB_NAMES: &[&str] = &["Signals", "Markets", "Chart", "Book", "Portfolio", "Chat"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Signals   = 0,
    Markets   = 1,
    Chart     = 2,
    Orderbook = 3,
    Portfolio = 4,
    Chat      = 5,
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
    pub search:            String,
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

    // Status
    pub status:            String,
    pub is_loading:        bool,
    pub backend_name:      String,
    pub last_updated:      Option<chrono::DateTime<chrono::Local>>,

    // Selected market ID (for chart / orderbook loading)
    pub selected_market_id: Option<String>,
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
            search_mode:       false,
            chart_interval:    ChartInterval::OneWeek,
            chat_msgs:         Vec::new(),
            input:             String::new(),
            sent_history:      Vec::new(),
            sent_cursor:       None,
            pos_input_mode:    false,
            pos_input_step:    PosInputStep::default(),
            pos_draft:         PosDraft::default(),
            status:            "Loading market data…".to_string(),
            is_loading:        true,
            backend_name,
            last_updated:      None,
            selected_market_id: None,
        }
    }

    // ── Filtered markets ──────────────────────────────────────────────────────

    pub fn filtered_markets(&self) -> Vec<&Market> {
        self.markets
            .iter()
            .filter(|m| {
                self.platform_filter.matches(&m.platform)
                    && (self.search.is_empty()
                        || m.title.to_lowercase().contains(&self.search.to_lowercase()))
            })
            .collect()
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
            Tab::Chat      => { self.chat_scroll = self.chat_scroll.saturating_add(1); }
            Tab::Orderbook => { self.book_scroll = self.book_scroll.saturating_add(1); }
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
            Tab::Chat      => { self.chat_scroll = self.chat_scroll.saturating_sub(1); }
            Tab::Orderbook => { self.book_scroll = self.book_scroll.saturating_sub(1); }
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
        Tab::Signals   => render_signals(f, chunks[2], app),
        Tab::Markets   => render_markets(f, chunks[2], app),
        Tab::Chart     => render_chart(f, chunks[2], app),
        Tab::Orderbook => render_orderbook(f, chunks[2], app),
        Tab::Portfolio => render_portfolio(f, chunks[2], app),
        Tab::Chat      => render_chat(f, chunks[2], app),
    }
    render_status(f, chunks[3], app);
    render_input(f, chunks[4], app);
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
        .map(|(i, name)| Line::from(format!(" [{}] {} ", i + 1, name)))
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
    render_signal_detail(f, chunks[1], app);
}

fn signal_kind_color(kind: &SignalKind) -> Color {
    match kind {
        SignalKind::Arb      => Color::Magenta,
        SignalKind::VolSpike => Color::Yellow,
        SignalKind::NearFifty => Color::Cyan,
        SignalKind::Thin     => Color::DarkGray,
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
        let p = Paragraph::new("\n  Select a signal with j/k\n\n  Press Enter to open the primary market.\n  Press 'a' to ask AI about it.")
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
    } else {
        let gap_label = match sig.kind {
            SignalKind::NearFifty => "Distance from 50%",
            SignalKind::VolSpike  => "Vol × avg",
            SignalKind::Thin      => "Liquidity ($)",
            SignalKind::Arb       => "Gap",
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {:>10}: ", gap_label), Style::default().fg(Color::DarkGray)),
            Span::raw(match sig.kind {
                SignalKind::NearFifty => format!("{:.1}¢", sig.gap * 100.0),
                SignalKind::VolSpike  => format!("{:.1}×", sig.gap),
                SignalKind::Thin      => format!("${:.0}K", sig.gap / 1000.0),
                SignalKind::Arb       => format!("{:.1}¢", sig.gap * 100.0),
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
        "  [Enter] open market  [a] ask AI  [n] add position",
        Style::default().fg(Color::DarkGray),
    )));

    let p = Paragraph::new(lines)
        .block(Block::default().title(" Signal Detail ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .wrap(Wrap { trim: false });
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
    let filter_label = app.platform_filter.label();
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

            let title_str = trunc(&m.title, 32);

            let line = Line::from(vec![
                Span::styled(m.platform.label(), Style::default().fg(platform_color)),
                Span::raw(" "),
                Span::styled(format!("{:5.1}%", pct), Style::default().fg(pct_color).bold()),
                Span::raw(format!(" {:>7} ", vol)),
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
        Line::from(Span::styled(
            "  [Enter] load chart/book  [n] add position  [a] ask AI  [/] search",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];

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

    f.render_widget(chart, area);
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

    let spread = book.spread().map(|s| format!("{:.3}", s)).unwrap_or_else(|| "N/A".into());
    let mid    = book.mid().map(|m| format!("{:.3}", m)).unwrap_or_else(|| "N/A".into());

    let title = app.selected_market_id
        .as_ref()
        .and_then(|id| app.markets.iter().find(|m| &m.id == id))
        .map(|m| format!(" Order Book — {} ", m.title))
        .unwrap_or_else(|| " Order Book ".to_string());

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::raw(format!(" Spread: {}  Mid: {}", spread, mid)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("{:>10}  {:>10}  {:>10}  │  {:>10}  {:>10}  {:>10}",
                    "TOTAL", "SIZE", "BID",
                    "ASK",   "SIZE", "TOTAL"),
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
        (b.price, b.size, bid_total)
    }).collect();
    let asks: Vec<(f64, f64, f64)> = book.asks.iter().take(depth).map(|a| {
        ask_total += a.size;
        (a.price, a.size, ask_total)
    }).collect();

    for i in 0..depth {
        let bid_part = bids.get(i).map(|(p, s, t)| {
            (
                Span::styled(format!("{:>10.0}", t), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("  {:>10.0}", s), Style::default().fg(Color::White)),
                Span::styled(format!("  {:>10.3}", p), Style::default().fg(Color::Green).bold()),
            )
        });
        let ask_part = asks.get(i).map(|(p, s, t)| {
            (
                Span::styled(format!("{:>10.3}", p), Style::default().fg(Color::Red).bold()),
                Span::styled(format!("  {:>10.0}", s), Style::default().fg(Color::White)),
                Span::styled(format!("  {:>10.0}", t), Style::default().fg(Color::DarkGray)),
            )
        });

        let mut spans = Vec::new();
        match bid_part {
            Some((total, size, price)) => { spans.push(total); spans.push(size); spans.push(price); }
            None => { spans.push(Span::raw(" ".repeat(34))); }
        }
        spans.push(Span::styled("  │  ", Style::default().fg(Color::DarkGray)));
        match ask_part {
            Some((price, size, total)) => { spans.push(price); spans.push(size); spans.push(total); }
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(0)])
        .split(area);

    render_portfolio_summary(f, chunks[0], app);
    render_portfolio_positions(f, chunks[1], app);
}

fn render_portfolio_summary(f: &mut Frame, area: Rect, app: &App) {
    let total_cost  = app.portfolio.total_cost();
    let total_value = app.portfolio.total_value();
    let total_pnl   = app.portfolio.total_pnl();
    let pnl_color   = if total_pnl >= 0.0 { Color::Green } else { Color::Red };

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
            Span::styled(
                "  [n] Add position  [d] Delete selected  [Enter] Load chart",
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
            "No positions. Navigate to Markets tab, select a market, press 'n' to add a position."
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

        let title_str = trunc(&pos.title, 30);

        let line = Line::from(vec![
            Span::styled(pos.platform.label(), Style::default().fg(platform_color)),
            Span::raw(" "),
            Span::styled(pos.side.label(), Style::default().fg(Color::White).bold()),
            Span::raw(format!(" {:>6.1}¢ entry  {:>6.1}¢ mark  ", pos.entry_price * 100.0, mark)),
            Span::styled(
                format!("{:+.2}$ ({:+.1}%)", pnl, pnl_pct),
                Style::default().fg(pnl_color).bold(),
            ),
            Span::raw("  "),
            Span::raw(title_str),
        ]);
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
    let mut lines: Vec<Line> = Vec::new();

    for msg in &app.chat_msgs {
        match msg {
            ChatMsg::User(text) => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    " You",
                    Style::default().fg(Color::Cyan).bold(),
                )));
                for l in text.lines() {
                    lines.push(Line::from(format!("  {}", l)));
                }
            }
            ChatMsg::Assistant(text) => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    " Claude",
                    Style::default().fg(Color::Green).bold(),
                )));
                for l in text.lines() {
                    lines.push(Line::from(format!("  {}", l)));
                }
            }
            ChatMsg::ToolCall { name, args } => {
                let preview = if args.len() > 60 { format!("{}…", &args[..60]) } else { args.clone() };
                lines.push(Line::from(vec![
                    Span::styled("  ⟳ ", Style::default().fg(Color::Yellow)),
                    Span::styled(name, Style::default().fg(Color::Yellow)),
                    Span::styled(format!("({})", preview), Style::default().fg(Color::DarkGray)),
                ]));
            }
            ChatMsg::ToolResult { name, preview } => {
                let p = if preview.len() > 80 { format!("{}…", &preview[..80]) } else { preview.clone() };
                lines.push(Line::from(vec![
                    Span::styled("  ✓ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(name, Style::default().fg(Color::DarkGray)),
                    Span::raw(format!(": {}", p)),
                ]));
            }
            ChatMsg::Error(e) => {
                lines.push(Line::from(Span::styled(
                    format!("  Error: {}", e),
                    Style::default().fg(Color::Red),
                )));
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

    let total_lines = lines.len() as u16;
    let visible_height = area.height.saturating_sub(2);
    let scroll = if total_lines > visible_height {
        total_lines - visible_height
    } else {
        0
    };
    let effective_scroll = scroll.saturating_sub(app.chat_scroll);

    let p = Paragraph::new(lines)
        .block(Block::default().title(" Chat ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .wrap(Wrap { trim: false })
        .scroll((effective_scroll, 0));

    f.render_widget(p, area);
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let filter = app.platform_filter.label();
    let interval = app.chart_interval.label();
    let line = Line::from(vec![
        Span::styled(
            if app.is_loading { " ⟳ Loading " } else { " ● Ready   " },
            Style::default().fg(if app.is_loading { Color::Yellow } else { Color::Green }),
        ),
        Span::raw(format!(" {}  Chart:{}  ", filter, interval)),
        Span::styled("│", Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(&app.status, Style::default().fg(Color::White)),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::DarkGray)),
        area,
    );
}

// ── Input box ────────────────────────────────────────────────────────────────

fn render_input(f: &mut Frame, area: Rect, app: &App) {
    let prompt = if app.search_mode {
        "/search: "
    } else if app.pos_input_mode {
        "pos> "
    } else {
        "> "
    };
    let line = Line::from(vec![
        Span::styled(prompt, Style::default().fg(Color::Cyan)),
        Span::raw(&app.input),
    ]);
    let p = Paragraph::new(line);
    f.render_widget(p, area);

    // Show cursor
    let x = area.x + prompt.len() as u16 + app.input.len() as u16;
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
        assert_eq!(Tab::Signals.next(),   Tab::Markets);
        assert_eq!(Tab::Markets.next(),   Tab::Chart);
        assert_eq!(Tab::Chart.next(),     Tab::Orderbook);
        assert_eq!(Tab::Orderbook.next(), Tab::Portfolio);
        assert_eq!(Tab::Portfolio.next(), Tab::Chat);
        assert_eq!(Tab::Chat.next(),      Tab::Signals); // wraps
    }

    #[test]
    fn tab_prev_cycles_backward() {
        assert_eq!(Tab::Signals.prev(),   Tab::Chat); // wraps
        assert_eq!(Tab::Markets.prev(),   Tab::Signals);
        assert_eq!(Tab::Chat.prev(),      Tab::Portfolio);
    }
}

// ─── Main TUI loop ────────────────────────────────────────────────────────────

pub async fn run_tui(
    backend:      Option<Arc<dyn LlmBackend>>,
    clients:      Arc<MarketClients>,
    backend_name: String,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend_term = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend_term)?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AppEvent>();
    let mut app = App::new(backend_name);
    let mut llm_history: Vec<LlmMessage> = Vec::new();

    // Kick off initial market data refresh
    {
        let clients_clone = clients.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move { agent::refresh_markets(clients_clone, tx).await });
    }

    let mut term_events = EventStream::new();

    loop {
        terminal.draw(|f| render(f, &app))?;

        tokio::select! {
            Some(ev) = event_rx.recv() => {
                match ev {
                    AppEvent::MarketsLoaded(markets) => {
                        app.markets = markets;
                        if app.market_list.selected().is_none() && !app.markets.is_empty() {
                            app.market_list.select(Some(0));
                        }
                        app.update_portfolio_marks();
                    }
                    AppEvent::EventsLoaded(_) => {}  // Events tab removed; ignore
                    AppEvent::SignalsComputed(sigs) => {
                        app.signals = sigs;
                        if app.signal_list.selected().is_none() && !app.signals.is_empty() {
                            app.signal_list.select(Some(0));
                        }
                    }
                    AppEvent::PriceHistoryLoaded { market_id, candles } => {
                        if Some(&market_id) == app.selected_market_id.as_ref() {
                            app.chart_data = candles
                                .iter()
                                .map(|c| (c.ts as f64, c.close * 100.0))
                                .collect();
                            app.chart_min = candles.iter().map(|c| c.low * 100.0).fold(f64::INFINITY, f64::min);
                            app.chart_max = candles.iter().map(|c| c.high * 100.0).fold(f64::NEG_INFINITY, f64::max);
                        }
                    }
                    AppEvent::OrderbookLoaded { market_id, orderbook } => {
                        if Some(&market_id) == app.selected_market_id.as_ref() {
                            app.orderbook = Some(orderbook);
                        }
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
                    }
                    AppEvent::AgentError(e) => {
                        app.is_loading = false;
                        app.chat_msgs.push(ChatMsg::Error(e.clone()));
                        app.status = format!("Error: {}", e);
                    }
                }
            }

            Some(Ok(ev)) = term_events.next() => {
                if let Event::Key(key) = ev {
                    if handle_key(&mut app, key, &backend, &clients, &event_tx, &mut llm_history).await {
                        break;
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

// ─── Key handling ─────────────────────────────────────────────────────────────

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

    // Ctrl+C always quits (or clears input)
    if key.modifiers == KeyModifiers::CONTROL && key.code == KC::Char('c') {
        if !app.input.is_empty() || app.pos_input_mode {
            app.input.clear();
            app.sent_cursor = None;
            app.pos_input_mode = false;
            app.pos_input_step = PosInputStep::EntryPrice;
            app.pos_draft = PosDraft::default();
            app.status = "Cancelled.".to_string();
            return false;
        }
        return true;
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

    // ── Search mode ───────────────────────────────────────────────────────────
    if app.search_mode {
        match key.code {
            KC::Esc => {
                app.search_mode = false;
                app.search.clear();
                app.status = "Search cleared".to_string();
            }
            KC::Enter => {
                app.search_mode = false;
                app.status = if app.search.is_empty() {
                    "Search cleared".to_string()
                } else {
                    format!("Filtering: '{}'", app.search)
                };
                app.market_list.select(Some(0));
            }
            KC::Backspace => { app.search.pop(); }
            KC::Char(c) => { app.search.push(c); }
            _ => {}
        }
        return false;
    }

    // ── Normal mode ───────────────────────────────────────────────────────────
    match key.code {
        KC::Char('q') if app.input.is_empty() => return true,

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

        KC::Tab => { app.active_tab = app.active_tab.next(); }
        KC::BackTab => { app.active_tab = app.active_tab.prev(); }

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
                    app.orderbook = None;
                    app.status = format!("Loading data for {}", id);
                    trigger_chart_load(app, clients, event_tx).await;
                    trigger_orderbook_load(app, clients, event_tx).await;
                    app.active_tab = AppTab::Chart;
                }
            }
        }

        // ── Refresh ───────────────────────────────────────────────────────────
        KC::Char('r') if app.input.is_empty() => {
            let clients_c = clients.clone();
            let tx = event_tx.clone();
            tokio::spawn(async move { agent::refresh_markets(clients_c, tx).await });
            app.status = "Refreshing markets…".to_string();
        }

        // ── Add position ──────────────────────────────────────────────────────
        KC::Char('n') if app.input.is_empty() => {
            // Works from Markets tab (picks selected market) or Signals tab (picks primary market)
            if app.active_tab == AppTab::Signals {
                if let Some(sig) = app.selected_signal() {
                    let id  = sig.id_a.clone();
                    let plat = sig.platform_a.clone();
                    let title = sig.title.clone();
                    let price = sig.price_a;
                    // Synthesise a dummy market entry to reuse start_add_position flow
                    if let Some(m) = app.markets.iter().find(|m| m.id == id && m.platform == plat) {
                        let idx = app.markets.iter().position(|m| m.id == id && m.platform == plat).unwrap();
                        app.market_list.select(Some(idx));
                        // Temporarily switch to Markets so start_add_position works
                        let prev_tab = app.active_tab;
                        app.active_tab = AppTab::Markets;
                        // Filter might hide it — just inline
                        let _ = m;
                        app.pos_draft = crate::tui::PosDraft {
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
                        let pct = price * 100.0;
                        app.status = format!(
                            "Add position [{:.1}¢] — Enter entry price (¢):", pct
                        );
                    } else {
                        app.start_add_position();
                    }
                }
            } else {
                app.start_add_position();
            }
        }

        // ── Delete portfolio position ─────────────────────────────────────────
        KC::Char('d') if app.input.is_empty() && app.active_tab == AppTab::Portfolio => {
            app.delete_selected_position();
        }

        // ── Search ────────────────────────────────────────────────────────────
        KC::Char('/') if app.input.is_empty() => {
            app.search_mode = true;
            app.search.clear();
            app.active_tab = AppTab::Markets;
        }
        KC::Esc if app.input.is_empty() && !app.search.is_empty() => {
            app.search.clear();
            app.status = "Search cleared".to_string();
        }

        // ── Platform filter ───────────────────────────────────────────────────
        KC::Char('p') if app.input.is_empty() => {
            app.platform_filter = match app.platform_filter {
                PlatformFilter::All        => PlatformFilter::Polymarket,
                PlatformFilter::Polymarket => PlatformFilter::Kalshi,
                PlatformFilter::Kalshi     => PlatformFilter::All,
            };
            app.market_list.select(Some(0));
            app.status = format!("Filter: {}", app.platform_filter.label());
        }

        // ── Chart interval ────────────────────────────────────────────────────
        KC::Char('c') if app.input.is_empty() => {
            app.chart_interval = app.chart_interval.next();
            app.chart_data.clear();
            app.status = format!("Chart interval: {}", app.chart_interval.label());
            trigger_chart_load(app, clients, event_tx).await;
        }

        // ── Ask AI ────────────────────────────────────────────────────────────
        KC::Char('a') if app.input.is_empty() => {
            let info = match app.active_tab {
                AppTab::Signals => {
                    app.selected_signal().map(|s| (
                        s.title.clone(),
                        s.id_a.clone(),
                        s.platform_a.name().to_lowercase(),
                    ))
                }
                _ => {
                    app.selected_market().map(|m| (
                        m.title.clone(),
                        m.id.clone(),
                        m.platform.name().to_lowercase(),
                    ))
                }
            };
            if let Some((title, id, plat)) = info {
                app.input = format!("Analyze the market: '{}' (platform: {}, id: {})", title, plat, id);
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
    app.is_loading = true;
    app.status = "Sending…".to_string();

    let backend_c  = backend_arc.clone();
    let clients_c  = clients.clone();
    let tx         = event_tx.clone();
    let mut hist   = std::mem::take(llm_history);

    let handle = tokio::spawn(async move {
        agent::run_turn(backend_c, clients_c, &mut hist, msg, tx).await;
        hist
    });

    let _ = handle; // detach; history is rebuilt per-turn
    *llm_history = Vec::new();
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
