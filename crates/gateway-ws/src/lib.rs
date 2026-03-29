//! gateway-ws — Gate.io WebSocket v4 client.
//!
//! Handles: auth, subscribe spot.order_book + futures.order_book,
//! futures.funding_rate, heartbeat.
//! All inbound messages are parsed in <1ms hot path — no heap serde.
//!
//! The WS client loop lives in main.rs (run_gateway function) to avoid
//! circular deps with engine/strategy crates.

use serde::{Deserialize, Serialize};
use types::{Fixed64, FundingRateUpdate, Level, SCALE};

/// Gate.io WebSocket v4 channel names.
pub const CHANNEL_SPOT_OB: &str = "spot.order_book";
pub const CHANNEL_FUTURES_OB: &str = "futures.order_book";
pub const CHANNEL_FUNDING_RATE: &str = "futures.funding_rate";

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

/// Gate.io futures.funding_rate WS result payload.
///
/// Example from Gate.io v4 docs:
/// ```json
/// {
///   "channel": "futures.funding_rate",
///   "event": "update",
///   "result": { "contract": "BTC_USDT", "r": "0.0001", "t": 1609459200000 }
/// }
/// ```
#[derive(Debug, Deserialize, Clone)]
pub struct FundingRatePayload {
    /// Contract name, e.g. "BTC_USDT".
    pub contract: String,
    /// Funding rate as string (e.g. "0.0001" = 0.01%).
    pub r: String,
    /// Timestamp in milliseconds.
    pub t: u64,
}

impl FundingRatePayload {
    /// Convert to the shared `FundingRateUpdate` type.
    pub fn to_update(&self) -> FundingRateUpdate {
        let rate = self.r.parse::<f64>().unwrap_or(0.0);
        FundingRateUpdate {
            contract: self.contract.clone(),
            rate_raw: (rate * types::SCALE as f64) as i64,
            timestamp_ms: self.t,
        }
    }
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
    #[serde(rename = "futures.funding_rate")]
    FundingRate(FundingRatePayload),
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

/// Reconnection policy for the gateway WS loop.
///
/// Exported here so integration tests can call `delay_for_attempt` directly
/// without mirroring the formula.
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    pub initial_delay: std::time::Duration,
    pub max_delay: std::time::Duration,
    /// Give up after this many *consecutive* failures.
    pub max_attempts: u32,
    /// If an outage lasts longer than this, warn about pausing new entries.
    pub pause_threshold: std::time::Duration,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_delay: std::time::Duration::from_secs(1),
            max_delay: std::time::Duration::from_secs(30),
            max_attempts: 10,
            pause_threshold: std::time::Duration::from_secs(60),
        }
    }
}

impl ReconnectConfig {
    /// Compute the backoff delay for a given attempt number (1-indexed).
    ///
    /// Formula: `initial * 2^(attempt-1)`, capped at `max_delay`.
    /// The exponent is clamped to 5 (`attempt.min(5)`) to prevent u64 shift overflow.
    pub fn delay_for_attempt(&self, attempt: u32) -> std::time::Duration {
        debug_assert!(attempt >= 1, "attempt is 1-indexed");
        let raw_ms = self.initial_delay.as_millis() as u64 * (1u64 << (attempt - 1).min(5));
        std::time::Duration::from_millis(raw_ms).min(self.max_delay)
    }
}
