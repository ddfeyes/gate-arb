//! Gate.io WS v4 order message serialisation/deserialisation.
//!
//! Place order: spot.orders channel, create event, post-only (poc time_in_force).
//! Cancel order: spot.orders channel, cancel event.
//! Decode updates: spot.orders update events → OrderAck.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use types::{OrderAck, OrderStatus, Side};

// ─── Outbound ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct WsRequest<T: Serialize> {
    time: u64,
    channel: &'static str,
    event: &'static str,
    payload: T,
    auth: AuthBlock,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct AuthBlock {
    method: &'static str,
    KEY: String,
    SIGN: String,
}

#[derive(Serialize)]
struct PlacePayload {
    text: String,
    currency_pair: String,
    #[serde(rename = "type")]
    order_type: &'static str,
    side: &'static str,
    price: String,
    amount: String,
    time_in_force: &'static str,
    account: &'static str,
}

#[derive(Serialize)]
struct CancelPayload {
    order_id: String,
    currency_pair: String,
    account: &'static str,
}

/// Parameters for encoding a place order message.
pub struct PlaceOrderParams<'a> {
    pub channel: &'static str,
    pub client_id: u64,
    pub symbol: &'a str,
    pub side: Side,
    pub price: types::Fixed64,
    pub qty: u64, // 1e8 units
    pub signature: &'a str,
    pub api_key: &'a str,
    pub ts: u64,
}

/// Encode a post-only spot limit order placement message.
///
/// Prices/amounts are formatted as 8-decimal strings per Gate.io spec.
pub fn encode_place_order(p: PlaceOrderParams<'_>) -> String {
    let side_str = match p.side {
        Side::Bid => "buy",
        Side::Ask => "sell",
    };

    let price_f = p.price.raw() as f64 / 1e8;
    let qty_f = p.qty as f64 / 1e8;
    let price_str = format!("{:.8}", price_f);
    let qty_str = format!("{:.8}", qty_f);

    let payload = PlacePayload {
        text: format!("t-{}", p.client_id),
        currency_pair: p.symbol.to_string(),
        order_type: "limit",
        side: side_str,
        price: price_str,
        amount: qty_str,
        time_in_force: "poc", // post-only cancel
        account: "spot",
    };

    let req = WsRequest {
        time: p.ts,
        channel: p.channel,
        event: "api",
        payload,
        auth: AuthBlock {
            method: "api_key",
            KEY: p.api_key.to_string(),
            SIGN: p.signature.to_string(),
        },
    };

    serde_json::to_string(&req).expect("serialise place order")
}

/// Encode a cancel order message.
pub fn encode_cancel_order(
    channel: &'static str,
    exchange_id: &str,
    symbol: &str,
    signature: &str,
    api_key: &str,
    ts: u64,
) -> String {
    let payload = CancelPayload {
        order_id: exchange_id.to_string(),
        currency_pair: symbol.to_string(),
        account: "spot",
    };

    let req = WsRequest {
        time: ts,
        channel,
        event: "cancel",
        payload,
        auth: AuthBlock {
            method: "api_key",
            KEY: api_key.to_string(),
            SIGN: signature.to_string(),
        },
    };

    serde_json::to_string(&req).expect("serialise cancel order")
}

// ─── Inbound ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WsOrderEvent {
    #[allow(dead_code)]
    channel: String,
    #[allow(dead_code)]
    event: String,
    result: Vec<OrderResult>,
}

#[derive(Debug, Deserialize)]
struct OrderResult {
    id: String,
    text: String,
    status: String,
    finish_as: Option<String>,
    left: String,
}

/// Decode a Gate.io spot.orders update event into an OrderAck.
pub fn decode_order_update(raw: &str) -> Result<OrderAck> {
    let event: WsOrderEvent = serde_json::from_str(raw).context("parse order event")?;

    let result = event
        .result
        .into_iter()
        .next()
        .context("empty result array")?;

    let client_id: u64 = result
        .text
        .strip_prefix("t-")
        .and_then(|s| s.parse().ok())
        .context("parse client_id from text field")?;

    let status = match (result.status.as_str(), result.finish_as.as_deref()) {
        ("open", _) => {
            if result.left == "0" {
                OrderStatus::Filled
            } else {
                OrderStatus::Open
            }
        }
        ("closed", Some("filled")) => OrderStatus::Filled,
        ("closed", Some("cancelled")) | ("closed", Some("poc")) => OrderStatus::Cancelled,
        ("closed", Some("ioc")) => OrderStatus::Cancelled,
        ("closed", _) => OrderStatus::Cancelled,
        _ => OrderStatus::Open,
    };

    Ok(OrderAck {
        client_id,
        exchange_id: result.id,
        status,
    })
}
