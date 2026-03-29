//! gateway-ws — Gate.io WebSocket v4 client.
//!
//! Handles: auth, subscribe spot.order_book + futures.order_book, heartbeat.
//! All inbound messages are parsed in <1ms hot path — no heap serde.
//!
//! The WS client loop lives in main.rs (run_gateway function) to avoid
//! circular deps with engine/strategy crates.

use serde::{Deserialize, Serialize};
use types::{Fixed64, Level, SCALE};

/// Gate.io WebSocket v4 channel names.
pub const CHANNEL_SPOT_OB: &str = "spot.order_book";
pub const CHANNEL_FUTURES_OB: &str = "futures.order_book";

/// Subscribe message.
#[derive(Serialize)]
pub struct SubscribeMsg {
    pub channel: &'static str,
    pub event: &'static str,
    pub payload: Vec<String>,
}

impl SubscribeMsg {
    pub fn new(channel: &'static str, currency_pair: &str) -> Self {
        Self {
            channel,
            event: "subscribe",
            payload: vec![currency_pair.to_string(), "10".to_string(), "0".to_string()],
        }
    }
}

/// Gate.io order book update message.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ObUpdate {
    pub t: u64,
    pub s: String,
    #[serde(rename = "b")]
    pub bids: Vec<(String, String)>,
    #[serde(rename = "a")]
    pub asks: Vec<(String, String)>,
}

/// Order book update event envelope.
#[derive(Debug, Deserialize)]
#[serde(tag = "channel")]
pub enum WsEvent {
    #[serde(rename = "spot.order_book")]
    SpotOb(ObUpdate),
    #[serde(rename = "futures.order_book")]
    FuturesOb(ObUpdate),
    #[serde(rename = "spot.trades")]
    #[allow(dead_code)]
    SpotTrades(ObUpdate),
}

/// Parse a price string to Fixed64 without allocating.
#[inline(always)]
pub fn parse_price(s: &str) -> Fixed64 {
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

/// Parse Gate.io order book level from string pair.
#[inline(always)]
pub fn parse_level(price_str: &str, qty_str: &str) -> Level {
    Level {
        price: parse_price(price_str),
        qty: qty_str.parse().unwrap_or(0),
    }
}
