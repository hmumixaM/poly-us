use crate::{
    api::{Api, now_ms, safe_id},
    engine::sweep,
    model::*,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    time::Instant,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instrument {
    pub event: String,
    pub slug: String,
    pub kind: String,
    pub group: String,
    /// Monotone difficulty: a larger value means a stricter YES event.
    pub difficulty: i64,
    pub tick: u64,
    pub lot: u64,
    pub theta: u64,
    pub rules: String,
}
fn signed_line(v: &Value) -> Result<i64> {
    let s = v.to_string();
    let (negative, s) = if let Some(s) = s.strip_prefix('-') {
        (true, s)
    } else {
        (false, s.as_str())
    };
    let n = fixed(s)?;
    ensure!(n <= 1_000_000 * SCALE, "line outside supported range");
    Ok(if negative { -(n as i64) } else { n as i64 })
}
pub fn instruments(event: &Value) -> Result<Vec<Instrument>> {
    let e = &event["event"];
    let es = e["slug"].as_str().context("missing event slug")?;
    let markets = e["markets"].as_array().context("missing markets")?;
    let mut out = vec![];
    for m in markets {
        if m["active"] != true || m["closed"] != false {
            continue;
        }
        let kind = m["sportsMarketType"].as_str().unwrap_or("");
        // Narrow, explicit schema allowlist. No title-based guessing across periods/sports.
        let spread = kind.ends_with("_team_full_game_spread");
        let total = kind.ends_with("_team_full_game_total");
        if !spread && !total {
            continue;
        }
        let line = signed_line(&m["line"])?;
        // Half-point lines exclude push semantics. Integer lines need separate rule modelling.
        if line.unsigned_abs() % SCALE != SCALE / 2 {
            continue;
        }
        let rules = m["description"].as_str().context("missing rules")?;
        let group = if spread {
            let (prefix, tail) = rules
                .split_once(" covers ")
                .context("unsupported spread rules")?;
            let (_, suffix) = tail
                .split_once(" point spread")
                .context("unsupported spread rules")?;
            format!("{es}|{kind}|{prefix}|{suffix}")
        } else {
            let (prefix, tail) = rules
                .split_once("combine for over ")
                .context("unsupported total rules")?;
            let (_, suffix) = tail
                .split_once(" points")
                .context("unsupported total rules")?;
            format!("{es}|{kind}|{prefix}|{suffix}")
        };
        let slug = m["slug"]
            .as_str()
            .context("missing market slug")?
            .to_owned();
        ensure!(safe_id(&slug), "invalid slug");
        let tick = fixed(&m["orderPriceMinTickSize"].to_string())?;
        let lot = fixed(&m["minimumTradeQty"].to_string())?;
        // Public metadata does not establish this account's volume tier.
        let theta = fixed(&m["feeCoefficient"].to_string())?.max(95_000);
        ensure!(
            (69_500..=SCALE).contains(&theta) && tick > 0 && lot > 0,
            "invalid market metadata"
        );
        out.push(Instrument {
            event: es.to_owned(),
            slug,
            kind: kind.to_owned(),
            group,
            difficulty: if spread { -line } else { line },
            tick,
            lot,
            theta,
            rules: rules.to_owned(),
        });
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct Observation {
    pub observed_ms: u64,
    pub yes_slug: String,
    pub no_slug: String,
    pub quantity: u64,
    pub gross_edge: i64,
    pub fees: u64,
    pub slippage_reserve: u64,
    pub net_edge: i64,
    pub source_skew_ms: u64,
    pub max_source_age_ms: u64,
    pub fresh: bool,
    pub regular_settlement_only: bool,
}
pub fn observe(
    a: &Instrument,
    b: &Instrument,
    ba: &Book,
    bb: &Book,
    qty: u64,
    now: u64,
) -> Option<Observation> {
    if a.group != b.group
        || a.difficulty >= b.difficulty
        || !qty.is_multiple_of(a.lot)
        || !qty.is_multiple_of(b.lot)
    {
        return None;
    }
    let yes = sweep(ba, Side::Yes, qty, FeeKind::Standard, a.theta)?;
    let no = sweep(bb, Side::No, qty, FeeKind::Standard, b.theta)?;
    let gross = qty as i64 - yes.cash as i64 - no.cash as i64;
    let fees = yes.fees + no.fees;
    let reserve = 2 * cost(1_000, qty); // 0.1 cent per leg, beyond observed VWAP.
    let age = now
        .saturating_sub(ba.source_ms)
        .max(now.saturating_sub(bb.source_ms));
    let skew = ba.source_ms.abs_diff(bb.source_ms);
    Some(Observation {
        observed_ms: now,
        yes_slug: a.slug.clone(),
        no_slug: b.slug.clone(),
        quantity: qty,
        gross_edge: gross,
        fees,
        slippage_reserve: reserve,
        net_edge: gross - fees as i64 - reserve as i64,
        source_skew_ms: skew,
        max_source_age_ms: age,
        fresh: ba.fresh(now, 2000) && bb.fresh(now, 2000) && skew <= 500,
        regular_settlement_only: true,
    })
}

pub async fn scan(
    events: Vec<String>,
    rounds: usize,
    output: PathBuf,
    contracts: u64,
) -> Result<()> {
    ensure!(
        (1..=1000).contains(&rounds) && (1..=1000).contains(&contracts),
        "scan bounds exceeded"
    );
    std::fs::create_dir_all(&output)?;
    // Create-new prevents silently replacing previous evidence.
    let mut raw = BufWriter::new(
        File::options()
            .write(true)
            .create_new(true)
            .open(output.join("raw.jsonl"))?,
    );
    let mut observations = BufWriter::new(
        File::options()
            .write(true)
            .create_new(true)
            .open(output.join("observations.jsonl"))?,
    );
    let mut api = Api::new(None)?;
    let mut all = vec![];
    let started = now_ms();
    for slug in &events {
        ensure!(safe_id(slug), "invalid event slug");
        let v = api.get(&format!("/v1/events/slug/{slug}"), false).await?;
        writeln!(
            raw,
            "{}",
            json!({"received_ms":now_ms(),"path":format!("/v1/events/slug/{slug}"),"response":v})
        )?;
        let discovered = instruments(&v)?;
        println!(
            "{}",
            json!({"event":slug,"score":v["event"]["score"],"period":v["event"]["period"],"supported_markets":discovered.len()})
        );
        all.extend(discovered);
    }
    ensure!(!all.is_empty(), "no supported markets");
    std::fs::write(
        output.join("instruments.json"),
        serde_json::to_vec_pretty(&all)?,
    )?;
    let mut pairs = vec![];
    for (i, a) in all.iter().enumerate() {
        for (j, b) in all.iter().enumerate() {
            if a.group == b.group && a.difficulty < b.difficulty {
                pairs.push((i, j));
            }
        }
    }
    let mut books = BTreeMap::new();
    let mut requests = 0u64;
    let mut errors = 0u64;
    let mut evaluations = 0u64;
    let mut fresh = 0u64;
    let mut gross_positive = 0u64;
    let mut net_positive = 0u64;
    let mut reconfirmed = 0u64;
    let mut best_net = i64::MIN;
    let mut rtt = vec![];
    for round in 0..rounds {
        for (i, m) in all.iter().enumerate() {
            let path = format!("/v1/markets/{}/book", m.slug);
            let timer = Instant::now();
            requests += 1;
            match api.get(&path, false).await {
                Ok(v) => {
                    let received = now_ms();
                    rtt.push(timer.elapsed().as_micros() as u64);
                    writeln!(
                        raw,
                        "{}",
                        json!({"received_ms":received,"path":path,"response":v})
                    )?;
                    if let Some((slug, b)) = wire_book(&v, received)? {
                        ensure!(slug == m.slug, "response market mismatch");
                        let meta = Market {
                            slug: m.slug.clone(),
                            fee: FeeKind::Standard,
                            tick: m.tick,
                            lot: m.lot,
                        };
                        if b.validate(&meta).is_ok() {
                            books.insert(i, b);
                        } else {
                            books.remove(&i);
                            errors += 1;
                        }
                    } else {
                        books.remove(&i);
                        errors += 1;
                    }
                }
                Err(_) => {
                    books.remove(&i);
                    errors += 1;
                }
            }
        }
        let now = now_ms();
        let mut candidates = vec![];
        for &(i, j) in &pairs {
            let (Some(a), Some(b)) = (books.get(&i), books.get(&j)) else {
                continue;
            };
            if let Some(o) = observe(&all[i], &all[j], a, b, contracts * SCALE, now) {
                evaluations += 1;
                if o.fresh {
                    fresh += 1;
                }
                if o.gross_edge > 0 {
                    gross_positive += 1;
                }
                if o.net_edge > 0 {
                    net_positive += 1;
                    candidates.push((i, j));
                }
                best_net = best_net.max(o.net_edge);
                writeln!(observations, "{}", serde_json::to_string(&o)?)?;
            }
        }
        // Re-fetch positive pairs. Stale, separately sampled prices are never called executable.
        for (i, j) in candidates.into_iter().take(10) {
            let mut refreshed = vec![];
            for k in [i, j] {
                requests += 1;
                let v = api
                    .get(&format!("/v1/markets/{}/book", all[k].slug), false)
                    .await?;
                let received = now_ms();
                writeln!(
                    raw,
                    "{}",
                    json!({"received_ms":received,"recheck":true,"response":v})
                )?;
                let (slug, b) = wire_book(&v, received)?.context("missing recheck book")?;
                ensure!(slug == all[k].slug, "recheck response market mismatch");
                b.validate(&Market {
                    slug: all[k].slug.clone(),
                    fee: FeeKind::Standard,
                    tick: all[k].tick,
                    lot: all[k].lot,
                })?;
                refreshed.push(b);
            }
            if let Some(o) = observe(
                &all[i],
                &all[j],
                &refreshed[0],
                &refreshed[1],
                contracts * SCALE,
                now_ms(),
            ) {
                if o.fresh && o.net_edge > 0 {
                    reconfirmed += 1;
                    println!("{}", json!({"reconfirmed_conditional_signal":o}));
                }
                writeln!(observations, "{}", json!({"recheck":o}))?;
            }
        }
        raw.flush()?;
        observations.flush()?;
        println!(
            "{}",
            json!({"round":round+1,"requests":requests,"evaluations":evaluations,"fresh":fresh,"positive_before_fees":gross_positive,"positive_after_fees":net_positive,"reconfirmed":reconfirmed,"best_net_micros":best_net})
        );
    }
    rtt.sort_unstable();
    let pct = |p: usize| rtt.get(rtt.len().saturating_sub(1) * p / 100).copied();
    let summary = json!({"started_ms":started,"ended_ms":now_ms(),"events":events,"markets":all.len(),"pairs":pairs.len(),"rounds":rounds,
        "contracts":contracts,"requests":requests,"errors":errors,"evaluations":evaluations,"fresh_evaluations":fresh,"gross_positive_observations":gross_positive,
        "net_positive_observations":net_positive,"reconfirmed_conditional_signals":reconfirmed,"best_net_micros":best_net,
        "request_duration_us_including_local_pacing":{"p50":pct(50),"p95":pct(95),"p99":pct(99)},
        "limits":["REST is not simultaneous or suitable to prove millisecond execution","normal-settlement dominance excludes LFMP and cancellations","score not independently verified","RFQ combo alpha untested without authenticated quotes","no orders sent; no realized PnL"]});
    std::fs::write(
        output.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    println!("{}", summary);
    Ok(())
}
