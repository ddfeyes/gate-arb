use order_manager::codec::{
    decode_order_update, encode_cancel_order, encode_place_order, PlaceOrderParams,
};
use types::{Fixed64, OrderStatus, Side};

#[test]
fn encode_place_order_post_only() {
    let json = encode_place_order(PlaceOrderParams {
        channel: "spot.orders",
        client_id: 1001,
        symbol: "BTC_USDT",
        side: Side::Bid,
        price: Fixed64::from_raw(5_000_000_000_000), // 50000.00000000
        qty: 1_000_000u64,                           // 0.01000000 BTC
        signature: "sig",
        api_key: "key",
        ts: 1_700_000_000u64,
    });
    assert!(json.contains("\"time_in_force\":\"poc\""));
    assert!(json.contains("\"text\":\"t-1001\""));
    assert!(json.contains("\"price\":\"50000.00000000\""));
    assert!(json.contains("\"amount\":\"0.01000000\""));
    assert!(json.contains("\"side\":\"buy\""));
}

#[test]
fn encode_cancel_order_fields() {
    let json = encode_cancel_order(
        "spot.orders",
        "ex-abc123",
        "BTC_USDT",
        "sig",
        "key",
        1_700_000_000u64,
    );
    assert!(json.contains("\"order_id\":\"ex-abc123\""));
    assert!(json.contains("\"currency_pair\":\"BTC_USDT\""));
}

#[test]
fn decode_order_update_filled() {
    let raw = r#"{
        "channel": "spot.orders",
        "event": "update",
        "result": [{
            "id": "ex-999",
            "text": "t-42",
            "status": "closed",
            "finish_as": "filled",
            "currency_pair": "BTC_USDT",
            "side": "buy",
            "price": "50000",
            "amount": "0.01",
            "left": "0"
        }]
    }"#;
    let update = decode_order_update(raw).unwrap();
    assert_eq!(update.client_id, 42);
    assert_eq!(update.exchange_id, "ex-999");
    assert_eq!(update.status, OrderStatus::Filled);
}

#[test]
fn decode_order_update_cancelled() {
    let raw = r#"{
        "channel": "spot.orders",
        "event": "update",
        "result": [{
            "id": "ex-7",
            "text": "t-7",
            "status": "closed",
            "finish_as": "cancelled",
            "currency_pair": "BTC_USDT",
            "side": "sell",
            "price": "50000",
            "amount": "0.01",
            "left": "0.01"
        }]
    }"#;
    let update = decode_order_update(raw).unwrap();
    assert_eq!(update.status, OrderStatus::Cancelled);
}
