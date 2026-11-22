use crate::model::*;
use serde::Serialize;

/// Conservative fee bound: round each level UP to cents, without rebate credit.
/// This deliberately overestimates the exchange's banker-rounded cumulative fees.
pub fn fee(kind: FeeKind, theta: u64, px: u64, qty: u64) -> u64 {
    let p = px as u128;
    let q = qty as u128;
    let s = SCALE as u128;
    let one = s - p;
    let standard = (q * p * one * theta as u128).div_ceil(s.pow(3));
    let mut inv4 = one;
    for _ in 0..3 {
        inv4 = (inv4 * one).div_ceil(s);
    }
    let extra = if kind == FeeKind::Combo {
        (q * p * inv4 * 60_000).div_ceil(s.pow(3))
    } else {
        0
    };
    ceil_div(standard + extra, 10_000) * 10_000
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Sweep {
    pub cash: u64,
    pub fees: u64,
    pub limit: u64,
    pub qty: u64,
}
pub fn sweep(book: &Book, side: Side, qty: u64, kind: FeeKind, theta: u64) -> Option<Sweep> {
    let mut left = qty;
    let mut out = Sweep {
        cash: 0,
        fees: 0,
        limit: 0,
        qty,
    };
    for l in book.levels(side) {
        let take = l.qty.min(left);
        if take == 0 {
            continue;
        }
        let p = if side == Side::Yes {
            l.px
        } else {
            SCALE - l.px
        };
        out.cash += cost(p, take);
        out.fees += fee(kind, theta, p, take);
        out.limit = l.px; // API always takes the YES instrument price, even for NO.
        left -= take;
        if left == 0 {
            return Some(out);
        }
    }
    None
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub candidate: usize,
    pub qty: u64,
    pub legs: [Sweep; 2],
    pub worst_payout: u64,
    pub reserved: u64,
    pub net_edge: i64,
    pub decided_ms: u64,
}

pub struct Engine {
    pub config: Config,
    pub books: Vec<Book>,
    dependencies: Vec<Vec<usize>>,
    pub evaluations: u64,
}
impl Engine {
    pub fn new(config: Config) -> Self {
        let mut dependencies = vec![vec![]; config.markets.len()];
        for (i, c) in config.candidates.iter().enumerate() {
            for l in &c.legs {
                dependencies[l.market].push(i);
            }
        }
        let books = vec![Book::default(); config.markets.len()];
        Self {
            config,
            books,
            dependencies,
            evaluations: 0,
        }
    }
    pub fn invalidate(&mut self) {
        for b in &mut self.books {
            b.clear();
        }
    }
    pub fn update(&mut self, market: usize, book: Book, now: u64) -> anyhow::Result<Option<Plan>> {
        let m = self
            .config
            .markets
            .get(market)
            .ok_or_else(|| anyhow::anyhow!("unknown market"))?;
        if let Err(e) = book.validate(m) {
            self.books[market].clear();
            return Err(e);
        }
        // Duplicate or backwards frames must not refresh an old timestamp or replenish paper depth.
        if book.source_ms <= self.books[market].source_ms {
            return Ok(None);
        }
        self.books[market] = book;
        let mut best: Option<Plan> = None;
        for &id in &self.dependencies[market] {
            self.evaluations += 1;
            if let Some(p) = self.evaluate(id, now)
                && best.as_ref().is_none_or(|b| p.net_edge > b.net_edge)
            {
                best = Some(p);
            }
        }
        Ok(best)
    }
    pub fn evaluate(&self, id: usize, now: u64) -> Option<Plan> {
        let c = &self.config.candidates[id];
        let r = &self.config.risk;
        let a = &self.books[c.legs[0].market];
        let b = &self.books[c.legs[1].market];
        if !c.approved
            || !a.fresh(now, r.max_book_age_ms)
            || !b.fresh(now, r.max_book_age_ms)
            || a.source_ms.abs_diff(b.source_ms) > r.max_skew_ms
        {
            return None;
        }
        let get = |i: usize| {
            let l = &c.legs[i];
            sweep(
                &self.books[l.market],
                l.side,
                r.quantity,
                self.config.markets[l.market].fee,
                r.theta,
            )
        };
        let legs = [get(0)?, get(1)?];
        let worst_payout = (c.floor() as u128 * r.quantity as u128 / SCALE as u128) as u64;
        let reserved = legs.iter().map(|l| l.cash + l.fees).sum::<u64>()
            + 2 * cost(r.slippage_per_leg, r.quantity);
        let net_edge = worst_payout as i64 - reserved as i64;
        if reserved > r.max_spend
            || net_edge <= 0
            || (net_edge as u128) * 10_000 < (reserved as u128) * r.min_edge_bps as u128
        {
            return None;
        }
        Some(Plan {
            candidate: id,
            qty: r.quantity,
            legs,
            worst_payout,
            reserved,
            net_edge,
            decided_ms: now,
        })
    }
}
