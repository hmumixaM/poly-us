use poly_us::{
    crypto::{Contract, IndexHistory, Sample, estimate},
    model::SCALE,
    replay::settlement,
};
use std::collections::BTreeMap;
fn contract() -> Contract {
    Contract {
        slug: "test".into(),
        asset: "btc".into(),
        index: "BRTI".into(),
        start: 700,
        end: 1000,
        beat: Some(100 * SCALE),
        decimals: 2,
        tick: 10_000,
        lot: 10_000,
        theta: 95_000,
        rules: "simple average of 60 BRTI prices".into(),
    }
}
#[test]
fn probability_never_reads_future_or_not_yet_received_prices() {
    let c = contract();
    let mut h = IndexHistory::default();
    h.samples.insert(
        "btc".into(),
        (800..1000)
            .map(|t| {
                (
                    t,
                    Sample {
                        price: 100 * SCALE + (t % 2) * 100_000,
                        received_ms: (t + 1) * 1000,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
    );
    let a = estimate(&c, &h, 990_000).unwrap();
    assert_eq!(a.known_seconds, 50);
    assert_eq!(a.remaining_seconds, 10);
    for (_, s) in h.samples.get_mut("btc").unwrap().range_mut(990..) {
        s.price = 999 * SCALE;
    }
    let b = estimate(&c, &h, 990_000).unwrap();
    assert_eq!(a.probability_up, b.probability_up);
    h.samples
        .get_mut("btc")
        .unwrap()
        .get_mut(&960)
        .unwrap()
        .received_ms = 990_001;
    assert!(estimate(&c, &h, 990_000).is_none());
}
#[test]
fn settlement_uses_exact_pre_boundary_window_and_rejects_gaps() {
    let c = contract();
    let mut h = IndexHistory::default();
    h.samples.insert(
        "btc".into(),
        (940..=1000)
            .map(|t| {
                (
                    t,
                    Sample {
                        price: if t == 1000 { 1 } else { 100 * SCALE + 6000 },
                        received_ms: 1_002_000,
                    },
                )
            })
            .collect(),
    );
    assert_eq!(settlement(&c, &h, 1_002_000), Some(true));
    h.samples.get_mut("btc").unwrap().remove(&940);
    assert_eq!(settlement(&c, &h, 1_002_000), None);
}

#[test]
fn probability_models_the_rounded_settlement_event() {
    let c = contract();
    let mut h = IndexHistory::default();
    h.samples.insert(
        "btc".into(),
        (800..999)
            .map(|t| {
                (
                    t,
                    Sample {
                        price: 99 * SCALE + 999_000 + (t % 2),
                        received_ms: (t + 1) * 1000,
                    },
                )
            })
            .collect(),
    );
    let e = estimate(&c, &h, 999_000).unwrap();
    assert_eq!(e.known_seconds, 59);
    assert!(e.probability_up > 0.999);
}

#[test]
fn replay_fills_only_a_later_distinct_exchange_snapshot() {
    use serde_json::json;
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!("poly-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let c = contract();
    let iso = |t| chrono::DateTime::from_timestamp(t, 0).unwrap().to_rfc3339();
    let metadata = json!({"markets":[{"slug":"test","active":true,"closed":false,"description":c.rules,"orderPriceMinTickSize":0.01,"minimumTradeQty":0.01,"feeCoefficient":0.095,"assetPriceTerms":{"marketType":"ASSET_PRICE_MARKET_TYPE_UP_DOWN","asset":{"assetClass":"ASSET_CLASS_CRYPTO","symbol":"btc"},"indexSymbol":"BRTI","windowStart":iso(700),"windowEnd":iso(1000),"priceToBeat":{"value":"100.00","currency":"USD"},"priceDecimals":2}}]});
    let reference = |end: u64| json!({"series":[{"asset":{"assetClass":"ASSET_CLASS_CRYPTO","symbol":"btc"},"bucket":"ASSET_PRICE_BUCKET_1S","currency":"USD","candles":(800..end).map(|t|json!({"timestamp":t,"close":if t%2==0{"99.999000"}else{"99.999001"},"tickCount":1})).collect::<Vec<_>>()}]});
    let book = |ts: &str| json!({"marketData":{"marketSlug":"test","bids":[{"px":{"value":"0.49","currency":"USD"},"qty":"10"}],"offers":[{"px":{"value":"0.50","currency":"USD"},"qty":"10"}],"state":"MARKET_STATE_OPEN","transactTime":ts}});
    let records = vec![
        json!({"kind":"crypto_markets","received_ms":980000,"response":metadata}),
        json!({"kind":"asset_prices","requested_ms":990000,"received_ms":990000,"response":reference(990)}),
        json!({"kind":"crypto_book","received_ms":990100,"response":book("1970-01-01T00:16:30Z")}),
        json!({"kind":"crypto_book","received_ms":990500,"response":book("1970-01-01T00:16:30Z")}),
        json!({"kind":"crypto_book","received_ms":990700,"response":book("1970-01-01T00:16:30.600Z")}),
        json!({"kind":"asset_prices","requested_ms":1002000,"received_ms":1002000,"response":reference(1000)}),
    ];
    let file = std::fs::File::create(dir.join("market-test.jsonl.gz")).unwrap();
    let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    for r in records {
        writeln!(gz, "{r}").unwrap();
    }
    gz.finish().unwrap();
    let out = dir.join("result.jsonl");
    poly_us::replay::analyze(&dir, &out, 250, 10).unwrap();
    let records: Vec<serde_json::Value> = std::fs::read_to_string(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let entries: Vec<_> = records
        .iter()
        .filter(|r| r["kind"] == "simulated_entry")
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["received_ms"], 990700);
    assert_eq!(
        records.last().unwrap()["report"]["duplicate_or_backwards_books_skipped"],
        1
    );
    std::fs::remove_dir_all(dir).unwrap();
}
