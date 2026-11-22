use poly_us::{
    engine::{fee, sweep},
    model::*,
};
use std::{hint::black_box, time::Instant};
fn main() {
    let b = Book {
        bids: vec![Level {
            px: 600_000,
            qty: 100 * SCALE,
        }],
        offers: vec![Level {
            px: 610_000,
            qty: 100 * SCALE,
        }],
        received_ms: 1,
        source_ms: 1,
        open: true,
    };
    let mut samples = Vec::with_capacity(100_000);
    for _ in 0..100_000 {
        let t = Instant::now();
        black_box(sweep(
            black_box(&b),
            Side::Yes,
            10 * SCALE,
            FeeKind::Standard,
            95_000,
        ));
        black_box(fee(FeeKind::Combo, 95_000, 300_000, 10 * SCALE));
        samples.push(t.elapsed().as_nanos());
    }
    samples.sort_unstable();
    println!(
        "depth sweep + combo fee, ns: p50={} p95={} p99={}; excludes JSON, signing, scheduling and network",
        samples[50_000], samples[95_000], samples[99_000]
    );
}
