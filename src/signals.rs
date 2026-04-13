//! Signal computation engine.
//!
//! Pure, synchronous functions that operate on already-loaded market data.
//! No network calls — call after every `MarketsLoaded` event.
//!
//! Signal types (in priority order):
//!   Arb       — cross-platform price gap on the same event
//!   VolSpike  — volume anomaly vs market average
//!   NearFifty — highly uncertain market (price ≈ 50%)
//!   Thin      — very low liquidity, high spread risk

use std::collections::HashSet;

use crate::markets::{Market, Platform};

// ─── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SignalKind {
    Arb,
    VolSpike,
    NearFifty,
    Thin,
}

impl SignalKind {
    pub fn label(&self) -> &str {
        match self {
            SignalKind::Arb      => "ARB",
            SignalKind::VolSpike => "VOL",
            SignalKind::NearFifty => "50/50",
            SignalKind::Thin     => "THIN",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Signal {
    pub kind:       SignalKind,
    /// 1–3 stars: rough importance.
    pub stars:      u8,
    /// Human-readable title.
    pub title:      String,

    // Primary market (always present)
    pub platform_a: Platform,
    pub id_a:       String,
    pub price_a:    f64,   // YES price 0.0–1.0

    // Secondary market (arb only)
    pub platform_b: Option<Platform>,
    pub id_b:       Option<String>,
    pub price_b:    Option<f64>,

    /// For Arb: abs(price_a - price_b).
    /// For others: magnitude of the signal.
    pub gap:        f64,

    /// Rough expected-value score (0–100), used for sorting.
    pub ev_score:   f64,

    /// One-line actionable hint.
    pub action:     String,
}

impl Signal {
    /// Which market to navigate to on Enter.
    pub fn primary_id(&self) -> &str {
        &self.id_a
    }

    pub fn primary_platform(&self) -> &Platform {
        &self.platform_a
    }
}

// ─── Top-level entry point ────────────────────────────────────────────────────

/// Recompute all signals from the current market snapshot.
/// Returns at most 30 signals, sorted by stars desc → ev_score desc.
pub fn compute_signals(markets: &[Market]) -> Vec<Signal> {
    let mut signals = Vec::new();
    signals.extend(find_arb_pairs(markets));
    signals.extend(find_near_fifty(markets));
    signals.extend(find_vol_spikes(markets));
    signals.extend(find_thin_markets(markets));

    signals.sort_by(|a, b| {
        b.stars
            .cmp(&a.stars)
            .then_with(|| b.ev_score.partial_cmp(&a.ev_score).unwrap_or(std::cmp::Ordering::Equal))
    });
    signals.dedup_by_key(|s| s.id_a.clone());
    signals.truncate(30);
    signals
}

// ─── Arb detection ───────────────────────────────────────────────────────────

const ARB_MIN_GAP: f64 = 0.025; // 2.5 cents minimum gap to surface

fn find_arb_pairs(markets: &[Market]) -> Vec<Signal> {
    let pm: Vec<&Market> = markets
        .iter()
        .filter(|m| m.platform == Platform::Polymarket)
        .collect();
    let kl: Vec<&Market> = markets
        .iter()
        .filter(|m| m.platform == Platform::Kalshi)
        .collect();

    let mut signals = Vec::new();

    for a in &pm {
        for b in &kl {
            let sim = title_similarity(&a.title, &b.title);
            if sim < 0.38 {
                continue;
            }
            let gap = (a.yes_price - b.yes_price).abs();
            if gap < ARB_MIN_GAP {
                continue;
            }

            let (buy_plat, buy_id, buy_price, sell_plat, sell_price) = if a.yes_price > b.yes_price {
                // PM overpriced → buy YES on KL, sell/short on PM
                (&b.platform, &b.id, b.yes_price, &a.platform, a.yes_price)
            } else {
                // KL overpriced → buy YES on PM, sell/short on KL
                (&a.platform, &a.id, a.yes_price, &b.platform, b.yes_price)
            };

            // Liquidity-adjusted EV: gap * sqrt(min liquidity) for dollar sizing
            let min_liq = a.liquidity.or(a.volume)
                .unwrap_or(0.0)
                .min(b.liquidity.or(b.volume).unwrap_or(0.0));
            let ev_score = gap * 100.0 * (min_liq.max(1.0).ln() + 1.0);

            let stars = if gap >= 0.08 { 3 } else if gap >= 0.04 { 2 } else { 1 };

            signals.push(Signal {
                kind:       SignalKind::Arb,
                stars,
                title:      a.title.clone(),
                platform_a: buy_plat.clone(),
                id_a:       buy_id.clone(),
                price_a:    buy_price,
                platform_b: Some(sell_plat.clone()),
                id_b:       Some(if a.yes_price > b.yes_price { a.id.clone() } else { b.id.clone() }),
                price_b:    Some(sell_price),
                gap,
                ev_score,
                action: format!(
                    "BUY YES on {} @ {:.1}¢  │  SELL/NO on {} @ {:.1}¢  │  GAP {:.1}¢",
                    buy_plat.label(),  buy_price  * 100.0,
                    sell_plat.label(), sell_price * 100.0,
                    gap * 100.0,
                ),
            });
        }
    }

    signals
}

// ─── Near-50 (high uncertainty) ───────────────────────────────────────────────

const NEAR_FIFTY_RANGE: f64 = 0.06; // 44–56% band

fn find_near_fifty(markets: &[Market]) -> Vec<Signal> {
    markets
        .iter()
        .filter(|m| (m.yes_price - 0.5).abs() <= NEAR_FIFTY_RANGE)
        .filter(|m| m.volume.unwrap_or(0.0) > 10_000.0)
        .map(|m| {
            let dist = (m.yes_price - 0.5).abs();
            let ev_score = (1.0 - dist / 0.5) * 50.0 * (m.volume.unwrap_or(1.0).ln() + 1.0);
            let stars = if dist < 0.01 { 3 } else if dist < 0.03 { 2 } else { 1 };
            Signal {
                kind:       SignalKind::NearFifty,
                stars,
                title:      m.title.clone(),
                platform_a: m.platform.clone(),
                id_a:       m.id.clone(),
                price_a:    m.yes_price,
                platform_b: None,
                id_b:       None,
                price_b:    None,
                gap:        dist,
                ev_score,
                action: format!(
                    "Near coin-flip ({:.1}%) — large moves likely on new info",
                    m.yes_price * 100.0,
                ),
            }
        })
        .collect()
}

// ─── Volume spike ────────────────────────────────────────────────────────────

fn find_vol_spikes(markets: &[Market]) -> Vec<Signal> {
    let volumes: Vec<f64> = markets
        .iter()
        .filter_map(|m| m.volume)
        .collect();

    if volumes.is_empty() {
        return Vec::new();
    }

    let mean_vol = volumes.iter().sum::<f64>() / volumes.len() as f64;
    let spike_threshold = mean_vol * 3.0;

    markets
        .iter()
        .filter(|m| m.volume.unwrap_or(0.0) >= spike_threshold)
        .map(|m| {
            let vol = m.volume.unwrap_or(0.0);
            let ratio = vol / mean_vol.max(1.0);
            let stars = if ratio >= 10.0 { 3 } else if ratio >= 5.0 { 2 } else { 1 };
            Signal {
                kind:       SignalKind::VolSpike,
                stars,
                title:      m.title.clone(),
                platform_a: m.platform.clone(),
                id_a:       m.id.clone(),
                price_a:    m.yes_price,
                platform_b: None,
                id_b:       None,
                price_b:    None,
                gap:        ratio,
                ev_score:   ratio * 10.0,
                action: format!(
                    "Volume {:.0}× above avg — unusual activity at {:.1}%",
                    ratio,
                    m.yes_price * 100.0,
                ),
            }
        })
        .collect()
}

// ─── Thin / illiquid markets ──────────────────────────────────────────────────

fn find_thin_markets(markets: &[Market]) -> Vec<Signal> {
    markets
        .iter()
        .filter(|m| {
            let liq = m.liquidity.unwrap_or(0.0);
            liq < 10_000.0 && liq > 0.0
        })
        .filter(|m| m.yes_price > 0.05 && m.yes_price < 0.95)
        .map(|m| {
            let liq = m.liquidity.unwrap_or(0.0);
            Signal {
                kind:       SignalKind::Thin,
                stars:      1,
                title:      m.title.clone(),
                platform_a: m.platform.clone(),
                id_a:       m.id.clone(),
                price_a:    m.yes_price,
                platform_b: None,
                id_b:       None,
                price_b:    None,
                gap:        liq,
                ev_score:   5.0,
                action: format!(
                    "Low liquidity (${:.0}K) — spreads may be wide, size carefully",
                    liq / 1000.0,
                ),
            }
        })
        .take(5)
        .collect()
}

// ─── Title similarity ─────────────────────────────────────────────────────────

static STOP_WORDS: &[&str] = &[
    "the", "a", "an", "in", "on", "at", "by", "for", "of", "to", "and",
    "or", "will", "be", "is", "are", "was", "were", "2024", "2025", "2026",
    "this", "that", "have", "has", "not", "win", "wins", "lose", "happen",
];

fn normalize_words(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !STOP_WORDS.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Jaccard similarity on word sets, ignoring short/common words.
pub fn title_similarity(a: &str, b: &str) -> f64 {
    let wa = normalize_words(a);
    let wb = normalize_words(b);
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    let inter = wa.intersection(&wb).count();
    let union = wa.union(&wb).count();
    if union == 0 { 0.0 } else { inter as f64 / union as f64 }
}
