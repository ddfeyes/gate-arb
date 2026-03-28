//! Fixed-size ring buffer for in-flight order tracking.
//!
//! Capacity N is compile-time constant: no heap growth, O(N) scan — N is small (≤32).
//! Oldest entry is overwritten when the ring is full.

use tracing::warn;
use types::{Fixed64, OrderStatus, Side};

#[derive(Debug, Clone)]
pub struct InFlightOrder {
    pub client_id: u64,
    pub exchange_id: String,
    pub symbol: &'static str,
    pub side: Side,
    pub price: Fixed64,
    pub qty: u64,
    pub status: OrderStatus,
}

pub struct OrderRing<const N: usize> {
    slots: [Option<InFlightOrder>; N],
    head: usize,
}

impl<const N: usize> OrderRing<N> {
    pub fn new() -> Self {
        let slots = std::array::from_fn(|_| None);
        Self { slots, head: 0 }
    }

    /// Insert a new in-flight order.
    ///
    /// If the ring is at capacity, the oldest slot is overwritten. This is a
    /// safety fallback — it should never happen in normal operation (max 32
    /// concurrent orders). A warn! is emitted to surface this condition.
    #[inline]
    pub fn insert(&mut self, order: InFlightOrder) {
        let slot = &mut self.slots[self.head % N];
        if let Some(evicted) = slot {
            warn!(
                "OrderRing at capacity (N={}): evicting in-flight order client_id={}",
                N, evicted.client_id
            );
        }
        *slot = Some(order);
        self.head = self.head.wrapping_add(1);
    }

    /// Find by client_id — O(N) scan, N is small (≤32).
    #[inline]
    pub fn find_by_client_id(&self, client_id: u64) -> Option<&InFlightOrder> {
        self.slots
            .iter()
            .flatten()
            .find(|o| o.client_id == client_id)
    }

    /// Update status + exchange_id when ack arrives.
    #[inline]
    pub fn update_status(&mut self, client_id: u64, exchange_id: String, status: OrderStatus) {
        for slot in self.slots.iter_mut().flatten() {
            if slot.client_id == client_id {
                slot.exchange_id = exchange_id;
                slot.status = status;
                return;
            }
        }
    }

    /// Remove a closed/cancelled order slot.
    #[inline]
    pub fn remove(&mut self, client_id: u64) {
        for slot in self.slots.iter_mut() {
            if matches!(slot, Some(o) if o.client_id == client_id) {
                *slot = None;
                return;
            }
        }
    }
}

impl<const N: usize> Default for OrderRing<N> {
    fn default() -> Self {
        Self::new()
    }
}
