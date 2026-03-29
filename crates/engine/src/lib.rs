//! engine — hot path: WS recv → book update → spread check → signal.
//!
//! Thread 1 (hot): <1ms processing, no heap, fixed-point arithmetic.

use parking_lot::RwLock;
use std::sync::Arc;
#[allow(unused_imports)]
use tracing::{debug, info};
use types::{Fixed64, Level, ObUpdateChannel, OrderBook, SpreadSignal, SCALE};

use gateway_ws::parse_level;

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

    /// Spawn a tokio task that receives OB updates from the gateway channel
    /// and applies them to the appropriate order book (spot or perp).
    ///
    /// `spot_symbol` and `perp_symbol` are used to route updates to the right book.
    /// The task runs until the receiver is dropped.
    pub fn spawn_ob_receiver(
        self: &Arc<Self>,
        mut rx: tokio::sync::mpsc::Receiver<ObUpdateChannel>,
        spot_symbol: String,
        perp_symbol: String,
    ) {
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(update) = rx.recv().await {
                let is_spot = update.symbol == spot_symbol;
                let is_perp = update.symbol == perp_symbol;

                if !is_spot && !is_perp {
                    continue;
                }

                // Parse bid levels
                let mut bid_levels = [Level {
                    price: Fixed64::zero(),
                    qty: 0,
                }; B];
                let bid_count = update.bids.len().min(B);
                for (i, ob_level) in update.bids.iter().take(B).enumerate() {
                    bid_levels[i] = parse_level(&ob_level.price, &ob_level.qty);
                }

                // Parse ask levels
                let mut ask_levels = [Level {
                    price: Fixed64::zero(),
                    qty: 0,
                }; A];
                let ask_count = update.asks.len().min(A);
                for (i, ob_level) in update.asks.iter().take(A).enumerate() {
                    ask_levels[i] = parse_level(&ob_level.price, &ob_level.qty);
                }

                let book = if is_spot {
                    &engine.spot_book
                } else {
                    &engine.perp_book
                };

                let mut guard = book.write();
                guard.update_bids(&bid_levels[..bid_count]);
                guard.update_asks(&ask_levels[..ask_count]);
                debug!(
                    "OB updated: {} bids={} asks={}",
                    update.symbol, bid_count, ask_count
                );
            }
        });
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
