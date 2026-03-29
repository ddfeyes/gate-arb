use engine::Engine;
use std::sync::Arc;
use strategy::Strategy;
use types::{Fixed64, Level};

fn make_strategy(spot_bid: f64, spot_ask: f64, perp_bid: f64, perp_ask: f64, threshold: f64) -> Strategy {
    let engine = Arc::new(Engine::<20, 20>::new());
    {
        let mut sb = engine.spot_book.write();
        sb.update_bids(&[Level { price: Fixed64::from_float(spot_bid), qty: 1_000_000 }]);
        sb.update_asks(&[Level { price: Fixed64::from_float(spot_ask), qty: 1_000_000 }]);
    }
    {
        let mut pb = engine.perp_book.write();
        pb.update_bids(&[Level { price: Fixed64::from_float(perp_bid), qty: 1_000_000 }]);
        pb.update_asks(&[Level { price: Fixed64::from_float(perp_ask), qty: 1_000_000 }]);
    }
    Strategy::new(engine, Fixed64::from_float(threshold).raw())
}

#[test]
fn no_signal_below_threshold() {
    // spread = 5 USDT < threshold 10 USDT → no position opened
    let strat = make_strategy(50000.0, 50001.0, 49999.0, 50005.0, 10.0);
    strat.on_tick();
    assert!(!strat.is_position_open());
}

#[test]
fn paper_open_on_signal() {
    // spread = 20 USDT > threshold 10 USDT → paper open
    let strat = make_strategy(50000.0, 50001.0, 49999.0, 50020.0, 10.0);
    strat.on_tick();
    assert!(strat.is_position_open());
}

#[test]
fn paper_close_when_spread_converges() {
    // Open with spread 20 USDT, then prices change so spread = 0 (inverted) → closes
    let engine = Arc::new(Engine::<20, 20>::new());
    {
        let mut sb = engine.spot_book.write();
        sb.update_bids(&[Level { price: Fixed64::from_float(50000.0), qty: 1_000_000 }]);
        sb.update_asks(&[Level { price: Fixed64::from_float(50001.0), qty: 1_000_000 }]);
    }
    {
        let mut pb = engine.perp_book.write();
        pb.update_bids(&[Level { price: Fixed64::from_float(49999.0), qty: 1_000_000 }]);
        pb.update_asks(&[Level { price: Fixed64::from_float(50020.0), qty: 1_000_000 }]);
    }

    let strat = Strategy::new(Arc::clone(&engine), Fixed64::from_float(10.0).raw());
    strat.on_tick();
    assert!(strat.is_position_open(), "should be open after signal");

    // Now move market: perp collapses below spot → spread inverted → exit
    {
        let mut pb = engine.perp_book.write();
        pb.update_bids(&[Level { price: Fixed64::from_float(49990.0), qty: 1_000_000 }]);
        pb.update_asks(&[Level { price: Fixed64::from_float(49995.0), qty: 1_000_000 }]); // perp_ask < spot_bid
    }

    strat.on_tick();
    assert!(!strat.is_position_open(), "should be closed after spread inversion");
}

#[test]
fn exit_spread_raw_used_for_close() {
    // Set exit_spread_raw = 5 USDT: close when spread drops to 5 (not 0)
    let engine = Arc::new(Engine::<20, 20>::new());
    {
        let mut sb = engine.spot_book.write();
        sb.update_bids(&[Level { price: Fixed64::from_float(50000.0), qty: 1_000_000 }]);
        sb.update_asks(&[Level { price: Fixed64::from_float(50001.0), qty: 1_000_000 }]);
    }
    {
        let mut pb = engine.perp_book.write();
        pb.update_bids(&[Level { price: Fixed64::from_float(49999.0), qty: 1_000_000 }]);
        pb.update_asks(&[Level { price: Fixed64::from_float(50020.0), qty: 1_000_000 }]);
    }

    let mut strat = Strategy::new(Arc::clone(&engine), Fixed64::from_float(10.0).raw());
    strat.exit_spread_raw = Fixed64::from_float(5.0).raw(); // exit when spread ≤ 5 USDT
    strat.on_tick();
    assert!(strat.is_position_open());

    // Spread narrows to 4 USDT (≤ 5 → should close)
    {
        let mut pb = engine.perp_book.write();
        pb.update_asks(&[Level { price: Fixed64::from_float(50004.0), qty: 1_000_000 }]);
    }
    strat.on_tick();
    assert!(!strat.is_position_open(), "should close when spread ≤ exit_spread_raw");
}

#[test]
fn min_profit_guard_blocks_graceful_close_at_zero() {
    // Open position, spread drifts to exactly 0 (not inverted) — min_profit blocks the close.
    // Note: full inversion (spread < 0) ALWAYS closes regardless of min_profit.
    let engine = Arc::new(Engine::<20, 20>::new());
    {
        let mut sb = engine.spot_book.write();
        sb.update_bids(&[Level { price: Fixed64::from_float(50000.0), qty: 1_000_000 }]);
        sb.update_asks(&[Level { price: Fixed64::from_float(50001.0), qty: 1_000_000 }]);
    }
    {
        let mut pb = engine.perp_book.write();
        pb.update_bids(&[Level { price: Fixed64::from_float(49999.0), qty: 1_000_000 }]);
        pb.update_asks(&[Level { price: Fixed64::from_float(50020.0), qty: 1_000_000 }]);
    }

    let mut strat = Strategy::new(Arc::clone(&engine), Fixed64::from_float(10.0).raw());
    // Set min_profit to $1000 (unreachable with 0.01 BTC qty)
    strat.min_profit_raw = 100_000_000_000; // $1000
    strat.on_tick();
    assert!(strat.is_position_open());

    // Spread drifts to exactly 0 (perp_ask == spot_bid) — not inverted, just converged
    {
        let mut pb = engine.perp_book.write();
        pb.update_asks(&[Level { price: Fixed64::from_float(50000.0), qty: 1_000_000 }]);
    }
    strat.on_tick();
    // min_profit guard blocks: spread converged gracefully, pnl < $1000
    assert!(strat.is_position_open(), "min_profit should block close when spread just reached 0");
}

#[test]
fn paper_stats_updated_on_close() {
    let engine = Arc::new(Engine::<20, 20>::new());
    {
        let mut sb = engine.spot_book.write();
        sb.update_bids(&[Level { price: Fixed64::from_float(50000.0), qty: 1_000_000 }]);
        sb.update_asks(&[Level { price: Fixed64::from_float(50001.0), qty: 1_000_000 }]);
    }
    {
        let mut pb = engine.perp_book.write();
        pb.update_bids(&[Level { price: Fixed64::from_float(49999.0), qty: 1_000_000 }]);
        pb.update_asks(&[Level { price: Fixed64::from_float(50020.0), qty: 1_000_000 }]);
    }

    let strat = Strategy::new(Arc::clone(&engine), Fixed64::from_float(10.0).raw());
    strat.on_tick(); // opens
    assert_eq!(strat.get_paper_stats().total_trades, 0);

    // Invert spread
    {
        let mut pb = engine.perp_book.write();
        pb.update_asks(&[Level { price: Fixed64::from_float(49990.0), qty: 1_000_000 }]);
    }
    strat.on_tick(); // closes
    let stats = strat.get_paper_stats();
    assert_eq!(stats.total_trades, 1, "trade count incremented on close");
    assert_eq!(stats.wins + stats.losses, 1, "win or loss recorded");
}
