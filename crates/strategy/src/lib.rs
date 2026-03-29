//! strategy — spread detection + trade signal emission.
//!
//! Paper-trading first: logs signals, does not execute.
//! Thread 2 (warm): position management, risk, funding monitor.

use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{info, warn};

use db::{Db, SpreadRecord, TradeRecord};
use engine::Engine;
use types::{ArbitragePosition, Leg, LegKind, SpreadSignal, TradeState, Fixed64, SCALE};

const LEG2_TIMEOUT_US: u64 = 500_000; // 500ms
#[allow(dead_code)]
const MAX_DRAWDOWN_RAW: u64 = 10_000_000_000; // $100 in 1e8 units

pub struct Strategy {
    pub engine: Arc<Engine<20, 20>>,
    pub paper_mode: bool,
    pub threshold_spread_raw: u64,
    /// Current active position (if any).
    position: RwLock<Option<ArbitragePosition>>,
    /// Cumulative P&L in raw units.
    cumulative_pnl: RwLock<i64>,
    /// Optional DB for persistence (can be None in tests).
    pub db: Option<Arc<Db>>,
    pub symbol: &'static str,
    /// Paper trading executor.
    paper_exec: PaperExecutor,
    /// Active paper position (separate from live position).
    paper_pos: RwLock<Option<PaperPosition>>,
}

impl Strategy {
    pub fn new(engine: Arc<Engine<20, 20>>, threshold_spread_raw: u64) -> Self {
        Self {
            engine,
            paper_mode: true,
            threshold_spread_raw,
            position: RwLock::new(None),
            cumulative_pnl: RwLock::new(0),
            db: None,
            symbol: "BTC_USDT",
            paper_exec: PaperExecutor::new(),
            paper_pos: RwLock::new(None),
        }
    }

    /// Set the DB handle (called after construction).
    pub fn with_db(mut self, db: Arc<Db>) -> Self {
        self.db = Some(db);
        self
    }

    /// Called every tick from the hot path — check spread, emit signals.
    #[inline(always)]
    pub fn on_tick(&self) {
        let signal = self.engine.check_spread(self.threshold_spread_raw);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as i64;

        if let Some(ref sig) = signal {
            // Log spread to DB (hot path — uses lock-free ring, zero blocking)
            if let Some(ref db) = self.db {
                db.push_spread(SpreadRecord {
                    ts: now,
                    spread_raw: sig.spread_raw as i64,
                    spread_pct_raw: sig.spread_pct.raw() as i64,
                    symbol: self.symbol,
                });
            }

            if self.position.read().is_none() && self.paper_pos.read().is_none() {
                info!(
                    "SPREAD SIGNAL: raw={} pct={} bid={} ask={}",
                    sig.spread_raw, sig.spread_pct, sig.bid_price, sig.ask_price
                );
                if !self.paper_mode {
                    self.open_position(sig.clone());
                } else {
                    // Paper mode: open simulated position
                    if let Some(pos) = self.paper_exec.on_entry_signal(sig, &self.db) {
                        *self.paper_pos.write() = Some(pos);
                        info!(
                            "PAPER ENTRY: spot={} perp={} size={}",
                            sig.bid_price, sig.ask_price,
                            self.paper_exec.size_qty
                        );
                    }
                }
            }
        } else {
            // No spread signal — check if paper position should close
            if self.paper_mode {
                let mut pp = self.paper_pos.write();
                if let Some(ref pos) = *pp {
                    // Exit paper position at current mid-market (estimated from last engine state)
                    // Use engine's current books to estimate mid; if no book, skip
                    let spot_bid = self.engine.spot_book.read().best_bid();
                    let perp_ask = self.engine.perp_book.read().best_ask();
                    if let (Some(sb), Some(pa)) = (spot_bid, perp_ask) {
                        let fake_sig = SpreadSignal {
                            spread_raw: 0,
                            spread_pct: Fixed64::zero(),
                            bid_price: sb,
                            ask_price: pa,
                            timestamp_us: now as u64,
                        };
                        let record = self.paper_exec.on_exit_signal(pos, &fake_sig, &self.db);
                        let pnl = record.pnl_usd_raw;
                        {
                            let mut cp = self.cumulative_pnl.write();
                            *cp += pnl;
                        }
                        info!(
                            "PAPER EXIT: pnl={} ({} USD) cumulative={} total_pnl_usd",
                            pnl,
                            pnl as f64 / SCALE as f64,
                            *self.cumulative_pnl.read()
                        );
                        *pp = None;
                    }
                }
            }
        }

        // Check position timeouts
        self.check_timeouts();
    }

    /// Log a closed trade to SQLite via the DB ring buffer.
    #[inline(always)]
    pub fn log_trade(&self, record: TradeRecord) {
        if let Some(ref db) = self.db {
            db.push_trade(record);
        }
    }

    fn open_position(&self, sig: SpreadSignal) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;

        let leg1 = Leg {
            kind: LegKind::SpotBuy,
            price: sig.bid_price,
            qty: 1_000_000, // 0.01 BTC
            state: TradeState::Leg1Sent,
            sent_at_us: Some(now),
            filled_at_us: None,
        };

        let leg2 = Leg {
            kind: LegKind::PerpShort,
            price: sig.ask_price,
            qty: 1_000_000,
            state: TradeState::Idle,
            sent_at_us: None,
            filled_at_us: None,
        };

        *self.position.write() = Some(ArbitragePosition::new(leg1, leg2, now));
    }

    fn check_timeouts(&self) {
        let mut pos = self.position.write();
        if let Some(ref mut p) = *pos {
            // LEG1_FILLED + LEG2 timeout check
            if p.state == TradeState::Leg1Filled {
                if let Some(leg1_filled) = p.legs[0].filled_at_us {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_micros() as u64;
                    if now.saturating_sub(leg1_filled) > LEG2_TIMEOUT_US {
                        warn!("LEG2 timeout — emergency close LEG1");
                        p.state = TradeState::Closing;
                    }
                }
            }
        }
    }

    pub fn get_pnl(&self) -> i64 {
        *self.cumulative_pnl.read()
    }
}

/// Paper trade executor — simulates entry/exit and records P&L to DB.
/// All prices in 1e8 fixed-point units.
pub struct PaperExecutor {
    pub size_qty: u64, // 1e8 units (e.g. 1_000_000 = 0.01 BTC)
    pub exit_spread_bps: i64, // close when spread ≤ this many bps
}

impl PaperExecutor {
    pub fn new() -> Self {
        Self {
            size_qty: 1_000_000, // 0.01 BTC
            exit_spread_bps: 0,  // close when spread ≤ 0
        }
    }

    /// Called on a new spread signal — open paper position.
    #[inline(always)]
    pub fn on_entry_signal(&self, sig: &SpreadSignal, db: &Option<Arc<Db>>) -> Option<PaperPosition> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as i64;

        let spot_entry = sig.bid_price.raw() as i64;   // bought spot at bid
        let perp_entry = sig.ask_price.raw() as i64;   // shorted perp at ask
        let size = self.size_qty as i64;
        let size_usd = (sig.bid_price.as_float() * (size as f64 / 1e8)) as i64;

        // P&L = (exit_spot - entry_spot) * size - (exit_perp - entry_perp) * size
        // When spread closes: perp goes up (perp_ask rises), spot goes down
        // Simple: PnL in quote currency = (perp_entry - perp_exit) * size + (spot_exit - spot_entry) * size

        Some(PaperPosition {
            entry_ts: now,
            spot_entry,
            perp_entry,
            size_usd,
            size,
        })
    }

    /// Called when spread closes — close paper position and record to DB.
    #[inline(always)]
    pub fn on_exit_signal(
        &self,
        pos: &PaperPosition,
        sig: &SpreadSignal,
        db: &Option<Arc<Db>>,
    ) -> TradeRecord {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as i64;

        let spot_exit = sig.bid_price.raw() as i64; // close spot at bid
        let perp_exit = sig.ask_price.raw() as i64; // cover perp at ask

        // P&L in 1e8 units
        // Spot P&L: (spot_exit - spot_entry) * size_qty / SCALE
        // Perp P&L: (perp_entry - perp_exit) * size_qty / SCALE  (short, so entry > exit = profit)
        let spot_pnl = (spot_exit - pos.spot_entry) * pos.size;
        let perp_pnl = (pos.perp_entry - perp_exit) * pos.size;
        let total_pnl = spot_pnl + perp_pnl;

        // P&L pct in basis points
        let pnl_pct = if pos.size_usd != 0 {
            (total_pnl as f64 / pos.size_usd as f64 * 10000.0) as i64
        } else {
            0
        };

        let record = TradeRecord {
            symbol: "BTC_USDT",
            entry_ts: pos.entry_ts,
            exit_ts: now,
            spot_entry: pos.spot_entry,
            perp_entry: pos.perp_entry,
            spot_exit,
            perp_exit,
            size_usd_raw: pos.size_usd,
            pnl_usd_raw: total_pnl,
            pnl_pct_raw: pnl_pct,
            exit_reason: "spread_closed",
        };

        if let Some(ref d) = db {
            d.push_trade(record);
        }

        record
    }
}

impl Default for PaperExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct PaperPosition {
    pub entry_ts: i64,
    pub spot_entry: i64,
    pub perp_entry: i64,
    pub size_usd: i64,
    pub size: i64,
}
