//! Immutable archives. Quota exhaustion stops collection; no archive is pruned.
use crate::{api::now_ms, collector::Settings};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, write::GzEncoder};
use parquet::{
    basic::{Compression as Codec, ZstdLevel},
    data_type::{ByteArray, ByteArrayType, Int64Type},
    file::{
        properties::{EnabledStatistics, WriterProperties},
        writer::SerializedFileWriter,
    },
    schema::{parser::parse_message_type, types::ColumnPath},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Gzip,
    Parquet,
}

const RESERVE: u64 = 32 * 1024 * 1024;
const MAX_RECORD: usize = 16 * 1024 * 1024;
const SCHEMA: &str = "message market_data {
 REQUIRED BINARY kind (UTF8);
 REQUIRED INT64 received_ms;
 REQUIRED INT64 requested_ms;
 REQUIRED BINARY market (UTF8);
 REQUIRED BINARY event (UTF8);
 REQUIRED BINARY path (UTF8);
 REQUIRED BINARY payload_json (UTF8);
}";

#[derive(Default)]
struct Batch {
    text: [Vec<ByteArray>; 5],
    received: Vec<i64>,
    requested: Vec<i64>,
    bytes: u64,
}
impl Batch {
    fn push(&mut self, v: &Value, record: Vec<u8>) {
        for (i, key) in ["kind", "market", "event", "path"].iter().enumerate() {
            // Crypto book slugs live in the response, rather than the envelope.
            let value = v[*key].as_str().unwrap_or_else(|| {
                if *key == "market" {
                    v["response"]["marketData"]["marketSlug"]
                        .as_str()
                        .unwrap_or("")
                } else {
                    ""
                }
            });
            self.text[i].push(ByteArray::from(value.as_bytes().to_vec()));
        }
        self.received.push(v["received_ms"].as_i64().unwrap_or(0));
        self.requested.push(v["requested_ms"].as_i64().unwrap_or(0));
        self.bytes += record.len() as u64;
        self.text[4].push(ByteArray::from(record));
    }
    fn write(&mut self, writer: &mut SerializedFileWriter<File>) -> Result<()> {
        if self.received.is_empty() {
            return Ok(());
        }
        let mut group = writer.next_row_group()?;
        for i in 0..7 {
            let mut col = group.next_column()?.context("missing parquet column")?;
            match i {
                1 => {
                    col.typed::<Int64Type>()
                        .write_batch(&self.received, None, None)?;
                }
                2 => {
                    col.typed::<Int64Type>()
                        .write_batch(&self.requested, None, None)?;
                }
                _ => {
                    let j = if i == 0 { 0 } else { i - 2 };
                    col.typed::<ByteArrayType>()
                        .write_batch(&self.text[j], None, None)?;
                }
            }
            col.close()?;
        }
        group.close()?;
        *self = Self::default();
        Ok(())
    }
}

enum Sink {
    Gzip(GzEncoder<File>),
    Parquet(Box<SerializedFileWriter<File>>),
}

pub struct Archive {
    settings: Settings,
    sink: Option<Sink>,
    current: PathBuf,
    opened: Instant,
    bytes: u64,
    sequence: u64,
    batch: Batch,
    batch_flushed: Instant,
    journal: Option<GzEncoder<File>>,
    journal_path: PathBuf,
    checked: Instant,
    used: u64,
    available: u64,
    charged: u64,
}
fn directory_bytes(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let m = entry.metadata()?;
        // Follow no symlinks. Data belongs in the dedicated bind mount.
        if entry.file_type()?.is_file() {
            total = total.saturating_add(m.len());
        } else if entry.file_type()?.is_dir() {
            total = total.saturating_add(directory_bytes(&entry.path())?);
        }
    }
    Ok(total)
}
impl Archive {
    pub fn new(settings: Settings) -> Result<Self> {
        settings.validate()?;
        std::fs::create_dir_all(&settings.data_dir)?;
        let used = directory_bytes(&settings.data_dir)?;
        let available = fs2::available_space(&settings.data_dir)?;
        let mut a = Self {
            settings,
            sink: None,
            current: PathBuf::new(),
            opened: Instant::now(),
            bytes: 0,
            sequence: 0,
            batch: Batch::default(),
            batch_flushed: Instant::now(),
            journal: None,
            journal_path: PathBuf::new(),
            checked: Instant::now(),
            used,
            available,
            charged: 0,
        };
        a.check_room(0)?;
        a.open()?;
        std::fs::write(
            a.settings.data_dir.join("storage_status.json"),
            serde_json::to_vec(
                &json!({"state":"collecting","updated_ms":now_ms(),"used_bytes":a.used,"max_disk_bytes":a.settings.max_disk_bytes,"preserve_all":true}),
            )?,
        )?;
        Ok(a)
    }
    fn check_room(&mut self, additional: u64) -> Result<()> {
        // Account in conservative *uncompressed* bytes between filesystem scans.
        if self.checked.elapsed().as_secs() >= 10 || self.charged >= RESERVE {
            self.used = directory_bytes(&self.settings.data_dir)?;
            self.available = fs2::available_space(&self.settings.data_dir)?;
            self.charged = self.batch.bytes;
            self.checked = Instant::now();
        }
        let reserve = self
            .charged
            .saturating_add(additional)
            .saturating_add(RESERVE);
        let quota_ok = self.used.saturating_add(reserve) <= self.settings.max_disk_bytes;
        let free_ok =
            self.available >= reserve.saturating_add(self.settings.min_free_disk_mb * 1024 * 1024);
        if !quota_ok || !free_ok {
            let status = json!({"state":"storage_paused","updated_ms":now_ms(),"used_bytes":self.used,"max_disk_bytes":self.settings.max_disk_bytes,"free_bytes":self.available,"preserve_all":true});
            // The small status write fits within the reserved space. Never remove data.
            let _ = std::fs::write(
                self.settings.data_dir.join("storage_status.json"),
                serde_json::to_vec(&status)?,
            );
            anyhow::bail!(
                "storage limit reached; collection stopped, all archives preserved: {status}"
            );
        }
        self.charged = self.charged.saturating_add(additional);
        Ok(())
    }
    fn open(&mut self) -> Result<()> {
        self.check_room(0)?;
        let suffix = match self.settings.archive_format {
            Format::Gzip => "jsonl.gz",
            Format::Parquet => "parquet",
        };
        let file = loop {
            self.sequence += 1;
            let final_path = self.settings.data_dir.join(format!(
                "market-{}-{}-{:06}.{suffix}",
                now_ms(),
                std::process::id(),
                self.sequence
            ));
            if final_path.exists()
                || (self.settings.archive_format == Format::Parquet
                    && (final_path.with_extension("jsonl.gz").exists()
                        || final_path.with_extension("jsonl.gz.open").exists()))
            {
                continue;
            }
            self.current = PathBuf::from(format!("{}.open", final_path.display()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.current)
            {
                Ok(f) => break f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        if self.settings.archive_format == Format::Parquet {
            self.journal_path = self
                .current
                .with_extension("")
                .with_extension("jsonl.gz.open");
            let journal = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.journal_path)?;
            self.journal = Some(GzEncoder::new(journal, Compression::fast()));
        }
        self.sink = Some(match self.settings.archive_format {
            Format::Gzip => Sink::Gzip(GzEncoder::new(file, Compression::fast())),
            Format::Parquet => {
                let mut props = WriterProperties::builder()
                    .set_compression(Codec::ZSTD(ZstdLevel::try_new(3)?))
                    .set_dictionary_enabled(false)
                    .set_data_page_size_limit(256 * 1024)
                    .set_write_batch_size(256)
                    .set_column_statistics_enabled(
                        ColumnPath::from("payload_json"),
                        EnabledStatistics::None,
                    );
                for key in ["kind", "market", "event", "path"] {
                    props = props.set_column_dictionary_enabled(ColumnPath::from(key), true);
                }
                Sink::Parquet(Box::new(SerializedFileWriter::new(
                    file,
                    Arc::new(parse_message_type(SCHEMA)?),
                    Arc::new(props.build()),
                )?))
            }
        });
        self.opened = Instant::now();
        self.bytes = 0;
        Ok(())
    }
    pub fn write(&mut self, v: &Value) -> Result<()> {
        let record = serde_json::to_vec(v)?;
        ensure!(record.len() <= MAX_RECORD, "oversize archive record");
        let size = record.len() as u64 + 1;
        self.check_room(
            size * if self.settings.archive_format == Format::Parquet {
                2
            } else {
                1
            },
        )?;
        if let Some(journal) = &mut self.journal {
            journal.write_all(&record)?;
            journal.write_all(b"\n")?;
        }
        match self.sink.as_mut().context("archive closed")? {
            Sink::Gzip(e) => {
                e.write_all(&record)?;
                e.write_all(b"\n")?;
            }
            Sink::Parquet(w) => {
                self.batch.push(v, record);
                if self.batch.bytes >= self.settings.row_group_mb * 1024 * 1024
                    || self.batch.received.len() >= 4096
                {
                    self.batch.write(w)?;
                    self.batch_flushed = Instant::now();
                }
            }
        }
        self.bytes += size;
        if self.bytes >= self.settings.rotate_uncompressed_mb * 1024 * 1024
            || self.opened.elapsed().as_secs() >= self.settings.rotate_seconds
        {
            self.close()?;
            self.open()?;
        }
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        if let Some(journal) = &mut self.journal {
            journal.flush()?;
            journal.get_ref().sync_data()?;
        }
        match self.sink.as_mut() {
            Some(Sink::Gzip(e)) => e.flush()?,
            Some(Sink::Parquet(w)) => {
                // Avoid tiny row groups at every health tick. At most 60 seconds
                // of small records stay buffered; .open requires a clean footer.
                if self.batch_flushed.elapsed().as_secs() >= 60 {
                    self.batch.write(w)?;
                    self.batch_flushed = Instant::now();
                }
                w.inner().sync_data()?;
            }
            None => {}
        }
        Ok(())
    }
    pub fn close(&mut self) -> Result<()> {
        if let Some(journal) = self.journal.take() {
            journal.finish()?.sync_all()?;
            publish(&self.journal_path)?;
        }
        if let Some(sink) = self.sink.take() {
            let file = match sink {
                Sink::Gzip(e) => e.finish()?,
                Sink::Parquet(mut w) => {
                    self.batch.write(&mut w)?;
                    w.into_inner()?
                }
            };
            file.sync_all()?;
            // hard_link fails if the destination exists, unlike rename which
            // can overwrite an old archive. Unlink only the temporary name.
            publish(&self.current)?;
            File::open(&self.settings.data_dir)?.sync_all()?;
        }
        Ok(())
    }
}
impl Drop for Archive {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn publish(open: &Path) -> Result<()> {
    std::fs::hard_link(open, open.with_extension(""))?;
    std::fs::remove_file(open)?;
    Ok(())
}

/// Select exactly one representation of each segment. Parquet is a lossless
/// materialization of its paired raw journal, not additional observations.
pub fn finalized_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.to_string_lossy();
        if name.ends_with(".parquet") {
            files.push(path);
        } else if name.ends_with(".jsonl.gz") {
            let sibling = path.with_extension("").with_extension("parquet");
            if !sibling.exists() {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

pub fn records(path: &Path) -> Result<Box<dyn Iterator<Item = Result<Value>>>> {
    use parquet::{
        file::serialized_reader::SerializedFileReader,
        record::{RowAccessor, reader::RowIter},
    };
    use std::io::{BufRead, BufReader, Read};
    let file = File::open(path)?;
    if path.extension().is_some_and(|e| e == "parquet") {
        let reader = SerializedFileReader::new(file)?;
        let rows = RowIter::from_file_into(Box::new(reader));
        return Ok(Box::new(rows.map(|row| {
            let row = row?;
            let json = row.get_string(6)?;
            ensure!(json.len() <= MAX_RECORD, "oversize parquet record");
            Ok(serde_json::from_str(json)?)
        })));
    }
    let mut reader = BufReader::new(flate2::read::GzDecoder::new(file));
    Ok(Box::new(std::iter::from_fn(move || {
        let mut buf = Vec::new();
        let result = reader
            .by_ref()
            .take(MAX_RECORD as u64 + 1)
            .read_until(b'\n', &mut buf);
        match result {
            Ok(0) => None,
            Ok(n) => Some((|| {
                ensure!(
                    n <= MAX_RECORD && buf.last() == Some(&b'\n'),
                    "oversize or partial archive record"
                );
                Ok(serde_json::from_slice(&buf)?)
            })()),
            Err(e) => Some(Err(e.into())),
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publishing_never_overwrites_a_previous_archive() {
        let dir = std::env::temp_dir().join(format!("poly-publish-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let open = dir.join("market.parquet.open");
        let old = dir.join("market.parquet");
        std::fs::write(&open, b"new").unwrap();
        std::fs::write(&old, b"old").unwrap();
        assert!(publish(&open).is_err());
        assert_eq!(std::fs::read(&open).unwrap(), b"new");
        assert_eq!(std::fs::read(&old).unwrap(), b"old");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
