# Issue #13: Reconnection + Gateway→Engine Data Flow

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix two bugs: (1) gateway-ws disconnects without reconnecting, (2) `process_ob_update` logs but never updates engine order books.

**Architecture:** Gateway runs in a loop with exponential backoff + jitter. Order book updates are sent via a bounded crossbeam channel to the engine's hot path. Engine receives updates from channel in its own tokio task.

**Tech Stack:** Rust (tokio + tokio-tungstenite + crossbeam-channel). No heap allocations in hot path.

---

## Issue 1: Exponential Backoff Reconnection

### Task 1: Add reconnection loop with backoff to gateway-ws

**Files:**
- Modify: `crates/gateway-ws/src/lib.rs`

- [ ] **Step 1: Add `ReconnectConfig` struct and `run_with_reconnect` method**

Add this before the `impl GatewayWs` block in `crates/gateway-ws/src/lib.rs`:

```rust
/// Reconnection configuration with exponential backoff + jitter.
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    /// Initial delay in seconds.
    pub base_delay_secs: f64,
    /// Maximum delay in seconds.
    pub max_delay_secs: f64,
    /// Jitter factor [0.0, 1.0) applied to delay.
    pub jitter: f64,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            base_delay_secs: 1.0,
            max_delay_secs: 30.0,
            jitter: 0.3,
        }
    }
}

/// Jittered exponential backoff.
fn backoff_duration(attempt: u32, config: &ReconnectConfig) -> std::time::Duration {
    let exp_delay = config.base_delay_secs * 2f64.powi(attempt as i32);
    let capped = exp_delay.min(config.max_delay_secs);
    let jitter_range = capped * config.jitter;
    let jitter = (jitter_range * fastrand::f64()).min(jitter_range);
    std::time::Duration::from_secs_f64(capped + jitter)
}
```

Add `fastrand = "0.8"` to `crates/gateway-ws/Cargo.toml` dependencies.

- [ ] **Step 2: Add `run_with_reconnect` method to `GatewayWs`**

Add after the existing `run` method (keep `run` as-is for now, add overloaded version):

```rust
/// Run with automatic reconnection — loop until shutdown signal.
/// Returns after receiving shutdown.
pub async fn run_with_reconnect(
    &self,
    spot_symbol: &str,
    perp_symbol: &str,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> Result<(), anyhow::Error> {
    let config = ReconnectConfig::default();
    let mut attempt = 0u32;

    loop {
        info!("[reconnect] attempt {} connecting to {}", attempt, self.ws_url);

        match self.connect_and_stream(spot_symbol, perp_symbol).await {
            Ok(()) => {
                info!("[reconnect] stream ended normally");
                break;
            }
            Err(e) => {
                error!("[reconnect] stream error: {:?}", e);
            }
        }

        // Wait for shutdown OR backoff timer
        let delay = backoff_duration(attempt, &config);
        info!("[reconnect] backing off {:?} (attempt {})", delay, attempt);
        attempt = attempt.saturating_add(1);

        let mut tick = tokio::time::Instant::now();
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.recv() => {
                info!("[reconnect] shutdown received, exiting");
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Internal: connect and run the WS session once.
/// Returns when the connection drops.
async fn connect_and_stream(
    &self,
    spot_symbol: &str,
    perp_symbol: &str,
) -> Result<(), anyhow::Error> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (ws, _) = connect_async(self.ws_url).await?;
    let (mut write, mut read) = ws.split();

    // Subscribe
    let spot_sub = SubscribeMsg::new(CHANNEL_SPOT_OB, spot_symbol);
    write.send(Message::Text(serde_json::to_string(&spot_sub)?.into())).await?;
    debug!("Subscribed to spot.order_book {}", spot_symbol);

    let futures_sub = SubscribeMsg::new(CHANNEL_FUTURES_OB, perp_symbol);
    write.send(Message::Text(serde_json::to_string(&futures_sub)?.into())).await?;
    debug!("Subscribed to futures.order_book {}", perp_symbol);

    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(20));

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                write.send(Message::Ping(vec![].into())).await?;
                debug!("Sent ping");
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        self.handle_message(&text).await;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        write.send(Message::Pong(data)).await?;
                    }
                    Some(Ok(Message::Close(e))) => {
                        warn!("WS closed: {:?}", e);
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        error!("WS error: {:?}", e);
                        return Err(anyhow::anyhow!("WS error: {:?}", e));
                    }
                    None => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}
```

- [ ] **Step 3: Add `tokio::sync::broadcast` to gateway-ws Cargo.toml**

Add `tokio = { version = "1", features = ["sync"] }` if not present.

- [ ] **Step 4: Verify compilation**

Run: `cd /home/hui20metrov/gate-arb && cargo build -p gateway-ws 2>&1`
Expected: Compiles with no errors

- [ ] **Step 5: Commit**

```bash
cd /home/hui20metrov/gate-arb
git add -A
git commit -m "feat(#13): add exponential backoff reconnection to gateway-ws"
```

---

## Issue 2: Gateway → Engine Data Flow

### Task 2: Add channel-based update path from gateway to engine

**Files:**
- Modify: `crates/engine/src/lib.rs`
- Modify: `crates/gateway-ws/src/lib.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add `ObUpdate` type and engine receiver in engine crate**

In `crates/engine/src/lib.rs`, add a new public struct for order book updates and a receiver method. Add BEFORE the existing impl block:

```rust
use crossbeam_channel::{Receiver, Sender};

/// Order book update command sent from gateway → engine hot path.
/// Each update replaces the full top-N levels (snapshot, not delta).
#[derive(Debug, Clone)]
pub struct ObUpdateCmd {
    pub is_spot: bool,
    pub bids: Vec<types::Level>,
    pub asks: Vec<types::Level>,
}

impl<const B: usize, const A: usize> Engine<B, A> {
    /// Spawn the engine's order-book receiver task.
    /// This drains the channel and updates the shared order books on the hot path.
    pub fn spawn_receiver(&self, rx: Receiver<ObUpdateCmd>) {
        let spot_book = Arc::clone(&self.spot_book);
        let perp_book = Arc::clone(&self.perp_book);

        tokio::spawn(async move {
            while let Ok(cmd) = rx.recv() {
                if cmd.is_spot {
                    if let Some(mut book) = try_write_lock(&spot_book) {
                        book.update_bids(&cmd.bids);
                        book.update_asks(&cmd.asks);
                    }
                } else {
                    if let Some(mut book) = try_write_lock(&perp_book) {
                        book.update_bids(&cmd.bids);
                        book.update_asks(&cmd.asks);
                    }
                }
            }
        });
    }
}

/// Try to acquire a write lock on the order book, returning None if busy.
fn try_write_lock<T: Default>(rw: &parking_lot::RwLock<T>) -> Option<parking_lot::RwLockWriteGuard<T>> {
    // parking_lot doesn't have try_write — use std's RwLock for try_write
    None // placeholder; actual implementation uses std::sync::RwLock below
}
```

**Important:** Replace the `Engine` struct to use `std::sync::RwLock` instead of `parking_lot::RwLock` for the hot path (parking_lot's `RwLock` doesn't expose `try_write`). Change the struct to:

```rust
use std::sync::{Arc, RwLock};

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

    pub fn spawn_receiver(&self, rx: Receiver<ObUpdateCmd>) {
        let spot_book = Arc::clone(&self.spot_book);
        let perp_book = Arc::clone(&self.perp_book);
        tokio::spawn(async move {
            while let Ok(cmd) = rx.recv() {
                if cmd.is_spot {
                    if let Ok(mut book) = spot_book.write() {
                        book.update_bids(&cmd.bids);
                        book.update_asks(&cmd.asks);
                    }
                } else {
                    if let Ok(mut book) = perp_book.write() {
                        book.update_bids(&cmd.bids);
                        book.update_asks(&cmd.asks);
                    }
                }
            }
        });
    }
}
```

Add `crossbeam-channel = "0.5"` to `crates/engine/Cargo.toml`.

- [ ] **Step 2: Update gateway-ws to send updates via channel**

In `crates/gateway-ws/src/lib.rs`:

Add a new field `ob_tx: Option<crossbeam_channel::Sender<ObUpdateCmd>>` to `GatewayWs`. But actually — to keep the API clean, add a `ObHandler` struct that wraps the gateway and channels.

**Alternative approach (cleaner):** Modify `process_ob_update` to call a callback. Change the signature:

```rust
pub struct GatewayWs {
    ws_url: &'static str,
    ob_tx: std::sync::Arc<std::sync::Mutex<Option<crossbeam_channel::Sender<ObUpdateCmd>>>>,
}

impl GatewayWs {
    pub fn new() -> Self {
        Self {
            ws_url: GATE_WS_URL,
            ob_tx: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Set the order book update channel sender. Must be called before run().
    pub fn with_ob_channel(self, tx: crossbeam_channel::Sender<ObUpdateCmd>) -> Self {
        *self.ob_tx.lock().unwrap() = Some(tx);
        self
    }

    async fn process_ob_update(&self, is_spot: bool, update: ObUpdate) {
        use types::Fixed64;

        let bids: Vec<_> = update
            .bids
            .iter()
            .take(20)
            .map(|(p, q)| Level {
                price: GatewayWs::parse_price(p),
                qty: q.parse().unwrap_or(0),
            })
            .collect();

        let asks: Vec<_> = update
            .asks
            .iter()
            .take(20)
            .map(|(p, q)| Level {
                price: GatewayWs::parse_price(p),
                qty: q.parse().unwrap_or(0),
            })
            .collect();

        debug!(
            "OB update: {} bids={} asks={}",
            update.s,
            bids.len(),
            asks.len()
        );

        if let Some(tx) = self.ob_tx.lock().unwrap().as_ref() {
            let _ = tx.send(ObUpdateCmd { is_spot, bids, asks });
        }
    }
}
```

Add `use types::{Fixed64, Level};` at the top (remove unused import warning).

Add `ObUpdateCmd` re-export or import at top of `gateway-ws/src/lib.rs` from `engine` crate. Since engine and gateway are separate crates, create a shared `ObUpdateCmd` in the `types` crate instead.

**Revised approach — put `ObUpdateCmd` in types crate:**

Add to `crates/types/src/lib.rs`:

```rust
/// Order book update sent from gateway → engine via channel.
#[derive(Debug, Clone)]
pub struct ObUpdateCmd {
    pub is_spot: bool,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
}
```

Then in `crates/gateway-ws/src/lib.rs`, add `use types::ObUpdateCmd;` and update `process_ob_update` to use it.

- [ ] **Step 3: Update main.rs to wire everything together**

In `src/main.rs`:

```rust
use crossbeam_channel::bounded;

// Create update channel before engine/gateway
let (ob_tx, ob_rx) = bounded::<types::ObUpdateCmd>(1024);

// Pass channel to engine receiver
engine.spawn_receiver(ob_rx);

// Pass channel to gateway
let gw = gateway_ws::GatewayWs::new()
    .with_ob_channel(ob_tx);

// Run gateway with reconnect loop + shutdown signal
let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
let gw_handle = tokio::spawn(async move {
    gw.run_with_reconnect(args::SPOT_SYMBOL, args::PERP_SYMBOL, shutdown_rx).await
});

// Keep main alive, shutdown on Ctrl+C
tokio::signal::ctrl_c().await?;
let _ = shutdown_tx.send(());
let _ = gw_handle.await;
```

- [ ] **Step 4: Verify compilation**

Run: `cd /home/hui20metrov/gate-arb && cargo build 2>&1`
Expected: Compiles with no errors

- [ ] **Step 5: Add test for backoff duration**

Create `crates/gateway-ws/tests/backoff_test.rs`:

```rust
use gateway_ws::ReconnectConfig;

#[test]
fn test_backoff_increases() {
    let config = ReconnectConfig::default();
    let prev = gateway_ws::backoff_duration(0, &config);
    let next = gateway_ws::backoff_duration(1, &config);
    assert!(next > prev, "backoff should increase: {:?} >= {:?}", next, prev);
}
```

Run: `cargo test -p gateway-ws 2>&1`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
cd /home/hui20metrov/gate-arb
git add -A
git commit -m "feat(#13): wire gateway→engine data flow via channel + spawn receiver"
```

---

## Verification

After both tasks complete:

1. `cargo build --release 2>&1 | tail -5` — clean build
2. `cargo test 2>&1 | tail -10` — all tests pass
3. Code review: check that `process_ob_update` sends on channel (not just logs), and that `run_with_reconnect` loops properly
