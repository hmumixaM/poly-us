use crate::model::*;
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue};
use zeroize::Zeroizing;

pub const API: &str = "https://api.polymarket.us";
pub const PUBLIC: &str = "https://gateway.polymarket.us";
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}

pub struct Auth {
    id: String,
    key: SigningKey,
}
impl Auth {
    pub fn from_env() -> Result<Self> {
        let id = std::env::var("POLYMARKET_US_KEY_ID")
            .context("set POLYMARKET_US_KEY_ID in your shell")?;
        let secret = Zeroizing::new(
            std::env::var("POLYMARKET_US_SECRET_KEY")
                .context("set POLYMARKET_US_SECRET_KEY in your shell; do not paste into chat")?,
        );
        Self::new(id, &secret)
    }
    pub fn new(id: String, secret: &str) -> Result<Self> {
        ensure!(!id.is_empty(), "empty API key id");
        let bytes = Zeroizing::new(
            STANDARD
                .decode(secret)
                .context("secret must be base64 Ed25519 material")?,
        );
        ensure!(
            bytes.len() == 32 || bytes.len() == 64,
            "secret must contain 32 or 64 bytes"
        );
        let seed = Zeroizing::new(<[u8; 32]>::try_from(&bytes[..32])?);
        Ok(Self {
            id,
            key: SigningKey::from_bytes(&seed),
        })
    }
    pub fn headers(
        &self,
        method: &str,
        path: &str,
        timestamp: u64,
    ) -> Result<reqwest::header::HeaderMap> {
        // The signing preimage excludes query string and request body, per US docs.
        let path = path.split('?').next().unwrap_or(path);
        let ts = timestamp.to_string();
        let msg = format!("{ts}{method}{path}");
        let sig = STANDARD.encode(self.key.sign(msg.as_bytes()).to_bytes());
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-pm-access-key", self.id.parse()?);
        headers.insert("x-pm-timestamp", ts.parse()?);
        headers.insert("x-pm-signature", sig.parse()?);
        Ok(headers)
    }
    pub fn ws_request(
        &self,
        private: bool,
    ) -> Result<tokio_tungstenite::tungstenite::http::Request<()>> {
        let path = if private {
            "/v1/ws/private"
        } else {
            "/v1/ws/markets"
        };
        let mut r = format!("wss://api.polymarket.us{path}").into_client_request()?;
        for (k, v) in self.headers("GET", path, now_ms())? {
            if let Some(k) = k {
                r.headers_mut().insert(
                    tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(
                        k.as_str().as_bytes(),
                    )?,
                    HeaderValue::from_bytes(v.as_bytes())?,
                );
            }
        }
        Ok(r)
    }
}

/// One shared connection pool. Writes are never retried automatically.
pub struct Api {
    client: Client,
    pub auth: Option<Auth>,
    next_request: Instant,
    next_rfq: Instant,
    min_interval: Duration,
}
impl Api {
    pub fn new(auth: Option<Auth>) -> Result<Self> {
        let client = Client::builder()
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("poly-us/0.1")
            .build()?;
        Ok(Self {
            client,
            auth,
            next_request: Instant::now(),
            next_rfq: Instant::now(),
            min_interval: Duration::from_millis(50),
        })
    }
    pub fn pace_at(&mut self, interval: Duration) {
        self.min_interval = interval.max(Duration::from_millis(50));
    }
    pub async fn request(
        &mut self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        private: bool,
    ) -> Result<Value> {
        ensure!(
            path.starts_with("/v1/") && !path.contains("..") && !path.contains('#'),
            "invalid API path"
        );
        // Reserve headroom below the shared 25/s quota. RFQ endpoints are 1/s.
        let rfq = path.starts_with("/v1/rfqs") || path.starts_with("/v1/combos");
        let deadline = if rfq {
            self.next_request.max(self.next_rfq)
        } else {
            self.next_request
        };
        tokio::time::sleep_until(deadline.into()).await;
        self.next_request = Instant::now() + self.min_interval;
        if rfq {
            self.next_rfq = Instant::now() + Duration::from_millis(1050);
        }
        let host = if private { API } else { PUBLIC };
        let mut req = self.client.request(method.clone(), format!("{host}{path}"));
        if private {
            req = req.headers(
                self.auth
                    .as_ref()
                    .context("API credentials required")?
                    .headers(method.as_str(), path, now_ms())?,
            );
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let mut resp = req
            .send()
            .await
            .context("request outcome unknown; reconcile any write before retry")?;
        let status = resp.status();
        if !status.is_success() {
            if status.as_u16() == 429 {
                self.next_request = Instant::now() + Duration::from_secs(2);
                self.next_rfq = self.next_request;
            }
            // Do not log response bodies: they may contain account information.
            anyhow::bail!("Polymarket US HTTP {status}; no automatic retry");
        }
        const MAX_BODY: usize = 8 * 1024 * 1024;
        ensure!(
            resp.content_length().is_none_or(|n| n <= MAX_BODY as u64),
            "response exceeds 8 MiB limit"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= MAX_BODY,
                "response exceeds 8 MiB limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).context("invalid response; reconcile any write before retry")
    }
    pub async fn get(&mut self, path: &str, private: bool) -> Result<Value> {
        self.request(Method::GET, path, None, private).await
    }
    pub async fn place_fok(
        &mut self,
        slug: &str,
        side: Side,
        qty: u64,
        limit: u64,
    ) -> Result<String> {
        let body = order_body(slug, side, qty, limit)?;
        let r = self
            .request(Method::POST, "/v1/orders", Some(&body), true)
            .await?;
        Ok(r["id"]
            .as_str()
            .filter(|x| !x.is_empty())
            .context("missing order id; reconcile before retry")?
            .to_owned())
    }
    pub async fn terminal(&mut self, id: &str) -> Result<OrderResult> {
        ensure!(safe_id(id), "invalid order id");
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let r = self.get(&format!("/v1/orders/{id}"), true).await?;
            if let Some(done) = terminal_order(&r)? {
                return Ok(done);
            }
            ensure!(
                Instant::now() < until,
                "order status unknown after timeout; stop and reconcile"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
pub fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
pub fn order_body(slug: &str, side: Side, qty: u64, limit: u64) -> Result<Value> {
    ensure!(
        safe_id(slug) && qty > 0 && qty <= MAX_QTY && limit > 0 && limit < SCALE,
        "invalid order"
    );
    // quantity is JSON number, serialized from an exact decimal representation.
    let quantity: Value = serde_json::from_str(&display(qty))?;
    Ok(
        json!({"marketSlug":slug,"intent":if side == Side::Yes {"ORDER_INTENT_BUY_LONG"} else {"ORDER_INTENT_BUY_SHORT"},
        "type":"ORDER_TYPE_LIMIT","price":{"value":display(limit),"currency":"USD"},"quantity":quantity,
        "tif":"TIME_IN_FORCE_FILL_OR_KILL","manualOrderIndicator":"MANUAL_ORDER_INDICATOR_AUTOMATIC"}),
    )
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OrderResult {
    pub filled: u64,
    pub cash: u64,
    pub fees: u64,
    pub state: String,
}
fn value_fixed(v: &Value) -> Result<u64> {
    if let Some(s) = v.as_str() {
        fixed(s)
    } else if v.is_number() {
        fixed(&v.to_string())
    } else {
        anyhow::bail!("missing numeric field")
    }
}
pub fn terminal_order(v: &Value) -> Result<Option<OrderResult>> {
    let o = &v["order"];
    let state = o["state"].as_str().context("missing order state")?;
    if ![
        "ORDER_STATE_FILLED",
        "ORDER_STATE_CANCELED",
        "ORDER_STATE_REJECTED",
        "ORDER_STATE_EXPIRED",
    ]
    .contains(&state)
    {
        return Ok(None);
    }
    let filled = value_fixed(&o["cumQuantity"])?;
    ensure!(filled <= MAX_QTY, "invalid filled quantity");
    if filled == 0 {
        return Ok(Some(OrderResult {
            filled,
            cash: 0,
            fees: 0,
            state: state.to_owned(),
        }));
    }
    ensure!(
        o["avgPx"]["currency"] == "USD"
            && o["commissionNotionalTotalCollected"]["currency"] == "USD",
        "missing currency"
    );
    let p = fixed(
        o["avgPx"]["value"]
            .as_str()
            .context("missing average fill price")?,
    )?;
    ensure!(p <= SCALE, "invalid fill price");
    let p = if o["intent"] == "ORDER_INTENT_BUY_SHORT" {
        SCALE - p
    } else {
        p
    };
    Ok(Some(OrderResult {
        filled,
        cash: cost(p, filled),
        fees: fixed(
            o["commissionNotionalTotalCollected"]["value"]
                .as_str()
                .context("missing fees")?,
        )?,
        state: state.to_owned(),
    }))
}
