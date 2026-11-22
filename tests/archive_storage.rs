use poly_us::{
    archive::{self, Archive, Format},
    collector::Settings,
};
use serde_json::{Value, json};
use std::{
    fs::{File, FileTimes},
    path::PathBuf,
    time::{Duration, SystemTime},
};

fn fixture(label: &str) -> (PathBuf, Settings) {
    let dir = std::env::temp_dir().join(format!("poly-storage-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut s: Settings = toml::from_str(include_str!("../config/collector.toml")).unwrap();
    s.data_dir = dir.clone();
    s.min_free_disk_mb = 1;
    s.max_disk_bytes = 256 * 1024 * 1024;
    s.rotate_uncompressed_mb = 2;
    s.row_group_mb = 1;
    (dir, s)
}

#[test]
fn parquet_and_raw_journals_round_trip_without_double_counting() {
    let (dir, s) = fixture("roundtrip");
    let expected: Vec<Value> = (0..42).map(|i| json!({"kind":"crypto_book","received_ms":i+1,"requested_ms":i,"response":{"marketData":{"marketSlug":"btc-test"},"raw":format!("{i}{}", "x".repeat(70_000))}})).collect();
    let mut a = Archive::new(s).unwrap();
    for v in &expected {
        a.write(v).unwrap();
    }
    a.flush().unwrap();
    a.close().unwrap();
    let files = archive::finalized_files(&dir).unwrap();
    assert!(
        files.len() >= 2,
        "size rotation must produce multiple larger segments"
    );
    assert!(files.iter().all(|p| p.extension().unwrap() == "parquet"));
    let mut actual = vec![];
    for p in &files {
        let parquet: Vec<_> = archive::records(p).unwrap().map(Result::unwrap).collect();
        let raw = p.with_extension("jsonl.gz");
        let journal: Vec<_> = archive::records(&raw)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(parquet, journal);
        actual.extend(parquet);
    }
    assert_eq!(actual, expected);
    // Legacy gzip is still selected alongside the new paired segments.
    let legacy = dir.join("market-0000000000000-legacy.jsonl.gz");
    std::fs::copy(files[0].with_extension("jsonl.gz"), &legacy).unwrap();
    assert_eq!(
        archive::finalized_files(&dir).unwrap().len(),
        files.len() + 1
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn legacy_retention_never_deletes_and_quota_stops_before_opening() {
    let (dir, mut s) = fixture("preservation");
    let old = dir.join("market-old.jsonl.gz");
    let orphan = dir.join("market-orphan.parquet.open");
    std::fs::write(&old, b"immutable old data").unwrap();
    std::fs::write(&orphan, b"recover this partial file").unwrap();
    let modified = SystemTime::now() - Duration::from_secs(120 * 86400);
    File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_times(FileTimes::new().set_modified(modified))
        .unwrap();
    s.retention_days = 1;
    s.max_disk_mb = 64; // Accepted legacy config, with no deletion behavior.
    s.archive_format = Format::Gzip;
    let mut a = Archive::new(s.clone()).unwrap();
    a.write(&json!({"kind":"book","received_ms":1})).unwrap();
    a.close().unwrap();
    assert_eq!(std::fs::read(&old).unwrap(), b"immutable old data");
    assert_eq!(
        std::fs::read(&orphan).unwrap(),
        b"recover this partial file"
    );
    assert_eq!(
        std::fs::metadata(&old).unwrap().modified().unwrap(),
        modified
    );
    // A sparse fixture exercises logical quota accounting without allocating 96 MiB.
    let huge = dir.join("untouched-history.parquet");
    File::create(&huge)
        .unwrap()
        .set_len(96 * 1024 * 1024)
        .unwrap();
    s.max_disk_bytes = 64 * 1024 * 1024;
    let before = std::fs::read_dir(&dir).unwrap().count();
    assert!(Archive::new(s).is_err());
    assert_eq!(std::fs::metadata(&huge).unwrap().len(), 96 * 1024 * 1024);
    assert_eq!(std::fs::read(&old).unwrap(), b"immutable old data");
    assert_eq!(
        std::fs::read(&orphan).unwrap(),
        b"recover this partial file"
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), before); // existing status updated, no new segment
    let status: Value =
        serde_json::from_slice(&std::fs::read(dir.join("storage_status.json")).unwrap()).unwrap();
    assert_eq!(status["state"], "storage_paused");
    std::fs::remove_dir_all(dir).unwrap();
}
