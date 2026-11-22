//! Durable read-only final-result capture, separate from watched book slots.
use crate::{
    api::{Api, now_ms, safe_id},
    archive::Archive,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, path::Path, time::Instant};

#[derive(Clone, Serialize, Deserialize)]
pub struct Pending {
    pub next_ms: u64,
    pub crypto: bool,
    pub payout_phase: bool,
}
#[derive(Default)]
pub struct Followup {
    pub pending: BTreeMap<String, Pending>,
    pub requests: u64,
    pub errors: u64,
}
impl Followup {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("pending_settlements.json");
        let pending = if path.exists() {
            serde_json::from_slice(&std::fs::read(path)?)?
        } else {
            BTreeMap::new()
        };
        ensure!(
            pending.keys().all(|s: &String| safe_id(s)),
            "invalid pending settlement slug"
        );
        Ok(Self {
            pending,
            requests: 0,
            errors: 0,
        })
    }
    pub fn add(&mut self, slug: &str, after_ms: u64, crypto: bool) {
        self.pending.entry(slug.to_owned()).or_insert(Pending {
            next_ms: after_ms,
            crypto,
            payout_phase: false,
        });
    }
    pub fn has_due(&self, now: u64) -> bool {
        self.pending.values().any(|p| p.next_ms <= now)
    }
    pub fn expedite(&mut self, slug: &str, now: u64) {
        if let Some(p) = self.pending.get_mut(slug) {
            p.next_ms = p.next_ms.min(now);
        }
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        let tmp = dir.join("pending_settlements.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&self.pending)?)?;
        std::fs::rename(tmp, dir.join("pending_settlements.json"))?;
        Ok(())
    }
    pub async fn step(&mut self, api: &mut Api, archive: &mut Archive) -> Result<bool> {
        let now = now_ms();
        let Some((slug, p)) = self
            .pending
            .iter()
            .filter(|(_, p)| p.next_ms <= now)
            .min_by_key(|(_, p)| p.next_ms)
            .map(|(s, p)| (s.clone(), p.clone()))
        else {
            return Ok(false);
        };
        let path = if p.payout_phase {
            format!("/v1/markets/{slug}/settlement")
        } else {
            format!("/v1/market/slug/{slug}")
        };
        // Keep failed/unresolved work across restarts, and avoid tight retries.
        self.pending.get_mut(&slug).unwrap().next_ms = now + 60_000;
        let requested = now_ms();
        let timer = Instant::now();
        self.requests += 1;
        match api.get(&path, false).await {
            Ok(v) => {
                archive.write(&json!({"kind":if p.payout_phase{"market_settlement"}else{"market_final"},"market":slug,"requested_ms":requested,"received_ms":now_ms(),"duration_us":timer.elapsed().as_micros(),"path":path,"response":v}))?;
                if p.payout_phase {
                    if v["slug"] == slug && v["settlement"].is_number() {
                        self.pending.remove(&slug);
                    }
                } else {
                    let m = &v["market"];
                    if m["slug"] == slug
                        && m["closed"] == true
                        && (!p.crypto || !m["assetPriceTerms"]["settlementPrice"].is_null())
                    {
                        let entry = self.pending.get_mut(&slug).unwrap();
                        entry.payout_phase = true;
                        entry.next_ms = now_ms();
                    }
                }
            }
            Err(e) => {
                self.errors += 1;
                archive.write(&json!({"kind":"request_error","feed":"settlement","market":slug,"requested_ms":requested,"received_ms":now_ms(),"path":path,"error":e.to_string()}))?;
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_work_survives_restart_without_resetting_its_phase() {
        let dir = std::env::temp_dir().join(format!("poly-followup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut q = Followup::default();
        q.add("btc-test", 1000, true);
        q.pending.get_mut("btc-test").unwrap().payout_phase = true;
        q.save(&dir).unwrap();
        let mut restored = Followup::load(&dir).unwrap();
        restored.add("btc-test", 9000, true);
        assert_eq!(restored.pending["btc-test"].next_ms, 1000);
        assert!(restored.pending["btc-test"].payout_phase);
        assert!(restored.pending["btc-test"].crypto);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
