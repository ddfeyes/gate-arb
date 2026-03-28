//! Fixed-size ring buffer for in-flight order tracking.
//!
//! Capacity N is compile-time constant: no heap growth, O(N) scan — N is small (≤32).
//! Oldest entry is overwritten when the ring is full.

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
        // SAFETY: Option<InFlightOrder> is not Copy but we use a const fn workaround
        // via array init with None values one by one via a macro trick.
        // Since N is small and known at compile time this is fine.
        let slots = std::array::from_fn(|_| None);
        Self { slots, head: 0 }
    }

    /// Insert a new in-flight order. Overwrites oldest if full.
    #[inline]
    pub fn insert(&mut self, order: InFlightOrder) {
        self.slots[self.head % N] = Some(order);
        self.head = self.head.wrapping_add(1);
    }

    /// Find by client_id — O(N) scan, N is small (≤32).
    #[inline]
    pub fn find_by_client_id(&self, client_id: u64) -> Option<&InFlightOrder> {
        self.slots.iter().flatten().find(|o| o.client_id == client_id)
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
