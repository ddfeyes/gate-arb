use engine::{Engine, SpreadDirection};
use types::{Fixed64, Level, OrderBook};

fn make_engine(spot_bid: f64, spot_ask: f64, perp_bid: f64, perp_ask: f64) -> Engine<20, 20> {
    let e = Engine::<20, 20>::new();
    {
        let mut sb = e.spot_book.write();
        sb.update_bids(&[Level {
            price: Fixed64::from_float(spot_bid),
            qty: 1_000_000,
        }]);
        sb.update_asks(&[Level {
            price: Fixed64::from_float(spot_ask),
            qty: 1_000_000,
        }]);
    }
    {
        let mut pb = e.perp_book.write();
        pb.update_bids(&[Level {
            price: Fixed64::from_float(perp_bid),
            qty: 1_000_000,
        }]);
        pb.update_asks(&[Level {
            price: Fixed64::from_float(perp_ask),
            qty: 1_000_000,
        }]);
    }
    e
}

#[test]
fn current_spread_normal_direction() {
    // perp_ask > spot_bid → positive spread
    let e = make_engine(50000.0, 50001.0, 49999.0, 50010.0);
    let snap = e.current_spread().unwrap();
    // spread = perp_ask - spot_bid = 50010 - 50000 = 10 USDT
    assert_eq!(snap.spread_raw, Fixed64::from_float(10.0).raw());
    assert!(!snap.inverted);
    assert_eq!(snap.direction, SpreadDirection::Normal);
    assert_eq!(snap.leg_a.raw(), Fixed64::from_float(50000.0).raw()); // spot_bid
    assert_eq!(snap.leg_b.raw(), Fixed64::from_float(50010.0).raw()); // perp_ask
}

#[test]
fn current_spread_inverted() {
    // perp_ask < spot_bid → inverted (exit territory)
    let e = make_engine(50010.0, 50011.0, 49999.0, 49998.0);
    let snap = e.current_spread().unwrap();
    assert!(snap.inverted);
    assert_eq!(snap.spread_raw, 0); // clamped to 0
}

#[test]
fn current_spread_exactly_zero() {
    // perp_ask == spot_bid → spread = 0, not inverted
    let e = make_engine(50000.0, 50001.0, 49999.0, 50000.0);
    let snap = e.current_spread().unwrap();
    assert_eq!(snap.spread_raw, 0);
    assert!(!snap.inverted);
}

#[test]
fn current_spread_inverse_direction() {
    // spot_ask > perp_bid → positive inverse spread
    let e = make_engine(49999.0, 50015.0, 50000.0, 50010.0);
    let snap = e.current_spread_inverse().unwrap();
    // inverse spread = spot_ask - perp_bid = 50015 - 50000 = 15 USDT
    assert_eq!(snap.spread_raw, Fixed64::from_float(15.0).raw());
    assert!(!snap.inverted);
    assert_eq!(snap.direction, SpreadDirection::Inverse);
    assert_eq!(snap.leg_a.raw(), Fixed64::from_float(50015.0).raw()); // spot_ask
    assert_eq!(snap.leg_b.raw(), Fixed64::from_float(50000.0).raw()); // perp_bid
}

#[test]
fn check_spread_below_threshold_returns_none() {
    let e = make_engine(50000.0, 50001.0, 49999.0, 50005.0);
    // spread = 5 USDT = 500_000_000 raw; threshold = 10 USDT = 1_000_000_000
    let threshold = Fixed64::from_float(10.0).raw();
    assert!(e.check_spread(threshold).is_none());
}

#[test]
fn check_spread_above_threshold_returns_signal() {
    let e = make_engine(50000.0, 50001.0, 49999.0, 50010.0);
    // spread = 10 USDT; threshold = 5 USDT
    let threshold = Fixed64::from_float(5.0).raw();
    let sig = e.check_spread(threshold).unwrap();
    assert_eq!(sig.spread_raw, Fixed64::from_float(10.0).raw());
    assert_eq!(sig.bid_price.raw(), Fixed64::from_float(50000.0).raw());
    assert_eq!(sig.ask_price.raw(), Fixed64::from_float(50010.0).raw());
}

#[test]
fn empty_books_returns_none() {
    let e = Engine::<20, 20>::new();
    assert!(e.current_spread().is_none());
    assert!(e.check_spread(0).is_none());
}
