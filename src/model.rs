use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Prices, cash and quantities use six decimal places. Never binary floats.
pub const SCALE: u64 = 1_000_000;
pub const MAX_QTY: u64 = 1_000_000 * SCALE;

pub fn fixed(s: &str) -> Result<u64> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    ensure!(
        !whole.is_empty() && whole.bytes().all(|b| b.is_ascii_digit()),
        "invalid decimal"
    );
    ensure!(
        frac.len() <= 6 && frac.bytes().all(|b| b.is_ascii_digit()),
        "decimal exceeds six places"
    );
    let a = whole
        .parse::<u64>()?
        .checked_mul(SCALE)
        .ok_or_else(|| anyhow::anyhow!("decimal overflow"))?;
    let b = if frac.is_empty() {
        0
    } else {
        frac.parse::<u64>()? * 10u64.pow(6 - frac.len() as u32)
    };
    a.checked_add(b)
        .ok_or_else(|| anyhow::anyhow!("decimal overflow"))
}
pub fn display(n: u64) -> String {
    format!("{}.{:06}", n / SCALE, n % SCALE)
}
pub fn ceil_div(a: u128, b: u128) -> u64 {
    a.div_ceil(b) as u64
}
pub fn cost(px: u64, qty: u64) -> u64 {
    ceil_div(px as u128 * qty as u128, SCALE as u128)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Yes,
    No,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeeKind {
    Standard,
    Combo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Market {
    pub slug: String,
    pub fee: FeeKind,
    pub tick: u64,
    pub lot: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Leg {
    pub market: usize,
    pub side: Side,
}

/// A scenario table is a user-reviewed contract, not a proof inferred from titles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub id: String,
    pub legs: [Leg; 2],
    pub scenarios: Vec<String>,
    /// Per-side payout per contract, including NO transformation, in micros.
    pub payouts: Vec<[u64; 2]>,
    pub rules_reference: String,
    pub approved: bool,
}
impl Candidate {
    pub fn floor(&self) -> u64 {
        self.payouts.iter().map(|p| p[0] + p[1]).min().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Risk {
    pub theta: u64,
    pub max_book_age_ms: u64,
    pub max_skew_ms: u64,
    pub min_edge_bps: u64,
    pub slippage_per_leg: u64,
    pub max_spend: u64,
    pub max_session_spend: u64,
    pub max_trades: u64,
    pub quantity: u64,
    pub paper_leg_latency_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub markets: Vec<Market>,
    pub candidates: Vec<Candidate>,
    pub risk: Risk,
}
impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let c: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.markets.is_empty(), "no markets");
        let mut slugs = HashSet::new();
        for m in &self.markets {
            ensure!(
                !m.slug.is_empty()
                    && m.slug
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
                "invalid slug"
            );
            ensure!(slugs.insert(&m.slug), "duplicate market");
            ensure!(
                m.tick > 0 && m.tick <= SCALE && SCALE.is_multiple_of(m.tick),
                "invalid tick"
            );
            ensure!(m.lot > 0 && m.lot <= MAX_QTY, "invalid lot");
        }
        let r = &self.risk;
        ensure!(
            (69_500..=SCALE).contains(&r.theta),
            "theta must be conservative and at least 0.0695"
        );
        ensure!(r.quantity > 0 && r.quantity <= MAX_QTY, "invalid quantity");
        ensure!(
            r.max_spend > 0 && r.max_spend <= r.max_session_spend && r.max_session_spend <= MAX_QTY,
            "invalid spend limits"
        );
        ensure!(
            r.max_book_age_ms > 0 && r.max_book_age_ms <= 60_000,
            "invalid book age"
        );
        ensure!(
            r.max_skew_ms <= r.max_book_age_ms
                && r.min_edge_bps <= 10_000
                && r.slippage_per_leg <= SCALE,
            "invalid risk settings"
        );
        ensure!(
            r.max_trades > 0 && r.paper_leg_latency_ms > 0 && r.paper_leg_latency_ms <= 60_000,
            "invalid execution settings"
        );
        let mut ids = HashSet::new();
        for c in &self.candidates {
            ensure!(
                !c.id.is_empty() && ids.insert(&c.id),
                "duplicate/empty candidate id"
            );
            ensure!(
                !c.scenarios.is_empty() && c.scenarios.len() == c.payouts.len(),
                "incomplete scenario table"
            );
            ensure!(
                c.scenarios.iter().collect::<HashSet<_>>().len() == c.scenarios.len(),
                "duplicate scenarios"
            );
            ensure!(!c.rules_reference.is_empty(), "missing rule evidence");
            for p in &c.payouts {
                ensure!(p.iter().all(|v| *v <= SCALE), "invalid payout");
            }
            ensure!(
                c.legs[0].market != c.legs[1].market,
                "pair must use distinct instruments"
            );
            for l in &c.legs {
                let m = self
                    .markets
                    .get(l.market)
                    .ok_or_else(|| anyhow::anyhow!("invalid market index"))?;
                ensure!(
                    r.quantity.is_multiple_of(m.lot),
                    "quantity not aligned to lot"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Level {
    pub px: u64,
    pub qty: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Book {
    pub bids: Vec<Level>,
    pub offers: Vec<Level>,
    pub received_ms: u64,
    pub source_ms: u64,
    pub open: bool,
}
impl Book {
    pub fn validate(&self, m: &Market) -> Result<()> {
        for l in self.bids.iter().chain(&self.offers) {
            ensure!(
                l.px > 0 && l.px < SCALE && l.px.is_multiple_of(m.tick),
                "bad book level"
            );
        }
        ensure!(
            self.bids.windows(2).all(|w| w[0].px > w[1].px),
            "unsorted bids"
        );
        ensure!(
            self.offers.windows(2).all(|w| w[0].px < w[1].px),
            "unsorted offers"
        );
        if let (Some(b), Some(a)) = (self.bids.first(), self.offers.first()) {
            ensure!(b.px < a.px, "crossed book");
        }
        Ok(())
    }
    pub fn fresh(&self, now: u64, age: u64) -> bool {
        self.open
            && now >= self.received_ms
            && now - self.received_ms <= age
            && now >= self.source_ms
            && now - self.source_ms <= age
    }
    pub fn levels(&self, side: Side) -> &[Level] {
        if side == Side::Yes {
            &self.offers
        } else {
            &self.bids
        }
    }
    pub fn clear(&mut self) {
        self.open = false;
    }
    /// Consume known liquidity after a simulated fill. A new snapshot is still optimistic.
    pub fn consume(&mut self, side: Side, mut qty: u64) {
        let levels = if side == Side::Yes {
            &mut self.offers
        } else {
            &mut self.bids
        };
        for l in levels {
            let take = qty.min(l.qty);
            l.qty -= take;
            qty -= take;
        }
    }
}

pub fn wire_book(v: &serde_json::Value, received_ms: u64) -> Result<Option<(String, Book)>> {
    let Some(m) = v.get("marketData") else {
        return Ok(None);
    };
    let slug = m["marketSlug"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing slug"))?
        .to_owned();
    let source = m["transactTime"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing exchange timestamp"))?;
    let ts = chrono::DateTime::parse_from_rfc3339(source)?.timestamp_millis();
    ensure!(ts >= 0, "negative timestamp");
    let parse = |key: &str| -> Result<Vec<Level>> {
        let Some(arr) = m.get(key) else {
            return Ok(vec![]);
        };
        let arr = arr
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("invalid levels"))?;
        arr.iter()
            .map(|x| {
                ensure!(x["px"]["currency"] == "USD", "unsupported currency");
                Ok(Level {
                    px: fixed(
                        x["px"]["value"]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("invalid price"))?,
                    )?,
                    qty: fixed(
                        x["qty"]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("invalid quantity"))?,
                    )?,
                })
            })
            .collect()
    };
    let state = m["state"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing state"))?;
    if state.is_empty() {
        bail!("empty state")
    }
    Ok(Some((
        slug,
        Book {
            bids: parse("bids")?,
            offers: parse("offers")?,
            received_ms,
            source_ms: ts as u64,
            open: state == "MARKET_STATE_OPEN",
        },
    )))
}
