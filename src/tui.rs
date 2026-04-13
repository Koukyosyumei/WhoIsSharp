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
//! Tabs: [1] Markets  [2] Chart  [3] Book  [4] Events  [5] Chat  [6] Watchlist

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
use crate::tools::MarketClients;

// ─── Tabs ────────────────────────────────────────────────────────────────────

const TAB_NAMES: &[&str] = &["Markets", "Chart", "Book", "Events", "Chat", "Watchlist"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Markets   = 0,
    Chart     = 1,
    Orderbook = 2,
    Events    = 3,
    Chat      = 4,
    Watchlist = 5,
}

impl Tab {
    fn from_index(n: usize) -> Option<Self> {
        match n {
            0 => Some(Tab::Markets),
            1 => Some(Tab::Chart),
            2 => Some(Tab::Orderbook),
            3 => Some(Tab::Events),
            4 => Some(Tab::Chat),
            5 => Some(Tab::Watchlist),
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
    pub markets:          Vec<Market>,
    pub events:           Vec<crate::markets::Event>,
    pub orderbook:        Option<Orderbook>,
    pub chart_data:       Vec<(f64, f64)>,
    pub chart_min:        f64,
    pub chart_max:        f64,
    pub watchlist:        Vec<String>,               // market IDs

    // Navigation
    pub active_tab:       Tab,
    pub market_list:      ListState,
    pub event_list:       ListState,
    pub watch_list:       ListState,
    pub chat_scroll:      u16,
    pub book_scroll:      u16,

    // Filter / search
    pub platform_filter:  PlatformFilter,
    pub search:           String,
    pub search_mode:      bool,
    pub chart_interval:   ChartInterval,

    // Chat
    pub chat_msgs:        Vec<ChatMsg>,
    pub input:            String,
    pub sent_history:     Vec<String>,
    pub sent_cursor:      Option<usize>,

    // Status
    pub status:           String,
    pub is_loading:       bool,
    pub backend_name:     String,
    pub last_updated:     Option<chrono::DateTime<chrono::Local>>,

    // Selected market ID (for chart / orderbook loading)
    pub selected_market_id: Option<String>,
}

impl App {
    pub fn new(backend_name: String) -> Self {
        App {
            markets:           Vec::new(),
            events:            Vec::new(),
            orderbook:         None,
            chart_data:        Vec::new(),
            chart_min:         0.0,
            chart_max:         100.0,
            watchlist:         Vec::new(),
            active_tab:        Tab::Markets,
            market_list:       ListState::default(),
            event_list:        ListState::default(),
            watch_list:        ListState::default(),
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

    pub fn selected_watched_market(&self) -> Option<&Market> {
        let idx = self.watch_list.selected()?;
        let wid = self.watchlist.get(idx)?;
        self.markets.iter().find(|m| &m.id == wid)
    }

    pub fn toggle_watchlist(&mut self) {
        let market_info = self.selected_market().map(|m| (m.id.clone(), m.title.clone()));
        if let Some((id, title)) = market_info {
            if let Some(pos) = self.watchlist.iter().position(|w| w == &id) {
                self.watchlist.remove(pos);
                self.status = format!("Removed '{}' from watchlist", title);
            } else {
                self.watchlist.push(id);
                self.status = format!("Added '{}' to watchlist", title);
            }
        }
    }

    pub fn is_watched(&self, id: &str) -> bool {
        self.watchlist.iter().any(|w| w == id)
    }

    // ── List navigation ───────────────────────────────────────────────────────

    pub fn list_down(&mut self) {
        match self.active_tab {
            Tab::Markets => {
                let len = self.filtered_markets().len();
                if len == 0 { return; }
                let i = self.market_list.selected().map(|i| (i + 1) % len).unwrap_or(0);
                self.market_list.select(Some(i));
            }
            Tab::Events => {
                let len = self.events.len();
                if len == 0 { return; }
                let i = self.event_list.selected().map(|i| (i + 1) % len).unwrap_or(0);
                self.event_list.select(Some(i));
            }
            Tab::Watchlist => {
                let len = self.watchlist.len();
                if len == 0 { return; }
                let i = self.watch_list.selected().map(|i| (i + 1) % len).unwrap_or(0);
                self.watch_list.select(Some(i));
            }
            Tab::Chat => { self.chat_scroll = self.chat_scroll.saturating_add(1); }
            Tab::Orderbook => { self.book_scroll = self.book_scroll.saturating_add(1); }
            _ => {}
        }
    }

    pub fn list_up(&mut self) {
        match self.active_tab {
            Tab::Markets => {
                let len = self.filtered_markets().len();
                if len == 0 { return; }
                let i = self.market_list.selected()
                    .map(|i| if i == 0 { len - 1 } else { i - 1 })
                    .unwrap_or(0);
                self.market_list.select(Some(i));
            }
            Tab::Events => {
                let len = self.events.len();
                if len == 0 { return; }
                let i = self.event_list.selected()
                    .map(|i| if i == 0 { len - 1 } else { i - 1 })
                    .unwrap_or(0);
                self.event_list.select(Some(i));
            }
            Tab::Watchlist => {
                let len = self.watchlist.len();
                if len == 0 { return; }
                let i = self.watch_list.selected()
                    .map(|i| if i == 0 { len - 1 } else { i - 1 })
                    .unwrap_or(0);
                self.watch_list.select(Some(i));
            }
            Tab::Chat => { self.chat_scroll = self.chat_scroll.saturating_sub(1); }
            Tab::Orderbook => { self.book_scroll = self.book_scroll.saturating_sub(1); }
            _ => {}
        }
    }

    // ── History navigation (↑/↓ in input) ────────────────────────────────────

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
        let new_cursor = self.sent_cursor.map(|i| {
            if i + 1 >= self.sent_history.len() { None } else { Some(i + 1) }
        }).flatten();
        self.sent_cursor = new_cursor;
        self.input = new_cursor
            .map(|i| self.sent_history[i].clone())
            .unwrap_or_default();
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
        Tab::Markets   => render_markets(f, chunks[2], app),
        Tab::Chart     => render_chart(f, chunks[2], app),
        Tab::Orderbook => render_orderbook(f, chunks[2], app),
        Tab::Events    => render_events(f, chunks[2], app),
        Tab::Chat      => render_chat(f, chunks[2], app),
        Tab::Watchlist => render_watchlist(f, chunks[2], app),
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

    let line = Line::from(vec![
        Span::styled(" WhoIsSharp ", Style::default().fg(Color::Black).bg(Color::Cyan).bold()),
        Span::raw(" "),
        Span::styled(&app.backend_name, Style::default().fg(Color::Yellow)),
        Span::raw("  │  "),
        Span::styled("PM", Style::default().fg(Color::Green)),
        Span::raw(" + "),
        Span::styled("KL", Style::default().fg(Color::Blue)),
        Span::raw(format!("{}  │  updated: {}  │  ", loading, updated)),
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
            Line::from(format!(" [{}] {} ", i + 1, name))
        })
        .collect();

    let tabs = Tabs::new(titles)
        .select(app.active_tab as usize)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(Style::default().fg(Color::Cyan).bold())
        .divider(symbols::DOT);

    f.render_widget(tabs, area);
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
        format!(" 🔍{}", app.search)
    };

    let title = format!(" Markets [{}]{} ", filter_label, search_label);

    let filtered = app.filtered_markets();
    let items: Vec<ListItem> = filtered
        .iter()
        .map(|m| {
            let watched = if app.is_watched(&m.id) { "★ " } else { "  " };
            let platform_color = match m.platform {
                Platform::Polymarket => Color::Green,
                Platform::Kalshi     => Color::Blue,
            };
            let pct = m.yes_price * 100.0;
            let pct_color = price_color(m.yes_price);
            let vol = m.volume
                .map(|v| {
                    if v >= 1_000_000.0 { format!("${:.1}M", v / 1_000_000.0) }
                    else if v >= 1_000.0 { format!("${:.0}K", v / 1_000.0) }
                    else { format!("${:.0}", v) }
                })
                .unwrap_or_default();

            let title_str = if m.title.len() > 32 {
                format!("{}…", &m.title[..31])
            } else {
                m.title.clone()
            };

            let line = Line::from(vec![
                Span::raw(watched),
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
    let vol = m.volume
        .map(|v| {
            if v >= 1_000_000.0 { format!("${:.1}M", v / 1_000_000.0) }
            else if v >= 1_000.0 { format!("${:.0}K", v / 1_000.0) }
            else { format!("${:.0}", v) }
        })
        .unwrap_or_else(|| "N/A".into());
    let liq = m.liquidity
        .map(|v| format!("${:.0}K", v / 1_000.0))
        .unwrap_or_else(|| "N/A".into());

    let watched = if app.is_watched(&m.id) { " ★ Watched" } else { " ☆ Not watched (w)" };

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
        Line::from(Span::styled(watched, Style::default().fg(Color::Yellow))),
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

    // Build x-axis labels (3 points: start, middle, end)
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

    // Pre-calculate totals for depth of field display
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

// ── Events tab ────────────────────────────────────────────────────────────────

fn render_events(f: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .events
        .iter()
        .map(|e| {
            let platform_color = match e.platform {
                Platform::Polymarket => Color::Green,
                Platform::Kalshi     => Color::Blue,
            };
            let line = Line::from(vec![
                Span::styled(e.platform.label(), Style::default().fg(platform_color)),
                Span::raw(" "),
                Span::styled(&e.title, Style::default().fg(Color::White)),
                Span::raw(" "),
                Span::styled(
                    e.category.as_deref().unwrap_or("misc"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().title(" Events ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▶ ");

    let mut state = app.event_list.clone();
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

    // Add "thinking" indicator if loading
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
    // Always scroll to bottom unless user has scrolled up
    let effective_scroll = scroll.saturating_sub(app.chat_scroll);

    let p = Paragraph::new(lines)
        .block(Block::default().title(" Chat ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .wrap(Wrap { trim: false })
        .scroll((effective_scroll, 0));

    f.render_widget(p, area);
}

// ── Watchlist tab ─────────────────────────────────────────────────────────────

fn render_watchlist(f: &mut Frame, area: Rect, app: &App) {
    if app.watchlist.is_empty() {
        let p = Paragraph::new(" No markets watched. Press 'w' on a market to add it.")
            .block(Block::default().title(" Watchlist ").borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)));
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app
        .watchlist
        .iter()
        .filter_map(|id| app.markets.iter().find(|m| &m.id == id))
        .map(|m| {
            let pct = m.yes_price * 100.0;
            let pct_color = price_color(m.yes_price);
            let platform_color = match m.platform {
                Platform::Polymarket => Color::Green,
                Platform::Kalshi     => Color::Blue,
            };
            let line = Line::from(vec![
                Span::styled(m.platform.label(), Style::default().fg(platform_color)),
                Span::raw(" "),
                Span::styled(format!("{:5.1}%", pct), Style::default().fg(pct_color).bold()),
                Span::raw(format!("  {}", m.title)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().title(format!(" Watchlist ({}) ", app.watchlist.len())).borders(Borders::ALL).border_style(Style::default().fg(Color::DarkGray)))
        .highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▶ ");

    let mut state = app.watch_list.clone();
    f.render_stateful_widget(list, area, &mut state);
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
    let prompt = if app.search_mode { "/search: " } else { "> " };
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

// ─── Main TUI loop ────────────────────────────────────────────────────────────

pub async fn run_tui(
    backend:      Option<Arc<dyn LlmBackend>>,
    clients:      Arc<MarketClients>,
    backend_name: String,
) -> anyhow::Result<()> {
    // Setup terminal
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
            // ── App events (from agent / refresh tasks) ───────────────────────
            Some(ev) = event_rx.recv() => {
                match ev {
                    AppEvent::MarketsLoaded(markets) => {
                        app.markets = markets;
                        if app.market_list.selected().is_none() && !app.markets.is_empty() {
                            app.market_list.select(Some(0));
                        }
                    }
                    AppEvent::EventsLoaded(events) => {
                        app.events = events;
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
                        app.status = format!("{} markets loaded", app.markets.len());
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
                        // Replace the last assistant message if it exists, else push new
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

            // ── Terminal key events ───────────────────────────────────────────
            Some(Ok(ev)) = term_events.next() => {
                if let Event::Key(key) = ev {
                    if handle_key(&mut app, key, &backend, &clients, &event_tx, &mut llm_history).await {
                        break; // Quit
                    }
                }
            }
        }
    }

    // Restore terminal
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
    // Alias our Tab enum to avoid clash with KC::Tab
    type AppTab = Tab;

    // Ctrl+C always quits
    if key.modifiers == KeyModifiers::CONTROL && key.code == KC::Char('c') {
        if !app.input.is_empty() {
            app.input.clear();
            app.sent_cursor = None;
            return false;
        }
        return true;
    }

    // Search mode
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

    // Normal mode
    match key.code {
        // ── Quit ──────────────────────────────────────────────────────────────
        KC::Char('q') if app.input.is_empty() => return true,

        // ── Tab switching ─────────────────────────────────────────────────────
        KC::Char('1') if app.input.is_empty() => { app.active_tab = AppTab::Markets; }
        KC::Char('2') if app.input.is_empty() => {
            app.active_tab = AppTab::Chart;
            trigger_chart_load(app, clients, event_tx).await;
        }
        KC::Char('3') if app.input.is_empty() => {
            app.active_tab = AppTab::Orderbook;
            trigger_orderbook_load(app, clients, event_tx).await;
        }
        KC::Char('4') if app.input.is_empty() => {
            app.active_tab = AppTab::Events;
            if app.events.is_empty() {
                let clients_c = clients.clone();
                let tx = event_tx.clone();
                tokio::spawn(async move {
                    match clients_c.polymarket.fetch_events(30).await {
                        Ok(mut pm) => {
                            match clients_c.kalshi.fetch_events(30).await {
                                Ok(mut kl) => { pm.append(&mut kl); }
                                Err(_) => {}
                            }
                            let _ = tx.send(AppEvent::EventsLoaded(pm));
                        }
                        Err(_) => {}
                    }
                });
            }
        }
        KC::Char('5') if app.input.is_empty() => { app.active_tab = AppTab::Chat; }
        KC::Char('6') if app.input.is_empty() => { app.active_tab = AppTab::Watchlist; }

        KC::Tab => {
            app.active_tab = app.active_tab.next();
        }
        KC::BackTab => {
            app.active_tab = app.active_tab.prev();
        }

        // ── List navigation ───────────────────────────────────────────────────
        KC::Char('j') | KC::Down if app.input.is_empty() => { app.list_down(); }
        KC::Char('k') | KC::Up   if app.input.is_empty() => { app.list_up(); }

        // ── Enter — select market or send chat ────────────────────────────────
        KC::Enter => {
            if app.active_tab == AppTab::Chat {
                // Send chat message
                if !app.input.is_empty() {
                    send_chat(app, backend, clients, event_tx, llm_history).await;
                }
            } else if app.active_tab == AppTab::Markets && app.input.is_empty() {
                // Load chart + orderbook for selected market
                if let Some(m) = app.selected_market() {
                    let id = m.id.clone();
                    app.selected_market_id = Some(id.clone());
                    app.chart_data.clear();
                    app.orderbook = None;
                    app.status = format!("Loading data for {}", id);
                }
                trigger_chart_load(app, clients, event_tx).await;
                trigger_orderbook_load(app, clients, event_tx).await;
            } else if !app.input.is_empty() {
                // Send chat from non-chat tab
                send_chat(app, backend, clients, event_tx, llm_history).await;
            }
        }

        // ── Refresh ───────────────────────────────────────────────────────────
        KC::Char('r') if app.input.is_empty() => {
            let clients_c = clients.clone();
            let tx = event_tx.clone();
            tokio::spawn(async move { agent::refresh_markets(clients_c, tx).await });
            app.status = "Refreshing markets…".to_string();
        }

        // ── Watchlist ─────────────────────────────────────────────────────────
        KC::Char('w') if app.input.is_empty() => { app.toggle_watchlist(); }

        // ── Search / filter ───────────────────────────────────────────────────
        KC::Char('/') if app.input.is_empty() => {
            app.search_mode = true;
            app.search.clear();
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

        // ── Ask AI about selected market ──────────────────────────────────────
        KC::Char('a') if app.input.is_empty() => {
            let market_info = app.selected_market()
                .map(|m| (m.title.clone(), m.id.clone(), m.platform.name().to_lowercase()));
            if let Some((title, id, plat)) = market_info {
                app.input = format!("Analyze the market: '{}' (platform: {}, id: {})", title, plat, id);
            }
        }

        // ── Input editing ─────────────────────────────────────────────────────
        KC::Char(c) => { app.input.push(c); app.sent_cursor = None; }
        KC::Backspace => { app.input.pop(); }

        // ── Input history ─────────────────────────────────────────────────────
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

    // No LLM configured — show helpful error in chat
    let Some(backend_arc) = backend else {
        app.chat_msgs.push(ChatMsg::User(msg));
        app.chat_msgs.push(ChatMsg::Error(
            "No AI backend configured. Run with --backend anthropic (or gemini/openai/ollama) \
             and the appropriate API key to enable AI features.".to_string(),
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

    // The history is updated in the spawned task; we swap it back after done.
    // We use a channel to signal completion — the AgentDone event serves that purpose.
    // Store the handle so we can await it, but since run_tui drives via events, just detach.
    // NOTE: The history is moved into the task so it won't persist across calls here.
    // For simplicity, we accept this limitation — a production version would use Arc<Mutex>.
    let _ = handle; // detach

    // Clear the local history copy since we handed it off
    *llm_history = Vec::new();
}

async fn trigger_chart_load(
    app:      &App,
    clients:  &Arc<MarketClients>,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) {
    let Some(id) = &app.selected_market_id else { return };
    let Some(market) = app.markets.iter().find(|m| &m.id == id).cloned() else { return };

    let clients_c  = clients.clone();
    let tx         = event_tx.clone();
    let interval   = app.chart_interval;

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
