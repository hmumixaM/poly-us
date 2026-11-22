//! Research hypotheses from forum 543634/663. No executable orders.
use crate::{
    api::{Api, now_ms, safe_id},
    collector::Archive,
    engine::sweep,
    model::*,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Contract {
    pub slug: String,
    pub asset: String,
    pub index: String,
    pub start: u64,
    pub end: u64,
    pub beat: Option<u64>,
    pub decimals: u32,
    pub tick: u64,
    pub lot: u64,
    pub theta: u64,
    pub rules: String,
}
fn seconds(v: &Value) -> Result<u64> {
    let t =
        chrono::DateTime::parse_from_rfc3339(v.as_str().context("missing window")?)?.timestamp();
    ensure!(t >= 0, "invalid window");
    Ok(t as u64)
}
pub fn contracts(v: &Value, assets: &[String], now: u64) -> Result<Vec<Contract>> {
    let mut out = vec![];
    for m in v["markets"].as_array().context("missing crypto markets")? {
        let t = &m["assetPriceTerms"];
        if m["active"] != true
            || m["closed"] != false
            || t["marketType"] != "ASSET_PRICE_MARKET_TYPE_UP_DOWN"
            || t["asset"]["assetClass"] != "ASSET_CLASS_CRYPTO"
        {
            continue;
        }
        let asset = t["asset"]["symbol"].as_str().unwrap_or("");
        if !assets.iter().any(|a| a == asset) {
            continue;
        }
        let start = seconds(&t["windowStart"])?;
        let end = seconds(&t["windowEnd"])?;
        ensure!(end > start, "invalid crypto window");
        if start > now + 30 || end <= now || end - start > 3600 {
            continue;
        }
        let slug = m["slug"].as_str().context("missing slug")?;
        ensure!(safe_id(slug), "invalid slug");
        let beat = if t["priceToBeat"].is_null() {
            None
        } else {
            ensure!(
                t["priceToBeat"]["currency"] == "USD",
                "invalid beat currency"
            );
            Some(fixed(
                t["priceToBeat"]["value"]
                    .as_str()
                    .context("missing price-to-beat")?,
            )?)
        };
        let index = t["indexSymbol"]
            .as_str()
            .context("missing reference index")?
            .to_owned();
        let rules = m["description"]
            .as_str()
            .context("missing crypto rules")?
            .to_owned();
        ensure!(
            rules.contains("simple average") && rules.contains("60 "),
            "unsupported up/down statistic"
        );
        let decimals = t["priceDecimals"].as_u64().context("missing decimals")?;
        ensure!(decimals <= 6, "unsupported precision");
        out.push(Contract {
            slug: slug.to_owned(),
            asset: asset.to_owned(),
            index,
            start,
            end,
            beat,
            decimals: decimals as u32,
            tick: fixed(&m["orderPriceMinTickSize"].to_string())?,
            lot: fixed(&m["minimumTradeQty"].to_string())?,
            theta: fixed(&m["feeCoefficient"].to_string())?.max(95_000),
            rules,
        });
    }
    out.sort_by_key(|c| (c.end, c.start));
    Ok(out)
}

pub struct Feed {
    pub assets: Vec<String>,
    pub active: Vec<Contract>,
    discover: Instant,
    discover_offset: usize,
    discovered: Vec<Contract>,
    discover_now: u64,
    reference: Instant,
    pub requests: u64,
    pub errors: u64,
}
impl Feed {
    pub fn new(assets: Vec<String>) -> Self {
        Self {
            assets,
            active: vec![],
            discover: Instant::now(),
            discover_offset: 0,
            discovered: vec![],
            discover_now: 0,
            reference: Instant::now(),
            requests: 0,
            errors: 0,
        }
    }
    fn reset_discovery(&mut self) {
        self.discover_offset = 0;
        self.discovered.clear();
        self.discover = Instant::now() + Duration::from_secs(15);
    }
    fn accept_discovery(&mut self, v: &Value, now: u64) -> Result<()> {
        let parsed = match contracts(v, &self.assets, now) {
            Ok(parsed) => parsed,
            Err(e) => {
                self.reset_discovery();
                return Err(e);
            }
        };
        self.discovered.extend(parsed);
        if v["markets"].as_array().is_some_and(|m| m.len() == 100) {
            self.discover_offset += 100;
        } else {
            self.discovered.sort_by(|a, b| a.slug.cmp(&b.slug));
            self.discovered.dedup_by(|a, b| a.slug == b.slug);
            self.active = std::mem::take(&mut self.discovered);
            self.reset_discovery();
        }
        Ok(())
    }
    pub async fn step_metadata(&mut self, api: &mut Api, archive: &mut Archive) -> Result<()> {
        let now = now_ms() / 1000;
        let (kind, path) = if self.discover_offset > 0 || Instant::now() >= self.discover {
            if self.discover_offset == 0 {
                self.discover_now = now;
                self.discovered.clear();
            }
            let from = chrono::DateTime::from_timestamp(self.discover_now as i64, 0)
                .context("clock")?
                .to_rfc3339();
            let to = chrono::DateTime::from_timestamp(self.discover_now as i64 + 7200, 0)
                .context("clock")?
                .to_rfc3339();
            let mut u = reqwest::Url::parse("https://gateway.polymarket.us/v1/markets")?;
            u.query_pairs_mut()
                .append_pair("limit", "100")
                .append_pair("offset", &self.discover_offset.to_string())
                .append_pair("categories", "crypto")
                .append_pair("active", "true")
                .append_pair("closed", "false")
                .append_pair("endDateMin", &from)
                .append_pair("endDateMax", &to)
                .append_pair("orderBy", "endDate")
                .append_pair("orderDirection", "asc");
            self.discover = Instant::now() + Duration::from_secs(15);
            (
                "crypto_markets",
                format!("{}?{}", u.path(), u.query().unwrap_or("")),
            )
        } else if Instant::now() >= self.reference {
            self.reference = Instant::now() + Duration::from_secs(1);
            let assets = self
                .assets
                .iter()
                .map(|a| format!("assets=crypto:{a}"))
                .collect::<Vec<_>>()
                .join("&");
            (
                "asset_prices",
                format!(
                    "/v1/asset-prices/history?{assets}&from={}&to=0&bucket=ASSET_PRICE_BUCKET_1S",
                    now.saturating_sub(180)
                ),
            )
        } else {
            return Ok(());
        };
        let requested = now_ms();
        let timer = Instant::now();
        self.requests += 1;
        match api.get(&path, false).await {
            Ok(v) => {
                let received = now_ms();
                archive.write(&json!({"kind":kind,"requested_ms":requested,"received_ms":received,"duration_us":timer.elapsed().as_micros(),"path":path,"response":v}))?;
                if kind == "crypto_markets" {
                    self.accept_discovery(&v, received / 1000)?;
                }
            }
            Err(e) => {
                self.errors += 1;
                if kind == "crypto_markets" {
                    self.reset_discovery();
                }
                archive.write(&json!({"kind":"request_error","feed":kind,"requested_ms":requested,"received_ms":now_ms(),"path":path,"error":e.to_string()}))?;
                if kind == "crypto_book" {
                    self.active
                        .retain(|c| path != format!("/v1/markets/{}/book", c.slug));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub price: u64,
    pub received_ms: u64,
}
#[derive(Default)]
pub struct IndexHistory {
    pub samples: BTreeMap<String, BTreeMap<u64, Sample>>,
}
impl IndexHistory {
    pub fn ingest(&mut self, v: &Value, received: u64, requested: u64) -> Result<()> {
        for s in v["series"].as_array().context("missing series")? {
            ensure!(
                s["bucket"] == "ASSET_PRICE_BUCKET_1S"
                    && s["asset"]["assetClass"] == "ASSET_CLASS_CRYPTO"
                    && s["currency"] == "USD",
                "unsupported reference series"
            );
            let asset = s["asset"]["symbol"]
                .as_str()
                .context("missing reference asset")?
                .to_owned();
            ensure!(
                self.samples.len() < 4 || self.samples.contains_key(&asset),
                "too many reference assets"
            );
            let h = self.samples.entry(asset).or_default();
            for c in s["candles"].as_array().context("missing candles")? {
                let ts = c["timestamp"].as_u64().context("missing candle time")?;
                if ts >= requested / 1000 || c["tickCount"].as_u64().unwrap_or(0) == 0 {
                    continue;
                }
                let p = fixed(c["close"].as_str().context("missing close")?)?;
                // Preserve first arrival. Later history must not backdate knowledge.
                h.entry(ts).or_insert(Sample {
                    price: p,
                    received_ms: received,
                });
            }
            let cutoff = (received / 1000).saturating_sub(7200);
            h.retain(|t, _| *t >= cutoff);
            while h.len() > 7200 {
                h.pop_first();
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct Estimate {
    pub probability_up: f64,
    pub known_seconds: usize,
    pub remaining_seconds: usize,
    pub expected_average: f64,
    pub standard_error: f64,
    pub reference_age_ms: u64,
}
/// Driftless Brownian baseline, not a calibrated or reconstructed private model.
pub fn estimate(c: &Contract, h: &IndexHistory, now: u64) -> Option<Estimate> {
    if now / 1000 < c.end.saturating_sub(60) || now / 1000 >= c.end {
        return None;
    }
    // P(round(mean, decimals) >= beat), matching reconstructed settlement.
    let beat = c.beat? as f64 / SCALE as f64 - 0.5 * 10f64.powi(-(c.decimals as i32));
    let samples = h.samples.get(&c.asset)?;
    let (&last, sample) = samples
        .iter()
        .rev()
        .find(|(t, s)| **t < now / 1000 && s.received_ms <= now)?;
    if now.saturating_sub((last + 1) * 1000) > 2500 {
        return None;
    }
    let start = c.end.checked_sub(60)?;
    if last < start {
        return None;
    }
    let mut sum = 0f64;
    let mut known = 0;
    for t in start..=last.min(c.end - 1) {
        let s = samples.get(&t)?;
        if s.received_ms > now {
            return None;
        }
        sum += s.price as f64 / SCALE as f64;
        known += 1;
    }
    let diffs: Vec<_> = samples
        .range(last.saturating_sub(120)..=last)
        .filter(|(_, s)| s.received_ms <= now)
        .collect();
    if diffs.len() < 31 || diffs.windows(2).any(|w| *w[1].0 != *w[0].0 + 1) {
        return None;
    }
    let variance = diffs
        .windows(2)
        .map(|w| {
            let d = (w[1].1.price as f64 - w[0].1.price as f64) / SCALE as f64;
            d * d
        })
        .sum::<f64>()
        / (diffs.len() - 1) as f64;
    let remaining = 60usize.checked_sub(known)?;
    let m = remaining as f64;
    let lastp = sample.price as f64 / SCALE as f64;
    let expected = (sum + m * lastp) / 60.;
    let se = (variance * m * (m + 1.) * (2. * m + 1.) / 6.).sqrt() / 60.;
    if se <= 0. || remaining == 0 {
        return None;
    }
    let z = (expected - beat) / se;
    let p = normal_cdf(z);
    Some(Estimate {
        probability_up: p,
        known_seconds: known,
        remaining_seconds: remaining,
        expected_average: expected,
        standard_error: se,
        reference_age_ms: now.saturating_sub((last + 1) * 1000),
    })
}
fn normal_cdf(z: f64) -> f64 {
    let x = z.abs();
    let t = 1. / (1. + 0.2316419 * x);
    let tail = 0.3989422804014327
        * (-x * x / 2.).exp()
        * t
        * (0.319381530
            + t * (-0.356563782 + t * (1.781477937 + t * (-1.821255978 + t * 1.330274429))));
    if z >= 0. { 1. - tail } else { tail }
}

pub fn diagnostic(c: &Contract, h: &IndexHistory, b: &Book, now: u64, qty: u64) -> Option<Value> {
    let e = estimate(c, h, now)?;
    if !qty.is_multiple_of(c.lot) || !b.open {
        return None;
    }
    let mut opportunities = vec![];
    for side in [Side::Yes, Side::No] {
        if let Some(s) = sweep(b, side, qty, FeeKind::Standard, c.theta) {
            let p = if side == Side::Yes {
                e.probability_up
            } else {
                1. - e.probability_up
            };
            let modeled = p * qty as f64 - (s.cash + s.fees + cost(2 * c.tick, qty)) as f64;
            opportunities.push(json!({"side":side,"modeled_net_micros":modeled,"limit_yes_price":s.limit,"quantity":qty,"fresh":b.fresh(now,2000)}));
        }
    }
    Some(
        json!({"slug":c.slug,"asset":c.asset,"estimate":e,"sides":opportunities,"status":"uncalibrated_model_diagnostic_not_executable_alpha"}),
    )
}

#[cfg(test)]
mod feed_tests {
    use super::*;
    #[test]
    fn later_page_parse_failure_preserves_active_feed_and_backs_off() {
        let now = 1_800_000_000;
        let market = json!({"slug":"btc-good", "active":true,"closed":false,
            "description":"simple average of 60 seconds", "orderPriceMinTickSize":0.01,
            "minimumTradeQty":0.01,"feeCoefficient":0.095,
            "assetPriceTerms":{"asset":{"symbol":"btc","assetClass":"ASSET_CLASS_CRYPTO"},
                "marketType":"ASSET_PRICE_MARKET_TYPE_UP_DOWN", "indexSymbol":"BTCUSD",
                "priceDecimals":2,"windowStart":chrono::DateTime::from_timestamp(now-120,0).unwrap().to_rfc3339(),
                "windowEnd":chrono::DateTime::from_timestamp(now+180,0).unwrap().to_rfc3339()}});
        let mut feed = Feed::new(vec!["btc".into()]);
        feed.active = contracts(
            &json!({"markets":[market.clone()]}),
            &feed.assets,
            now as u64,
        )
        .unwrap();
        assert_eq!(feed.active.len(), 1);
        feed.discovered = feed.active.clone();
        feed.discover_offset = 100;
        let mut unsupported = market;
        unsupported["description"] = json!("new statistic not implemented");
        assert!(
            feed.accept_discovery(&json!({"markets":[unsupported]}), now as u64)
                .is_err()
        );
        assert_eq!(feed.discover_offset, 0);
        assert!(feed.discovered.is_empty());
        assert_eq!(feed.active[0].slug, "btc-good");
        assert!(feed.discover > Instant::now() + Duration::from_secs(14));
    }
}
