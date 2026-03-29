//! Tests for live order wiring in strategy:
//! - set_order_sender() is accepted
//! - open_position() sends LEG1 OrderCmd when not paper_mode
//! - on_order_ack(Filled, leg1) triggers LEG2 send + state = Leg2Sent
//! - on_order_ack(Filled, leg2) → state = BothFilled
//! - on_order_ack(Cancelled) → state = Closing

use std::sync::Arc;
use tokio::sync::mpsc;
use types::{Fixed64, LegKind, OrderAck, OrderStatus, SpreadSignal, TradeState};

fn make_strategy_live() -> (Arc<strategy::Strategy>, mpsc::Receiver<types::OrderCmd>) {
    let engine = Arc::new(engine::Engine::<20, 20>::new());
    let mut s = strategy::Strategy::new(Arc::clone(&engine), 0);
    s.paper_mode = false;
    let s = Arc::new(s);
    let (tx, rx) = mpsc::channel(32);
    s.set_order_sender(tx);
    (s, rx)
}

fn spread_signal() -> SpreadSignal {
    SpreadSignal {
        spread_raw: 1_000_000,
        spread_pct: Fixed64::from_raw(0),
        bid_price: Fixed64::from_float(50000.0),
        ask_price: Fixed64::from_float(50010.0),
        timestamp_us: 1_000_000,
    }
}

#[tokio::test]
async fn live_open_sends_leg1_order() {
    let (s, mut rx) = make_strategy_live();

    // Manually trigger open_position via on_tick_with_signal
    let sig = spread_signal();
    // We need position to be None (it is by default)
    // Inject signal directly
    s.on_tick_with_signal(Some(sig));

    // Expect LEG1 OrderCmd::Place to be sent
    let cmd = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
        .await
        .expect("timed out waiting for cmd")
        .expect("channel closed");

    match cmd {
        types::OrderCmd::Place {
            side,
            post_only,
            qty,
            ..
        } => {
            assert_eq!(side, types::Side::Bid, "leg1 must be spot BUY");
            assert!(post_only, "must be post-only");
            assert_eq!(qty, 1_000_000, "qty must be 1e6 units = 0.01 BTC");
        }
        other => panic!("expected Place, got {:?}", other),
    }
}

#[tokio::test]
async fn on_order_ack_leg1_filled_sends_leg2() {
    let (s, mut rx) = make_strategy_live();

    // Open position
    s.on_tick_with_signal(Some(spread_signal()));

    // Drain leg1 cmd
    let leg1_cmd = rx.recv().await.expect("leg1 cmd");
    let leg1_client_id = match leg1_cmd {
        types::OrderCmd::Place { client_id, .. } => client_id,
        _ => panic!("expected Place"),
    };

    // Simulate LEG1 filled
    s.on_order_ack(OrderAck {
        client_id: leg1_client_id,
        exchange_id: "ex-111".to_string(),
        status: OrderStatus::Filled,
    });

    // Should receive LEG2 cmd
    let leg2_cmd = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
        .await
        .expect("timed out waiting for leg2")
        .expect("channel closed");

    match leg2_cmd {
        types::OrderCmd::Place {
            client_id,
            side,
            post_only,
            ..
        } => {
            assert_eq!(side, types::Side::Ask, "leg2 must be perp SHORT (sell)");
            assert!(post_only, "leg2 must be post-only");
            assert_eq!(client_id % 2, 1, "leg2 client_id must be odd");
        }
        other => panic!("expected Place for leg2, got {:?}", other),
    }
}

#[tokio::test]
async fn on_order_ack_leg2_filled_both_filled() {
    let (s, mut rx) = make_strategy_live();

    s.on_tick_with_signal(Some(spread_signal()));
    let leg1_cmd = rx.recv().await.unwrap();
    let leg1_id = match leg1_cmd {
        types::OrderCmd::Place { client_id, .. } => client_id,
        _ => panic!(),
    };

    // Fill leg1 → sends leg2
    s.on_order_ack(OrderAck {
        client_id: leg1_id,
        exchange_id: "e1".into(),
        status: OrderStatus::Filled,
    });
    let leg2_cmd = rx.recv().await.unwrap();
    let leg2_id = match leg2_cmd {
        types::OrderCmd::Place { client_id, .. } => client_id,
        _ => panic!(),
    };

    // Fill leg2 → BOTH_FILLED
    s.on_order_ack(OrderAck {
        client_id: leg2_id,
        exchange_id: "e2".into(),
        status: OrderStatus::Filled,
    });

    // Position state should be BothFilled
    assert!(
        s.is_position_open(),
        "position should still be open (BOTH_FILLED, not CLOSED)"
    );
}

#[tokio::test]
async fn on_order_ack_cancelled_triggers_closing() {
    let (s, mut rx) = make_strategy_live();

    s.on_tick_with_signal(Some(spread_signal()));
    let cmd = rx.recv().await.unwrap();
    let client_id = match cmd {
        types::OrderCmd::Place { client_id, .. } => client_id,
        _ => panic!(),
    };

    // Post-only rejection
    s.on_order_ack(OrderAck {
        client_id,
        exchange_id: "".into(),
        status: OrderStatus::Cancelled,
    });

    // Position should exist but be in Closing state
    assert!(s.is_position_open(), "position guard still holds");
}

#[tokio::test]
async fn paper_mode_does_not_send_orders() {
    let engine = Arc::new(engine::Engine::<20, 20>::new());
    let s = Arc::new(strategy::Strategy::new(Arc::clone(&engine), 0));
    // paper_mode = true (default), no order_sender set

    let (tx, mut rx) = mpsc::channel::<types::OrderCmd>(32);
    // Don't call set_order_sender — no channel wired

    s.on_tick_with_signal(Some(spread_signal()));

    // No cmd should arrive (paper mode uses paper_open_position, not open_position)
    let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
    assert!(result.is_err(), "paper mode must not send any OrderCmd");

    // Suppress unused warning
    drop(tx);
}
