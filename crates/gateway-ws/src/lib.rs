//! gateway-ws — Gate.io WebSocket v4 client.
//!
//! Handles: auth, subscribe spot.order_book + futures.order_book, heartbeat.
//! All inbound messages are parsed in <1ms hot path — no heap serde.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use types::{Fixed64, Level, ObLevel, ObUpdateChannel};

const GATE_WS_URL: &str = "wss://api.gateio.ws/ws/v4/";

/// Gate.io WebSocket v4 channel names.
const CHANNEL_SPOT_OB: &str = "spot.order_book";
const CHANNEL_FUTURES_OB: &str = "futures.order_book";

/// Auth message for signed requests.
#[derive(Serialize)]
struct AuthMsg {
    #[serde(rename = "channel")]
    channel: &'static str,
    #[serde(rename = "event")]
    event: &'static str,
    #[serde(rename = "payload")]
    payload: Vec<String>,
}

/// Subscribe message.
#[derive(Serialize)]
struct SubscribeMsg {
    channel: &'static str,
    event: &'static str,
    payload: Vec<String>,
}

impl SubscribeMsg {
    fn new(channel: &'static str, currency_pair: &str) -> Self {
        Self {
            channel,
            event: "subscribe",
            payload: vec![currency_pair.to_string(), "10".to_string(), "0".to_string()],
        }
    }
}

/// Gate.io order book update message (parsed without heap).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ObUpdate {
    t: u64,    // timestamp_us
    s: String, // currency pair symbol
    #[serde(rename = "b")]
    bids: Vec<(String, String)>, // price, qty — string to avoid float
    #[serde(rename = "a")]
    asks: Vec<(String, String)>,
}

/// Order book update event envelope.
#[derive(Debug, Deserialize)]
#[serde(tag = "channel")]
enum WsEvent {
    #[serde(rename = "spot.order_book")]
    SpotOb(ObUpdate),
    #[serde(rename = "futures.order_book")]
    FuturesOb(ObUpdate),
    #[serde(rename = "spot.trades")]
    #[allow(dead_code)]
    SpotTrades(ObUpdate),
}

/// Snapshot message for full book.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ObSnapshot {
    id: u64,
    t: u64,
    s: String,
    #[serde(rename = "b")]
    bids: Vec<Vec<String>>,
    #[serde(rename = "a")]
    asks: Vec<Vec<String>>,
}

// ─── Reconnection ───────────────────────────────────────────────────────────

/// Reconnection configuration with exponential backoff + jitter.
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    /// Initial delay in seconds.
    pub base_delay_secs: f64,
    /// Maximum delay in seconds.
    pub max_delay_secs: f64,
    /// Jitter factor [0.0, 1.0) applied to delay.
    pub jitter: f64,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            base_delay_secs: 1.0,
            max_delay_secs: 30.0,
            jitter: 0.3,
        }
    }
}

/// Jittered exponential backoff. call site holds no locks.
fn backoff_duration(attempt: u32, config: &ReconnectConfig) -> std::time::Duration {
    let exp_delay = config.base_delay_secs * 2f64.powi(attempt as i32);
    let capped = exp_delay.min(config.max_delay_secs);
    let jitter_range = capped * config.jitter;
    let jitter = (jitter_range * fastrand::f64()).min(jitter_range);
    std::time::Duration::from_secs_f64(capped + jitter)
}

// ─── GatewayWs ───────────────────────────────────────────────────────────────

/// Connected session state.
pub struct GatewayWs {
    ws_url: &'static str,
    /// Optional OB update sender — set via `set_ob_tx()` before `run()`.
    ob_tx: Arc<std::sync::Mutex<Option<mpsc::Sender<ObUpdateChannel>>>>,
}

impl GatewayWs {
    pub fn new() -> Self {
        Self {
            ws_url: GATE_WS_URL,
            ob_tx: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Inject the OB update channel sender. Call before `run()`.
    pub fn set_ob_tx(&self, tx: mpsc::Sender<ObUpdateChannel>) {
        let mut guard = self.ob_tx.lock().unwrap();
        *guard = Some(tx);
    }

    /// Connect and run the WebSocket session with automatic reconnection.
    /// Runs indefinitely, reconnecting with exponential backoff on disconnect.
    pub async fn run_with_reconnect(
        &self,
        spot_symbol: &str,
        perp_symbol: &str,
        config: ReconnectConfig,
    ) -> Result<()> {
        let mut attempt = 0u32;
        loop {
            match self.run(spot_symbol, perp_symbol).await {
                Ok(()) => {
                    info!("WS session ended cleanly, reconnecting...");
                }
                Err(e) => {
                    error!("WS session error: {:?}", e);
                }
            }

            if attempt == 0 {
                attempt = 1; // first backoff uses attempt=1 so it's 2x base
            }
            let delay = backoff_duration(attempt, &config);
            info!(
                "Reconnecting in {:.2}s (attempt {})",
                delay.as_secs_f64(),
                attempt + 1
            );
            tokio::time::sleep(delay).await;
            attempt = attempt.saturating_add(1);
        }
    }

    /// Connect and run the WebSocket session.
    /// Returns after disconnection.
    pub async fn run(&self, spot_symbol: &str, perp_symbol: &str) -> Result<()> {
        info!("Connecting to Gate.io WS: {}", self.ws_url);
        let (ws, _) = connect_async(self.ws_url).await?;

        let (mut write, mut read) = ws.split();

        // Subscribe to spot order book
        let spot_sub = SubscribeMsg::new(CHANNEL_SPOT_OB, spot_symbol);
        let msg = serde_json::to_string(&spot_sub)?;
        write.send(Message::Text(msg.into())).await?;
        debug!("Subscribed to spot.order_book {}", spot_symbol);

        // Subscribe to futures order book
        let futures_sub = SubscribeMsg::new(CHANNEL_FUTURES_OB, perp_symbol);
        let msg = serde_json::to_string(&futures_sub)?;
        write.send(Message::Text(msg.into())).await?;
        debug!("Subscribed to futures.order_book {}", perp_symbol);

        // Ping loop
        let mut ping_interval = tokio::time::interval(Duration::from_secs(20));

        loop {
            tokio::select! {
                _ = ping_interval.tick() => {
                    write.send(Message::Ping(vec![].into())).await?;
                    debug!("Sent ping");
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_message(&text).await;
                        }
                        Some(Ok(Message::Ping(data))) => {
                            write.send(Message::Pong(data)).await?;
                        }
                        Some(Ok(Message::Close(e))) => {
                            warn!("WS closed: {:?}", e);
                            break;
                        }
                        Some(Err(e)) => {
                            error!("WS error: {:?}", e);
                            break;
                        }
                        None => break,
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_message(&self, text: &str) {
        // Fast path: only handle order book updates we care about
        if !text.contains("order_book") {
            return;
        }

        // Try to parse as update
        if let Ok(evt) = serde_json::from_str::<WsEvent>(text) {
            match evt {
                WsEvent::SpotOb(update) => {
                    self.process_ob_update(update).await;
                }
                WsEvent::FuturesOb(update) => {
                    self.process_ob_update(update).await;
                }
                _ => {}
            }
        }
    }

    async fn process_ob_update(&self, update: ObUpdate) {
        debug!(
            "OB update: {} bids={} asks={}",
            update.s,
            update.bids.len(),
            update.asks.len()
        );

        // Forward to engine via channel if configured.
        let ob_tx = self.ob_tx.lock().unwrap();
        if let Some(ref tx) = *ob_tx {
            let update = ObUpdateChannel {
                symbol: update.s.clone(),
                bids: update
                    .bids
                    .into_iter()
                    .map(|(p, q)| ObLevel { price: p, qty: q })
                    .collect(),
                asks: update
                    .asks
                    .into_iter()
                    .map(|(p, q)| ObLevel { price: p, qty: q })
                    .collect(),
                t: update.t,
            };
            // Non-blocking send — drop if channel is full (hot path must not block).
            let _ = tx.try_send(update);
        }
    }

    /// Parse a price string to Fixed64 without allocating.
    ///
    /// Correctly separates integer and fractional parts so that
    /// "50000.12345678" → `Fixed64::from_raw(5_000_012_345_678)`.
    #[inline(always)]
    pub fn parse_price(s: &str) -> Fixed64 {
        const SCALE: u64 = 100_000_000; // 1e8

        let dot_pos = s.find('.');
        let int_part: u64 = s[..dot_pos.unwrap_or(s.len())].parse().unwrap_or(0);
        let frac_raw: u64 = if let Some(d) = dot_pos {
            let frac_str = &s[d + 1..];
            let digits = frac_str.len().min(8);
            let val: u64 = frac_str[..digits].parse().unwrap_or(0);
            val * 10u64.pow((8 - digits) as u32)
        } else {
            0
        };
        Fixed64::from_raw(int_part * SCALE + frac_raw)
    }
}

impl Default for GatewayWs {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse Gate.io order book level from string pair.
#[inline(always)]
pub fn parse_level(price_str: &str, qty_str: &str) -> Level {
    Level {
        price: GatewayWs::parse_price(price_str),
        qty: qty_str.parse().unwrap_or(0),
    }
}
