//! Volume-based admission. Cold instruments are probed, not continuously watched.
use crate::{collector::Settings, model::fixed};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub slug: String,
    pub event: String,
    pub kind: String,
    pub asset: Option<String>,
    pub end_ms: Option<u64>,
    pub settle_after_ms: Option<u64>,
    pub retry_after_ms: Option<u64>,
    pub notional_usd_micros: Option<u64>,
    pub last_trade_ms: Option<u64>,
    pub checked_ms: u64,
    pub open: bool,
    pub epoch: u64,
}
impl Candidate {
    pub fn new(
        slug: String,
        event: String,
        kind: String,
        asset: Option<String>,
        end_ms: Option<u64>,
        epoch: u64,
    ) -> Self {
        Self {
            slug,
            event,
            kind,
            asset,
            end_ms,
            settle_after_ms: None,
            retry_after_ms: None,
            notional_usd_micros: None,
            last_trade_ms: None,
            checked_ms: 0,
            open: false,
            epoch,
        }
    }
    pub fn eligible(&self, s: &Settings, now: u64) -> bool {
        let (floor, age) = if self.asset.is_some() {
            (s.min_crypto_notional_usd, s.crypto_last_trade_seconds)
        } else {
            (s.min_sports_notional_usd, s.sports_last_trade_seconds)
        };
        self.open
            && self.end_ms.is_none_or(|end| end > now)
            && self
                .notional_usd_micros
                .is_some_and(|n| n >= floor * 1_000_000)
            && self
                .last_trade_ms
                .is_some_and(|t| t <= now && now - t <= age * 1000)
            && self.checked_ms <= now
            && now - self.checked_ms <= s.probe_seconds * 1000
    }
}
#[derive(Default)]
pub struct Universe {
    pub candidates: BTreeMap<String, Candidate>,
    probe_round: u64,
}
impl Universe {
    pub fn upsert(&mut self, c: Candidate) {
        if let Some(old) = self.candidates.get_mut(&c.slug) {
            old.event = c.event;
            old.kind = c.kind;
            old.asset = c.asset;
            old.end_ms = c.end_ms;
            old.settle_after_ms = c.settle_after_ms.or(old.settle_after_ms);
            old.epoch = c.epoch;
        } else {
            self.candidates.insert(c.slug.clone(), c);
        }
    }
    pub fn observe(&mut self, slug: &str, v: &Value, now: u64) -> Result<()> {
        let m = &v["marketData"];
        ensure!(
            m["marketSlug"] == slug,
            "volume probe returned another instrument"
        );
        let c = self
            .candidates
            .get_mut(slug)
            .context("unknown volume candidate")?;
        c.checked_ms = now;
        c.retry_after_ms = None;
        c.open = m["state"] == "MARKET_STATE_OPEN";
        let n = &m["stats"]["notionalTraded"];
        c.notional_usd_micros = if n["currency"] == "USD" {
            let v = &n["value"];
            Some(fixed(v.as_str().unwrap_or(&v.to_string()))?)
        } else {
            None
        };
        c.last_trade_ms = m["stats"]["lastTradeSetTime"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .and_then(|t| u64::try_from(t.timestamp_millis()).ok());
        Ok(())
    }
    pub fn failed(&mut self, slug: &str, now: u64) {
        if let Some(c) = self.candidates.get_mut(slug) {
            c.checked_ms = now;
            c.open = false;
            c.retry_after_ms = Some(now + 5000);
        }
    }
    pub fn ranked(&self, s: &Settings, now: u64) -> Vec<Candidate> {
        let mut out: Vec<_> = self
            .candidates
            .values()
            .filter(|c| c.eligible(s, now))
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            b.notional_usd_micros
                .cmp(&a.notional_usd_micros)
                .then_with(|| a.slug.cmp(&b.slug))
        });
        // Preserve both research directions: eligible crypto takes reserved
        // slots first; every unused slot is available to either group.
        let reserved: Vec<_> = out
            .iter()
            .filter(|c| c.asset.is_some())
            .take(s.crypto_reserved_slots.min(s.max_subscriptions))
            .map(|c| c.slug.clone())
            .collect();
        let mut picked: Vec<_> = out
            .iter()
            .filter(|c| reserved.contains(&c.slug))
            .cloned()
            .collect();
        for c in out {
            if picked.len() >= s.max_subscriptions {
                break;
            }
            if !reserved.contains(&c.slug) {
                picked.push(c);
            }
        }
        picked.sort_by(|a, b| {
            b.notional_usd_micros
                .cmp(&a.notional_usd_micros)
                .then_with(|| a.slug.cmp(&b.slug))
        });
        picked
    }
    pub fn probe(&mut self, s: &Settings, now: u64) -> Option<String> {
        // Round-robin service classes: recovery, crypto discovery, sports
        // discovery. Empty classes lend their turn; no backlog owns all probes.
        let alive = |c: &&Candidate| c.end_ms.is_none_or(|t| t > now);
        let retry = self
            .candidates
            .values()
            .filter(alive)
            .filter(|c| c.retry_after_ms.is_some_and(|t| t <= now))
            .min_by(|a, b| {
                a.checked_ms
                    .cmp(&b.checked_ms)
                    .then_with(|| a.slug.cmp(&b.slug))
            });
        let normal_due = |c: &&Candidate| {
            c.retry_after_ms.is_none()
                && (c.checked_ms == 0
                    || now.saturating_sub(c.checked_ms)
                        >= (if c.asset.is_some() {
                            s.crypto_probe_seconds
                        } else {
                            s.probe_seconds
                        }) * 1000)
        };
        let crypto = self
            .candidates
            .values()
            .filter(alive)
            .filter(normal_due)
            .filter(|c| c.asset.is_some())
            .min_by(|a, b| {
                a.checked_ms
                    .cmp(&b.checked_ms)
                    .then_with(|| a.slug.cmp(&b.slug))
            });
        let sports = self
            .candidates
            .values()
            .filter(alive)
            .filter(normal_due)
            .filter(|c| c.asset.is_none())
            .min_by(|a, b| {
                (a.checked_ms != 0)
                    .cmp(&(b.checked_ms != 0))
                    .then_with(|| {
                        (!a.kind.ends_with("_full_game_winner"))
                            .cmp(&(!b.kind.ends_with("_full_game_winner")))
                    })
                    .then_with(|| a.checked_ms.cmp(&b.checked_ms))
                    .then_with(|| a.slug.cmp(&b.slug))
            });
        let classes = [retry, crypto, sports];
        for offset in 0..3 {
            let index = (self.probe_round as usize + offset) % 3;
            if let Some(c) = classes[index] {
                self.probe_round = ((index + 1) % 3) as u64;
                return Some(c.slug.clone());
            }
        }
        None
    }
    pub fn expire(&mut self, now: u64) {
        self.candidates
            .retain(|_, c| c.end_ms.is_none_or(|t| t > now));
    }
    pub fn finish_sports(&mut self, epoch: u64) {
        self.candidates
            .retain(|_, c| c.asset.is_some() || c.epoch == epoch);
    }
    pub fn unprobed(&self) -> usize {
        self.candidates
            .values()
            .filter(|c| c.checked_ms == 0)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::now_ms;
    fn sample(slug: &str, usd: u64, last: u64) -> Value {
        serde_json::json!({"marketData":{"marketSlug":slug,"state":"MARKET_STATE_OPEN","stats":{"notionalTraded":{"currency":"USD","value":usd.to_string()},"lastTradeSetTime":chrono::DateTime::from_timestamp_millis(last as i64).unwrap().to_rfc3339()}}})
    }
    #[test]
    fn persistent_failures_and_cold_catalog_both_make_probe_progress() {
        let s: Settings = toml::from_str(include_str!("../config/collector.toml")).unwrap();
        let now = now_ms();
        let mut u = Universe::default();
        for i in 0..20 {
            let slug = format!("failed-{i:02}");
            u.upsert(Candidate::new(
                slug.clone(),
                "event".into(),
                "winner".into(),
                None,
                None,
                1,
            ));
            u.failed(&slug, now);
        }
        u.upsert(Candidate::new(
            "fresh-btc".into(),
            "".into(),
            "crypto_up_down".into(),
            Some("btc".into()),
            Some(now + 300_000),
            0,
        ));
        // A full queue of zero-volume crypto must not starve sports either.
        for i in 0..30 {
            u.upsert(Candidate::new(
                format!("cold-crypto-{i:02}"),
                "".into(),
                "crypto_up_down".into(),
                Some("eth".into()),
                Some(now + 300_000),
                0,
            ));
        }
        u.upsert(Candidate::new(
            "fresh-sports".into(),
            "event".into(),
            "winner".into(),
            None,
            None,
            1,
        ));
        let mut seen = std::collections::BTreeSet::new();
        for turn in 0..100 {
            let at = now + 5000 + turn * 268;
            if let Some(slug) = u.probe(&s, at) {
                seen.insert(slug.clone());
                if slug.starts_with("failed-") {
                    u.failed(&slug, at);
                } else {
                    u.observe(&slug, &sample(&slug, 20000, at), at).unwrap();
                }
            }
        }
        assert!(seen.contains("fresh-btc"));
        assert!(seen.contains("fresh-sports"));
        assert_eq!(seen.iter().filter(|s| s.starts_with("failed-")).count(), 20);
        assert!(
            u.ranked(&s, now + 32_000)
                .iter()
                .any(|c| c.slug == "fresh-btc")
        );
    }
    #[test]
    fn transient_sports_failure_is_reprobed_without_a_fifteen_minute_gap() {
        let s: Settings = toml::from_str(include_str!("../config/collector.toml")).unwrap();
        let now = now_ms();
        let mut u = Universe::default();
        u.upsert(Candidate::new(
            "sports".into(),
            "event".into(),
            "winner".into(),
            None,
            None,
            1,
        ));
        u.observe("sports", &sample("sports", 20000, now), now)
            .unwrap();
        assert_eq!(u.ranked(&s, now).len(), 1);
        u.failed("sports", now);
        assert!(u.ranked(&s, now + 1).is_empty());
        // A large cold-start queue must not make an admitted contract's
        // short error retry wait behind every never-probed instrument.
        u.upsert(Candidate::new(
            "fresh".into(),
            "event".into(),
            "winner".into(),
            None,
            None,
            1,
        ));
        assert_eq!(u.probe(&s, now + 4999).as_deref(), Some("fresh"));
        assert_eq!(u.probe(&s, now + 5000).as_deref(), Some("sports"));
        u.observe("sports", &sample("sports", 20000, now), now + 5000)
            .unwrap();
        assert_eq!(u.ranked(&s, now + 5000).len(), 1);
        assert!(u.candidates["sports"].retry_after_ms.is_none());
    }
    #[test]
    fn crypto_research_is_not_crowded_out_by_older_sports_turnover() {
        let mut s: Settings = toml::from_str(include_str!("../config/collector.toml")).unwrap();
        s.max_subscriptions = 3;
        s.crypto_reserved_slots = 1;
        let now = now_ms();
        let mut u = Universe::default();
        for (slug, usd, asset) in [
            ("sports-a", 1_000_000, None),
            ("sports-b", 900_000, None),
            ("sports-c", 800_000, None),
            ("btc", 1000, Some("btc")),
        ] {
            u.upsert(Candidate::new(
                slug.into(),
                "e".into(),
                "winner".into(),
                asset.map(str::to_owned),
                None,
                1,
            ));
            u.observe(slug, &sample(slug, usd, now), now).unwrap();
        }
        let selected = u.ranked(&s, now);
        assert_eq!(selected.len(), 3);
        assert!(selected.iter().any(|c| c.slug == "btc"));
        assert!(!selected.iter().any(|c| c.slug == "sports-c"));
        let mut cold = Candidate::new(
            "cold-crypto".into(),
            "".into(),
            "crypto_up_down".into(),
            Some("eth".into()),
            None,
            1,
        );
        cold.checked_ms = now - 16_000;
        u.upsert(cold);
        assert_eq!(u.probe(&s, now).as_deref(), Some("cold-crypto"));
    }
    #[test]
    fn ranks_by_real_usd_volume_with_one_shared_subscription_budget() {
        let mut s: Settings = toml::from_str(include_str!("../config/collector.toml")).unwrap();
        s.max_subscriptions = 2;
        let now = now_ms();
        let mut u = Universe::default();
        for (slug, usd, asset, last) in [
            ("sports-low", 9999, None, now),
            ("sports-high", 20000, None, now),
            ("crypto", 3000, Some("btc"), now),
            ("stale", 500000, None, now - 86_401_000),
            ("cold", 100000, None, now),
        ] {
            u.upsert(Candidate::new(
                slug.into(),
                "event".into(),
                "winner".into(),
                asset.map(str::to_owned),
                None,
                1,
            ));
            if slug != "cold" {
                u.observe(slug, &sample(slug, usd, last), now).unwrap();
            }
        }
        let selected = u.ranked(&s, now);
        assert_eq!(
            selected.iter().map(|c| c.slug.as_str()).collect::<Vec<_>>(),
            ["sports-high", "crypto"]
        );
        assert_eq!(u.probe(&s, now).as_deref(), Some("cold"));
        assert!(
            u.observe("crypto", &sample("other", 999999, now), now)
                .is_err()
        );
        assert_eq!(u.ranked(&s, now).len(), 2);
        // A cached large cumulative value cannot keep a dead short-window contract admitted.
        assert_eq!(u.ranked(&s, now + 121_000).len(), 1);
    }
}
