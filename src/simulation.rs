//! Sandboxed portfolio simulation.
//!
//! The LLM supplies a structured JSON spec. WhoIsSharp runs trusted Rust code:
//! no shell, no Python, no arbitrary file access.

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::markets::Market;
use crate::portfolio::{Portfolio, Position, Side};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationSpec {
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default = "default_trials")]
    pub trials: usize,
    #[serde(default)]
    pub shocks: Vec<ScenarioShock>,
    #[serde(default)]
    pub correlation_groups: Vec<CorrelationGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioShock {
    /// "all", "topic", "category", "market_id", or "title_contains".
    pub target_type: String,
    pub target: String,
    /// Shift to the YES probability, e.g. -0.12 means YES down 12 points.
    #[serde(default)]
    pub yes_price_shift: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationGroup {
    pub name: String,
    /// Targets can be topic/category labels, market IDs, or title substrings.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Latent-factor correlation, clamped to 0..0.95.
    #[serde(default = "default_rho")]
    pub rho: f64,
}

#[derive(Debug, Clone)]
pub struct SimulationResult {
    pub spec: SimulationSpec,
    pub positions: usize,
    pub expected_pnl: f64,
    pub median_pnl: f64,
    pub p05_pnl: f64,
    pub p01_pnl: f64,
    pub cvar_05: f64,
    pub cvar_01: f64,
    pub prob_loss: f64,
    pub worst_pnl: f64,
    pub best_pnl: f64,
    pub tail_drivers: Vec<TailDriver>,
}

#[derive(Debug, Clone)]
pub struct TailDriver {
    pub title: String,
    pub avg_tail_pnl: f64,
    pub cost: f64,
}

fn default_name() -> String { "Base Monte Carlo".to_string() }
fn default_trials() -> usize { 50_000 }
fn default_rho() -> f64 { 0.55 }

pub fn default_spec() -> SimulationSpec {
    SimulationSpec {
        name: default_name(),
        trials: default_trials(),
        shocks: Vec::new(),
        correlation_groups: Vec::new(),
    }
}

pub fn parse_spec_json(spec_json: Option<&str>) -> Result<SimulationSpec> {
    match spec_json {
        Some(s) if !s.trim().is_empty() => {
            let mut spec: SimulationSpec = serde_json::from_str(s)
                .map_err(|e| anyhow!("Invalid simulation spec JSON: {}", e))?;
            spec.trials = spec.trials.clamp(1_000, 200_000);
            Ok(spec)
        }
        _ => Ok(default_spec()),
    }
}

pub fn run(portfolio: &Portfolio, markets: &[Market], mut spec: SimulationSpec) -> Result<SimulationResult> {
    if portfolio.positions.is_empty() {
        return Err(anyhow!("Portfolio is empty."));
    }
    spec.trials = spec.trials.clamp(1_000, 200_000);

    let ctx = MarketContext::new(markets);
    let positions: Vec<SimPosition> = portfolio.positions.iter()
        .map(|p| SimPosition::from_position(p, &ctx, &spec))
        .collect();

    let mut rng = Lcg::new(0x9e37_79b9_7f4a_7c15 ^ positions.len() as u64 ^ spec.trials as u64);
    let mut trials: Vec<(f64, Vec<f64>)> = Vec::with_capacity(spec.trials);
    let group_members = build_group_members(&positions, &spec.correlation_groups);

    for _ in 0..spec.trials {
        let common: Vec<f64> = spec.correlation_groups.iter().map(|_| rng.normal()).collect();
        let mut pnl = 0.0;
        let mut contrib = Vec::with_capacity(positions.len());

        for (i, pos) in positions.iter().enumerate() {
            let win = if let Some(group_idx) = group_members[i] {
                let rho = spec.correlation_groups[group_idx].rho.clamp(0.0, 0.95);
                let latent = rho.sqrt() * common[group_idx] + (1.0 - rho).sqrt() * rng.normal();
                latent <= inv_norm_cdf(pos.win_prob)
            } else {
                rng.next_f64() <= pos.win_prob
            };
            let c = if win { pos.win_pnl } else { pos.lose_pnl };
            pnl += c;
            contrib.push(c);
        }
        trials.push((pnl, contrib));
    }

    trials.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let n = trials.len();
    let expected_pnl = trials.iter().map(|t| t.0).sum::<f64>() / n as f64;
    let median_pnl = percentile(&trials, 0.50);
    let p05_pnl = percentile(&trials, 0.05);
    let p01_pnl = percentile(&trials, 0.01);
    let cvar_05 = tail_mean(&trials, 0.05);
    let cvar_01 = tail_mean(&trials, 0.01);
    let prob_loss = trials.iter().filter(|t| t.0 < 0.0).count() as f64 / n as f64;
    let worst_pnl = trials.first().map(|t| t.0).unwrap_or(0.0);
    let best_pnl = trials.last().map(|t| t.0).unwrap_or(0.0);
    let tail_drivers = tail_drivers(&positions, &trials, 0.05);

    Ok(SimulationResult {
        spec,
        positions: positions.len(),
        expected_pnl,
        median_pnl,
        p05_pnl,
        p01_pnl,
        cvar_05,
        cvar_01,
        prob_loss,
        worst_pnl,
        best_pnl,
        tail_drivers,
    })
}

pub fn render_report(result: &SimulationResult) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "=== PORTFOLIO SIMULATION ===");
    let _ = writeln!(out, "Scenario: {}", result.spec.name);
    let _ = writeln!(out, "Trials: {}   Positions: {}", result.spec.trials, result.positions);
    if !result.spec.shocks.is_empty() {
        let _ = writeln!(out, "\nShocks:");
        for s in &result.spec.shocks {
            let _ = writeln!(out, "  - {}={}  YES shift {:+.1}pp",
                s.target_type, s.target, s.yes_price_shift * 100.0);
        }
    }
    if !result.spec.correlation_groups.is_empty() {
        let _ = writeln!(out, "\nCorrelation groups:");
        for g in &result.spec.correlation_groups {
            let _ = writeln!(out, "  - {}  rho {:.2}  targets: {}",
                g.name, g.rho.clamp(0.0, 0.95), g.targets.join(", "));
        }
    }
    let _ = writeln!(out, "\nDistribution:");
    let _ = writeln!(out, "  Expected P&L: {:+.2}", result.expected_pnl);
    let _ = writeln!(out, "  Median P&L:   {:+.2}", result.median_pnl);
    let _ = writeln!(out, "  5% worst:     {:+.2}", result.p05_pnl);
    let _ = writeln!(out, "  1% worst:     {:+.2}", result.p01_pnl);
    let _ = writeln!(out, "  CVaR 5%:      {:+.2}", result.cvar_05);
    let _ = writeln!(out, "  CVaR 1%:      {:+.2}", result.cvar_01);
    let _ = writeln!(out, "  P(loss):      {:.1}%", result.prob_loss * 100.0);
    let _ = writeln!(out, "  Worst / Best: {:+.2} / {:+.2}", result.worst_pnl, result.best_pnl);

    if !result.tail_drivers.is_empty() {
        let _ = writeln!(out, "\nMain 5% tail-loss drivers:");
        for d in &result.tail_drivers {
            let _ = writeln!(out, "  {:+8.2}  cost ${:>7.2}  {}", d.avg_tail_pnl, d.cost, trunc(&d.title, 58));
        }
    }
    out
}

struct MarketContext {
    yes_by_id: HashMap<String, f64>,
    category_by_id: HashMap<String, String>,
    title_by_id: HashMap<String, String>,
}

impl MarketContext {
    fn new(markets: &[Market]) -> Self {
        let mut yes_by_id = HashMap::new();
        let mut category_by_id = HashMap::new();
        let mut title_by_id = HashMap::new();
        for m in markets {
            yes_by_id.insert(m.id.clone(), m.yes_price);
            category_by_id.insert(m.id.clone(), m.category.clone().unwrap_or_else(|| infer_topic(&m.title)));
            title_by_id.insert(m.id.clone(), m.title.clone());
        }
        Self { yes_by_id, category_by_id, title_by_id }
    }
}

struct SimPosition {
    title: String,
    market_id: String,
    category: String,
    win_prob: f64,
    win_pnl: f64,
    lose_pnl: f64,
    cost: f64,
}

impl SimPosition {
    fn from_position(pos: &Position, ctx: &MarketContext, spec: &SimulationSpec) -> Self {
        let base_yes = ctx.yes_by_id.get(&pos.market_id).copied().unwrap_or_else(|| {
            match pos.side {
                Side::Yes => pos.mark_price.unwrap_or(pos.entry_price),
                Side::No => 1.0 - pos.mark_price.unwrap_or(pos.entry_price),
            }
        });
        let category = ctx.category_by_id.get(&pos.market_id)
            .cloned()
            .unwrap_or_else(|| infer_topic(&pos.title));
        let title = ctx.title_by_id.get(&pos.market_id).cloned().unwrap_or_else(|| pos.title.clone());
        let shocked_yes = apply_shocks(base_yes, &pos.market_id, &title, &category, &spec.shocks);
        let win_prob = match pos.side {
            Side::Yes => shocked_yes,
            Side::No => 1.0 - shocked_yes,
        }.clamp(0.001, 0.999);
        Self {
            title,
            market_id: pos.market_id.clone(),
            category,
            win_prob,
            win_pnl: (1.0 - pos.entry_price) * pos.shares,
            lose_pnl: -pos.entry_price * pos.shares,
            cost: pos.cost(),
        }
    }
}

fn apply_shocks(base_yes: f64, market_id: &str, title: &str, category: &str, shocks: &[ScenarioShock]) -> f64 {
    let mut yes = base_yes;
    let title_l = title.to_lowercase();
    for s in shocks {
        let target_l = s.target.to_lowercase();
        let matched = match s.target_type.as_str() {
            "all" => true,
            "topic" | "category" => category.eq_ignore_ascii_case(&s.target),
            "market_id" => market_id.eq_ignore_ascii_case(&s.target),
            "title_contains" => title_l.contains(&target_l),
            _ => false,
        };
        if matched {
            yes += s.yes_price_shift;
        }
    }
    yes.clamp(0.001, 0.999)
}

fn build_group_members(positions: &[SimPosition], groups: &[CorrelationGroup]) -> Vec<Option<usize>> {
    positions.iter().map(|p| {
        groups.iter().position(|g| {
            g.targets.iter().any(|t| {
                let tl = t.to_lowercase();
                p.market_id.eq_ignore_ascii_case(t)
                    || p.category.eq_ignore_ascii_case(t)
                    || p.title.to_lowercase().contains(&tl)
            })
        })
    }).collect()
}

fn percentile(trials: &[(f64, Vec<f64>)], q: f64) -> f64 {
    if trials.is_empty() { return 0.0; }
    let idx = ((trials.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    trials[idx].0
}

fn tail_mean(trials: &[(f64, Vec<f64>)], q: f64) -> f64 {
    if trials.is_empty() { return 0.0; }
    let n = ((trials.len() as f64 * q).ceil() as usize).max(1).min(trials.len());
    trials.iter().take(n).map(|t| t.0).sum::<f64>() / n as f64
}

fn tail_drivers(positions: &[SimPosition], trials: &[(f64, Vec<f64>)], q: f64) -> Vec<TailDriver> {
    if positions.is_empty() || trials.is_empty() { return Vec::new(); }
    let n = ((trials.len() as f64 * q).ceil() as usize).max(1).min(trials.len());
    let mut drivers: Vec<TailDriver> = positions.iter().enumerate().map(|(i, p)| {
        let avg_tail_pnl = trials.iter().take(n).map(|t| t.1[i]).sum::<f64>() / n as f64;
        TailDriver { title: p.title.clone(), avg_tail_pnl, cost: p.cost }
    }).collect();
    drivers.sort_by(|a, b| a.avg_tail_pnl.partial_cmp(&b.avg_tail_pnl).unwrap_or(std::cmp::Ordering::Equal));
    drivers.truncate(8);
    drivers
}

struct Lcg { state: u64 }

impl Lcg {
    fn new(seed: u64) -> Self { Self { state: seed } }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.state
    }

    fn next_f64(&mut self) -> f64 {
        let x = self.next_u64() >> 11;
        (x as f64) / ((1u64 << 53) as f64)
    }

    fn normal(&mut self) -> f64 {
        let u1 = self.next_f64().clamp(1e-12, 1.0);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

fn inv_norm_cdf(p: f64) -> f64 {
    // Peter J. Acklam's rational approximation.
    let p = p.clamp(1e-9, 1.0 - 1e-9);
    const A: [f64; 6] = [-39.69683028665376, 220.9460984245205, -275.9285104469687, 138.3577518672690, -30.66479806614716, 2.506628277459239];
    const B: [f64; 5] = [-54.47609879822406, 161.5858368580409, -155.6989798598866, 66.80131188771972, -13.28068155288572];
    const C: [f64; 6] = [-0.007784894002430293, -0.3223964580411365, -2.400758277161838, -2.549732539343734, 4.374664141464968, 2.938163982698783];
    const D: [f64; 4] = [0.007784695709041462, 0.3224671290700398, 2.445134137142996, 3.754408661907416];
    const P_LOW: f64 = 0.02425;
    const P_HIGH: f64 = 1.0 - P_LOW;

    if p < P_LOW {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= P_HIGH {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

fn infer_topic(title: &str) -> String {
    let t = title.to_lowercase();
    let topics: &[(&str, &[&str])] = &[
        ("politics", &["election", "trump", "biden", "senate", "president", "governor"]),
        ("macro", &["fed", "rate", "cpi", "inflation", "gdp", "jobs", "unemployment"]),
        ("crypto", &["bitcoin", "btc", "ethereum", "eth", "crypto", "solana"]),
        ("sports", &["nba", "nfl", "mlb", "nhl", "cup", "championship"]),
        ("weather", &["hurricane", "temperature", "weather", "storm", "rain"]),
    ];
    for (topic, words) in topics {
        if words.iter().any(|w| t.contains(w)) {
            return topic.to_string();
        }
    }
    "uncategorized".to_string()
}

fn trunc(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let end = s.char_indices().nth(max_chars.saturating_sub(1)).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markets::Platform;

    fn pos(title: &str, price: f64, shares: f64) -> Position {
        Position::new(Platform::Kalshi, title, title, price, shares, Side::Yes, None)
    }

    #[test]
    fn parse_empty_spec_uses_default() {
        let spec = parse_spec_json(None).unwrap();
        assert_eq!(spec.trials, 50_000);
    }

    #[test]
    fn shock_changes_tail_result() {
        let mut pf = Portfolio::default();
        pf.add(pos("Fed rate cut", 0.50, 100.0));
        let base = run(&pf, &[], default_spec()).unwrap();
        let spec = SimulationSpec {
            name: "hot CPI".to_string(),
            trials: 5_000,
            shocks: vec![ScenarioShock {
                target_type: "topic".to_string(),
                target: "macro".to_string(),
                yes_price_shift: -0.20,
            }],
            correlation_groups: Vec::new(),
        };
        let shocked = run(&pf, &[], spec).unwrap();
        assert!(shocked.expected_pnl < base.expected_pnl);
    }
}
