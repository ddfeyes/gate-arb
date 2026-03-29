//! engine — hot path: WS recv → book update → spread check → signal.
//!
//! Thread 1 (hot): <1ms processing, no heap, fixed-point arithmetic.

use parking_lot::RwLock;
use std::sync::Arc;
#[allow(unused_imports)]
use tracing::info;

use types::{Fixed64, OrderBook, SpreadSignal, SCALE};

/// Current spread snapshot — returned regardless of threshold.
#[derive(Debug, Clone, Copy)]
pub struct SpreadSnapshot {
    /// Spread in raw Fixed64 units. Saturating at 0 (negative = inverted).
    pub spread_raw: u64,
    /// True if the spread is inverted (spot > perp — exit territory).
    pub inverted: bool,
    /// Spot best bid at snapshot time.
    pub spot_bid: Fixed64,
    /// Perp best ask at snapshot time.
    pub perp_ask: Fixed64,
}

/// Hot engine state — lock-free read path.
pub struct Engine<const B: usize, const A: usize> {
    pub spot_book: Arc<RwLock<OrderBook<B, A>>>,
    pub perp_book: Arc<RwLock<OrderBook<B, A>>>,
}

impl<const B: usize, const A: usize> Engine<B, A> {
    pub fn new() -> Self {
        Self {
            spot_book: Arc::new(RwLock::new(OrderBook::new())),
            perp_book: Arc::new(RwLock::new(OrderBook::new())),
        }
    }

    /// Return current spread snapshot (no threshold gate — always returns if books have data).
    ///
    /// Normal direction: short perp + buy spot.
    /// spread_raw = perp_ask − spot_bid (clamped to 0 when negative/inverted).
    /// inverted = true when perp_ask < spot_bid (i.e. spread has converged past 0 → exit signal).
    #[inline(always)]
    pub fn current_spread(&self) -> Option<SpreadSnapshot> {
        let spot = self.spot_book.read();
        let perp = self.perp_book.read();

        let spot_bid = spot.best_bid()?;
        let perp_ask = perp.best_ask()?;

        let inverted = perp_ask.raw() < spot_bid.raw();
        let spread_raw = if inverted {
            0
        } else {
            perp_ask.raw() - spot_bid.raw()
        };

        Some(SpreadSnapshot {
            spread_raw,
            inverted,
            spot_bid,
            perp_ask,
        })
    }

    /// Inverse spread snapshot: spot_ask − perp_bid direction.
    ///
    /// Positive when spot is trading above perp (buy perp, short spot).
    /// Used for monitoring and future inverse arb strategy.
    #[inline(always)]
    pub fn current_spread_inverse(&self) -> Option<SpreadSnapshot> {
        let spot = self.spot_book.read();
        let perp = self.perp_book.read();

        let spot_ask = spot.best_ask()?;
        let perp_bid = perp.best_bid()?;

        let inverted = spot_ask.raw() < perp_bid.raw();
        let spread_raw = if inverted {
            0
        } else {
            spot_ask.raw() - perp_bid.raw()
        };

        Some(SpreadSnapshot {
            spread_raw,
            inverted,
            // Reuse fields: bid_price = spot_ask, ask_price = perp_bid (inverse dir)
            spot_bid: spot_ask,
            perp_ask: perp_bid,
        })
    }

    /// Check spread — returns Some(signal) only when spread exceeds `threshold_raw`.
    ///
    /// Used for entry detection. For exit logic, use `current_spread()` directly.
    #[inline(always)]
    pub fn check_spread(&self, threshold_raw: u64) -> Option<SpreadSignal> {
        let snap = self.current_spread()?;

        if snap.spread_raw < threshold_raw {
            return None;
        }

        let spread_pct_raw = if snap.spot_bid.raw() > 0 {
            (snap.spread_raw as u128 * SCALE as u128 / snap.spot_bid.raw() as u128) as u64
        } else {
            return None;
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;

        Some(SpreadSignal {
            spread_raw: snap.spread_raw,
            spread_pct: Fixed64::from_raw(spread_pct_raw),
            bid_price: snap.spot_bid,
            ask_price: snap.perp_ask,
            timestamp_us: now,
        })
    }
}

impl<const B: usize, const A: usize> Default for Engine<B, A> {
    fn default() -> Self {
        Self::new()
    }
}
