//! Research ledger, signal history, and professional analytics.
//!
//! This module is deliberately local-file backed. It gives WhoIsSharp durable
//! process memory without introducing a database or exchange credentials.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::markets::{Market, Platform};
use crate::portfolio::{Portfolio, Side};
use crate::signals::Signal;

const MAX_SIGNAL_HISTORY: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ThesisStatus {
    Open,
    Closed,
    Invalidated,
}

impl Default for ThesisStatus {
    fn default() -> Self {
        ThesisStatus::Open
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thesis {
    pub id: String,
    #[serde(with = "chrono::serde::ts_seconds")]
    pub created_at: DateTime<Utc>,
    pub platform: Platform,
    pub market_id: String,
    pub title: String,
    pub side: Side,
    pub entry_price: f64,
    pub fair_value: f64,
    pub confidence: f64,
    pub thesis: String,
    pub catalyst: String,
    pub invalidation: String,
    #[serde(default)]
    pub status: ThesisStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalRecord {
    #[serde(with = "chrono::serde::ts_seconds")]
    pub observed_at: DateTime<Utc>,
    pub kind: String,
    pub stars: u8,
    pub title: String,
    pub platform: Platform,
    pub market_id: String,
    pub price: f64,
    pub gap: f64,
    pub ev_score: f64,
    pub action: String,
}

#[derive(Debug, Clone)]
pub struct SignalKindStats {
    pub kind: String,
    pub count: usize,
    pub avg_entry: f64,
    pub avg_latest: f64,
    pub avg_drift: f64,
    pub positive_rate: f64,
    pub avg_stars: f64,
}

#[derive(Debug, Clone)]
pub struct CalibrationBucket {
    pub label: String,
    pub count: usize,
    pub avg_entry: f64,
    pub avg_latest: f64,
    pub avg_drift: f64,
}

#[derive(Debug, Clone)]
pub struct ExposureCluster {
    pub topic: String,
    pub n_positions: usize,
    pub cost: f64,
    pub market_value: f64,
    pub pnl: f64,
}

fn whoissharp_dir() -> PathBuf {
    let mut p = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("."));
    p.push(".whoissharp");
    p
}

fn thesis_path() -> PathBuf {
    let mut p = whoissharp_dir();
    p.push("thesis_ledger.json");
    p
}

fn signal_history_path() -> PathBuf {
    let mut p = whoissharp_dir();
    p.push("signal_history.json");
    p
}

fn reports_dir() -> PathBuf {
    let mut p = whoissharp_dir();
    p.push("reports");
    p
}

fn stable_id(seed: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    Utc::now().timestamp_nanos_opt().unwrap_or(0).hash(&mut h);
    seed.hash(&mut h);
    format!("{:016x}", h.finish())
}

pub fn load_theses() -> Vec<Thesis> {
    let path = thesis_path();
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

pub fn save_theses(theses: &[Thesis]) -> Result<()> {
    let path = thesis_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create directory '{}'", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(theses)?)
        .with_context(|| format!("Cannot write thesis ledger to '{}'", path.display()))?;
    Ok(())
}

pub fn add_thesis(mut thesis: Thesis) -> Result<Thesis> {
    if thesis.id.is_empty() {
        thesis.id = stable_id(&thesis.market_id);
    }
    let mut theses = load_theses();
    theses.push(thesis.clone());
    save_theses(&theses)?;
    Ok(thesis)
}

pub fn thesis_from_market(m: &Market, note: impl Into<String>) -> Thesis {
    Thesis {
        id: stable_id(&m.id),
        created_at: Utc::now(),
        platform: m.platform.clone(),
        market_id: m.id.clone(),
        title: m.title.clone(),
        side: Side::Yes,
        entry_price: m.yes_price,
        fair_value: m.yes_price,
        confidence: 0.50,
        thesis: note.into(),
        catalyst: String::new(),
        invalidation: String::new(),
        status: ThesisStatus::Open,
        exit_price: None,
        outcome: None,
    }
}

pub fn load_signal_history() -> Vec<SignalRecord> {
    let path = signal_history_path();
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

fn save_signal_history(records: &[SignalRecord]) -> Result<()> {
    let path = signal_history_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create directory '{}'", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(records)?)
        .with_context(|| format!("Cannot write signal history to '{}'", path.display()))?;
    Ok(())
}

pub fn append_signal_snapshot(signals: &[Signal]) -> Result<usize> {
    if signals.is_empty() {
        return Ok(0);
    }
    let now = Utc::now();
    let mut records = load_signal_history();
    let mut added = 0usize;

    for s in signals {
        let duplicate = records.iter().rev().take(200).any(|r| {
            r.kind == s.kind.label()
                && r.market_id == s.id_a
                && (r.price - s.price_a).abs() < 0.0001
        });
        if duplicate {
            continue;
        }
        records.push(SignalRecord {
            observed_at: now,
            kind: s.kind.label().to_string(),
            stars: s.stars,
            title: s.title.clone(),
            platform: s.platform_a.clone(),
            market_id: s.id_a.clone(),
            price: s.price_a,
            gap: s.gap,
            ev_score: s.ev_score,
            action: s.action.clone(),
        });
        added += 1;
    }

    if records.len() > MAX_SIGNAL_HISTORY {
        let start = records.len() - MAX_SIGNAL_HISTORY;
        records = records.split_off(start);
    }
    save_signal_history(&records)?;
    Ok(added)
}

pub fn signal_backtest_stats(history: &[SignalRecord]) -> Vec<SignalKindStats> {
    let mut by_key: HashMap<(String, String), Vec<&SignalRecord>> = HashMap::new();
    for r in history {
        by_key
            .entry((r.kind.clone(), r.market_id.clone()))
            .or_default()
            .push(r);
    }

    #[derive(Default)]
    struct Acc {
        count: usize,
        entry: f64,
        latest: f64,
        drift: f64,
        positive: usize,
        stars: f64,
    }

    let mut by_kind: BTreeMap<String, Acc> = BTreeMap::new();
    for ((kind, _), mut rows) in by_key {
        rows.sort_by_key(|r| r.observed_at);
        let Some(first) = rows.first() else {
            continue;
        };
        let Some(last) = rows.last() else {
            continue;
        };
        let drift = last.price - first.price;
        let acc = by_kind.entry(kind).or_default();
        acc.count += 1;
        acc.entry += first.price;
        acc.latest += last.price;
        acc.drift += drift;
        acc.positive += usize::from(drift > 0.0);
        acc.stars += first.stars as f64;
    }

    by_kind
        .into_iter()
        .map(|(kind, acc)| {
            let n = acc.count.max(1) as f64;
            SignalKindStats {
                kind,
                count: acc.count,
                avg_entry: acc.entry / n,
                avg_latest: acc.latest / n,
                avg_drift: acc.drift / n,
                positive_rate: acc.positive as f64 / n,
                avg_stars: acc.stars / n,
            }
        })
        .collect()
}

pub fn calibration_buckets(history: &[SignalRecord]) -> Vec<CalibrationBucket> {
    let mut latest: HashMap<String, &SignalRecord> = HashMap::new();
    for r in history {
        latest
            .entry(r.market_id.clone())
            .and_modify(|cur| {
                if r.observed_at > cur.observed_at {
                    *cur = r;
                }
            })
            .or_insert(r);
    }

    #[derive(Default)]
    struct Acc {
        count: usize,
        entry: f64,
        latest: f64,
    }
    let mut buckets: BTreeMap<usize, Acc> = BTreeMap::new();
    for r in history {
        let Some(last) = latest.get(&r.market_id) else {
            continue;
        };
        let b = ((r.price * 10.0).floor() as usize).min(9);
        let acc = buckets.entry(b).or_default();
        acc.count += 1;
        acc.entry += r.price;
        acc.latest += last.price;
    }

    buckets
        .into_iter()
        .map(|(b, acc)| {
            let n = acc.count.max(1) as f64;
            let avg_entry = acc.entry / n;
            let avg_latest = acc.latest / n;
            CalibrationBucket {
                label: format!("{}-{}%", b * 10, b * 10 + 10),
                count: acc.count,
                avg_entry,
                avg_latest,
                avg_drift: avg_latest - avg_entry,
            }
        })
        .collect()
}

pub fn exposure_clusters(portfolio: &Portfolio, markets: &[Market]) -> Vec<ExposureCluster> {
    let mut category_by_id = HashMap::new();
    let mut title_by_id = HashMap::new();
    for m in markets {
        category_by_id.insert(
            m.id.clone(),
            m.category.clone().unwrap_or_else(|| infer_topic(&m.title)),
        );
        title_by_id.insert(m.id.clone(), m.title.clone());
    }

    let mut groups: BTreeMap<String, ExposureCluster> = BTreeMap::new();
    for p in &portfolio.positions {
        let topic = category_by_id
            .get(&p.market_id)
            .cloned()
            .unwrap_or_else(|| infer_topic(title_by_id.get(&p.market_id).unwrap_or(&p.title)));
        let entry = groups.entry(topic.clone()).or_insert(ExposureCluster {
            topic,
            n_positions: 0,
            cost: 0.0,
            market_value: 0.0,
            pnl: 0.0,
        });
        entry.n_positions += 1;
        entry.cost += p.cost();
        entry.market_value += p.market_value();
        entry.pnl += p.pnl();
    }
    let mut out: Vec<_> = groups.into_values().collect();
    out.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn infer_topic(title: &str) -> String {
    let t = title.to_lowercase();
    let topics: &[(&str, &[&str])] = &[
        (
            "politics",
            &[
                "election",
                "trump",
                "biden",
                "senate",
                "president",
                "governor",
            ],
        ),
        (
            "macro",
            &[
                "fed",
                "rate",
                "cpi",
                "inflation",
                "gdp",
                "jobs",
                "unemployment",
            ],
        ),
        (
            "crypto",
            &["bitcoin", "btc", "ethereum", "eth", "crypto", "solana"],
        ),
        (
            "sports",
            &["nba", "nfl", "mlb", "nhl", "cup", "championship"],
        ),
        (
            "weather",
            &["hurricane", "temperature", "weather", "storm", "rain"],
        ),
    ];
    for (topic, words) in topics {
        if words.iter().any(|w| t.contains(w)) {
            return topic.to_string();
        }
    }
    "uncategorized".to_string()
}

pub fn thesis_report() -> String {
    let theses = load_theses();
    if theses.is_empty() {
        return "Research ledger is empty. Use /thesis <note> on a selected market or the create_thesis tool.".to_string();
    }

    let mut lines = vec![format!("=== RESEARCH LEDGER ({} theses) ===", theses.len())];
    for t in theses.iter().rev().take(30) {
        lines.push(format!(
            "\n[{}] {} {} @ {:.1}¢  FV {:.1}¢  conf {:.0}%",
            &t.id[..t.id.len().min(8)],
            t.platform.label(),
            t.side.label(),
            t.entry_price * 100.0,
            t.fair_value * 100.0,
            t.confidence * 100.0,
        ));
        lines.push(format!("{} — {}", t.title, t.thesis));
        if !t.catalyst.is_empty() {
            lines.push(format!("Catalyst: {}", t.catalyst));
        }
        if !t.invalidation.is_empty() {
            lines.push(format!("Invalidation: {}", t.invalidation));
        }
    }
    lines.join("\n")
}

pub fn backtest_report() -> String {
    let history = load_signal_history();
    if history.is_empty() {
        return "No signal history yet. Leave the app running through refreshes or call get_signals to begin collecting snapshots.".to_string();
    }
    let stats = signal_backtest_stats(&history);
    let mut lines = vec![
        format!(
            "=== SIGNAL MARK-TO-MARKET BACKTEST ({} snapshots) ===",
            history.len()
        ),
        "Uses repeated observed marks, not final market resolutions.".to_string(),
        format!(
            "\n{:<8} {:>6} {:>6} {:>9} {:>9} {:>9} {:>8}",
            "Kind", "N", "Stars", "Entry", "Latest", "Drift", "Hit%"
        ),
        "-".repeat(70),
    ];
    for s in stats {
        lines.push(format!(
            "{:<8} {:>6} {:>6.1} {:>8.1}¢ {:>8.1}¢ {:>+8.1}¢ {:>7.0}%",
            s.kind,
            s.count,
            s.avg_stars,
            s.avg_entry * 100.0,
            s.avg_latest * 100.0,
            s.avg_drift * 100.0,
            s.positive_rate * 100.0,
        ));
    }
    lines.join("\n")
}

pub fn calibration_report() -> String {
    let history = load_signal_history();
    if history.is_empty() {
        return "No calibration history yet. Signal snapshots will accumulate automatically after market refreshes.".to_string();
    }
    let buckets = calibration_buckets(&history);
    let mut lines = vec![
        "=== PRICE CALIBRATION / FORWARD MARK DRIFT ===".to_string(),
        "Buckets compare observed signal prices with latest observed marks, not final resolutions."
            .to_string(),
        format!(
            "\n{:<10} {:>6} {:>9} {:>9} {:>9}",
            "Bucket", "N", "Entry", "Latest", "Drift"
        ),
        "-".repeat(52),
    ];
    for b in buckets {
        lines.push(format!(
            "{:<10} {:>6} {:>8.1}¢ {:>8.1}¢ {:>+8.1}¢",
            b.label,
            b.count,
            b.avg_entry * 100.0,
            b.avg_latest * 100.0,
            b.avg_drift * 100.0,
        ));
    }
    lines.join("\n")
}

pub fn professional_report(
    portfolio: &Portfolio,
    markets: &[Market],
    signals: &[Signal],
) -> String {
    let mut md = String::new();
    let _ = writeln!(md, "# WhoIsSharp Professional Book Review");
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "_Generated {}_",
        Utc::now().format("%Y-%m-%d %H:%M UTC")
    );
    let _ = writeln!(md);

    let theses = load_theses();
    let _ = writeln!(md, "## Process");
    let _ = writeln!(md);
    let _ = writeln!(
        md,
        "- Open theses: {}",
        theses
            .iter()
            .filter(|t| matches!(t.status, ThesisStatus::Open))
            .count()
    );
    let _ = writeln!(md, "- Signal snapshots: {}", load_signal_history().len());
    let _ = writeln!(md, "- Current active signals: {}", signals.len());
    let _ = writeln!(md);

    let _ = writeln!(md, "## Exposure Clusters");
    let clusters = exposure_clusters(portfolio, markets);
    if clusters.is_empty() {
        let _ = writeln!(md, "No portfolio positions are currently tracked.");
    } else {
        let _ = writeln!(md, "| Topic | Positions | Cost | Mark | P&L |");
        let _ = writeln!(md, "|---|---:|---:|---:|---:|");
        for c in clusters {
            let _ = writeln!(
                md,
                "| {} | {} | ${:.2} | ${:.2} | {:+.2} |",
                c.topic, c.n_positions, c.cost, c.market_value, c.pnl
            );
        }
    }
    let _ = writeln!(md);

    let _ = writeln!(md, "## Signal Backtest");
    for line in backtest_report().lines() {
        let _ = writeln!(md, "{}", line);
    }
    let _ = writeln!(md);

    let _ = writeln!(md, "## Calibration");
    for line in calibration_report().lines() {
        let _ = writeln!(md, "{}", line);
    }
    md
}

pub fn save_professional_report(
    portfolio: &Portfolio,
    markets: &[Market],
    signals: &[Signal],
) -> Result<PathBuf> {
    let dir = reports_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Cannot create reports directory '{}'", dir.display()))?;
    let filename = format!("book_review_{}.md", Utc::now().format("%Y%m%d_%H%M%S"));
    let path = dir.join(filename);
    std::fs::write(&path, professional_report(portfolio, markets, signals))
        .with_context(|| format!("Cannot write report to '{}'", path.display()))?;
    Ok(path)
}
