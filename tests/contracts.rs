use poly_us::{
    collector::{Archive, Settings},
    engine::{fee, sweep},
    model::*,
    validation::{Instrument, instruments, observe},
};
use serde_json::json;
use std::io::Read;

#[test]
fn fees_can_erase_a_gross_arbitrage_and_no_uses_bid_complement() {
    let b = Book {
        bids: vec![Level {
            px: 530_000,
            qty: 100 * SCALE,
        }],
        offers: vec![Level {
            px: 490_000,
            qty: 100 * SCALE,
        }],
        received_ms: 100,
        source_ms: 100,
        open: true,
    };
    let a = Instrument {
        event: "e".into(),
        slug: "easy".into(),
        kind: "total".into(),
        group: "g".into(),
        difficulty: 1,
        tick: 10_000,
        lot: SCALE,
        theta: 95_000,
        rules: "r".into(),
    };
    let mut harder = a.clone();
    harder.slug = "hard".into();
    harder.difficulty = 2;
    let signal = observe(&a, &harder, &b, &b, 100 * SCALE, 100).unwrap();
    assert_eq!(signal.gross_edge, (4 * SCALE) as i64);
    assert!(signal.net_edge < 0);
    assert_eq!(
        sweep(&b, Side::No, 100 * SCALE, FeeKind::Standard, 95_000)
            .unwrap()
            .limit,
        530_000
    );
    assert!(fee(FeeKind::Combo, 95_000, 500_000, MAX_QTY) > 0);
}

#[test]
fn unknown_account_uses_conservative_theta_and_rules_stay_grouped() {
    let market = |slug: &str, line: i64, rules: &str| json!({"slug":slug,"active":true,"closed":false,"sportsMarketType":"football_team_full_game_total","line":format!("{line}.5").parse::<serde_json::Number>().unwrap(),"description":rules,"orderPriceMinTickSize":0.01,"minimumTradeQty":0.01,"feeCoefficient":0.0695});
    let v = json!({"event":{"slug":"game","markets":[market("a",40,"This market settles if A and B combine for over 40.5 points. Overtime included."),market("b",42,"This market settles if A and B combine for over 42.5 points. Overtime included."),market("c",42,"This market settles if A and B combine for over 42.5 points. Regulation only.")]}});
    let m = instruments(&v).unwrap();
    assert_eq!(m[0].theta, 95_000);
    assert_eq!(m[0].group, m[1].group);
    assert_ne!(m[0].group, m[2].group);
}

#[test]
fn book_depth_is_not_limited_by_order_size() {
    let b = Book {
        bids: vec![Level {
            px: 500_000,
            qty: 2 * MAX_QTY,
        }],
        offers: vec![],
        received_ms: 100,
        source_ms: 100,
        open: true,
    };
    b.validate(&Market {
        slug: "x".into(),
        fee: FeeKind::Standard,
        tick: 10_000,
        lot: 10_000,
    })
    .unwrap();
    assert_eq!(
        sweep(&b, Side::No, SCALE, FeeKind::Standard, 95_000)
            .unwrap()
            .cash,
        500_000
    );
}

#[test]
fn archive_round_trips_and_quota_does_not_delete_unrelated_files() {
    let dir = std::env::temp_dir().join(format!("poly-archive-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let s = Settings {
        data_dir: dir.clone(),
        requests_per_second: 8,
        discover_seconds: 60,
        max_events: 1,
        markets_per_event: 2,
        max_discovery_pages: 2,
        max_subscriptions: 100,
        crypto_reserved_slots: 12,
        crypto_probe_seconds: 15,
        min_sports_notional_usd: 10000,
        min_crypto_notional_usd: 1000,
        sports_last_trade_seconds: 86400,
        crypto_last_trade_seconds: 120,
        probe_seconds: 900,
        rotate_seconds: 60,
        rotate_uncompressed_mb: 1,
        retention_days: 1,
        max_disk_mb: 64,
        max_disk_bytes: 256 * 1024 * 1024,
        min_free_disk_mb: 1,
        archive_format: poly_us::archive::Format::Gzip,
        row_group_mb: 1,
        leagues: vec!["nfl".into()],
        sports_live_only: true,
        sports_lookahead_hours: 72,
        crypto_assets: vec!["btc".into()],
    };
    std::fs::write(dir.join("unrelated.txt"), "keep").unwrap();
    let mut a = Archive::new(s).unwrap();
    a.write(&json!({"kind":"book","response":{"bids":[]}}))
        .unwrap();
    a.close().unwrap();
    drop(a);
    let file = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with(".gz"))
        .unwrap();
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(file).unwrap())
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(text.trim()).unwrap()["kind"],
        "book"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("unrelated.txt")).unwrap(),
        "keep"
    );
    std::fs::remove_dir_all(dir).unwrap();
}
