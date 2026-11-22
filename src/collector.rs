use crate::api::{Api, now_ms, safe_id};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub data_dir: PathBuf,
    pub requests_per_second: u64,
    pub discover_seconds: u64,
    // Accepted legacy fields; no event, per-event, or page-count cutoffs.
    #[serde(default)]
    pub max_events: usize,
    #[serde(default)]
    pub markets_per_event: usize,
    #[serde(default)]
    pub max_discovery_pages: usize,
    pub max_subscriptions: usize,
    pub crypto_reserved_slots: usize,
    pub crypto_probe_seconds: u64,
    pub min_sports_notional_usd: u64,
    pub min_crypto_notional_usd: u64,
    pub sports_last_trade_seconds: u64,
    pub crypto_last_trade_seconds: u64,
    pub probe_seconds: u64,
    pub rotate_seconds: u64,
    pub rotate_uncompressed_mb: u64,
    /// Legacy retention fields are accepted but never trigger deletion.
    #[serde(default)]
    pub retention_days: u64,
    #[serde(default)]
    pub max_disk_mb: u64,
    #[serde(default = "default_disk_bytes")]
    pub max_disk_bytes: u64,
    #[serde(default = "default_free_mb")]
    pub min_free_disk_mb: u64,
    #[serde(default)]
    pub archive_format: crate::archive::Format,
    #[serde(default = "default_row_group_mb")]
    pub row_group_mb: u64,
    pub leagues: Vec<String>,
    #[serde(default = "default_live_only")]
    pub sports_live_only: bool,
    #[serde(default = "default_lookahead")]
    pub sports_lookahead_hours: u64,
    #[serde(default = "default_crypto_assets")]
    pub crypto_assets: Vec<String>,
}
fn default_live_only() -> bool {
    true
}
fn default_lookahead() -> u64 {
    72
}
fn default_disk_bytes() -> u64 {
    1_000_000_000_000
}
fn default_free_mb() -> u64 {
    1024
}
fn default_row_group_mb() -> u64 {
    8
}
fn default_crypto_assets() -> Vec<String> {
    vec!["btc".to_owned()]
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=15).contains(&self.requests_per_second),
            "request rate must be 1..15"
        );
        ensure!(
            (30..=3600).contains(&self.discover_seconds),
            "discovery interval must be 30..3600"
        );
        ensure!(
            self.crypto_reserved_slots <= self.max_subscriptions
                && (1..=120).contains(&self.crypto_probe_seconds),
            "invalid crypto research allocation"
        );
        ensure!(
            (1..=100).contains(&self.max_subscriptions),
            "subscription budget must be 1..100"
        );
        ensure!(
            self.min_sports_notional_usd <= 1_000_000_000
                && self.min_crypto_notional_usd <= 1_000_000_000,
            "invalid volume threshold"
        );
        ensure!(
            (1..=604800).contains(&self.sports_last_trade_seconds)
                && (1..=3600).contains(&self.crypto_last_trade_seconds)
                && (30..=86400).contains(&self.probe_seconds),
            "invalid activity/probe interval"
        );
        ensure!(
            (10..=86400).contains(&self.rotate_seconds)
                && (1..=1024).contains(&self.rotate_uncompressed_mb),
            "invalid rotation"
        );
        ensure!(
            (64 * 1024 * 1024..=100_000_000_000_000).contains(&self.max_disk_bytes)
                && self.min_free_disk_mb <= 1_048_576
                && (1..=16).contains(&self.row_group_mb),
            "invalid storage quota/row-group size"
        );
        ensure!(
            !self.leagues.is_empty()
                && self.leagues.iter().all(|s| safe_id(s))
                && (1..=168).contains(&self.sports_lookahead_hours),
            "invalid league filter"
        );
        ensure!(
            self.crypto_assets.len() <= 4
                && self
                    .crypto_assets
                    .iter()
                    .all(|a| ["btc", "eth", "sol", "xrp"].contains(&a.as_str())),
            "unsupported crypto assets"
        );
        Ok(())
    }
}

pub use crate::archive::Archive;

#[derive(Clone, Serialize)]
struct Tracked {
    slug: String,
    event: String,
    kind: String,
}
fn resume_cursor(old: &[Tracked], index: usize, new: &[Tracked]) -> usize {
    let next = old.get(index % old.len().max(1)).map(|m| m.slug.as_str());
    next.and_then(|slug| new.iter().position(|m| m.slug == slug))
        .unwrap_or(index % new.len().max(1))
}
fn select(event: &Value, s: &Settings) -> Vec<Tracked> {
    let slug = event["slug"].as_str().unwrap_or("");
    let league = slug.split('-').next().unwrap_or("");
    if !s.leagues.iter().any(|l| l == league)
        || (s.sports_live_only && event["live"] != true)
        || event["ended"] == true
    {
        return vec![];
    }
    let Some(markets) = event["markets"].as_array() else {
        return vec![];
    };
    let mut candidates = vec![];
    for m in markets {
        if m["active"] != true || m["closed"] != false {
            continue;
        }
        let kind = m["sportsMarketType"].as_str().unwrap_or("");
        let winner = kind.ends_with("_full_game_winner");
        if !winner
            && !kind.ends_with("_full_game_spread")
            && !kind.ends_with("_full_game_total")
            && !kind.ends_with("_points_full_game_total")
        {
            continue;
        }
        let Some(ms) = m["slug"].as_str().filter(|x| safe_id(x)) else {
            continue;
        };
        let prices = m["outcomePrices"]
            .as_str()
            .and_then(|x| serde_json::from_str::<Vec<String>>(x).ok());
        let p = prices
            .and_then(|x| x.first().and_then(|x| crate::model::fixed(x).ok()))
            .unwrap_or(0);
        candidates.push((!winner, p.abs_diff(500_000), ms.to_owned(), kind.to_owned()));
    }
    candidates.sort();
    candidates
        .into_iter()
        .map(|(_, _, market, kind)| Tracked {
            slug: market,
            event: slug.to_owned(),
            kind,
        })
        .collect()
}

fn track_event(event: &Value, s: &Settings, epoch: u64, universe: &mut crate::watch::Universe) {
    let after = event["startTime"]
        .as_str()
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        .and_then(|v| u64::try_from(v.timestamp_millis()).ok())
        .map(|t| t + 6 * 3600 * 1000);
    for m in select(event, s) {
        let mut candidate =
            crate::watch::Candidate::new(m.slug, m.event, m.kind, None, None, epoch);
        candidate.settle_after_ms = after;
        universe.upsert(candidate);
    }
}
fn queue_selected_results(
    selected: &[crate::watch::Candidate],
    now: u64,
    q: &mut crate::followup::Followup,
) {
    for c in selected.iter().filter(|c| c.asset.is_none()) {
        q.add(
            &c.slug,
            c.settle_after_ms.unwrap_or(now + 6 * 3600 * 1000),
            false,
        );
    }
}
#[derive(Debug, PartialEq, Eq)]
enum Maintenance {
    Settlement,
    Event,
    Idle,
}
fn maintenance_action(settlement_due: bool, event_due: bool, turn: u64) -> Maintenance {
    match (settlement_due, event_due) {
        (true, true) => {
            if turn.is_multiple_of(2) {
                Maintenance::Settlement
            } else {
                Maintenance::Event
            }
        }
        (true, false) => Maintenance::Settlement,
        (false, true) => Maintenance::Event,
        (false, false) => Maintenance::Idle,
    }
}

/// Each step issues at most one request, so catalog pagination never blocks books.
struct Discovery {
    offset: usize,
    pending: Option<String>,
    running: bool,
    epoch: u64,
    due: Instant,
    from: String,
    to: String,
}
impl Discovery {
    fn new() -> Self {
        Self {
            offset: 0,
            pending: None,
            running: false,
            epoch: 0,
            due: Instant::now(),
            from: String::new(),
            to: String::new(),
        }
    }
    async fn step(
        &mut self,
        api: &mut Api,
        s: &Settings,
        archive: &mut Archive,
        universe: &mut crate::watch::Universe,
    ) -> Result<bool> {
        if !self.running {
            if Instant::now() < self.due {
                return Ok(false);
            }
            self.running = true;
            self.offset = 0;
            self.epoch += 1;
            let now = (now_ms() / 1000) as i64;
            self.from = chrono::DateTime::from_timestamp(now - 86400, 0)
                .context("clock")?
                .to_rfc3339();
            self.to =
                chrono::DateTime::from_timestamp(now + s.sports_lookahead_hours as i64 * 3600, 0)
                    .context("clock")?
                    .to_rfc3339();
        }
        let detail = self.pending.is_some();
        let path = if let Some(slug) = &self.pending {
            format!("/v1/events/slug/{slug}")
        } else {
            let mut url = reqwest::Url::parse("https://gateway.polymarket.us/v1/events")?;
            url.query_pairs_mut()
                .append_pair("limit", "1")
                .append_pair("offset", &self.offset.to_string())
                .append_pair("active", "true")
                .append_pair("closed", "false")
                .append_pair("ended", "false")
                .append_pair("categories", "sports")
                .append_pair("orderBy", "volume")
                .append_pair("orderDirection", "desc")
                .append_pair("startTimeMin", &self.from)
                .append_pair("startTimeMax", &self.to);
            if s.sports_live_only {
                url.query_pairs_mut().append_pair("live", "true");
            }
            format!("{}?{}", url.path(), url.query().unwrap_or(""))
        };
        let requested = now_ms();
        let timer = Instant::now();
        let v = match api.get(&path, false).await {
            Ok(v) => v,
            Err(e) => {
                self.running = false;
                self.pending = None;
                self.due = Instant::now() + Duration::from_secs(s.discover_seconds);
                return Err(e);
            }
        };
        archive.write(&json!({"kind":if detail{"event_detail"}else{"event_discovery"},"requested_ms":requested,"received_ms":now_ms(),"duration_us":timer.elapsed().as_micros(),"path":path,"response":v}))?;
        let parsed = (|| -> Result<()> {
            if detail {
                ensure!(
                    v["event"]["slug"].as_str() == self.pending.as_deref(),
                    "event detail identity mismatch"
                );
                track_event(&v["event"], s, self.epoch, universe);
                self.pending = None;
            } else {
                let events = v["events"].as_array().context("missing event list")?;
                if events.is_empty() {
                    universe.finish_sports(self.epoch);
                    self.running = false;
                    self.due = Instant::now() + Duration::from_secs(s.discover_seconds);
                } else {
                    self.offset += events.len();
                    let slug = events[0]["slug"].as_str().context("missing event slug")?;
                    if safe_id(slug)
                        && s.leagues
                            .iter()
                            .any(|l| slug.split('-').next() == Some(l.as_str()))
                    {
                        self.pending = Some(slug.to_owned());
                    }
                }
            }
            Ok(())
        })();
        if parsed.is_err() {
            self.running = false;
            self.pending = None;
            self.due = Instant::now() + Duration::from_secs(s.discover_seconds);
        }
        parsed?;
        Ok(true)
    }
}

pub async fn run(path: &Path) -> Result<()> {
    let settings: Settings = toml::from_str(&std::fs::read_to_string(path)?)?;
    settings.validate()?;
    std::fs::create_dir_all(&settings.data_dir)?;
    let mut lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(settings.data_dir.join("collector.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("another collector owns this data directory")?;
    lock.set_len(0)?;
    writeln!(lock, "{}", std::process::id())?;
    let mut archive = Archive::new(settings.clone())?;
    archive.write(&json!({"kind":"session_start","received_ms":now_ms(),"version":env!("CARGO_PKG_VERSION"),"source_commit":option_env!("SOURCE_COMMIT").unwrap_or("unversioned"),"mode":"public_read_only","data_capabilities":{"books":"REST snapshots","trades":"last_trade_stats_only","rfq":"unavailable_after_auth_401","scores":"public_event_REST","fees":"market_metadata_coefficient; account_tier_unverified"}}))?;
    let mut api = Api::new(None)?;
    let pacing = Duration::from_millis(1000_u64.div_ceil(settings.requests_per_second));
    api.pace_at(pacing);
    use crate::watch::{Candidate, Universe};
    let mut universe = Universe::default();
    let mut followup = crate::followup::Followup::load(&settings.data_dir)?;
    let mut event_due = BTreeMap::<String, u64>::new();
    let mut catalog = Discovery::new();
    let mut selected: Vec<Tracked> = vec![];
    let mut index = 0usize;
    let mut crypto_index = 0usize;
    let mut requests = 0u64;
    let mut crypto_books = 0u64;
    let mut errors = 0u64;
    let mut health_due = Instant::now();
    let mut rank_due = Instant::now();
    let mut shutdown = Box::pin(shutdown());
    let mut crypto = crate::crypto::Feed::new(settings.crypto_assets.clone());
    let mut lane = 0u64;
    loop {
        lane = lane.wrapping_add(1);
        let mut used = false;
        if lane % 8 == 4 {
            let result = tokio::select! {r=catalog.step(&mut api,&settings,&mut archive,&mut universe)=>Some(r),_=&mut shutdown=>None};
            let Some(result) = result else { break };
            match result {
                Ok(b) => {
                    used = b;
                    if b {
                        rank_due = Instant::now();
                    }
                }
                Err(e) => {
                    errors += 1;
                    eprintln!("discovery failed: {e}");
                    used = true;
                }
            }
        } else if lane.is_multiple_of(8) && !settings.crypto_assets.is_empty() {
            let before = crypto.requests;
            let result = tokio::select! {r=crypto.step_metadata(&mut api,&mut archive)=>Some(r),_=&mut shutdown=>None};
            let Some(result) = result else { break };
            if let Err(e) = result {
                errors += 1;
                archive.write(&json!({"kind":"crypto_processing_error","received_ms":now_ms(),"error":e.to_string()}))?;
            }
            used = crypto.requests > before;
            for c in &crypto.active {
                followup.add(&c.slug, c.end * 1000 + 10_000, true);
                universe.upsert(Candidate::new(
                    c.slug.clone(),
                    String::new(),
                    "crypto_up_down".into(),
                    Some(c.asset.clone()),
                    Some(c.end * 1000),
                    0,
                ));
            }
        }
        if lane % 8 == 7 {
            let now = now_ms();
            let event = selected
                .iter()
                .filter(|m| !m.event.is_empty())
                .map(|m| (&m.event, *event_due.get(&m.event).unwrap_or(&0)))
                .filter(|(_, due)| *due <= now)
                .min_by_key(|(_, due)| *due)
                .map(|(e, _)| e.clone());
            match maintenance_action(followup.has_due(now), event.is_some(), lane / 8) {
                Maintenance::Settlement => {
                    let result = tokio::select! {r=followup.step(&mut api,&mut archive)=>Some(r),_=&mut shutdown=>None};
                    let Some(result) = result else { break };
                    used = result?;
                }
                Maintenance::Event => {
                    if let Some(event) = event {
                        event_due.insert(event.clone(), now + 15_000);
                        let path = format!("/v1/events/slug/{event}");
                        let requested = now_ms();
                        let timer = Instant::now();
                        let result =
                            tokio::select! {r=api.get(&path,false)=>Some(r),_=&mut shutdown=>None};
                        let Some(result) = result else { break };
                        match result {
                            Ok(v) => {
                                ensure!(
                                    v["event"]["slug"] == event,
                                    "selected event identity mismatch"
                                );
                                archive.write(&json!({"kind":"event_detail","refresh":"selected_event","requested_ms":requested,"received_ms":now_ms(),"duration_us":timer.elapsed().as_micros(),"path":path,"response":v}))?;
                                if v["event"]["ended"] == true {
                                    for m in selected.iter().filter(|m| m.event == event) {
                                        followup.add(&m.slug, now_ms(), false);
                                        followup.expedite(&m.slug, now_ms());
                                    }
                                    universe.candidates.retain(|_, c| c.event != event);
                                    rank_due = Instant::now();
                                } else {
                                    track_event(
                                        &v["event"],
                                        &settings,
                                        catalog.epoch,
                                        &mut universe,
                                    );
                                }
                            }
                            Err(e) => {
                                errors += 1;
                                archive.write(&json!({"kind":"request_error","feed":"selected_event","requested_ms":requested,"received_ms":now_ms(),"path":path,"error":e.to_string()}))?;
                            }
                        }
                        used = true;
                    }
                }
                Maintenance::Idle => {}
            }
            event_due.retain(|event, _| selected.iter().any(|m| &m.event == event));
        }
        let now = now_ms();
        if Instant::now() >= rank_due {
            universe.expire(now);
            let ranked = universe.ranked(&settings, now);
            queue_selected_results(&ranked, now, &mut followup);
            let new: Vec<_> = ranked
                .iter()
                .map(|c| Tracked {
                    slug: c.slug.clone(),
                    event: c.event.clone(),
                    kind: c.kind.clone(),
                })
                .collect();
            if selected.iter().map(|m| &m.slug).collect::<Vec<_>>()
                != new.iter().map(|m| &m.slug).collect::<Vec<_>>()
            {
                archive.write(&json!({"kind":"watchlist_update","received_ms":now,"ranking":"volume_desc_with_crypto_reserve","selected":ranked}))?;
            }
            let old_sports: Vec<_> = selected
                .iter()
                .filter(|m| m.kind != "crypto_up_down")
                .cloned()
                .collect();
            let new_sports: Vec<_> = new
                .iter()
                .filter(|m| m.kind != "crypto_up_down")
                .cloned()
                .collect();
            let old_crypto: Vec<_> = selected
                .iter()
                .filter(|m| m.kind == "crypto_up_down")
                .cloned()
                .collect();
            let new_crypto: Vec<_> = new
                .iter()
                .filter(|m| m.kind == "crypto_up_down")
                .cloned()
                .collect();
            index = resume_cursor(&old_sports, index, &new_sports);
            crypto_index = resume_cursor(&old_crypto, crypto_index, &new_crypto);
            selected = new;
            rank_due = Instant::now() + Duration::from_secs(1);
        }
        if !used {
            let probe = if lane % 8 == 2 || lane % 8 == 6 || selected.is_empty() {
                universe.probe(&settings, now)
            } else {
                None
            };
            let is_probe = probe.is_some();
            let chosen = probe.or_else(|| {
                if selected.is_empty() {
                    return None;
                }
                let prefer_crypto = matches!(lane % 8, 1 | 5);
                let mut pool: Vec<_> = selected
                    .iter()
                    .filter(|m| (m.kind == "crypto_up_down") == prefer_crypto)
                    .collect();
                if pool.is_empty() {
                    pool = selected.iter().collect();
                }
                let cursor = if prefer_crypto {
                    &mut crypto_index
                } else {
                    &mut index
                };
                let m = pool[*cursor % pool.len()];
                *cursor = (*cursor + 1) % pool.len();
                Some(m.slug.clone())
            });
            if let Some(slug) = chosen {
                let c = universe
                    .candidates
                    .get(&slug)
                    .context("missing candidate")?
                    .clone();
                let path = format!("/v1/markets/{slug}/book");
                let requested = now_ms();
                let timer = Instant::now();
                let result = tokio::select! {r=api.get(&path,false)=>Some(r),_=&mut shutdown=>None};
                let Some(result) = result else { break };
                if c.asset.is_some() {
                    crypto_books += 1;
                } else {
                    requests += 1;
                }
                match result {
                    Ok(v) => {
                        let received = now_ms();
                        archive.write(&json!({"kind":if is_probe{"volume_probe"}else if c.asset.is_some(){"crypto_book"}else{"book"},"probe":is_probe,"requested_ms":requested,"received_ms":received,"duration_us":timer.elapsed().as_micros(),"event":c.event,"market":slug,"path":path,"response":v}))?;
                        if let Err(e) = universe.observe(&slug, &v, received) {
                            errors += 1;
                            universe.failed(&slug, received);
                            eprintln!("volume stats unavailable: {e}");
                        } else if c.asset.is_none()
                            && v["marketData"]["state"] != "MARKET_STATE_OPEN"
                        {
                            followup.add(&slug, received, false);
                            followup.expedite(&slug, received);
                        }
                    }
                    Err(e) => {
                        errors += 1;
                        universe.failed(&slug, now_ms());
                        archive.write(&json!({"kind":"request_error","requested_ms":requested,"received_ms":now_ms(),"path":path,"error":e.to_string()}))?;
                    }
                }
                used = true;
            }
        }
        if Instant::now() >= health_due {
            let now = now_ms();
            let ranked = universe.ranked(&settings, now);
            let sports = ranked.iter().filter(|c| c.asset.is_none()).count();
            let cryptos = ranked.len() - sports;
            let health = json!({"updated_ms":now,"tracked_sports_markets":sports,"tracked_crypto_markets":cryptos,"selected_contracts":ranked.len(),"max_subscriptions":settings.max_subscriptions,"discovered_candidates":universe.candidates.len(),"unprobed_candidates":universe.unprobed(),"sports_discovery_running":catalog.running,"sports_discovery_pages":catalog.offset,"sports_book_requests":requests,"crypto_requests":crypto.requests+crypto_books,"crypto_request_errors":crypto.errors,"errors":errors+crypto.errors+followup.errors,"mode":"public_read_only","storage_policy":"preserve_all","max_disk_bytes":settings.max_disk_bytes,"archive_format":format!("{:?}",settings.archive_format),"requests_per_second_limit":settings.requests_per_second,"pending_settlements":followup.pending.len(),"settlement_requests":followup.requests,"settlement_request_errors":followup.errors});
            atomic_json(&settings.data_dir, "health.json", &health)?;
            let watchlist = json!({"updated_ms":now,"ranking":"cumulative_notional_traded_usd_desc_with_crypto_reserve","selected":ranked,"scope":{"leagues":settings.leagues,"crypto_assets":settings.crypto_assets,"max_subscriptions":settings.max_subscriptions,"crypto_reserved_slots":settings.crypto_reserved_slots,"crypto_probe_seconds":settings.crypto_probe_seconds,"min_sports_notional_usd":settings.min_sports_notional_usd,"min_crypto_notional_usd":settings.min_crypto_notional_usd,"sports_last_trade_seconds":settings.sports_last_trade_seconds,"crypto_last_trade_seconds":settings.crypto_last_trade_seconds,"probe_seconds":settings.probe_seconds,"sports_lookahead_hours":settings.sports_lookahead_hours}});
            atomic_json(&settings.data_dir, "watchlist.json", &watchlist)?;
            followup.save(&settings.data_dir)?;
            archive.flush()?;
            println!("{health}");
            health_due = Instant::now() + Duration::from_secs(10);
        }
        tokio::select! {_=tokio::time::sleep(if used{pacing}else{Duration::from_millis(100)})=>{},_=&mut shutdown=>break}
    }
    archive.write(&json!({"kind":"session_stop","received_ms":now_ms(),"book_requests":requests+crypto_books,"errors":errors+crypto.errors+followup.errors}))?;
    followup.save(&settings.data_dir)?;
    archive.close()?;
    Ok(())
}
fn atomic_json(dir: &Path, name: &str, value: &Value) -> Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(value)?)?;
    std::fs::rename(tmp, dir.join(name))?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=sig.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settlement_backlog_cannot_starve_selected_event_refreshes() {
        let work: Vec<_> = (0..120)
            .map(|turn| maintenance_action(true, true, turn))
            .collect();
        assert_eq!(
            work.iter().filter(|a| **a == Maintenance::Event).count(),
            60
        );
        assert_eq!(
            work.iter()
                .filter(|a| **a == Maintenance::Settlement)
                .count(),
            60
        );
        assert_eq!(maintenance_action(false, true, 0), Maintenance::Event);
        assert_eq!(maintenance_action(true, false, 1), Maintenance::Settlement);
    }
    #[test]
    fn closing_sports_leg_keeps_its_result_followup_after_leaving_watchlist() {
        let s: Settings = toml::from_str(include_str!("../config/collector.toml")).unwrap();
        let now = now_ms();
        let mut u = crate::watch::Universe::default();
        let event = json!({"slug":"nfl-test-game","ended":false,"startTime":chrono::DateTime::from_timestamp_millis(now as i64).unwrap().to_rfc3339(),"markets":[{"slug":"sports-leg","active":true,"closed":false,"sportsMarketType":"nfl_full_game_winner"}]});
        track_event(&event, &s, 1, &mut u);
        let response = json!({"marketData":{"marketSlug":"sports-leg","state":"MARKET_STATE_OPEN","stats":{"notionalTraded":{"currency":"USD","value":"20000"},"lastTradeSetTime":chrono::DateTime::from_timestamp_millis(now as i64).unwrap().to_rfc3339()}}});
        u.observe("sports-leg", &response, now).unwrap();
        let mut q = crate::followup::Followup::default();
        queue_selected_results(&u.ranked(&s, now), now, &mut q);
        assert_eq!(q.pending["sports-leg"].next_ms, now + 6 * 3600 * 1000);
        let mut closed = response;
        closed["marketData"]["state"] = json!("MARKET_STATE_CLOSED");
        u.observe("sports-leg", &closed, now + 1).unwrap();
        assert!(u.ranked(&s, now + 1).is_empty());
        assert!(q.pending.contains_key("sports-leg"));
        q.expedite("sports-leg", now + 1);
        assert!(q.has_due(now + 1));
    }
    #[test]
    fn upcoming_full_game_markets_are_optional_and_ended_events_stay_excluded() {
        let mut s: Settings = toml::from_str(include_str!("../config/collector.toml")).unwrap();
        let mut event = json!({"slug":"nfl-test-game", "live":false, "ended":false,
            "markets":[{"slug":"nfl-test-winner","active":true,"closed":false,
            "sportsMarketType":"nfl_full_game_winner","outcomePrices":"[\"0.5\",\"0.5\"]"}]});
        s.sports_live_only = true;
        assert!(select(&event, &s).is_empty());
        s.sports_live_only = false;
        assert_eq!(select(&event, &s).len(), 1);
        event["ended"] = json!(true);
        assert!(select(&event, &s).is_empty());
    }
    #[test]
    fn rediscovery_during_slow_cycle_does_not_starve_tail() {
        let markets: Vec<_> = (0..96)
            .map(|i| Tracked {
                slug: format!("market-{i:03}"),
                event: "event".into(),
                kind: "winner".into(),
            })
            .collect();
        let mut index = 0;
        let mut seen = std::collections::BTreeSet::new();
        // Only 82 responses fit before each rediscovery; all 96 still get sampled.
        for _ in 0..2 {
            for _ in 0..82 {
                seen.insert(markets[index].slug.clone());
                index = (index + 1) % markets.len();
            }
            index = resume_cursor(&markets, index, &markets);
        }
        assert_eq!(seen.len(), 96);
        let mut changed = markets.clone();
        changed.remove(0);
        assert_eq!(
            changed[resume_cursor(&markets, 10, &changed)].slug,
            markets[10].slug
        );
    }
}
