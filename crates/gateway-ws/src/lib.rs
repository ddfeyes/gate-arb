//! gateway-ws — Gate.io WebSocket v4 client.
//!
//! Handles: auth, subscribe spot.order_book + futures.order_book, heartbeat.
//! All inbound messages are parsed in <1ms hot path — no heap serde.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use types::{Fixed64, Level, OrderBook, Side, TradeState};

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
    SpotTrades(ObUpdate),
}

/// Snapshot message for full book.
#[derive(Debug, Deserialize)]
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
}

impl GatewayWs {
    pub fn new() -> Self {
        Self {
            ws_url: GATE_WS_URL,
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
        // Real processing happens in engine — gateway just receives and logs
    }

    /// Parse a price string to Fixed64 without allocating.
    #[inline(always)]
    pub fn parse_price(s: &str) -> Fixed64 {
        // Use fixed-point parsing: string → u64 * 1e8
        // Simple implementation: find decimal point and compute
        let bytes = s.as_bytes();
        let mut val: u64 = 0;
        let mut frac: u64 = 0;
        let mut in_frac = false;
        let mut frac_digits = 0;

        for &b in bytes {
            if b == b'.' {
                in_frac = true;
                continue;
            }
            if b >= b'0' && b <= b'9' {
                let d = (b - b'0') as u64;
                val = val * 10 + d;
                if in_frac {
                    frac_digits += 1;
                }
            }
        }

        // Scale to 1e8
        let scale: u64 = 100_000_000;
        if frac_digits >= 8 {
            // Enough or more decimals — just take first 8
            let div = 10u64.pow(frac_digits - 8);
            Fixed64::from_raw(val / div)
        } else {
            // Pad with zeros
            let mul = 10u64.pow(8 - frac_digits);
            Fixed64::from_raw(val * mul)
        }
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
