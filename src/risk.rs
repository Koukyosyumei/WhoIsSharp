//! Analytical portfolio risk — no simulation, no false precision.
//!
//! Prediction market positions are Bernoulli random variables. Their full
//! joint P&L distribution is characterised exactly (under the independence
//! assumption) using closed-form expressions.  We use the Central Limit
//! Theorem for P(profit > 0) when there are multiple positions.
//!
//! # P&L model
//!
//! The portfolio system stores `entry_price` as the price of the side bought:
//!   YES position at 68¢  → entry_price = 0.68
//!   NO  position at 32¢  → entry_price = 0.32  (= 1 - YES price)
//!
//! `mark_price` is similarly the price of the held side:
//!   YES position   → mark_price = current YES price
//!   NO  position   → mark_price = 1.0 - current YES price
//!
//! On resolution, each position either:
//!   Wins:  receives $1/share → net = (1 − entry_price) × shares
//!   Loses: receives $0/share → net = −entry_price × shares
//!
//! win_prob = mark_price  (the current price of your side IS the implied
//!            probability it resolves in your favour)

use std::collections::HashMap;

use crate::markets::Market;
use crate::portfolio::{Portfolio, Position};

// ─── Number of histogram buckets ─────────────────────────────────────────────

pub const HIST_BUCKETS: usize = 16;

// ─── Types ────────────────────────────────────────────────────────────────────

/// Risk decomposition for a single position.
#[derive(Debug, Clone)]
pub struct PositionRisk {
    pub title:        String,
    pub category:     String,
    /// P(position resolves in holder's favour) = mark_price.
    pub win_prob:     f64,
    /// P&L if the position wins (always positive).
    pub win_pnl:      f64,
    /// P&L if the position loses (always negative).
    pub lose_pnl:     f64,
    /// E[PnL] = win_prob × win_pnl + (1 − win_prob) × lose_pnl.
    pub expected_pnl: f64,
    /// Var[PnL] = (win_pnl − lose_pnl)² × win_prob × (1 − win_prob).
    pub variance:     f64,
}

/// "What if every position in this category resolves against me?"
#[derive(Debug, Clone)]
pub struct CategoryStress {
    pub category:      String,
    pub n_positions:   usize,
    /// Portfolio P&L under this stress: stressed positions all lose,
    /// all other positions held at their individual E[PnL].
    pub stressed_pnl:  f64,
    /// Fraction of total portfolio cost in this category (0.0–1.0).
    pub concentration: f64,
}

/// Full portfolio risk summary — computed analytically, always fast.
#[derive(Debug, Clone)]
pub struct PortfolioRisk {
    pub positions:       Vec<PositionRisk>,
    /// Σ E[PnL_i]  — exact under any correlation structure.
    pub expected_pnl:    f64,
    /// √ Σ Var[PnL_i]  — exact under independence.
    pub std_dev:         f64,
    /// P(total PnL > 0) via normal approximation Φ(μ/σ).
    pub prob_profit:     f64,
    /// All positions win.
    pub best_case:       f64,
    /// All positions lose.
    pub worst_case:      f64,
    /// Category-level stress tests, most severe first.
    pub category_stress: Vec<CategoryStress>,
    /// Histogram: (pnl_midpoint, normalised_height 0.0–1.0).
    /// Empty when there are fewer than 2 positions or σ ≈ 0.
    pub histogram:       Vec<(f64, f64)>,
}

impl PortfolioRisk {
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Compute the complete risk summary for a portfolio.
///
/// `markets` is used to look up each position's category label.
pub fn compute(portfolio: &Portfolio, markets: &[Market]) -> PortfolioRisk {
    // Build category map from live market data
    let cat_map: HashMap<String, String> = markets
        .iter()
        .filter_map(|m| m.category.as_ref().map(|c| (m.id.clone(), c.clone())))
        .collect();

    // ── Per-position risk ─────────────────────────────────────────────────────
    let positions: Vec<PositionRisk> = portfolio
        .positions
        .iter()
        .map(|p| position_risk(p, &cat_map))
        .collect();

    if positions.is_empty() {
        return PortfolioRisk {
            positions,
            expected_pnl:    0.0,
            std_dev:         0.0,
            prob_profit:     0.0,
            best_case:       0.0,
            worst_case:      0.0,
            category_stress: Vec::new(),
            histogram:       Vec::new(),
        };
    }

    // ── Portfolio-level stats ─────────────────────────────────────────────────
    let expected_pnl: f64 = positions.iter().map(|p| p.expected_pnl).sum();
    let variance:     f64 = positions.iter().map(|p| p.variance).sum();
    let std_dev             = variance.sqrt();
    let best_case:  f64 = positions.iter().map(|p| p.win_pnl).sum();
    let worst_case: f64 = positions.iter().map(|p| p.lose_pnl).sum();

    let prob_profit = if std_dev < 1e-9 {
        if expected_pnl > 0.0 { 1.0 } else if expected_pnl < 0.0 { 0.0 } else { 0.5 }
    } else {
        normal_cdf(expected_pnl / std_dev)
    };

    // ── Category stress tests ─────────────────────────────────────────────────
    let total_cost: f64 = portfolio.positions.iter().map(|p| p.cost()).sum::<f64>().max(1e-9);

    // Group position indices by category
    let mut cat_groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, pr) in positions.iter().enumerate() {
        cat_groups.entry(pr.category.clone()).or_default().push(i);
    }

    let mut category_stress: Vec<CategoryStress> = cat_groups
        .iter()
        .map(|(cat, idxs)| {
            // Replace each stressed position's contribution with its lose_pnl
            let stressed_delta: f64 = idxs.iter()
                .map(|&i| positions[i].lose_pnl - positions[i].expected_pnl)
                .sum();
            let stressed_pnl = expected_pnl + stressed_delta;

            let cat_cost: f64 = idxs.iter()
                .map(|&i| portfolio.positions[i].cost())
                .sum();

            CategoryStress {
                category:      cat.clone(),
                n_positions:   idxs.len(),
                stressed_pnl,
                concentration: cat_cost / total_cost,
            }
        })
        .collect();

    // Sort: most severe (lowest stressed_pnl) first
    category_stress.sort_by(|a, b| {
        a.stressed_pnl
            .partial_cmp(&b.stressed_pnl)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // ── Histogram ─────────────────────────────────────────────────────────────
    let histogram = if std_dev > 1e-9 && positions.len() >= 2 {
        build_histogram(worst_case, best_case, expected_pnl, std_dev)
    } else {
        Vec::new()
    };

    PortfolioRisk {
        positions,
        expected_pnl,
        std_dev,
        prob_profit,
        best_case,
        worst_case,
        category_stress,
        histogram,
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn position_risk(pos: &Position, cat_map: &HashMap<String, String>) -> PositionRisk {
    let mark = pos.mark_price.unwrap_or(pos.entry_price).clamp(0.001, 0.999);

    // mark_price already encodes P(win): for YES it IS the YES price;
    // for NO it is (1 - YES price) = the NO price.
    let win_prob = mark;

    // On resolution: win → receive $1/share; lose → receive $0/share.
    // entry_price is the cost per share of the held side.
    let win_pnl  = (1.0 - pos.entry_price) * pos.shares;
    let lose_pnl = -pos.entry_price * pos.shares;

    let expected_pnl = win_prob * win_pnl + (1.0 - win_prob) * lose_pnl;
    let variance     = (win_pnl - lose_pnl).powi(2) * win_prob * (1.0 - win_prob);

    let category = cat_map
        .get(&pos.market_id)
        .cloned()
        .unwrap_or_else(|| "Uncategorised".to_string());

    PositionRisk {
        title: pos.title.clone(),
        category,
        win_prob,
        win_pnl,
        lose_pnl,
        expected_pnl,
        variance,
    }
}

/// Normal CDF via Abramowitz & Stegun rational approximation (max error 7.5e-8).
fn normal_cdf(z: f64) -> f64 {
    const P: f64 = 0.2316419;
    const B: [f64; 5] = [0.319381530, -0.356563782, 1.781477937, -1.821255978, 1.330274429];
    let t = 1.0 / (1.0 + P * z.abs());
    let poly = t * (B[0] + t * (B[1] + t * (B[2] + t * (B[3] + t * B[4]))));
    let pdf  = (-0.5 * z * z).exp() / (2.0 * std::f64::consts::PI).sqrt();
    let cdf  = 1.0 - pdf * poly;
    if z >= 0.0 { cdf } else { 1.0 - cdf }
}

/// Build a normalised histogram over `HIST_BUCKETS` buckets from `lo` to `hi`
/// using the normal approximation N(mean, std²).
fn build_histogram(lo: f64, hi: f64, mean: f64, std: f64) -> Vec<(f64, f64)> {
    if (hi - lo).abs() < 1e-9 {
        return Vec::new();
    }
    let step = (hi - lo) / HIST_BUCKETS as f64;
    let buckets: Vec<(f64, f64)> = (0..HIST_BUCKETS)
        .map(|i| {
            let bucket_lo = lo + i as f64 * step;
            let bucket_hi = bucket_lo + step;
            let mid = (bucket_lo + bucket_hi) / 2.0;
            let prob = normal_cdf((bucket_hi - mean) / std) - normal_cdf((bucket_lo - mean) / std);
            (mid, prob.max(0.0))
        })
        .collect();

    let max_prob = buckets.iter().map(|(_, p)| *p).fold(0.0f64, f64::max);
    if max_prob < 1e-12 {
        return Vec::new();
    }
    buckets
        .into_iter()
        .map(|(mid, p)| (mid, p / max_prob))
        .collect()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markets::Platform;
    use crate::portfolio::{Portfolio, Position, Side};

    fn yes_pos(entry: f64, shares: f64, mark: f64) -> Position {
        let mut p = Position::new(
            Platform::Polymarket,
            "mkt1",
            "Test market",
            entry,
            shares,
            Side::Yes,
            None,
        );
        p.mark_price = Some(mark);
        p
    }

    fn no_pos(yes_price_at_entry: f64, shares: f64, current_yes: f64) -> Position {
        let mut p = Position::new(
            Platform::Polymarket,
            "mkt1",
            "Test market",
            1.0 - yes_price_at_entry, // NO cost per share
            shares,
            Side::No,
            None,
        );
        // mark_price for NO = 1.0 - current YES price
        p.mark_price = Some(1.0 - current_yes);
        p
    }

    #[test]
    fn fair_price_yes_position_has_zero_expected_pnl() {
        // Bought at the current market price → zero edge → E[PnL] = 0
        let mut p = Portfolio::default();
        p.add(yes_pos(0.68, 100.0, 0.68));
        let risk = compute(&p, &[]);
        assert!(risk.expected_pnl.abs() < 1e-6, "E[PnL] should be ~0 at fair price");
    }

    #[test]
    fn positive_edge_yes_position() {
        // Bought at 60¢ but true probability is 70% (mark = 0.70)
        let mut p = Portfolio::default();
        p.add(yes_pos(0.60, 100.0, 0.70));
        let risk = compute(&p, &[]);
        // E[PnL] = 0.70 × (1-0.60)×100 + 0.30 × (-0.60×100)
        //        = 0.70 × 40 − 0.30 × 60 = 28 − 18 = +10
        assert!((risk.expected_pnl - 10.0).abs() < 1e-6);
    }

    #[test]
    fn win_pnl_and_lose_pnl_correct_for_yes() {
        let mut p = Portfolio::default();
        p.add(yes_pos(0.68, 100.0, 0.68));
        let pr = &compute(&p, &[]).positions[0];
        // win  = (1 - 0.68) × 100 = 32
        // lose = -0.68 × 100 = -68
        assert!((pr.win_pnl  - 32.0).abs() < 1e-6);
        assert!((pr.lose_pnl + 68.0).abs() < 1e-6);
    }

    #[test]
    fn win_pnl_and_lose_pnl_correct_for_no() {
        // NO position when YES was 68¢ → paid 32¢/share
        let mut p = Portfolio::default();
        p.add(no_pos(0.68, 100.0, 0.68));
        let pr = &compute(&p, &[]).positions[0];
        // win  = (1 - 0.32) × 100 = 68  (NO wins → receive $1 on 32¢ cost)
        // lose = -0.32 × 100 = -32
        assert!((pr.win_pnl  - 68.0).abs() < 1e-6);
        assert!((pr.lose_pnl + 32.0).abs() < 1e-6);
    }

    #[test]
    fn std_dev_correct_for_single_position() {
        // Var = (win - lose)² × p × (1-p) = (32 + 68)² × 0.68 × 0.32 = 10000 × 0.2176 = 2176
        // σ = √2176 ≈ 46.65
        let mut p = Portfolio::default();
        p.add(yes_pos(0.68, 100.0, 0.68));
        let risk = compute(&p, &[]);
        let expected_var = 100.0f64.powi(2) * 0.68 * 0.32;
        assert!((risk.std_dev - expected_var.sqrt()).abs() < 1e-4);
    }

    #[test]
    fn best_and_worst_case_correct() {
        let mut p = Portfolio::default();
        p.add(yes_pos(0.60, 100.0, 0.60));  // win +40, lose -60
        p.add(yes_pos(0.70, 200.0, 0.70));  // win +60, lose -140
        let risk = compute(&p, &[]);
        assert!((risk.best_case  - 100.0).abs() < 1e-6);  // +40 + +60
        assert!((risk.worst_case + 200.0).abs() < 1e-6);  // -60 + -140
    }

    #[test]
    fn prob_profit_above_half_for_positive_edge() {
        let mut p = Portfolio::default();
        // Bought at 40¢, mark at 70% → strong edge
        p.add(yes_pos(0.40, 100.0, 0.70));
        p.add(yes_pos(0.40, 100.0, 0.70));
        let risk = compute(&p, &[]);
        assert!(risk.prob_profit > 0.5, "P(profit) should be above 50% with positive edge");
    }

    #[test]
    fn category_stress_all_lose_equals_worst_case_when_one_category() {
        let mut p = Portfolio::default();
        p.add(yes_pos(0.68, 100.0, 0.68));
        p.add(yes_pos(0.50, 50.0,  0.50));
        let risk = compute(&p, &[]);
        // Only one category (both "Uncategorised"), stress = worst case
        assert_eq!(risk.category_stress.len(), 1);
        assert!((risk.category_stress[0].stressed_pnl - risk.worst_case).abs() < 1e-6);
    }

    #[test]
    fn normal_cdf_at_zero_is_half() {
        assert!((normal_cdf(0.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn normal_cdf_monotone() {
        assert!(normal_cdf(-1.0) < normal_cdf(0.0));
        assert!(normal_cdf(0.0)  < normal_cdf(1.0));
        assert!(normal_cdf(1.0)  < normal_cdf(2.0));
    }

    #[test]
    fn histogram_has_correct_bucket_count() {
        let mut p = Portfolio::default();
        p.add(yes_pos(0.60, 100.0, 0.70));
        p.add(yes_pos(0.40, 80.0,  0.55));
        let risk = compute(&p, &[]);
        assert_eq!(risk.histogram.len(), HIST_BUCKETS);
    }

    #[test]
    fn histogram_heights_normalised_to_one() {
        let mut p = Portfolio::default();
        p.add(yes_pos(0.60, 100.0, 0.70));
        p.add(yes_pos(0.40, 80.0,  0.55));
        let risk = compute(&p, &[]);
        let max_h = risk.histogram.iter().map(|(_, h)| *h).fold(0.0f64, f64::max);
        assert!((max_h - 1.0).abs() < 1e-9, "histogram max height should be 1.0");
    }
}
