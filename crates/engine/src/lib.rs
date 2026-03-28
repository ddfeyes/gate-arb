//! engine — hot path: WS recv → book update → spread check → signal.
//!
//! Thread 1 (hot): <1ms processing, no heap, fixed-point arithmetic.

use parking_lot::RwLock;
use std::sync::Arc;
#[allow(unused_imports)]
use tracing::info;

use types::{Fixed64, OrderBook, SpreadSignal, SCALE};

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

    /// Check spread between spot and perp — called from hot path.
    /// Returns Some(signal) if spread exceeds threshold.
    #[inline(always)]
    pub fn check_spread(&self, threshold_raw: u64) -> Option<SpreadSignal> {
        let spot = self.spot_book.read();
        let perp = self.perp_book.read();

        let best_bid = spot.best_bid()?;
        let _best_ask = spot.best_ask()?;
        let _perp_bid = perp.best_bid()?;
        let perp_ask = perp.best_ask()?;

        // Cross-exchange spread: (perp_ask - spot_bid) / spot_bid
        let spread_raw = perp_ask.raw().saturating_sub(best_bid.raw());
        let spread_pct_raw = if best_bid.raw() > 0 {
            (spread_raw as u128 * SCALE as u128 / best_bid.raw() as u128) as u64
        } else {
            return None;
        };

        if spread_raw >= threshold_raw {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_micros() as u64;

            Some(SpreadSignal {
                spread_raw,
                spread_pct: Fixed64::from_raw(spread_pct_raw),
                bid_price: best_bid,
                ask_price: perp_ask,
                timestamp_us: now,
            })
        } else {
            None
        }
    }
}

impl<const B: usize, const A: usize> Default for Engine<B, A> {
    fn default() -> Self {
        Self::new()
    }
}
