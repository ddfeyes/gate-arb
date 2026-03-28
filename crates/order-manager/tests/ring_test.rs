use order_manager::ring::{InFlightOrder, OrderRing};
use types::{Fixed64, OrderStatus, Side};

fn make_order(client_id: u64) -> InFlightOrder {
    InFlightOrder {
        client_id,
        exchange_id: String::new(),
        symbol: "BTC_USDT",
        side: Side::Bid,
        price: Fixed64::from_raw(5_000_000_000_000),
        qty: 1_000_000,
        status: OrderStatus::Open,
    }
}

#[test]
fn ring_insert_and_find() {
    let mut ring = OrderRing::<8>::new();
    ring.insert(make_order(1));
    ring.insert(make_order(2));
    assert!(ring.find_by_client_id(1).is_some());
    assert!(ring.find_by_client_id(2).is_some());
    assert!(ring.find_by_client_id(99).is_none());
}

#[test]
fn ring_update_status() {
    let mut ring = OrderRing::<8>::new();
    ring.insert(make_order(10));
    ring.update_status(10, "ex-abc".to_string(), OrderStatus::Filled);
    let order = ring.find_by_client_id(10).unwrap();
    assert_eq!(order.status, OrderStatus::Filled);
    assert_eq!(order.exchange_id, "ex-abc");
}

#[test]
fn ring_wraps_at_capacity() {
    let mut ring = OrderRing::<4>::new();
    for i in 0..6u64 {
        ring.insert(make_order(i));
    }
    // Only last 4 should be findable (ring wrapped)
    assert!(ring.find_by_client_id(0).is_none());
    assert!(ring.find_by_client_id(1).is_none());
    assert!(ring.find_by_client_id(2).is_some());
    assert!(ring.find_by_client_id(5).is_some());
}

#[test]
fn ring_remove() {
    let mut ring = OrderRing::<8>::new();
    ring.insert(make_order(7));
    ring.remove(7);
    assert!(ring.find_by_client_id(7).is_none());
}

#[test]
fn ring_capacity_32_no_alloc() {
    let mut ring = OrderRing::<32>::new();
    for i in 0u64..32 {
        ring.insert(make_order(i));
    }
    for i in 0u64..32 {
        assert!(ring.find_by_client_id(i).is_some());
    }
    // Insert one more — should wrap and overwrite slot 0 (client_id 0)
    ring.insert(make_order(32));
    assert!(ring.find_by_client_id(0).is_none());
    assert!(ring.find_by_client_id(32).is_some());
}
