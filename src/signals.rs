//! Signal computation engine.
//!
//! Pure, synchronous functions that operate on already-loaded market data.
//! No network calls — call after every `MarketsLoaded` event.
//!
//! Signal types (in priority order):
//!   Arb           — cross-platform price gap on the same event
//!   InsiderAlert  — suspicious vol/liquidity pattern suggesting informed flow
//!   VolSpike      — volume anomaly vs market average
//!   NearFifty     — highly uncertain market (price ≈ 50%)
//!   Thin          — very low liquidity, high spread risk

use std::collections::HashSet;

use crate::markets::{Market, Platform};

// ─── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SignalKind {
    Arb,
    InsiderAlert,
    VolSpike,
    NearFifty,
    Thin,
    /// Price moved sharply in a short window — possible catalyst or news.
    Momentum,
}

impl SignalKind {
    pub fn label(&self) -> &str {
        match self {
            SignalKind::Arb          => "ARB",
            SignalKind::InsiderAlert => "INSDR",
            SignalKind::VolSpike     => "VOL",
            SignalKind::NearFifty    => "50/50",
            SignalKind::Thin         => "THIN",
            SignalKind::Momentum     => "MOMT",
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
///
/// `prev_prices`: previous YES prices keyed by market ID — used for momentum
/// detection.  Pass an empty map on the first call.
///
/// `dismissed`: set of signal market IDs the user has explicitly dismissed —
/// filtered out of results.
///
/// Returns at most 30 signals, sorted by stars desc → ev_score desc.
pub fn compute_signals(
    markets:   &[Market],
    prev_prices: &std::collections::HashMap<String, f64>,
    dismissed:   &std::collections::HashSet<String>,
) -> Vec<Signal> {
    let mut signals = Vec::new();
    signals.extend(find_arb_pairs(markets));
    signals.extend(find_insider_alerts(markets));
    signals.extend(find_near_fifty(markets));
    signals.extend(find_vol_spikes(markets));
    signals.extend(find_thin_markets(markets));
    signals.extend(find_momentum(markets, prev_prices));

    signals.sort_by(|a, b| {
        b.stars
            .cmp(&a.stars)
            .then_with(|| b.ev_score.partial_cmp(&a.ev_score).unwrap_or(std::cmp::Ordering::Equal))
    });
    signals.dedup_by_key(|s| s.id_a.clone());
    signals.retain(|s| !dismissed.contains(&s.id_a));
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

// ─── Insider-alert detection ──────────────────────────────────────────────────
//
// Heuristic: a market with a strongly directional price (>75% or <25%) that is
// consuming far more volume than its liquidity pool suggests may reflect informed
// flow — insiders buying before news drops.  We use:
//
//   vol_liq_ratio = volume / max(liquidity, 1.0)
//
// A healthy market has vol_liq_ratio of 1–5×.  Ratios above INSIDER_VOL_LIQ_RATIO
// at an extreme price level (>75% YES or <25% YES) are flagged.

const INSIDER_VOL_LIQ_RATIO: f64 = 15.0; // volume ≥ 15× liquidity pool
const INSIDER_PRICE_EXTREME: f64 = 0.25;  // flag when price < 25% or > 75%

fn find_insider_alerts(markets: &[Market]) -> Vec<Signal> {
    markets
        .iter()
        .filter(|m| {
            let vol = m.volume.unwrap_or(0.0);
            let liq = m.liquidity.unwrap_or(0.0);
            // Need real volume and liquidity to compute the ratio
            if vol < 1_000.0 || liq < 1.0 {
                return false;
            }
            let ratio = vol / liq;
            let extreme = m.yes_price > (1.0 - INSIDER_PRICE_EXTREME)
                || m.yes_price < INSIDER_PRICE_EXTREME;
            ratio >= INSIDER_VOL_LIQ_RATIO && extreme
        })
        .map(|m| {
            let vol  = m.volume.unwrap_or(0.0);
            let liq  = m.liquidity.unwrap_or(1.0);
            let ratio = vol / liq;

            // Higher ratio + more extreme price = more suspicious
            let price_extremity = (m.yes_price - 0.5).abs() * 2.0; // 0.0 – 1.0
            let ev_score = ratio * price_extremity * 10.0;
            let stars = if ratio >= 50.0 { 3 } else if ratio >= 25.0 { 2 } else { 1 };

            let direction = if m.yes_price > 0.5 { "YES" } else { "NO" };
            Signal {
                kind:       SignalKind::InsiderAlert,
                stars,
                title:      m.title.clone(),
                platform_a: m.platform.clone(),
                id_a:       m.id.clone(),
                price_a:    m.yes_price,
                platform_b: None,
                id_b:       None,
                price_b:    None,
                gap:        ratio,
                ev_score,
                action: format!(
                    "Vol/Liq {:.0}× at {:.1}% {} — possible informed flow; ask AI: analyze_insider",
                    ratio,
                    m.yes_price * 100.0,
                    direction,
                ),
            }
        })
        .collect()
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

// ─── Momentum / price velocity ───────────────────────────────────────────────
//
// Compare current YES price to the previous snapshot (from the last refresh).
// A move of ≥ MOMENTUM_THRESHOLD percentage points in either direction is flagged.

const MOMENTUM_THRESHOLD: f64 = 0.04; // 4 percentage points per refresh cycle

fn find_momentum(
    markets:     &[Market],
    prev_prices: &std::collections::HashMap<String, f64>,
) -> Vec<Signal> {
    markets
        .iter()
        .filter_map(|m| {
            let prev = *prev_prices.get(&m.id)?;
            let delta = m.yes_price - prev;
            if delta.abs() < MOMENTUM_THRESHOLD { return None; }

            let pct_move = delta * 100.0;
            let direction = if delta > 0.0 { "▲ UP" } else { "▼ DOWN" };
            let stars = if delta.abs() >= 0.12 { 3 } else if delta.abs() >= 0.07 { 2 } else { 1 };
            let ev_score = delta.abs() * 100.0 * m.volume.unwrap_or(1.0).ln();

            Some(Signal {
                kind:       SignalKind::Momentum,
                stars,
                title:      m.title.clone(),
                platform_a: m.platform.clone(),
                id_a:       m.id.clone(),
                price_a:    m.yes_price,
                platform_b: None,
                id_b:       None,
                price_b:    None,
                gap:        delta.abs(),
                ev_score,
                action: format!(
                    "{} {:.1}pp since last refresh → now {:.1}% YES — check for news catalyst",
                    direction, pct_move.abs(), m.yes_price * 100.0,
                ),
            })
        })
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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markets::{Market, Platform};

    fn market(platform: Platform, id: &str, title: &str, yes: f64,
              vol: Option<f64>, liq: Option<f64>) -> Market {
        Market {
            id:           id.to_string(),
            platform,
            title:        title.to_string(),
            description:  None,
            yes_price:    yes,
            no_price:     1.0 - yes,
            volume:       vol,
            liquidity:    liq,
            end_date:     None,
            category:     None,
            status:       "open".to_string(),
            token_id:     None,
            event_ticker: None,
        }
    }

    // ── title_similarity ──────────────────────────────────────────────────────

    #[test]
    fn identical_titles_score_one() {
        let s = title_similarity("Trump wins 2024 election", "Trump wins 2024 election");
        assert!((s - 1.0).abs() < 1e-9);
    }

    #[test]
    fn completely_different_titles_score_low() {
        let s = title_similarity("Aliens land on Mars tomorrow", "Super Bowl winner 2025");
        assert!(s < 0.15, "expected < 0.15, got {}", s);
    }

    #[test]
    fn similar_titles_exceed_arb_threshold() {
        let s = title_similarity(
            "Will Trump win the 2024 presidential election?",
            "Trump wins 2024 presidential election",
        );
        assert!(s >= 0.38, "expected >= 0.38, got {}", s);
    }

    #[test]
    fn empty_string_scores_zero() {
        assert_eq!(title_similarity("", "something relevant"), 0.0);
        assert_eq!(title_similarity("something relevant", ""), 0.0);
    }

    #[test]
    fn stop_words_do_not_contribute() {
        // "the", "a", "in", "and" are stop words → ignored
        let s = title_similarity("the a in and", "the a in and");
        assert_eq!(s, 0.0, "all stop words → both sets empty → 0");
    }

    // ── Arb detection ─────────────────────────────────────────────────────────

    #[test]
    fn arb_detected_for_large_gap() {
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "Trump wins 2024 presidential election", 0.70,
                   Some(100_000.0), Some(50_000.0)),
            market(Platform::Kalshi, "kl1", "Trump wins 2024 presidential election", 0.60,
                   Some(100_000.0), Some(50_000.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        let arb = sigs.iter().find(|s| s.kind == SignalKind::Arb).expect("arb signal");
        assert!((arb.gap - 0.10).abs() < 1e-9);
        // gap 0.10 ≥ 0.08 threshold → 3 stars
        assert_eq!(arb.stars, 3);
    }

    #[test]
    fn arb_star_rating() {
        // gap = 0.10 ≥ 0.08 → 3 stars
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "Trump wins 2024 presidential election", 0.70,
                   Some(100_000.0), Some(50_000.0)),
            market(Platform::Kalshi, "kl1", "Trump wins 2024 presidential election", 0.60,
                   Some(100_000.0), Some(50_000.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        let arb = sigs.iter().find(|s| s.kind == SignalKind::Arb).unwrap();
        assert_eq!(arb.stars, 3);
    }

    #[test]
    fn no_arb_when_gap_below_threshold() {
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "Trump wins 2024 presidential election", 0.600,
                   Some(100_000.0), Some(50_000.0)),
            market(Platform::Kalshi, "kl1", "Trump wins 2024 presidential election", 0.585,
                   Some(100_000.0), Some(50_000.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(!sigs.iter().any(|s| s.kind == SignalKind::Arb),
                "gap < 2.5% should not surface");
    }

    #[test]
    fn no_arb_when_titles_too_different() {
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "Will the Fed cut rates in December?", 0.80,
                   Some(100_000.0), Some(50_000.0)),
            market(Platform::Kalshi, "kl1", "Trump wins 2024 presidential election", 0.50,
                   Some(100_000.0), Some(50_000.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(!sigs.iter().any(|s| s.kind == SignalKind::Arb));
    }

    // ── Near-50 ───────────────────────────────────────────────────────────────

    #[test]
    fn near_fifty_at_exactly_half() {
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "Coin flip event", 0.50, Some(100_000.0), None),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        let s = sigs.iter().find(|s| s.kind == SignalKind::NearFifty).expect("near-50");
        assert_eq!(s.stars, 3); // dist = 0.0 < 0.01 → 3 stars
    }

    #[test]
    fn near_fifty_outside_band_not_detected() {
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "One-sided event", 0.80, Some(100_000.0), None),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(!sigs.iter().any(|s| s.kind == SignalKind::NearFifty));
    }

    #[test]
    fn near_fifty_requires_minimum_volume() {
        let mkts = vec![
            // vol = 5000 < 10000 threshold
            market(Platform::Polymarket, "pm1", "Low volume coin flip", 0.50, Some(5_000.0), None),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(!sigs.iter().any(|s| s.kind == SignalKind::NearFifty));
    }

    // ── Volume spike ──────────────────────────────────────────────────────────

    #[test]
    fn vol_spike_detected() {
        // Need enough baseline markets so the spike remains > 3× mean.
        // 10 markets @ 10K + 1 spike @ 1M:
        //   mean = (10 × 10K + 1M) / 11 ≈ 100K  →  threshold = 300K  <  1M ✓
        let mut mkts: Vec<Market> = (0..10)
            .map(|i| market(Platform::Polymarket, &format!("base{}", i),
                            &format!("Baseline event {}", i), 0.60, Some(10_000.0), None))
            .collect();
        mkts.push(market(Platform::Polymarket, "spike", "High vol spike", 0.55, Some(1_000_000.0), None));
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(sigs.iter().any(|s| s.kind == SignalKind::VolSpike && s.id_a == "spike"));
    }

    #[test]
    fn no_vol_spike_when_volumes_uniform() {
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "Event A", 0.60, Some(10_000.0), None),
            market(Platform::Polymarket, "pm2", "Event B", 0.40, Some(10_000.0), None),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(!sigs.iter().any(|s| s.kind == SignalKind::VolSpike));
    }

    // ── Thin markets ─────────────────────────────────────────────────────────

    #[test]
    fn thin_market_detected() {
        let mkts = vec![
            market(Platform::Kalshi, "kl1", "Illiquid event", 0.50, None, Some(500.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(sigs.iter().any(|s| s.kind == SignalKind::Thin && s.id_a == "kl1"));
    }

    #[test]
    fn thin_market_skipped_at_extremes() {
        // yes_price < 0.05 or > 0.95 → excluded
        let mkts = vec![
            market(Platform::Kalshi, "kl1", "Near-certain event", 0.97, None, Some(500.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(!sigs.iter().any(|s| s.kind == SignalKind::Thin));
    }

    #[test]
    fn thin_market_skipped_when_liquid() {
        // liquidity ≥ 10_000 → not thin
        let mkts = vec![
            market(Platform::Kalshi, "kl1", "Well-funded market", 0.50, None, Some(50_000.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(!sigs.iter().any(|s| s.kind == SignalKind::Thin));
    }

    // ── Insider alerts ────────────────────────────────────────────────────────

    #[test]
    fn insider_alert_detected_high_vol_liq_extreme_yes() {
        // vol=200K, liq=5K → ratio=40× at 80% YES → should flag
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "Suspicious directional market",
                   0.80, Some(200_000.0), Some(5_000.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        let s = sigs.iter().find(|s| s.kind == SignalKind::InsiderAlert)
            .expect("insider alert should be present");
        assert!((s.gap - 40.0).abs() < 1e-6, "ratio should be ~40×");
        assert_eq!(s.stars, 2); // 40 ≥ 25 but < 50 → 2 stars
    }

    #[test]
    fn insider_alert_detected_extreme_no_side() {
        // vol=300K, liq=5K → ratio=60× at 15% YES (NO extreme)
        let mkts = vec![
            market(Platform::Kalshi, "kl1", "Smart money shorting this one",
                   0.15, Some(300_000.0), Some(5_000.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        let s = sigs.iter().find(|s| s.kind == SignalKind::InsiderAlert)
            .expect("insider alert should be present");
        assert!(s.gap >= 50.0);
        assert_eq!(s.stars, 3); // ratio ≥ 50 → 3 stars
    }

    #[test]
    fn insider_alert_not_triggered_near_fifty_percent() {
        // High vol/liq ratio but price is near 50% — not extreme enough
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "Uncertain market with heavy trading",
                   0.52, Some(200_000.0), Some(5_000.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(!sigs.iter().any(|s| s.kind == SignalKind::InsiderAlert),
                "near-50% price should not trigger insider alert");
    }

    #[test]
    fn insider_alert_not_triggered_low_vol_liq_ratio() {
        // Price is extreme but vol/liq ratio is normal (< 15×)
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "Normal high-confidence market",
                   0.85, Some(50_000.0), Some(100_000.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(!sigs.iter().any(|s| s.kind == SignalKind::InsiderAlert),
                "low vol/liq ratio should not trigger insider alert");
    }

    #[test]
    fn insider_alert_not_triggered_when_volume_too_low() {
        // vol < 1000 minimum
        let mkts = vec![
            market(Platform::Kalshi, "kl1", "Tiny market barely trading",
                   0.80, Some(500.0), Some(10.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(!sigs.iter().any(|s| s.kind == SignalKind::InsiderAlert));
    }

    #[test]
    fn insider_alert_not_triggered_when_no_liquidity() {
        // liquidity < 1 — no pool to compare against
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "Zero liquidity market",
                   0.80, Some(50_000.0), None),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(!sigs.iter().any(|s| s.kind == SignalKind::InsiderAlert));
    }

    #[test]
    fn insider_alert_action_mentions_analyze_tool() {
        let mkts = vec![
            market(Platform::Polymarket, "pm1", "Suspicious directional market",
                   0.80, Some(200_000.0), Some(5_000.0)),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        if let Some(s) = sigs.iter().find(|s| s.kind == SignalKind::InsiderAlert) {
            assert!(s.action.contains("analyze_insider"),
                    "action should mention the AI tool");
        }
    }

    // ── Sorting + dedup ───────────────────────────────────────────────────────

    #[test]
    fn signals_sorted_stars_descending() {
        let mkts = vec![
            // arb pair with large gap (3 stars)
            market(Platform::Polymarket, "pm1", "Trump wins 2024 presidential election", 0.80,
                   Some(200_000.0), Some(100_000.0)),
            market(Platform::Kalshi, "kl1", "Trump wins 2024 presidential election", 0.70,
                   Some(200_000.0), Some(100_000.0)),
            // near-50 (1 star, 46–56% band only)
            market(Platform::Polymarket, "pm2", "Another uncertain event happens", 0.56,
                   Some(100_000.0), None),
        ];
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        for i in 0..sigs.len().saturating_sub(1) {
            assert!(sigs[i].stars >= sigs[i + 1].stars,
                    "signal {} (stars={}) before {} (stars={})",
                    i, sigs[i].stars, i + 1, sigs[i + 1].stars);
        }
    }

    #[test]
    fn empty_input_yields_empty_signals() {
        assert!(compute_signals(&[]).is_empty());
    }

    #[test]
    fn capped_at_thirty_signals() {
        // Generate 40 near-50 markets
        let mkts: Vec<Market> = (0..40).map(|i| {
            market(Platform::Polymarket, &format!("pm{}", i),
                   &format!("Uncertain event number {}", i), 0.50, Some(100_000.0), None)
        }).collect();
        let sigs = compute_signals(&mkts, &Default::default(), &Default::default());
        assert!(sigs.len() <= 30);
    }
}
