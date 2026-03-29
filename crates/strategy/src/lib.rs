//! strategy — spread detection + trade signal emission.
//!
//! Paper-trading first: logs signals, does not execute.
//! Thread 2 (warm): position management, risk, funding monitor.

pub mod funding;
pub use funding::FundingStrategy;

use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{info, warn};

use engine::Engine;
use types::{ArbitragePosition, Leg, LegKind, SpreadSignal, TradeState, SCALE};

const LEG2_TIMEOUT_US: u64 = 500_000; // 500ms
const MAX_HOLD_TIME_US: u64 = 4 * 3600 * 1_000_000; // 4 hours
const MAKER_FEE_BPS: u64 = 2; // 0.02% maker fee per leg
const PAPER_SUMMARY_EVERY_N_TICKS: u64 = 100;

#[derive(Debug, Clone, Default)]
pub struct PaperStats {
    pub total_trades: u64,
    pub wins: u64,
    pub losses: u64,
    pub pnl_raw: i64,
    pub last_signal_at_us: Option<u64>,
}

pub struct Strategy {
    pub engine: Arc<Engine<20, 20>>,
    pub paper_mode: bool,
    pub threshold_spread_raw: u64,
    /// Current active position (if any).
    position: RwLock<Option<ArbitragePosition>>,
    /// Cumulative P&L in raw units.
    cumulative_pnl: RwLock<i64>,
    /// Paper-mode statistics.
    paper_stats: RwLock<PaperStats>,
    /// Tick counter for summary printing.
    tick_counter: RwLock<u64>,
}

impl Strategy {
    pub fn new(engine: Arc<Engine<20, 20>>, threshold_spread_bps: u64) -> Self {
        Self {
            engine,
            paper_mode: true,
            threshold_spread_raw: threshold_spread_bps,
            position: RwLock::new(None),
            cumulative_pnl: RwLock::new(0),
            paper_stats: RwLock::new(PaperStats::default()),
            tick_counter: RwLock::new(0),
        }
    }

    /// Called every tick from the hot path — check spread, emit signals.
    #[inline(always)]
    pub fn on_tick(&self) {
        let signal = self.engine.check_spread(self.threshold_spread_raw);
        self.on_tick_with_signal(signal);
    }

    /// Called with a pre-computed signal (from engine) or None.
    /// Handles: signal logging, position open, close checks, periodic summary.
    #[inline(always)]
    pub fn on_tick_with_signal(&self, sig: Option<SpreadSignal>) {
        if let Some(ref sig) = sig {
            let mut stats = self.paper_stats.write();
            stats.last_signal_at_us = Some(sig.timestamp_us);
            drop(stats);

            if self.position.read().is_none() {
                info!(
                    "SPREAD SIGNAL: raw={} pct={} bid={} ask={}",
                    sig.spread_raw, sig.spread_pct, sig.bid_price, sig.ask_price
                );
                if self.paper_mode {
                    self.paper_open_position(sig.clone());
                } else {
                    self.open_position(sig.clone());
                }
            }
        }

        // Paper mode: check spread inversion and max hold
        if self.paper_mode {
            self.paper_close_if_needed(sig);
        } else {
            self.check_timeouts();
        }

        // Periodic paper summary
        let mut tick = self.tick_counter.write();
        *tick += 1;
        if *tick % PAPER_SUMMARY_EVERY_N_TICKS == 0 {
            drop(tick);
            self.paper_print_summary();
        }
    }

    /// Paper mode: simulate immediate fill on both legs.
    fn paper_open_position(&self, sig: SpreadSignal) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;

        // Leg1: buy spot at bid (our long leg = we buy spot)
        let leg1 = Leg {
            kind: LegKind::SpotBuy,
            price: sig.bid_price,
            qty: 1_000_000, // 0.01 BTC in 1e8 units
            state: TradeState::BothFilled,
            sent_at_us: Some(now),
            filled_at_us: Some(now),
        };

        // Leg2: short perp at ask
        let leg2 = Leg {
            kind: LegKind::PerpShort,
            price: sig.ask_price,
            qty: 1_000_000,
            state: TradeState::BothFilled,
            sent_at_us: Some(now),
            filled_at_us: Some(now),
        };

        let mut pos = ArbitragePosition::new(leg1, leg2, now);
        pos.state = TradeState::BothFilled;
        *self.position.write() = Some(pos);

        info!(
            "PAPER OPEN: spot_buy@{} perp_short@{} spread_raw={} spread_pct={}",
            sig.bid_price, sig.ask_price, sig.spread_raw, sig.spread_pct
        );
    }

    /// Close paper position: check spread inversion or max hold time.
    fn paper_close_if_needed(&self, sig: Option<SpreadSignal>) {
        let mut pos_guard = self.position.write();
        let pos = match pos_guard.as_mut() {
            Some(p) => p,
            None => return,
        };

        if pos.state != TradeState::BothFilled {
            return;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;

        let hold_time = now.saturating_sub(pos.opened_at_us);
        let spread_inverted = sig
            .as_ref()
            .map(|s| s.spread_raw < self.threshold_spread_raw)
            .unwrap_or(false);
        let max_hold = hold_time >= MAX_HOLD_TIME_US;

        if !spread_inverted && !max_hold {
            return;
        }

        let reason = if spread_inverted {
            "spread_inverted"
        } else {
            "max_hold"
        };

        // P&L = (spot_sell - spot_buy) + (perp_short_entry - perp_close) - 2 * fee
        let spot_buy_price = pos.legs[0].price.raw(); // entry spot buy
        let perp_short_price = pos.legs[1].price.raw(); // entry perp short

        // Close: sell spot at current bid, buy back perp at current ask
        let (spot_exit_price, perp_exit_price) = if spread_inverted {
            (
                sig.as_ref()
                    .map(|s| s.bid_price.raw())
                    .unwrap_or(spot_buy_price),
                sig.as_ref()
                    .map(|s| s.ask_price.raw())
                    .unwrap_or(perp_short_price),
            )
        } else {
            (
                sig.as_ref()
                    .map(|s| s.bid_price.raw())
                    .unwrap_or(spot_buy_price),
                sig.as_ref()
                    .map(|s| s.ask_price.raw())
                    .unwrap_or(perp_short_price),
            )
        };

        let qty = pos.legs[0].qty;

        // Spot P&L: (exit_bid - entry_bid) * qty / SCALE
        let spot_pnl = (spot_exit_price as i64 - spot_buy_price as i64) * (qty as i64);
        // Perp P&L: (entry_perp - exit_perp) * qty / SCALE (short profits if price falls)
        let perp_pnl = (perp_short_price as i64 - perp_exit_price as i64) * (qty as i64);

        // Fee: 2 legs × 0.02% = 0.04% of notional
        // fee = 2 * qty * (entry_spot + entry_perp) * MAKER_FEE_BPS / 10000 / SCALE
        let notional = (spot_buy_price as i128 + perp_short_price as i128) * (qty as i128);
        let fee_raw =
            ((notional * (MAKER_FEE_BPS * 2) as i128 / 10_000i128) / (SCALE as i128)) as i64;

        let trade_pnl_raw = (spot_pnl + perp_pnl) / (SCALE as i64) - fee_raw;

        // Update cumulative
        let mut cum_pnl = self.cumulative_pnl.write();
        *cum_pnl += trade_pnl_raw;
        let pnl_after = *cum_pnl;
        drop(cum_pnl);

        // Update stats
        let mut stats = self.paper_stats.write();
        stats.total_trades += 1;
        if trade_pnl_raw >= 0 {
            stats.wins += 1;
        } else {
            stats.losses += 1;
        }
        stats.pnl_raw = pnl_after;

        info!(
            "PAPER CLOSE [{}]: spot_pnl={:.6} perp_pnl={:.6} fee={:.6} trade_pnl={:.6} cum_pnl={:.6} hold={}s",
            reason,
            spot_pnl as f64 / SCALE as f64,
            perp_pnl as f64 / SCALE as f64,
            fee_raw as f64 / SCALE as f64,
            trade_pnl_raw as f64 / SCALE as f64,
            pnl_after as f64 / SCALE as f64,
            hold_time / 1_000_000
        );

        pos.state = TradeState::Closed;
        *pos_guard = None;
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

    fn paper_print_summary(&self) {
        let stats = self.paper_stats.read();
        let pnl_usd = stats.pnl_raw as f64 / SCALE as f64;
        let win_rate = if stats.total_trades > 0 {
            stats.wins as f64 / stats.total_trades as f64 * 100.0
        } else {
            0.0
        };
        info!(
            "PAPER SUMMARY [tick={}]: trades={} wins={} losses={} win_rate={:.1}% pnl_usd={:.4} last_signal={}",
            self.tick_counter.read(),
            stats.total_trades,
            stats.wins,
            stats.losses,
            win_rate,
            pnl_usd,
            stats.last_signal_at_us.map(|t| t.to_string()).unwrap_or_else(|| "none".into())
        );
    }

    pub fn get_pnl(&self) -> i64 {
        *self.cumulative_pnl.read()
    }

    pub fn get_paper_stats(&self) -> PaperStats {
        self.paper_stats.read().clone()
    }

    /// Returns true if a paper position is currently open.
    pub fn is_position_open(&self) -> bool {
        self.position.read().is_some()
    }
}
