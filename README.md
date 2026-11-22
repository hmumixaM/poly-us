# poly-us

[![CI](https://github.com/hmumixaM/poly-us/actions/workflows/ci.yml/badge.svg)](https://github.com/hmumixaM/poly-us/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A lightweight Rust collector and arrival-time replay toolkit for **Polymarket US**.
It archives sports and short-window crypto markets, then tests explicit research
baselines against observed depth, fees and delayed execution.

The CLI is read-only: it does not place or accept orders. The library contains
experimental order-construction/client helpers; they are not exposed as live
execution commands and have not been validated as a production trading system.
This project targets the US API, not the international CLOB/Polygon SDK.

## Features

- Incremental discovery of live and upcoming sports, including full contract rules,
  scores/status, fee coefficients and full-game winner/spread/total/team-total lines.
- BTC, ETH, SOL and XRP Up/Down contracts up to one hour, selected from typed terms:
  index identity, price-to-beat, exact windows and settlement precision.
- Public order-book snapshots, overlapping one-second reference-index history,
  source/arrival timestamps, watchlist changes and explicit request-error records.
- At most 100 admitted contracts by default. Volume and recent-trade filters select
  active instruments; eligible crypto has reserved priority slots within that budget.
- Parquet + Zstd archives with matching gzip raw journals. Replay selects each paired
  segment once and also reads legacy gzip archives.
- No automatic history deletion. Collection stops before the configured storage cap.
- Restart-persistent final-terms and payout follow-up, with fair scheduling between
  recovery, discovery, admitted books and event/settlement maintenance.

These are research tools. A displayed spread or model signal is not evidence of
an executable profit. REST snapshots do not establish millisecond execution,
queue position, atomic multi-leg fills or guaranteed stop exits.

## Quick start: Docker

Use a non-root host account with Docker Compose. Public collection needs no API key.
Run from the repository root:

```sh
git clone https://github.com/hmumixaM/poly-us.git
cd poly-us
mkdir -p data
printf 'POLY_US_UID=%s\nPOLY_US_GID=%s\nPOLY_US_SOURCE_COMMIT=%s\n' \
  "$(id -u)" "$(id -g)" "$(git rev-parse HEAD)" > .env
docker compose up -d --build
docker compose logs --tail 10 collector
```

The Compose service mounts `config/collector.toml` read-only and writes only to
`data/`. Its root filesystem is read-only, it exposes no port, and it requests a
192 MiB memory/swap ceiling. CPU shares are a scheduling weight, not a hard cap;
PID/CPU constraints depend on the host kernel. There is one collector per data
directory, enforced with a process-held file lock.

Inspect `data/health.json`, `data/watchlist.json` and `data/storage_status.json`.
Use `docker compose stop` for a graceful archive checkpoint and
`docker compose start` to resume. Configuration changes require container recreation
with `docker compose up -d --force-recreate collector`.

## Quick start: native Rust

The manifest declares Rust 1.88 or newer; CI uses Rust 1.92.0 on Linux. The filesystem
locking and archive publication paths are intended for POSIX hosts.

```sh
cargo build --release --locked
mkdir -p run
sed 's|data_dir = "/data"|data_dir = "run/data"|' \
  config/collector.toml > run/collector.local.toml
cargo run --release --locked -- collect --config run/collector.local.toml
```

`Ctrl-C` or `SIGTERM` requests a clean stop and archive finalization. For native
build provenance, set `SOURCE_COMMIT` to the checkout's Git SHA when building.

## Default collection policy

| Setting | Default |
|---|---|
| Shared request pacing | At most 15 requests/s, one request at a time |
| Watch budget | 100 sports + crypto contracts total |
| Crypto priority | Up to 12 reserved slots; unused slots are borrowed |
| Sports | NFL, CFB, NBA, CBB, NHL, MLB, MLS, EPL; live or starting within 72 hours |
| Sports admission | USD `notionalTraded` >= 10,000; last trade within 24 hours |
| Crypto admission | USD `notionalTraded` >= 1,000; last trade within 120 seconds |
| Archive rotation | Target 512 MiB uncompressed input or 6 hours |
| Parquet batches | Target 8 MiB or 4,096 records, with partial-batch flushing |
| Storage budget | Decimal 1 TB across raw journals, Parquet and unfinished files |
| Free filesystem reserve | At least 1 GiB plus conservative write headroom |

The volume field is treated as the API's cumulative USD notional, not as a
24-hour turnover field. Catalog discovery has no event, per-event-line or page
cutoff. Rankings are provisional until candidates have been probed; health reports
both discovered and unprobed counts. This is a strategy allocation, not a claim
that the list is the exchange-wide top 100 by volume.

Low-volume sports becomes eligible for another probe after 15 minutes; crypto after
15 seconds. Failed books have a separate five-second retry deadline. Actual cadence
also depends on network time and ready work. Never infer freshness from polling
frequency alone. Respect the current [official rate limits](https://docs.polymarket.us/api-reference/rate-limits);
using multiple API keys does not create independent counters for a shared source IP.

`max_disk_bytes` counts all regular files below the dedicated data directory.
At the limit, the collector records `storage_paused` and exits without pruning
history. A restart policy may retry, but writes cannot resume until the configured
budget or available space permits them. Keep unrelated writers out of this directory;
this is an application budget, not a filesystem quota.

The legacy `retention_days`, `max_disk_mb`, `max_events`, `markets_per_event` and
`max_discovery_pages` settings are accepted for compatibility and ignored.

## Recorded data

Parquet has metadata columns `kind`, `received_ms`, `requested_ms`, `market`,
`event` and `path`, plus a lossless `payload_json` column containing the complete
record. Absent projected fields use zero/empty values. Records include:

- `event_discovery`, `event_detail`: catalog, rules, game status and fee evidence.
- `crypto_markets`, `asset_prices`: typed contract terms and reference-index history.
- `book`, `crypto_book`: admitted order-book snapshots, including source timestamps.
- `volume_probe`, `watchlist_update`: candidate evidence and arrival-time membership.
- `market_final`, `market_settlement`: final terms/index values and official payouts.
- `request_error`, `session_start`, `session_stop`: failures, provenance and counters.

Both `.parquet` and `.jsonl.gz` representations are retained and count toward the
storage budget. `.open` files remain unfinished until footer completion and fsync.
Raw journals are periodically flushed; a crash may leave partial files that are
preserved for recovery. In-memory/unflushed records are not crash durable.
`pending_settlements.json` is checkpointed periodically and on a clean exit.
Do not remove a live `collector.lock` file or mix concurrent collector directories.

## Replay and validation

```sh
cargo run --release --locked -- analyze-crypto run/data \
  --output run/replay-001.jsonl --latency-ms 250 --contracts 10
cargo run --release --locked -- --help
```

Each replay output must be a new file. `analyze-crypto` implements two separate
baselines, late direction and last-minute jumps. It preserves first-arrival index
knowledge, rejects duplicate/backwards source updates, walks depth and defers
FOK-like entry/exit checks until a later observed snapshot. Reconstructed settlement
remains provisional until reconciled with official values. Unresolved outcomes
are not counted as wins. The probability model is not independently calibrated.

The `scan` command evaluates normal-settlement price relationships from public
sports snapshots. It does not obtain executable combo/RFQ quotes or simulate
atomic multi-leg execution. See [research methods and data requirements](docs/research-methods.md).

Authenticated `auth-check` and `ws-test` diagnostics are optional. They read
`POLYMARKET_US_KEY_ID` and `POLYMARKET_US_SECRET_KEY` from the process environment.
Account permissions and authenticated feeds must be verified separately; no key
or account-specific connectivity is bundled with this repository.

## Development

```sh
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo bench --bench engine
```

The benchmark measures local depth/fee calculations only. It excludes networking,
JSON, signing, scheduling and execution latency. Tests use synthetic fixtures and
local temporary directories; they do not need credentials or submit orders.

## License and data

Code is licensed under [MIT](LICENSE). Captured market data, credentials, private
discussions and deployment logs are not distributed with this project. The code
license does not grant rights to redistribute third-party market data or documents.
