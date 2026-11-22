use anyhow::Result;
use clap::{Parser, Subcommand};
use poly_us::{
    api::{Api, Auth, now_ms, safe_id},
    validation,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Read-only Polymarket US market-data and strategy validation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Replay actual-arrival crypto data; simulate delayed FOK entries and stop exits.
    AnalyzeCrypto {
        data: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 250)]
        latency_ms: u64,
        #[arg(long, default_value_t = 10)]
        contracts: u64,
    },
    /// Archive public data with bounded memory; stop at the storage cap, preserve history.
    Collect {
        #[arg(long, default_value = "config/collector.toml")]
        config: PathBuf,
    },
    /// Fetch public event, full rules and associated markets.
    Event {
        slug: String,
        #[arg(long, default_value = "run/event.json")]
        output: PathBuf,
    },
    /// Sample actual order books and test normal-settlement price dominance.
    Scan {
        #[arg(long, required = true)]
        event: Vec<String>,
        #[arg(long, default_value_t = 10)]
        rounds: usize,
        #[arg(long, default_value = "run/validation")]
        output: PathBuf,
        #[arg(long, default_value_t = 1)]
        contracts: u64,
    },
    /// Verify credentials using only GET requests; prints no account values.
    AuthCheck,
    /// Test authenticated market-data WebSocket without submitting an order.
    WsTest {
        slug: String,
        #[arg(long, default_value_t = 15)]
        seconds: u64,
    },
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::AnalyzeCrypto {
            data,
            output,
            latency_ms,
            contracts,
        } => poly_us::replay::analyze(&data, &output, latency_ms, contracts)?,
        Command::Collect { config } => poly_us::collector::run(&config).await?,
        Command::Event { slug, output } => {
            anyhow::ensure!(safe_id(&slug), "invalid event slug");
            let v = Api::new(None)?
                .get(&format!("/v1/events/slug/{slug}"), false)
                .await?;
            if let Some(p) = output.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(&output, serde_json::to_vec_pretty(&v)?)?;
            println!(
                "{}",
                serde_json::json!({"status":"ok","market_count":v["event"]["markets"].as_array().map(Vec::len),"output":output})
            );
        }
        Command::Scan {
            event,
            rounds,
            output,
            contracts,
        } => validation::scan(event, rounds, output, contracts).await?,
        Command::AuthCheck => {
            let mut api = Api::new(Some(Auth::from_env()?))?;
            for p in [
                "/v1/account/balances",
                "/v1/orders/open",
                "/v1/rfqs/user-id",
            ] {
                match api.get(p, true).await {
                    Ok(v) => println!(
                        "{}",
                        serde_json::json!({"path":p,"ok":true,"fields":v.as_object().map(|o|o.keys().collect::<Vec<_>>())})
                    ),
                    Err(e) => println!(
                        "{}",
                        serde_json::json!({"path":p,"ok":false,"error":e.to_string()})
                    ),
                }
            }
        }
        Command::WsTest { slug, seconds } => {
            use futures_util::{SinkExt, StreamExt};
            anyhow::ensure!(
                safe_id(&slug) && (1..=300).contains(&seconds),
                "invalid probe settings"
            );
            let auth = Auth::from_env()?;
            let (mut ws, _) = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio_tungstenite::connect_async(auth.ws_request(false)?),
            )
            .await??;
            ws.send(tokio_tungstenite::tungstenite::Message::Text(serde_json::json!({"subscribe":{"requestId":"validation","subscriptionType":"SUBSCRIPTION_TYPE_MARKET_DATA","marketSlugs":[slug],"responsesDebounced":false}}).to_string().into())).await?;
            let until = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
            let mut count = 0;
            let mut sources = std::collections::BTreeSet::new();
            while let Ok(msg) = tokio::time::timeout_at(until, ws.next()).await {
                let Some(msg) = msg else { break };
                match msg? {
                    tokio_tungstenite::tungstenite::Message::Text(t) => {
                        let v: serde_json::Value = serde_json::from_str(&t)?;
                        if let Some((_, b)) = poly_us::model::wire_book(&v, now_ms())? {
                            count += 1;
                            sources.insert(b.source_ms);
                        }
                    }
                    tokio_tungstenite::tungstenite::Message::Ping(p) => {
                        ws.send(tokio_tungstenite::tungstenite::Message::Pong(p))
                            .await?
                    }
                    tokio_tungstenite::tungstenite::Message::Close(_) => break,
                    _ => {}
                }
            }
            println!(
                "{}",
                serde_json::json!({"connected":true,"book_messages":count,"unique_timestamps":sources.len()})
            );
            ws.close(None).await?;
        }
    }
    Ok(())
}
