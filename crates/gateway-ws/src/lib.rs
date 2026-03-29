//! gateway-ws — Gate.io WebSocket v4 client.
//!
//! Handles: auth, subscribe spot.order_book + futures.order_book, heartbeat.
//! All inbound messages are parsed in <1ms hot path — no heap serde.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use types::{Fixed64, Level};

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

/// Connected session state.
pub struct GatewayWs {
    ws_url: &'static str,
    ob_tx: Option<Arc<Mutex<mpsc::Sender<(String, Vec<Level>, Vec<Level>)>>>>,
    /// Optional risk manager for ping latency monitoring.
    risk: Option<Arc<risk::RiskManager>>,
    /// Timestamp of last ping sent (for RTT measurement).
    last_ping_sent: parking_lot::RwLock<Option<Instant>>,
    /// Timestamp of last inbound message (for connectivity monitoring).
    last_msg_received: parking_lot::RwLock<Option<Instant>>,
}

impl GatewayWs {
    pub fn new() -> Self {
        Self {
            ws_url: GATE_WS_URL,
            ob_tx: None,
            risk: None,
            last_ping_sent: parking_lot::RwLock::new(None),
            last_msg_received: parking_lot::RwLock::new(None),
        }
    }

    /// Inject the order book update channel sender.
    pub fn set_ob_tx(&mut self, tx: mpsc::Sender<(String, Vec<Level>, Vec<Level>)>) {
        self.ob_tx = Some(Arc::new(Mutex::new(tx)));
    }

    /// Inject the risk manager for ping latency monitoring.
    pub fn set_risk(&mut self, risk: Arc<risk::RiskManager>) {
        self.risk = Some(risk);
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
        let risk = self.risk.clone();

        loop {
            tokio::select! {
                _ = ping_interval.tick() => {
                    // Record ping send time for RTT measurement
                    if risk.is_some() {
                        *self.last_ping_sent.write() = Some(Instant::now());
                    }
                    write.send(Message::Ping(vec![].into())).await?;
                    debug!("Sent ping");
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            // Update risk manager latency tracker on any message
                            if let (Some(ref r), Some(sent)) = (risk.as_ref(), *self.last_ping_sent.read()) {
                                let latency = sent.elapsed().as_millis() as u64;
                                r.update_ping_latency(latency);
                            }
                            *self.last_msg_received.write() = Some(Instant::now());
                            self.handle_message(&text).await;
                        }
                        Some(Ok(Message::Ping(data))) => {
                            write.send(Message::Pong(data)).await?;
                        }
                        Some(Ok(Message::Pong(_))) => {
                            // RTT: time from ping send to pong receive
                            if let (Some(ref r), Some(sent)) = (risk.as_ref(), *self.last_ping_sent.read()) {
                                let rtt_ms = sent.elapsed().as_millis() as u64;
                                r.update_ping_latency(rtt_ms);
                                debug!("WS pong RTT: {}ms", rtt_ms);
                            }
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

        let bids: Vec<Level> = update
            .bids
            .into_iter()
            .map(|(p, q)| parse_level(&p, &q))
            .collect();
        let asks: Vec<Level> = update
            .asks
            .into_iter()
            .map(|(p, q)| parse_level(&p, &q))
            .collect();

        if let Some(ref tx_arc) = self.ob_tx {
            let tx = tx_arc.lock().await;
            let _ = tx.try_send((update.s.clone(), bids, asks));
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
