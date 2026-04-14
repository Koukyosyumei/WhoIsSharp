//! Cross-platform market pair matching and arbitrage analysis.
//!
//! Two matching strategies:
//!   - Jaccard (always available): word-set overlap on market titles.
//!   - LLM (when a backend is configured): semantic similarity + resolution-risk
//!     assessment, with Jaccard as a pre-filter to limit the number of API calls.

use std::sync::Arc;

use crate::llm::{LlmBackend, LlmMessage};
use crate::markets::{Market, Platform};
use crate::signals::title_similarity;

// ─── Fee constants ────────────────────────────────────────────────────────────

/// Estimated taker fee as a fraction of notional on Polymarket CLOB.
pub const PM_TAKER_FEE: f64 = 0.02;

/// Estimated taker fee as a fraction of notional on Kalshi.
pub const KL_TAKER_FEE: f64 = 0.02;

/// Low Jaccard threshold used only as a pre-filter before LLM matching.
const JACCARD_PREFILTER: f64 = 0.15;

/// Jaccard threshold used when no LLM is available.
const JACCARD_ACCEPT: f64 = 0.35;

/// Maximum candidate pairs submitted per LLM call (keep prompt manageable).
const LLM_BATCH_LIMIT: usize = 25;

// ─── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchType {
    /// Same event, same resolution criteria — true arb if price gap exists.
    Identical,
    /// Same event, minor wording or timing differences — likely arb but verify.
    NearIdentical,
    /// Related events that could still resolve differently.
    Related,
    /// Not meaningfully related (should be filtered out before storage).
    Different,
}

impl MatchType {
    pub fn label(&self) -> &'static str {
        match self {
            MatchType::Identical     => "IDENTICAL",
            MatchType::NearIdentical => "NEAR-IDENTICAL",
            MatchType::Related       => "RELATED",
            MatchType::Different     => "DIFFERENT",
        }
    }
    pub fn is_arb_candidate(&self) -> bool {
        matches!(self, MatchType::Identical | MatchType::NearIdentical)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionRisk {
    /// Very likely to resolve the same way.
    Low,
    /// Some ambiguity — different wording, timing, or source definition.
    Medium,
    /// Significant risk of different resolution despite similar titles.
    High,
}

impl ResolutionRisk {
    pub fn label(&self) -> &'static str {
        match self {
            ResolutionRisk::Low    => "LOW",
            ResolutionRisk::Medium => "MEDIUM",
            ResolutionRisk::High   => "HIGH",
        }
    }
}

/// A matched pair of markets — one from Polymarket, one from Kalshi.
#[derive(Debug, Clone)]
pub struct MarketPair {
    pub pm_market:      Market,
    pub kl_market:      Market,

    /// 0.0–1.0: Jaccard score or LLM confidence.
    pub similarity:     f64,
    pub match_type:     MatchType,
    pub res_risk:       ResolutionRisk,
    /// One-sentence explanation of resolution-criteria differences.
    pub res_risk_note:  String,

    /// |pm_yes_price - kl_yes_price|
    pub gross_gap:      f64,
    /// gross_gap minus estimated taker fees on both legs. Negative = not profitable.
    pub net_gap:        f64,

    /// Platform on which to buy YES (cheaper side).
    pub buy_yes_on:     Platform,
    /// Platform on which to buy NO (more expensive YES side).
    pub buy_no_on:      Platform,

    /// Estimated capturable dollar profit at max liquidity.
    /// = max(0, net_gap) * min(pm_liq, kl_liq)
    pub capturable_usd: f64,

    /// 0–3 stars based on net_gap magnitude.
    pub stars:          u8,

    /// True when matched by LLM, false when matched by Jaccard heuristic.
    pub llm_matched:    bool,
}

impl MarketPair {
    /// Short direction label for display.
    pub fn direction_label(&self) -> String {
        let yes_tag = match self.buy_yes_on {
            Platform::Polymarket => "PM",
            Platform::Kalshi     => "KL",
        };
        let no_tag = match self.buy_no_on {
            Platform::Polymarket => "PM",
            Platform::Kalshi     => "KL",
        };
        format!("BUY YES@{}  /  NO@{}", yes_tag, no_tag)
    }
}

// ─── Net-gap math ─────────────────────────────────────────────────────────────
//
// Arb strategy when pm_price > kl_price:
//   Buy YES on KL at kl_price, Buy NO on PM at (1 - pm_price)
//   Total outlay = kl_price + (1 - pm_price) = 1 - gap
//   Guaranteed payout = $1
//   Gross profit per dollar = gap
//
// Transaction costs (taker fees charged on the notional of each leg):
//   PM fee = PM_TAKER_FEE * (1 - pm_price)   [buying NO on PM]
//   KL fee = KL_TAKER_FEE * kl_price          [buying YES on KL]
//
// Net profit = gap - PM_fee - KL_fee

fn compute_net_gap(pm_price: f64, kl_price: f64) -> (f64, f64, Platform, Platform) {
    let gross_gap = (pm_price - kl_price).abs();
    let (buy_yes_on, buy_no_on, yes_price, no_price) = if pm_price > kl_price {
        // PM overpriced → buy YES on KL, buy NO on PM
        (Platform::Kalshi, Platform::Polymarket, kl_price, 1.0 - pm_price)
    } else {
        // KL overpriced → buy YES on PM, buy NO on KL
        (Platform::Polymarket, Platform::Kalshi, pm_price, 1.0 - kl_price)
    };
    let fee_yes = match buy_yes_on {
        Platform::Polymarket => PM_TAKER_FEE * yes_price,
        Platform::Kalshi     => KL_TAKER_FEE * yes_price,
    };
    let fee_no = match buy_no_on {
        Platform::Polymarket => PM_TAKER_FEE * no_price,
        Platform::Kalshi     => KL_TAKER_FEE * no_price,
    };
    let net_gap = gross_gap - fee_yes - fee_no;
    (gross_gap, net_gap, buy_yes_on, buy_no_on)
}

fn make_pair(
    pm: Market,
    kl: Market,
    similarity: f64,
    llm_matched: bool,
    match_type: MatchType,
    res_risk: ResolutionRisk,
    res_risk_note: String,
) -> MarketPair {
    let (gross_gap, net_gap, buy_yes_on, buy_no_on) =
        compute_net_gap(pm.yes_price, kl.yes_price);

    let min_liq = pm.liquidity.unwrap_or(0.0).min(kl.liquidity.unwrap_or(0.0));
    let capturable_usd = net_gap.max(0.0) * min_liq;

    let stars = if net_gap >= 0.05 { 3 }
                else if net_gap >= 0.02 { 2 }
                else if net_gap > 0.0  { 1 }
                else { 0 };

    MarketPair {
        pm_market: pm,
        kl_market: kl,
        similarity,
        match_type,
        res_risk,
        res_risk_note,
        gross_gap,
        net_gap,
        buy_yes_on,
        buy_no_on,
        capturable_usd,
        stars,
        llm_matched,
    }
}

// ─── Jaccard matching ─────────────────────────────────────────────────────────

/// Fast local matching using Jaccard word-set similarity.
/// Always available, no network call needed.
///
/// `accept_threshold` overrides `JACCARD_ACCEPT`.  Pass `None` to use the
/// compiled-in default (0.35).
pub fn jaccard_pairs(markets: &[Market], accept_threshold: Option<f64>) -> Vec<MarketPair> {
    let threshold = accept_threshold.unwrap_or(JACCARD_ACCEPT).clamp(0.05, 0.95);
    let pm: Vec<&Market> = markets
        .iter()
        .filter(|m| m.platform == Platform::Polymarket)
        .collect();
    let kl: Vec<&Market> = markets
        .iter()
        .filter(|m| m.platform == Platform::Kalshi)
        .collect();

    let mut pairs: Vec<MarketPair> = Vec::new();
    for a in &pm {
        for b in &kl {
            let sim = title_similarity(&a.title, &b.title);
            if sim < threshold {
                continue;
            }
            pairs.push(make_pair(
                (*a).clone(),
                (*b).clone(),
                sim,
                false,
                MatchType::NearIdentical,
                ResolutionRisk::Medium,
                "Matched by keyword similarity — verify resolution criteria manually.".to_string(),
            ));
        }
    }
    sort_pairs(&mut pairs);
    pairs
}

// ─── LLM matching ─────────────────────────────────────────────────────────────

/// LLM-powered pair matching. Pre-filters candidates with a low Jaccard threshold,
/// then asks the LLM for semantic match type and resolution risk.
/// Falls back to `jaccard_pairs` on any error.
///
/// `accept_threshold` sets the Jaccard pre-filter floor (overrides
/// `JACCARD_PREFILTER`).  Lowering it widens the candidate set sent to the LLM.
pub async fn llm_match_pairs(
    markets:          &[Market],
    backend:          &Arc<dyn LlmBackend>,
    accept_threshold: Option<f64>,
) -> Vec<MarketPair> {
    let prefilter = accept_threshold
        .map(|t| (t * 0.5).max(JACCARD_PREFILTER))  // half of accept, but never below compiled default
        .unwrap_or(JACCARD_PREFILTER)
        .clamp(0.05, 0.95);

    let pm: Vec<&Market> = markets.iter().filter(|m| m.platform == Platform::Polymarket).collect();
    let kl: Vec<&Market> = markets.iter().filter(|m| m.platform == Platform::Kalshi).collect();

    // Pre-filter at a lower threshold than Jaccard-only mode
    let mut candidates: Vec<(&Market, &Market, f64)> = Vec::new();
    for a in &pm {
        for b in &kl {
            let sim = title_similarity(&a.title, &b.title);
            if sim >= prefilter {
                candidates.push((*a, *b, sim));
            }
        }
    }

    if candidates.is_empty() {
        return Vec::new();
    }

    // Sort by descending Jaccard so best candidates come first when we cap
    candidates.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(std::cmp::Ordering::Equal));
    let candidates = &candidates[..candidates.len().min(LLM_BATCH_LIMIT)];

    // Build prompt
    let pairs_text: String = candidates
        .iter()
        .enumerate()
        .map(|(i, (pm_m, kl_m, _))| {
            let pm_desc = pm_m.description.as_deref()
                .map(|d| format!(" [{}]", &d[..d.len().min(120)]))
                .unwrap_or_default();
            let kl_desc = kl_m.description.as_deref()
                .map(|d| format!(" [{}]", &d[..d.len().min(120)]))
                .unwrap_or_default();
            format!(
                "[{}] PM: \"{}\"{} | KL: \"{}\"{}",
                i, pm_m.title, pm_desc, kl_m.title, kl_desc,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "You are a prediction-market analyst assessing cross-platform market pairs.\n\
         For each pair, return a JSON object with EXACTLY these fields:\n\
           match_type   : \"identical\" | \"near_identical\" | \"related\" | \"different\"\n\
           res_risk     : \"low\" | \"medium\" | \"high\"\n\
           res_risk_note: one sentence on resolution differences, or \"Same criteria.\" if identical\n\
           confidence   : float 0.0–1.0\n\n\
         Definitions:\n\
           identical      — same underlying event AND same resolution criteria (true arb if price gap)\n\
           near_identical — same event, minor wording/timing/source differences (likely arb, check criteria)\n\
           related        — related events that could resolve differently\n\
           different      — not meaningfully related\n\n\
         Resolution risk:\n\
           low    — almost certain to resolve identically\n\
           medium — some ambiguity in criteria or timing\n\
           high   — meaningful risk of different resolution\n\n\
         Respond ONLY with a valid JSON array in the same order as the pairs below.\n\
         Do not include markdown, prose, or any other text.\n\n\
         Pairs to assess:\n{}",
        pairs_text
    );

    let history = vec![LlmMessage::user_text(prompt)];

    let response = match backend
        .generate(
            "You are a JSON-only prediction market analyst. Output only valid JSON arrays.",
            &history,
            &[],
        )
        .await
    {
        Ok(r) => r,
        Err(_) => return jaccard_pairs(markets, accept_threshold),
    };

    let text: String = response
        .texts()
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join("");

    let mut pairs = parse_llm_response(&text, candidates);
    if pairs.is_empty() {
        // LLM parse failed — fall back
        return jaccard_pairs(markets, accept_threshold);
    }

    sort_pairs(&mut pairs);
    pairs
}

fn parse_llm_response(
    text: &str,
    candidates: &[(&Market, &Market, f64)],
) -> Vec<MarketPair> {
    // Extract JSON array from the response text (may have surrounding whitespace)
    let start = text.find('[').unwrap_or(0);
    let end = text.rfind(']').map(|i| i + 1).unwrap_or(text.len());
    if start >= end { return Vec::new(); }
    let json_str = &text[start..end];

    let parsed: Vec<serde_json::Value> = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut pairs = Vec::new();
    for (i, (pm, kl, jaccard_sim)) in candidates.iter().enumerate() {
        let obj = match parsed.get(i) {
            Some(v) => v,
            None    => continue,
        };

        let match_type = match obj["match_type"].as_str().unwrap_or("different") {
            "identical"      => MatchType::Identical,
            "near_identical" => MatchType::NearIdentical,
            "related"        => MatchType::Related,
            _                => MatchType::Different,
        };
        if match_type == MatchType::Different {
            continue; // filter out unrelated pairs
        }

        let res_risk = match obj["res_risk"].as_str().unwrap_or("medium") {
            "low"  => ResolutionRisk::Low,
            "high" => ResolutionRisk::High,
            _      => ResolutionRisk::Medium,
        };
        let res_risk_note = obj["res_risk_note"]
            .as_str()
            .unwrap_or("No note provided.")
            .to_string();
        let confidence = obj["confidence"].as_f64().unwrap_or(*jaccard_sim);

        pairs.push(make_pair(
            (*pm).clone(),
            (*kl).clone(),
            confidence,
            true,
            match_type,
            res_risk,
            res_risk_note,
        ));
    }
    pairs
}

fn sort_pairs(pairs: &mut Vec<MarketPair>) {
    pairs.sort_by(|a, b| {
        // Primary: descending net_gap (most profitable first)
        b.net_gap
            .partial_cmp(&a.net_gap)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Secondary: better match type first
            .then_with(|| {
                let rank = |m: &MatchType| match m {
                    MatchType::Identical     => 0,
                    MatchType::NearIdentical => 1,
                    MatchType::Related       => 2,
                    MatchType::Different     => 3,
                };
                rank(&a.match_type).cmp(&rank(&b.match_type))
            })
    });
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markets::Market;

    fn mkt(platform: Platform, id: &str, title: &str, yes: f64, liq: f64) -> Market {
        Market {
            id:           id.to_string(),
            platform,
            title:        title.to_string(),
            description:  None,
            yes_price:    yes,
            no_price:     1.0 - yes,
            volume:       Some(liq * 3.0),
            liquidity:    Some(liq),
            end_date:     None,
            category:     None,
            status:       "open".to_string(),
            token_id:     None,
            event_ticker: None,
        }
    }

    #[test]
    fn net_gap_less_than_gross() {
        let (gross, net, _, _) = compute_net_gap(0.72, 0.68);
        assert!((gross - 0.04).abs() < 1e-9);
        // net must be less than gross (fees deducted)
        assert!(net < gross);
        assert!(net > 0.0); // still positive arb
    }

    #[test]
    fn net_gap_negative_for_small_gap() {
        // 0.5pp gap — far too small to be profitable after fees
        let (_, net, _, _) = compute_net_gap(0.505, 0.50);
        assert!(net < 0.0);
    }

    #[test]
    fn buy_direction_correct() {
        // PM 72%, KL 68% → buy YES on KL (cheaper), buy NO on PM
        let (_, _, buy_yes, buy_no) = compute_net_gap(0.72, 0.68);
        assert_eq!(buy_yes, Platform::Kalshi);
        assert_eq!(buy_no,  Platform::Polymarket);
    }

    #[test]
    fn jaccard_pairs_finds_similar_markets() {
        let markets = vec![
            mkt(Platform::Polymarket, "pm1", "Fed rate cut September 2024", 0.70, 100_000.0),
            mkt(Platform::Kalshi,     "kl1", "Fed September rate cut 2024", 0.62, 80_000.0),
        ];
        let pairs = jaccard_pairs(&markets, None);
        assert!(!pairs.is_empty(), "should find at least one pair");
        let p = &pairs[0];
        assert!(p.gross_gap > 0.0);
        assert!(p.stars > 0);
    }

    #[test]
    fn jaccard_pairs_no_match_for_unrelated() {
        let markets = vec![
            mkt(Platform::Polymarket, "pm1", "Super Bowl winner 2025", 0.60, 50_000.0),
            mkt(Platform::Kalshi,     "kl1", "Fed rate decision March", 0.70, 50_000.0),
        ];
        let pairs = jaccard_pairs(&markets, None);
        assert!(pairs.is_empty(), "unrelated markets should not match");
    }

    #[test]
    fn stars_three_for_large_net_gap() {
        // Force a large gap to test star rating
        let pm = mkt(Platform::Polymarket, "pm1", "Election winner candidate", 0.80, 100_000.0);
        let kl = mkt(Platform::Kalshi,     "kl1", "Election winner candidate", 0.60, 100_000.0);
        let pair = make_pair(pm, kl, 1.0, false,
            MatchType::Identical, ResolutionRisk::Low, "Same.".to_string());
        // gross_gap = 0.20, net ~ 0.20 - fees ≈ 0.18
        assert_eq!(pair.stars, 3);
    }

    #[test]
    fn capturable_usd_positive_when_net_gap_positive() {
        let pm = mkt(Platform::Polymarket, "pm1", "same event title here", 0.75, 200_000.0);
        let kl = mkt(Platform::Kalshi,     "kl1", "same event title here", 0.65, 100_000.0);
        let pair = make_pair(pm, kl, 1.0, false,
            MatchType::Identical, ResolutionRisk::Low, "Same.".to_string());
        assert!(pair.capturable_usd > 0.0);
    }

    #[test]
    fn parse_llm_response_filters_different() {
        let candidates_pm = mkt(Platform::Polymarket, "pm1", "title a", 0.70, 1000.0);
        let candidates_kl = mkt(Platform::Kalshi,     "kl1", "title a", 0.65, 1000.0);
        let candidates = vec![(&candidates_pm, &candidates_kl, 0.8f64)];

        let json = r#"[{"match_type":"different","res_risk":"low","res_risk_note":"Not related.","confidence":0.1}]"#;
        let result = parse_llm_response(json, &candidates);
        assert!(result.is_empty(), "Different match type should be filtered out");
    }

    #[test]
    fn parse_llm_response_keeps_identical() {
        let candidates_pm = mkt(Platform::Polymarket, "pm1", "title b", 0.72, 1000.0);
        let candidates_kl = mkt(Platform::Kalshi,     "kl1", "title b", 0.68, 1000.0);
        let candidates = vec![(&candidates_pm, &candidates_kl, 0.9f64)];

        let json = r#"[{"match_type":"identical","res_risk":"low","res_risk_note":"Same criteria.","confidence":0.95}]"#;
        let result = parse_llm_response(json, &candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].match_type, MatchType::Identical);
        assert_eq!(result[0].res_risk,   ResolutionRisk::Low);
        assert!((result[0].similarity - 0.95).abs() < 1e-9);
    }
}
