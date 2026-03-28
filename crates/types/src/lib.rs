//! types — fixed-point primitives and data structures for gate-arb.
//!
//! All price/qty values use u64 with 1e-8 precision (8 decimal places).
//! No heap allocations in the hot path.

/// Scale factor: 10^8 → all values stored as integer units.
pub const SCALE: u64 = 100_000_000;

/// Signed fixed-point for P&L (also u64 internally, with sign in context).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fixed64(u64);

impl Fixed64 {
    #[inline(always)]
    pub const fn from_raw(val: u64) -> Self {
        Self(val)
    }

    #[inline(always)]
    pub const fn from_float(val: f64) -> Self {
        Self((val * SCALE as f64) as u64)
    }

    #[inline(always)]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[inline(always)]
    pub fn as_float(self) -> f64 {
        self.0 as f64 / SCALE as f64
    }

    #[inline(always)]
    pub const fn zero() -> Self {
        Self(0)
    }

    #[inline(always)]
    pub const fn one() -> Self {
        Self(SCALE)
    }
}

impl std::fmt::Display for Fixed64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_float())
    }
}

/// Order side — always post-only (maker).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Bid,
    Ask,
}

/// Pre-allocated order book level (no heap, stack-only).
#[derive(Debug, Clone, Copy)]
pub struct Level {
    pub price: Fixed64,
    pub qty: u64,
}

/// Pre-allocated order book — fixed capacity, no Vec/Heap.
///
/// Capacity is compile-time constant to保证 <1ms hot path.
/// Book is split into bids (ascending) and asks (descending).
#[derive(Debug, Clone)]
pub struct OrderBook<const B: usize, const A: usize> {
    pub bids: [Level; B],
    pub asks: [Level; A],
    pub bid_count: usize,
    pub ask_count: usize,
}

impl<const B: usize, const A: usize> OrderBook<B, A> {
    /// Create a zero-filled order book (stack-allocated, no heap).
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            bids: [Level {
                price: Fixed64::zero(),
                qty: 0,
            }; B],
            asks: [Level {
                price: Fixed64::zero(),
                qty: 0,
            }; A],
            bid_count: 0,
            ask_count: 0,
        }
    }

    /// Update bids from a snapshot slice (first `n` levels).
    #[inline(always)]
    pub fn update_bids(&mut self, levels: &[Level]) {
        self.bid_count = levels.len().min(B);
        self.bids[..self.bid_count].copy_from_slice(&levels[..self.bid_count]);
    }

    /// Update asks from a snapshot slice.
    #[inline(always)]
    pub fn update_asks(&mut self, levels: &[Level]) {
        self.ask_count = levels.len().min(A);
        self.asks[..self.ask_count].copy_from_slice(&levels[..self.ask_count]);
    }

    /// Best bid price (highest) — top of book.
    #[inline(always)]
    pub fn best_bid(&self) -> Option<Fixed64> {
        self.bid_count.checked_sub(1).map(|i| self.bids[i].price)
    }

    /// Best ask price (lowest) — top of book.
    #[inline(always)]
    pub fn best_ask(&self) -> Option<Fixed64> {
        self.ask_count.checked_sub(1).map(|i| self.asks[i].price)
    }

    /// Spread = ask - bid in raw units.
    #[inline(always)]
    pub fn spread_raw(&self) -> Option<u64> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => ask.raw().checked_sub(bid.raw()),
            _ => None,
        }
    }
}

impl<const B: usize, const A: usize> Default for OrderBook<B, A> {
    fn default() -> Self {
        Self::new()
    }
}

/// Trade signal from spread detection.
#[derive(Debug, Clone)]
pub struct SpreadSignal {
    pub spread_raw: u64,
    pub spread_pct: Fixed64,
    pub bid_price: Fixed64,
    pub ask_price: Fixed64,
    pub timestamp_us: u64,
}

/// Order request — always post-only (maker).
#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub symbol: &'static str,
    pub side: Side,
    pub price: Fixed64,
    pub qty: u64,
    /// post_only = true means we only place if it goes on the book (no taker fill).
    pub post_only: bool,
}

/// Trade state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeState {
    Idle,
    Leg1Sent,
    Leg1Filled,
    Leg2Sent,
    BothFilled,
    Closing,
    Closed,
}

/// Arbitrage leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegKind {
    SpotBuy,
    SpotSell,
    PerpShort,
    PerpLong,
}

/// One leg of an arbitrage position.
#[derive(Debug, Clone)]
pub struct Leg {
    pub kind: LegKind,
    pub price: Fixed64,
    pub qty: u64,
    pub state: TradeState,
    pub sent_at_us: Option<u64>,
    pub filled_at_us: Option<u64>,
}

/// Full arbitrage position (2 legs: spot + perp).
#[derive(Debug, Clone)]
pub struct ArbitragePosition {
    pub legs: [Leg; 2],
    pub opened_at_us: u64,
    pub state: TradeState,
}

impl ArbitragePosition {
    pub fn new(leg1: Leg, leg2: Leg, opened_at_us: u64) -> Self {
        Self {
            legs: [leg1, leg2],
            opened_at_us,
            state: TradeState::Leg1Sent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixed64_conversion() {
        let price = Fixed64::from_float(50000.12345678);
        assert_eq!(price.raw(), 5_000_012_345_678);
        assert!((price.as_float() - 50000.12345678).abs() < 1e-6);
    }

    #[test]
    fn test_order_book_new() {
        let book: OrderBook<10, 10> = OrderBook::new();
        assert_eq!(book.bid_count, 0);
        assert_eq!(book.ask_count, 0);
    }

    #[test]
    fn test_order_book_update() {
        let mut book: OrderBook<10, 10> = OrderBook::new();
        let bids = [
            Level {
                price: Fixed64::from_float(50000.0),
                qty: 1000,
            },
            Level {
                price: Fixed64::from_float(49999.0),
                qty: 2000,
            },
        ];
        book.update_bids(&bids);
        assert_eq!(book.bid_count, 2);
        assert_eq!(
            book.best_bid().unwrap().raw(),
            Fixed64::from_float(50000.0).raw()
        );
    }

    #[test]
    fn test_spread_raw() {
        let mut book: OrderBook<10, 10> = OrderBook::new();
        book.update_bids(&[Level {
            price: Fixed64::from_float(50000.0),
            qty: 1000,
        }]);
        book.update_asks(&[Level {
            price: Fixed64::from_float(50001.0),
            qty: 1000,
        }]);
        let spread = book.spread_raw().unwrap();
        let expected = Fixed64::from_float(1.0).raw();
        assert_eq!(spread, expected);
    }
}
