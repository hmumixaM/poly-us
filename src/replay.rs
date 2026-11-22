//! Arrival-time replay. Baseline simulated PnL is not evidence of executable alpha.
use crate::{
    crypto::{Contract, IndexHistory, estimate},
    engine::sweep,
    model::*,
};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
};

#[derive(Clone)]
struct Pending {
    side: Side,
    at: u64,
    limit: u64,
    exit: bool,
}
struct Position {
    side: Side,
    cash: u64,
    fees: u64,
    qty: u64,
}
#[derive(Default)]
struct Stats {
    signals: u64,
    entries: u64,
    failed_fok: u64,
    exits: u64,
    settled: u64,
    net: i64,
    modeled_signals: u64,
}

pub fn analyze(dir: &Path, out: &Path, latency_ms: u64, quantity: u64) -> Result<()> {
    ensure!(
        (1..=5000).contains(&latency_ms) && (1..=1000).contains(&quantity),
        "invalid replay settings"
    );
    let files = crate::archive::finalized_files(dir)?;
    ensure!(
        !files.is_empty(),
        "no finalized archives; wait for rotation"
    );
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)?;
    let mut history = IndexHistory::default();
    let mut contracts = BTreeMap::<String, Contract>::new();
    let mut pending = BTreeMap::<(String, String), Pending>::new();
    let mut held = BTreeMap::<(String, String), Position>::new();
    let mut done = BTreeSet::new();
    let mut previous = BTreeMap::<String, (u64, f64)>::new();
    let mut stats = BTreeMap::<String, Stats>::new();
    let mut records = 0u64;
    let mut last = 0u64;
    let qty = quantity * SCALE;
    let mut index_requests = 0;
    let mut books = 0;
    let mut book_sources = BTreeMap::<String, u64>::new();
    let mut skipped_books = 0u64;
    for path in &files {
        for record in crate::archive::records(path)? {
            let v = record?;
            records += 1;
            let now = v["received_ms"].as_u64().context("missing arrival time")?;
            ensure!(
                now >= last,
                "archives not in arrival order; do not combine concurrent collectors"
            );
            last = now;
            let response = &v["response"];
            match v["kind"].as_str().unwrap_or("") {
                "crypto_markets" => {
                    for c in crate::crypto::contracts(
                        response,
                        &["btc".into(), "eth".into(), "sol".into(), "xrp".into()],
                        now / 1000,
                    )? {
                        contracts.insert(c.slug.clone(), c);
                    }
                }
                "asset_prices" => {
                    index_requests += 1;
                    history.ingest(
                        response,
                        now,
                        v["requested_ms"].as_u64().context("missing request time")?,
                    )?;
                }
                "crypto_book" => {
                    books += 1;
                    let Some((slug, b)) = wire_book(response, now)? else {
                        continue;
                    };
                    let Some(c) = contracts.get(&slug) else {
                        continue;
                    };
                    b.validate(&Market {
                        slug: slug.clone(),
                        fee: FeeKind::Standard,
                        tick: c.tick,
                        lot: c.lot,
                    })?;
                    if book_sources.get(&slug).is_some_and(|ts| b.source_ms <= *ts) {
                        skipped_books += 1;
                        continue;
                    }
                    book_sources.insert(slug.clone(), b.source_ms);
                    for strategy in ["late_direction", "last_minute_jump"] {
                        // Independent counterfactual portfolios must not consume each other's depth.
                        let mut b = b.clone();
                        let key = (strategy.to_owned(), slug.clone());
                        let st = stats.entry(strategy.to_owned()).or_default();
                        if let Some(p) = pending.get(&key).cloned()
                            && now >= p.at + latency_ms
                        {
                            pending.remove(&key);
                            // Observe a later snapshot, within a bounded latency gap; never fill on signal snapshot.
                            if now > p.at + 2000 || !b.fresh(now, 2000) || now / 1000 >= c.end {
                                st.failed_fok += 1;
                                continue;
                            }
                            let opposite = if p.side == Side::Yes {
                                Side::No
                            } else {
                                Side::Yes
                            };
                            let side = if p.exit { opposite } else { p.side };
                            if let Some(s) = sweep(&b, side, qty, FeeKind::Standard, c.theta) {
                                let within = if p.exit {
                                    true
                                } else if side == Side::Yes {
                                    s.limit <= p.limit
                                } else {
                                    s.limit >= p.limit
                                };
                                if within {
                                    b.consume(side, qty);
                                    if p.exit {
                                        if let Some(position) = held.remove(&key) {
                                            let pnl = qty as i64
                                                - s.cash as i64
                                                - s.fees as i64
                                                - position.cash as i64
                                                - position.fees as i64;
                                            st.net += pnl;
                                            st.exits += 1;
                                            done.insert(key.clone());
                                            writeln!(
                                                output,
                                                "{}",
                                                json!({"kind":"simulated_exit","strategy":strategy,"slug":slug,"received_ms":now,"net_micros":pnl})
                                            )?;
                                        }
                                    } else {
                                        held.insert(
                                            key.clone(),
                                            Position {
                                                side,
                                                cash: s.cash,
                                                fees: s.fees,
                                                qty,
                                            },
                                        );
                                        st.entries += 1;
                                        writeln!(
                                            output,
                                            "{}",
                                            json!({"kind":"simulated_entry","strategy":strategy,"slug":slug,"received_ms":now,"side":side,"cash":s.cash,"fees":s.fees,"limit_yes_px":s.limit})
                                        )?;
                                    }
                                } else {
                                    st.failed_fok += 1;
                                }
                            } else {
                                st.failed_fok += 1;
                            }
                        }
                        let Some(e) = estimate(c, &history, now) else {
                            continue;
                        };
                        st.modeled_signals += 1;
                        if let Some(position) = held.get(&key) {
                            let p = if position.side == Side::Yes {
                                e.probability_up
                            } else {
                                1. - e.probability_up
                            };
                            if p < 0.5 && !pending.contains_key(&key) {
                                pending.insert(
                                    key.clone(),
                                    Pending {
                                        side: position.side,
                                        at: now,
                                        limit: 0,
                                        exit: true,
                                    },
                                );
                            }
                            continue;
                        }
                        if done.contains(&key)
                            || pending.contains_key(&key)
                            || !b.fresh(now, 2000)
                            || !qty.is_multiple_of(c.lot)
                        {
                            continue;
                        }
                        let jump = previous
                            .get(&slug)
                            .filter(|(t, _)| now.saturating_sub(*t) <= 2500)
                            .map(|(_, p)| e.probability_up - *p);
                        if strategy == "last_minute_jump" && jump.is_none_or(|j| j.abs() < 0.10) {
                            continue;
                        }
                        let side = if e.probability_up >= 0.5 {
                            Side::Yes
                        } else {
                            Side::No
                        };
                        if strategy == "last_minute_jump"
                            && jump.is_some_and(|j| (j > 0.) != (side == Side::Yes))
                        {
                            continue;
                        }
                        if let Some(s) = sweep(&b, side, qty, FeeKind::Standard, c.theta) {
                            let p = if side == Side::Yes {
                                e.probability_up
                            } else {
                                1. - e.probability_up
                            };
                            let edge =
                                p * qty as f64 - (s.cash + s.fees + cost(2 * c.tick, qty)) as f64;
                            if edge > cost(30_000, qty) as f64 {
                                st.signals += 1;
                                pending.insert(
                                    key,
                                    Pending {
                                        side,
                                        at: now,
                                        limit: s.limit,
                                        exit: false,
                                    },
                                );
                            }
                        }
                    }
                    if let Some(e) = estimate(c, &history, now) {
                        previous.insert(slug, (now, e.probability_up));
                    }
                }
                _ => {}
            }
            // Settlement uses only reference candles already received, not a later backfilled index.
            let keys: Vec<_> = held.keys().cloned().collect();
            for key in keys {
                let Some(c) = contracts.get(&key.1) else {
                    continue;
                };
                if now / 1000 < c.end + 2 {
                    continue;
                }
                if let Some(up) = settlement(c, &history, now) {
                    let p = held.remove(&key).unwrap();
                    let win = up == (p.side == Side::Yes);
                    let payout = if win { p.qty } else { 0 };
                    let pnl = payout as i64 - p.cash as i64 - p.fees as i64;
                    let st = stats.entry(key.0.clone()).or_default();
                    st.net += pnl;
                    st.settled += 1;
                    done.insert(key.clone());
                    writeln!(
                        output,
                        "{}",
                        json!({"kind":"reconstructed_settlement","strategy":key.0,"slug":key.1,"up":up,"net_micros":pnl,"official_settlement_verified":false})
                    )?;
                }
            }
            // Bounded state for long archives. Missing settlement remains unresolved, never a win.
            contracts
                .retain(|slug, c| c.end + 7200 > now / 1000 || held.keys().any(|k| &k.1 == slug));
            pending.retain(|key, p| {
                if now > p.at + 5000 {
                    stats.entry(key.0.clone()).or_default().failed_fok += 1;
                    false
                } else {
                    true
                }
            });
            previous.retain(|_, (t, _)| now <= *t + 7_200_000);
            done.retain(|k| contracts.contains_key(&k.1));
            book_sources.retain(|slug, _| contracts.contains_key(slug));
            ensure!(
                held.len() <= 128 && contracts.len() <= 1024,
                "replay state limit; split archive into smaller time windows"
            );
        }
    }
    let strategy_reports:Vec<_>=stats.iter().map(|(k,s)|{let open_cost:u64=held.iter().filter(|(key,_)|&key.0==k).map(|(_,p)|p.cash+p.fees).sum();json!({"strategy":k,"model_evaluations":s.modeled_signals,"signals":s.signals,"entries":s.entries,"failed_fok":s.failed_fok,"exits":s.exits,"reconstructed_settlements":s.settled,"realized_simulated_net_micros":s.net,"unresolved_cost_micros":open_cost,"worst_case_net_micros":s.net-open_cost as i64})}).collect();
    let summary = json!({"files":files.len(),"records":records,"reference_responses":index_requests,"crypto_book_responses":books,"duplicate_or_backwards_books_skipped":skipped_books,"latency_ms":latency_ms,"contracts_per_trade":quantity,"unresolved_positions":held.len(),"pending_intents":pending.len(),"strategies":strategy_reports,"conclusion":"research baseline only; alpha unproven","limitations":["uncalibrated driftless Brownian probabilities","REST snapshots miss intervening liquidity; no order queue simulation","settlement reconstructed; official values not yet reconciled","no live trades, rebates, guaranteed stop fills or statistical significance claimed"]});
    writeln!(output, "{}", json!({"kind":"summary","report":summary}))?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
pub fn settlement(c: &Contract, h: &IndexHistory, now: u64) -> Option<bool> {
    if now / 1000 < c.end {
        return None;
    }
    let s = h.samples.get(&c.asset)?;
    let mut sum = 0u128;
    for t in c.end.checked_sub(60)?..c.end {
        let x = s.get(&t)?;
        if x.received_ms > now {
            return None;
        }
        sum += x.price as u128;
    }
    let unit = 10u128.pow(6 - c.decimals);
    let rounded = (sum + 30 * unit) / (60 * unit) * unit;
    Some(rounded >= c.beat? as u128)
}
