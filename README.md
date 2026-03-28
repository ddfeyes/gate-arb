# gate-arb — HFT Spot-Perp & Funding Arbitrage

High-frequency trading engine for Gate.io spot-perpetual futures arbitrage.

## Architecture

```
Thread 1 (hot):  WS recv → parse → update book → spread check → signal
Thread 2 (warm): position management, risk, funding monitor  
Thread 3 (cold): DB writes, frontend WS broadcast, logging
```

## Crates

| Crate | Purpose |
|-------|---------|
| `types` | Fixed-point primitives (u64×1e8), OrderBook, TradeState |
| `gateway-ws` | Gate.io WebSocket v4 client (auth, subscribe, heartbeat) |
| `engine` | Hot path: book update → spread detection |
| `strategy` | Signal emission, position state machine |
| `frontend-ws` | WebSocket broadcast to trading dashboard |

## Strategies

1. **Spot-Perp**: buy spot + short perp when spread > threshold, close when spread ≤ 0
2. **Funding**: short perp + buy spot before funding snapshot, collect funding, exit after

## Position Safety

- State machine per trade: `IDLE → LEG1_SENT → LEG1_FILLED → LEG2_SENT → BOTH_FILLED → CLOSING → CLOSED`
- LEG1_FILLED + LEG2 timeout >500ms → emergency market close LEG1
- WS ping >100ms → pause new entries
- Paper trading by default (`PAPER_MODE=false` to enable live)

## Quick Start

```bash
# Build
cargo build --release --all

# Run (paper mode)
cargo run --release --bin gate-arb

# Run (live trading)
PAPER_MODE=false cargo run --release --bin gate-arb
```

## WebSocket Dashboard

Connects to `ws://localhost:8080` — broadcasts realtime spreads, P&L, positions.

## Fixed-Point Math

All prices use `u64 × 10^8` precision (8 decimal places). No float in the hot path.

```rust
let price = Fixed64::from_float(50000.12345678);
assert_eq!(price.raw(), 5_000_012_345_678);
```

## Deploy

- **Engine**: AWS Tokyo — `ssh -o StrictHostKeyChecking=no ivan@13.158.239.11`
- **DB + Frontend**: Hetzner EU
