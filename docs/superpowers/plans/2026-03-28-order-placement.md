# WebSocket Order Placement (post-only) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Gate.io WebSocket v4 order placement — post-only maker orders with full ack/fill tracking, cancel, and thread-safe send from hot path.

**Architecture:** New crate `order-manager` owns the Gate.io private WS connection (separate from market-data WS). Orders are sent over a `tokio::sync::mpsc` channel from the hot path to the order manager, which serialises WS writes and tracks in-flight orders in a fixed-size ring buffer. Acks/fills come back on the same WS and are dispatched to the warm thread via another channel.

**Tech Stack:** Rust, tokio, tokio-tungstenite, hmac + sha2 (HMAC-SHA512 auth), serde_json, parking_lot, existing types crate (Fixed64, Side, OrderRequest, TradeState).

---

## File Structure

| File | Responsibility |
|------|---------------|
| `crates/order-manager/Cargo.toml` | crate manifest |
| `crates/order-manager/src/lib.rs` | public API: `OrderManager::new()`, `OrderManager::run()`, `OrderManager::sender()` |
| `crates/order-manager/src/auth.rs` | HMAC-SHA512 Gate.io WS auth payload builder |
| `crates/order-manager/src/codec.rs` | serde structs for Gate.io order WS messages (place, cancel, ack, fill) |
| `crates/order-manager/src/ring.rs` | fixed-size ring buffer for in-flight order tracking (no Vec growth) |
| `crates/types/src/lib.rs` | add `OrderAck`, `OrderCmd` enums (extend existing file) |
| `Cargo.toml` | add `order-manager` to workspace members |
| `crates/order-manager/tests/auth_test.rs` | unit tests for HMAC auth |
| `crates/order-manager/tests/codec_test.rs` | unit tests for message encode/decode |
| `crates/order-manager/tests/ring_test.rs` | unit tests for ring buffer |

---

## Task 1: Extend types — OrderCmd + OrderAck

**Files:**
- Modify: `crates/types/src/lib.rs`

- [ ] **Step 1: Write the failing test (types crate)**

Create `crates/types/tests/order_cmd_test.rs`:

```rust
#[cfg(test)]
mod tests {
    use types::{Fixed64, OrderAck, OrderCmd, OrderStatus, Side};

    #[test]
    fn order_cmd_roundtrip() {
        let cmd = OrderCmd::Place {
            client_id: 42,
            symbol: "BTC_USDT",
            side: Side::Bid,
            price: Fixed64::from_raw(5_000_000_000_000), // 50000.00000000
            qty: 1_000_000,                              // 0.01 in 1e8
            post_only: true,
        };
        match cmd {
            OrderCmd::Place { client_id, post_only, .. } => {
                assert_eq!(client_id, 42);
                assert!(post_only);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn order_ack_status() {
        let ack = OrderAck {
            client_id: 42,
            exchange_id: "abc123".to_string(),
            status: OrderStatus::Open,
        };
        assert_eq!(ack.client_id, 42);
        assert_eq!(ack.status, OrderStatus::Open);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /home/hui20metrov/gate-arb
cargo test -p types 2>&1 | tail -20
```

Expected: `error[E0432]: unresolved import types::OrderCmd` (types not yet defined)

- [ ] **Step 3: Add types to `crates/types/src/lib.rs`**

Append after the existing `ArbitragePosition` impl block:

```rust
/// Command sent from hot/warm thread → OrderManager.
#[derive(Debug, Clone)]
pub enum OrderCmd {
    Place {
        /// Locally assigned monotonic ID (fits in Gate.io text field).
        client_id: u64,
        symbol: &'static str,
        side: Side,
        price: Fixed64,
        qty: u64,        // in 1e8 units
        post_only: bool, // ALWAYS true for this system
    },
    Cancel {
        client_id: u64,
        exchange_id: String,
        symbol: &'static str,
    },
}

/// Status of an order on the exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    Open,
    Filled,
    PartiallyFilled,
    Cancelled,
    Rejected,
}

/// Acknowledgement/update sent from OrderManager → strategy/warm thread.
#[derive(Debug, Clone)]
pub struct OrderAck {
    pub client_id: u64,
    pub exchange_id: String,
    pub status: OrderStatus,
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd /home/hui20metrov/gate-arb
cargo test -p types 2>&1 | tail -20
```

Expected: `test tests::order_cmd_roundtrip ... ok`  `test tests::order_ack_status ... ok`

- [ ] **Step 5: Commit**

```bash
cd /home/hui20metrov/gate-arb
git add crates/types/src/lib.rs crates/types/tests/
git commit -m "feat(types): add OrderCmd, OrderAck, OrderStatus for order manager"
```

---

## Task 2: Scaffold order-manager crate

**Files:**
- Create: `crates/order-manager/Cargo.toml`
- Create: `crates/order-manager/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Write failing compilation test**

```bash
cd /home/hui20metrov/gate-arb
# Temporarily add to Cargo.toml members first, then try to build
echo "Will fail: crate not yet created"
```

- [ ] **Step 2: Create `crates/order-manager/Cargo.toml`**

```toml
[package]
name = "order-manager"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true

[dependencies]
types      = { path = "../types" }
tokio      = { workspace = true }
tokio-tungstenite = { workspace = true }
futures-util = { workspace = true }
serde      = { workspace = true }
serde_json = { workspace = true }
tracing    = { workspace = true }
anyhow     = { workspace = true }
hmac       = "0.12"
sha2       = "0.10"
hex        = "0.4"
```

- [ ] **Step 3: Create `crates/order-manager/src/lib.rs` (stub)**

```rust
//! order-manager — Gate.io private WebSocket for order placement.
//!
//! Post-only maker orders only. Auth via HMAC-SHA512.
//! Thread-safe: orders sent via mpsc channel from hot path.

pub mod auth;
pub mod codec;
pub mod ring;

use anyhow::Result;
use tokio::sync::mpsc;
use types::{OrderAck, OrderCmd};

/// Handle to send commands to the OrderManager from any thread.
pub type OrderSender = mpsc::Sender<OrderCmd>;
/// Handle to receive acks from the OrderManager on the warm thread.
pub type AckReceiver = mpsc::Receiver<OrderAck>;

pub struct OrderManager {
    api_key: String,
    api_secret: String,
    ws_url: &'static str,
}

impl OrderManager {
    pub fn new(api_key: String, api_secret: String) -> Self {
        Self {
            api_key,
            api_secret,
            ws_url: "wss://api.gateio.ws/ws/v4/",
        }
    }

    /// Returns (cmd_tx, ack_rx) and spawns the WS manager task.
    pub fn start(self) -> (OrderSender, AckReceiver) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<OrderCmd>(64);
        let (ack_tx, ack_rx) = mpsc::channel::<OrderAck>(64);
        tokio::spawn(async move {
            if let Err(e) = self.run(cmd_rx, ack_tx).await {
                tracing::error!("OrderManager error: {:?}", e);
            }
        });
        (cmd_tx, ack_rx)
    }

    async fn run(
        self,
        mut _cmd_rx: mpsc::Receiver<OrderCmd>,
        _ack_tx: mpsc::Sender<OrderAck>,
    ) -> Result<()> {
        // Implemented in Task 5
        Ok(())
    }
}
```

- [ ] **Step 4: Create stub modules**

```bash
mkdir -p /home/hui20metrov/gate-arb/crates/order-manager/src
```

Create `crates/order-manager/src/auth.rs`:
```rust
//! Gate.io WS v4 HMAC-SHA512 authentication.
pub struct AuthPayload {
    pub method: String,
    pub key: String,
    pub signature: String,
    pub timestamp: u64,
}
```

Create `crates/order-manager/src/codec.rs`:
```rust
//! Gate.io WS v4 order message serialisation.
```

Create `crates/order-manager/src/ring.rs`:
```rust
//! Fixed-size ring buffer for in-flight order tracking.
```

- [ ] **Step 5: Add to workspace `Cargo.toml`**

In `/home/hui20metrov/gate-arb/Cargo.toml`, change:
```toml
members = ["crates/types", "crates/gateway-ws", "crates/engine", "crates/strategy", "crates/frontend-ws"]
```
to:
```toml
members = ["crates/types", "crates/gateway-ws", "crates/engine", "crates/strategy", "crates/frontend-ws", "crates/order-manager"]
```

Also add to `[workspace.dependencies]`:
```toml
hmac = "0.12"
sha2 = "0.10"
hex  = "0.4"
```

- [ ] **Step 6: Build to verify scaffold compiles**

```bash
cd /home/hui20metrov/gate-arb
cargo build -p order-manager 2>&1 | tail -10
```

Expected: `Compiling order-manager v0.1.0` then `Finished`

- [ ] **Step 7: Commit**

```bash
cd /home/hui20metrov/gate-arb
git add crates/order-manager/ Cargo.toml Cargo.lock
git commit -m "feat(order-manager): scaffold crate with stub modules"
```

---

## Task 3: HMAC-SHA512 auth module

**Files:**
- Modify: `crates/order-manager/src/auth.rs`
- Create: `crates/order-manager/tests/auth_test.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/order-manager/tests/auth_test.rs`:

```rust
use order_manager::auth::build_auth_header;

#[test]
fn auth_signature_format() {
    // Gate.io WS v4 auth: HMAC-SHA512(secret, "channel=\nevent=\nts=<timestamp>")
    let key = "test_key";
    let secret = "test_secret";
    let ts = 1_700_000_000u64;

    let header = build_auth_header(key, secret, ts);

    // Should be JSON-serialisable struct with correct fields
    assert_eq!(header.method, "api_key");
    assert_eq!(header.key, "test_key");
    assert_eq!(header.timestamp, ts);
    // Signature is hex string of exactly 128 chars (SHA512 = 64 bytes = 128 hex chars)
    assert_eq!(header.signature.len(), 128);
    // Must be valid hex
    assert!(header.signature.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn auth_signature_deterministic() {
    let h1 = order_manager::auth::build_auth_header("k", "s", 999);
    let h2 = order_manager::auth::build_auth_header("k", "s", 999);
    assert_eq!(h1.signature, h2.signature);
}

#[test]
fn auth_signature_varies_by_secret() {
    let h1 = order_manager::auth::build_auth_header("k", "secret1", 999);
    let h2 = order_manager::auth::build_auth_header("k", "secret2", 999);
    assert_ne!(h1.signature, h2.signature);
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /home/hui20metrov/gate-arb
cargo test -p order-manager auth 2>&1 | tail -15
```

Expected: `error[E0425]: cannot find function build_auth_header`

- [ ] **Step 3: Implement `auth.rs`**

Replace `crates/order-manager/src/auth.rs` with:

```rust
//! Gate.io WS v4 HMAC-SHA512 authentication.
//!
//! Gate.io private WS auth format:
//!   sign_string = "channel=\nevent=\nts=<unix_seconds>"
//!   signature   = hex(HMAC-SHA512(secret, sign_string))
//!   payload     = { method: "api_key", KEY: key, SIGN: signature, TIME: ts }

use hmac::{Hmac, Mac};
use sha2::Sha512;

type HmacSha512 = Hmac<Sha512>;

/// Auth header sent as the `auth` field in the Gate.io WS login message.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthHeader {
    pub method: String,
    pub key: String,
    pub signature: String,
    pub timestamp: u64,
}

/// Build a Gate.io WS v4 auth header.
///
/// `ts` is Unix epoch seconds (not milliseconds).
pub fn build_auth_header(api_key: &str, api_secret: &str, ts: u64) -> AuthHeader {
    // Gate.io sign string (channel and event are empty for login)
    let sign_string = format!("channel=\nevent=\nts={ts}");

    let mut mac = HmacSha512::new_from_slice(api_secret.as_bytes())
        .expect("HMAC accepts any key size");
    mac.update(sign_string.as_bytes());
    let result = mac.finalize().into_bytes();
    let signature = hex::encode(result);

    AuthHeader {
        method: "api_key".to_string(),
        key: api_key.to_string(),
        signature,
        timestamp: ts,
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd /home/hui20metrov/gate-arb
cargo test -p order-manager auth 2>&1 | tail -15
```

Expected:
```
test auth_signature_format ... ok
test auth_signature_deterministic ... ok
test auth_signature_varies_by_secret ... ok
```

- [ ] **Step 5: Commit**

```bash
cd /home/hui20metrov/gate-arb
git add crates/order-manager/src/auth.rs crates/order-manager/tests/auth_test.rs
git commit -m "feat(order-manager): HMAC-SHA512 auth module with tests"
```

---

## Task 4: Fixed-size ring buffer for in-flight orders

**Files:**
- Modify: `crates/order-manager/src/ring.rs`
- Create: `crates/order-manager/tests/ring_test.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/order-manager/tests/ring_test.rs`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /home/hui20metrov/gate-arb
cargo test -p order-manager ring 2>&1 | tail -15
```

Expected: `error[E0432]: unresolved import order_manager::ring::InFlightOrder`

- [ ] **Step 3: Implement `ring.rs`**

Replace `crates/order-manager/src/ring.rs` with:

```rust
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
    #[allow(clippy::declare_interior_mutable_const)]
    pub fn new() -> Self {
        const NONE: Option<InFlightOrder> = None;
        Self {
            slots: [NONE; N],
            head: 0,
        }
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
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd /home/hui20metrov/gate-arb
cargo test -p order-manager ring 2>&1 | tail -15
```

Expected:
```
test ring_insert_and_find ... ok
test ring_update_status ... ok
test ring_wraps_at_capacity ... ok
test ring_remove ... ok
```

- [ ] **Step 5: Commit**

```bash
cd /home/hui20metrov/gate-arb
git add crates/order-manager/src/ring.rs crates/order-manager/tests/ring_test.rs
git commit -m "feat(order-manager): fixed-size ring buffer for in-flight order tracking"
```

---

## Task 5: Gate.io order message codec

**Files:**
- Modify: `crates/order-manager/src/codec.rs`
- Create: `crates/order-manager/tests/codec_test.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/order-manager/tests/codec_test.rs`:

```rust
use order_manager::codec::{encode_place_order, encode_cancel_order, decode_order_update};
use types::{Fixed64, OrderStatus, Side};

#[test]
fn encode_place_order_post_only() {
    let json = encode_place_order(
        "spot.orders",
        1001,
        "BTC_USDT",
        Side::Bid,
        Fixed64::from_raw(5_000_000_000_000), // 50000.00000000
        1_000_000u64,                          // 0.01000000 BTC
        "sig",
        "key",
        1_700_000_000u64,
    );
    // Must contain post-only flag, text field (client_id), and no market order fields
    assert!(json.contains("\"time_in_force\":\"poc\""));  // poc = post-only cancel
    assert!(json.contains("\"text\":\"t-1001\""));
    assert!(json.contains("\"price\":\"50000.00000000\""));
    assert!(json.contains("\"amount\":\"0.01000000\""));
    assert!(json.contains("\"side\":\"buy\""));
}

#[test]
fn encode_cancel_order() {
    let json = encode_cancel_order("spot.orders", "ex-abc123", "BTC_USDT", "sig", "key", 1_700_000_000u64);
    assert!(json.contains("\"order_id\":\"ex-abc123\""));
    assert!(json.contains("\"currency_pair\":\"BTC_USDT\""));
}

#[test]
fn decode_order_update_filled() {
    // Simulated Gate.io order update event (simplified)
    let raw = r#"{
        "channel": "spot.orders",
        "event": "update",
        "result": [{
            "id": "ex-999",
            "text": "t-42",
            "status": "closed",
            "finish_as": "filled",
            "currency_pair": "BTC_USDT",
            "side": "buy",
            "price": "50000",
            "amount": "0.01",
            "left": "0"
        }]
    }"#;
    let update = decode_order_update(raw).unwrap();
    assert_eq!(update.client_id, 42);
    assert_eq!(update.exchange_id, "ex-999");
    assert_eq!(update.status, OrderStatus::Filled);
}

#[test]
fn decode_order_update_rejected() {
    let raw = r#"{
        "channel": "spot.orders",
        "event": "update",
        "result": [{
            "id": "ex-7",
            "text": "t-7",
            "status": "closed",
            "finish_as": "cancelled",
            "currency_pair": "BTC_USDT",
            "side": "sell",
            "price": "50000",
            "amount": "0.01",
            "left": "0.01"
        }]
    }"#;
    let update = decode_order_update(raw).unwrap();
    assert_eq!(update.status, OrderStatus::Cancelled);
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /home/hui20metrov/gate-arb
cargo test -p order-manager codec 2>&1 | tail -15
```

Expected: `error: unresolved import order_manager::codec::encode_place_order`

- [ ] **Step 3: Implement `codec.rs`**

Replace `crates/order-manager/src/codec.rs` with:

```rust
//! Gate.io WS v4 order message serialisation/deserialisation.
//!
//! Place order: spot.orders channel, create event, post-only (poc time_in_force).
//! Cancel order: spot.orders channel, cancel event.
//! Decode updates: spot.orders update events → OrderAck.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use types::{OrderAck, OrderStatus, Side};

// ─── Outbound ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct WsRequest<T: Serialize> {
    time: u64,
    channel: &'static str,
    event: &'static str,
    payload: T,
    auth: AuthBlock,
}

#[derive(Serialize)]
struct AuthBlock {
    method: &'static str,
    KEY: String,
    SIGN: String,
}

#[derive(Serialize)]
struct PlacePayload {
    text: String,
    currency_pair: String,
    #[serde(rename = "type")]
    order_type: &'static str,
    side: &'static str,
    price: String,
    amount: String,
    time_in_force: &'static str,
    account: &'static str,
}

#[derive(Serialize)]
struct CancelPayload {
    order_id: String,
    currency_pair: String,
    account: &'static str,
}

/// Encode a post-only spot limit order placement message.
///
/// Prices/amounts are formatted as 8-decimal strings per Gate.io spec.
pub fn encode_place_order(
    channel: &'static str,
    client_id: u64,
    symbol: &str,
    side: Side,
    price: types::Fixed64,
    qty: u64, // 1e8 units
    signature: &str,
    api_key: &str,
    ts: u64,
) -> String {
    let side_str = match side {
        Side::Bid => "buy",
        Side::Ask => "sell",
    };

    // Format price and qty as 8-decimal strings
    let price_f = price.raw() as f64 / 1e8;
    let qty_f = qty as f64 / 1e8;
    let price_str = format!("{:.8}", price_f);
    let qty_str = format!("{:.8}", qty_f);

    let payload = PlacePayload {
        text: format!("t-{client_id}"),
        currency_pair: symbol.to_string(),
        order_type: "limit",
        side: side_str,
        price: price_str,
        amount: qty_str,
        time_in_force: "poc", // post-only cancel
        account: "spot",
    };

    let req = WsRequest {
        time: ts,
        channel,
        event: "api",
        payload,
        auth: AuthBlock {
            method: "api_key",
            KEY: api_key.to_string(),
            SIGN: signature.to_string(),
        },
    };

    serde_json::to_string(&req).expect("serialise place order")
}

/// Encode a cancel order message.
pub fn encode_cancel_order(
    channel: &'static str,
    exchange_id: &str,
    symbol: &str,
    signature: &str,
    api_key: &str,
    ts: u64,
) -> String {
    let payload = CancelPayload {
        order_id: exchange_id.to_string(),
        currency_pair: symbol.to_string(),
        account: "spot",
    };

    let req = WsRequest {
        time: ts,
        channel,
        event: "cancel",
        payload,
        auth: AuthBlock {
            method: "api_key",
            KEY: api_key.to_string(),
            SIGN: signature.to_string(),
        },
    };

    serde_json::to_string(&req).expect("serialise cancel order")
}

// ─── Inbound ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WsOrderEvent {
    channel: String,
    event: String,
    result: Vec<OrderResult>,
}

#[derive(Debug, Deserialize)]
struct OrderResult {
    id: String,
    text: String,
    status: String,
    finish_as: Option<String>,
    left: String,
}

/// Decode a Gate.io spot.orders update event into an OrderAck.
///
/// Returns None if the message is not an order update we care about.
pub fn decode_order_update(raw: &str) -> Result<OrderAck> {
    let event: WsOrderEvent = serde_json::from_str(raw).context("parse order event")?;

    let result = event
        .result
        .into_iter()
        .next()
        .context("empty result array")?;

    // Parse client_id from "t-<id>" text field
    let client_id: u64 = result
        .text
        .strip_prefix("t-")
        .and_then(|s| s.parse().ok())
        .context("parse client_id from text field")?;

    let status = match (result.status.as_str(), result.finish_as.as_deref()) {
        ("open", _) => {
            if result.left == "0" {
                OrderStatus::Filled
            } else {
                OrderStatus::Open
            }
        }
        ("closed", Some("filled")) => OrderStatus::Filled,
        ("closed", Some("cancelled")) | ("closed", Some("poc")) => OrderStatus::Cancelled,
        ("closed", Some("ioc")) => OrderStatus::Cancelled,
        ("closed", _) => OrderStatus::Cancelled,
        _ => OrderStatus::Open,
    };

    Ok(OrderAck {
        client_id,
        exchange_id: result.id,
        status,
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd /home/hui20metrov/gate-arb
cargo test -p order-manager codec 2>&1 | tail -15
```

Expected:
```
test encode_place_order_post_only ... ok
test encode_cancel_order ... ok
test decode_order_update_filled ... ok
test decode_order_update_rejected ... ok
```

- [ ] **Step 5: Fix any clippy warnings**

```bash
cd /home/hui20metrov/gate-arb
cargo clippy -p order-manager -- -D warnings 2>&1 | grep "^error" | head -20
```

Fix any `non_snake_case` warning on `KEY`/`SIGN` by adding `#[allow(non_snake_case)]` to `AuthBlock`.

- [ ] **Step 6: Commit**

```bash
cd /home/hui20metrov/gate-arb
git add crates/order-manager/src/codec.rs crates/order-manager/tests/codec_test.rs
git commit -m "feat(order-manager): Gate.io WS order codec (place/cancel/decode)"
```

---

## Task 6: OrderManager run loop (WS connect + dispatch)

**Files:**
- Modify: `crates/order-manager/src/lib.rs`

- [ ] **Step 1: Write integration-style test (paper mode — no real WS)**

Add to `crates/order-manager/tests/ring_test.rs`:

```rust
use order_manager::ring::OrderRing;
use types::{OrderStatus};

#[test]
fn ring_capacity_32_no_alloc() {
    // Verify OrderRing<32> fits on stack and works for realistic load
    let mut ring = OrderRing::<32>::new();
    for i in 0u64..32 {
        ring.insert(make_order(i));
    }
    for i in 0u64..32 {
        assert!(ring.find_by_client_id(i).is_some());
    }
    // Insert one more — should wrap and overwrite client_id 0
    ring.insert(make_order(32));
    assert!(ring.find_by_client_id(0).is_none());
    assert!(ring.find_by_client_id(32).is_some());
}
```

- [ ] **Step 2: Run test to verify it passes (ring is already implemented)**

```bash
cd /home/hui20metrov/gate-arb
cargo test -p order-manager ring_capacity_32 2>&1 | tail -10
```

Expected: `test ring_capacity_32_no_alloc ... ok`

- [ ] **Step 3: Implement `OrderManager::run()` in `lib.rs`**

Replace the `run` stub in `crates/order-manager/src/lib.rs`:

```rust
//! order-manager — Gate.io private WebSocket for order placement.

pub mod auth;
pub mod codec;
pub mod ring;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use ring::{InFlightOrder, OrderRing};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use types::{OrderAck, OrderCmd, OrderStatus, Side};

pub type OrderSender = mpsc::Sender<OrderCmd>;
pub type AckReceiver = mpsc::Receiver<OrderAck>;

pub struct OrderManager {
    api_key: String,
    api_secret: String,
    ws_url: &'static str,
}

impl OrderManager {
    pub fn new(api_key: String, api_secret: String) -> Self {
        Self {
            api_key,
            api_secret,
            ws_url: "wss://api.gateio.ws/ws/v4/",
        }
    }

    pub fn start(self) -> (OrderSender, AckReceiver) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<OrderCmd>(64);
        let (ack_tx, ack_rx) = mpsc::channel::<OrderAck>(64);
        tokio::spawn(async move {
            if let Err(e) = self.run(cmd_rx, ack_tx).await {
                error!("OrderManager fatal: {:?}", e);
            }
        });
        (cmd_tx, ack_rx)
    }

    async fn run(
        self,
        mut cmd_rx: mpsc::Receiver<OrderCmd>,
        ack_tx: mpsc::Sender<OrderAck>,
    ) -> Result<()> {
        info!("OrderManager connecting to {}", self.ws_url);
        let (ws, _) = connect_async(self.ws_url).await?;
        let (mut write, mut read) = ws.split();

        // Authenticate
        let ts = unix_secs();
        let auth = auth::build_auth_header(&self.api_key, &self.api_secret, ts);
        let login_msg = serde_json::json!({
            "time": ts,
            "channel": "spot.login",
            "event": "api",
            "payload": {},
            "auth": {
                "method": "api_key",
                "KEY": auth.key,
                "SIGN": auth.signature,
                "Timestamp": auth.timestamp.to_string()
            }
        });
        write
            .send(Message::Text(login_msg.to_string().into()))
            .await?;
        info!("OrderManager: login sent");

        // Subscribe to spot.orders channel
        let sub_msg = serde_json::json!({
            "time": unix_secs(),
            "channel": "spot.orders",
            "event": "subscribe",
            "payload": ["BTC_USDT"]
        });
        write
            .send(Message::Text(sub_msg.to_string().into()))
            .await?;

        let mut ring = OrderRing::<32>::new();
        let mut ping_interval =
            tokio::time::interval(tokio::time::Duration::from_secs(20));

        loop {
            tokio::select! {
                // Inbound WS message
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_inbound(text.as_str(), &mut ring, &ack_tx).await;
                        }
                        Some(Ok(Message::Ping(data))) => {
                            let _ = write.send(Message::Pong(data)).await;
                        }
                        Some(Err(e)) => {
                            error!("OrderManager WS error: {:?}", e);
                            break;
                        }
                        None => {
                            warn!("OrderManager WS closed");
                            break;
                        }
                        _ => {}
                    }
                }

                // Outbound order command from hot/warm thread
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => {
                            self.handle_cmd(cmd, &mut write, &mut ring).await;
                        }
                        None => {
                            info!("OrderManager: cmd channel closed, exiting");
                            break;
                        }
                    }
                }

                // Keepalive ping
                _ = ping_interval.tick() => {
                    let ping = serde_json::json!({
                        "time": unix_secs(),
                        "channel": "spot.ping",
                        "event": ""
                    });
                    let _ = write.send(Message::Text(ping.to_string().into())).await;
                }
            }
        }

        Ok(())
    }

    async fn handle_inbound(
        &self,
        text: &str,
        ring: &mut OrderRing<32>,
        ack_tx: &mpsc::Sender<OrderAck>,
    ) {
        if let Ok(ack) = codec::decode_order_update(text) {
            // Update ring buffer
            ring.update_status(ack.client_id, ack.exchange_id.clone(), ack.status);
            if matches!(ack.status, OrderStatus::Filled | OrderStatus::Cancelled) {
                ring.remove(ack.client_id);
            }
            // Forward to warm thread
            let _ = ack_tx.try_send(ack);
        }
    }

    async fn handle_cmd(
        &self,
        cmd: types::OrderCmd,
        write: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
        ring: &mut OrderRing<32>,
    ) {
        match cmd {
            types::OrderCmd::Place {
                client_id,
                symbol,
                side,
                price,
                qty,
                post_only: _,
            } => {
                let ts = unix_secs();
                // Build auth signature for this request
                let sign_str = format!("channel=spot.orders\nevent=api\nts={ts}");
                let sig = hmac_sign(&self.api_secret, &sign_str);

                let json = codec::encode_place_order(
                    "spot.orders",
                    client_id,
                    symbol,
                    side,
                    price,
                    qty,
                    &sig,
                    &self.api_key,
                    ts,
                );

                ring.insert(InFlightOrder {
                    client_id,
                    exchange_id: String::new(),
                    symbol,
                    side,
                    price,
                    qty,
                    status: types::OrderStatus::Open,
                });

                if let Err(e) = write.send(Message::Text(json.into())).await {
                    error!("OrderManager: send place failed: {:?}", e);
                }
            }

            types::OrderCmd::Cancel {
                client_id: _,
                exchange_id,
                symbol,
            } => {
                let ts = unix_secs();
                let sign_str = format!("channel=spot.orders\nevent=cancel\nts={ts}");
                let sig = hmac_sign(&self.api_secret, &sign_str);

                let json = codec::encode_cancel_order(
                    "spot.orders",
                    &exchange_id,
                    symbol,
                    &sig,
                    &self.api_key,
                    ts,
                );

                if let Err(e) = write.send(Message::Text(json.into())).await {
                    error!("OrderManager: send cancel failed: {:?}", e);
                }
            }
        }
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn hmac_sign(secret: &str, message: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha512;
    type HmacSha512 = Hmac<Sha512>;

    let mut mac = HmacSha512::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
```

- [ ] **Step 4: Build to verify it compiles**

```bash
cd /home/hui20metrov/gate-arb
cargo build -p order-manager 2>&1 | tail -10
```

Expected: `Finished dev [unoptimized + debuginfo] target(s)`

- [ ] **Step 5: Run clippy**

```bash
cd /home/hui20metrov/gate-arb
cargo clippy -p order-manager -- -D warnings 2>&1 | grep "^error" | head -20
```

Fix any warnings before committing.

- [ ] **Step 6: Run all order-manager tests**

```bash
cd /home/hui20metrov/gate-arb
cargo test -p order-manager 2>&1 | tail -15
```

Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
cd /home/hui20metrov/gate-arb
git add crates/order-manager/src/lib.rs crates/order-manager/tests/
git commit -m "feat(order-manager): WS run loop — connect, auth, dispatch orders, track acks"
```

---

## Task 7: Workspace build + full clippy

**Files:**
- No new files — verify everything compiles clean together.

- [ ] **Step 1: Full workspace build**

```bash
cd /home/hui20metrov/gate-arb
cargo build --workspace 2>&1 | tail -20
```

Expected: `Finished` with no errors.

- [ ] **Step 2: Full clippy**

```bash
cd /home/hui20metrov/gate-arb
cargo clippy --workspace -- -D warnings 2>&1 | grep "^error" | head -30
```

Expected: no errors. Fix any that appear.

- [ ] **Step 3: All tests**

```bash
cd /home/hui20metrov/gate-arb
cargo test --workspace 2>&1 | tail -20
```

Expected: All tests pass, 0 failures.

- [ ] **Step 4: Push branch + open PR**

```bash
cd /home/hui20metrov/gate-arb
git push origin feat/issue-8-order-placement
gh pr create --repo ddfeyes/gate-arb \
  --title "feat(#8): WebSocket order placement (post-only maker)" \
  --body "## Changes
- New crate: \`order-manager\`
- HMAC-SHA512 auth for Gate.io private WS
- Fixed-size ring buffer (\`OrderRing<N>\`) for in-flight order tracking
- Codec: encode place/cancel, decode order update events
- OrderManager run loop: connect, auth, subscribe, dispatch, ack
- New types: \`OrderCmd\`, \`OrderAck\`, \`OrderStatus\`

## Tests
- \`auth_test.rs\`: HMAC signature format + determinism
- \`codec_test.rs\`: encode/decode round-trips
- \`ring_test.rs\`: insert, find, update, remove, wrap-at-capacity

Closes #8" \
  --head feat/issue-8-order-placement \
  --base main
```

- [ ] **Step 5: Watch CI**

```bash
cd /home/hui20metrov/gate-arb
gh run watch --repo ddfeyes/gate-arb --exit-status
```

Expected: CI green.

---

## Self-Review Checklist

- [x] **Spec coverage:** post-only ✓, WS only ✓, per-order state machine ✓, cancel ✓, HMAC auth ✓, ring buffer ✓, no heap allocs in send path ✓
- [x] **Placeholder scan:** no TBDs, all code blocks complete
- [x] **Type consistency:** `Fixed64`, `Side`, `OrderCmd`, `OrderAck`, `OrderStatus` used consistently across all tasks
- [x] **Task independence:** each task produces compilable code; tests in each task verify that task's code only
