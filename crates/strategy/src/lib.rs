//! strategy — spread detection + trade signal emission.
//!
//! Paper-trading first: logs signals, does not execute.
//! Thread 2 (warm): position management, risk, funding monitor.
//!
//! ## Thresholds
//! - `threshold_spread_raw`: entry signal — open position when spread >= this value
//! - `exit_spread_raw`: close position when spread <= this value (default 0 = spread ≤ 0)
//! - `min_profit_raw`: minimum net profit required to close (default 0 = any profit)
//!
//! All values are in Fixed64 raw units (1 USDT = 100_000_000).

pub mod funding;
pub use funding::FundingStrategy;

use engine::Engine;
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{info, warn};
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
    /// Entry threshold: open position when spread >= this (raw Fixed64 units).
    pub threshold_spread_raw: u64,
    /// Exit threshold: close position when spread <= this (default 0 = spread reaches zero or inverts).
    pub exit_spread_raw: u64,
    /// Minimum net profit to allow close (default 0 = close at any non-loss).
    /// Guards against closing too early when fees would eat the gain.
    pub min_profit_raw: i64,
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
    /// Create strategy with entry threshold in raw Fixed64 units.
    /// Exit threshold defaults to 0 (spread ≤ 0 triggers close).
    /// Min profit defaults to 0 (any non-loss close is allowed).
    pub fn new(engine: Arc<Engine<20, 20>>, threshold_spread_raw: u64) -> Self {
        Self {
            engine,
            paper_mode: true,
            threshold_spread_raw,
            exit_spread_raw: 0, // close when spread ≤ 0
            min_profit_raw: 0,
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

        // Check exit conditions for open positions
        if self.paper_mode {
            self.paper_close_if_needed();
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

        let leg1 = Leg {
            kind: LegKind::SpotBuy,
            price: sig.bid_price,
            qty: 1_000_000, // 0.01 BTC in 1e8 units
            state: TradeState::BothFilled,
            sent_at_us: Some(now),
            filled_at_us: Some(now),
        };
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

    /// Check whether an open paper position should be closed.
    ///
    /// Exit conditions (any one triggers):
    /// 1. Current spread ≤ exit_spread_raw (default 0 = spread has converged/inverted)
    /// 2. Max hold time exceeded (4h safety cap)
    ///
    /// Gate: close is skipped if estimated net profit < min_profit_raw (default 0).
    fn paper_close_if_needed(&self) {
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
        let max_hold = hold_time >= MAX_HOLD_TIME_US;

        // Get current spread from engine (not threshold-gated)
        let snap = self.engine.current_spread();
        let inverted = snap.as_ref().map(|s| s.inverted).unwrap_or(false);
        let spread_at_exit = snap.as_ref().map(|s| s.spread_raw <= self.exit_spread_raw).unwrap_or(false);
        let spread_converged = inverted || spread_at_exit;

        if !spread_converged && !max_hold {
            return;
        }

        let spot_buy_price = pos.legs[0].price.raw();
        let perp_short_price = pos.legs[1].price.raw();
        let qty = pos.legs[0].qty;

        // Exit prices: use current book if available, else entry prices (conservative)
        let (spot_exit_price, perp_exit_price) = match snap.as_ref() {
            Some(s) => (s.spot_bid.raw(), s.perp_ask.raw()),
            None => (spot_buy_price, perp_short_price),
        };

        // P&L = (spot_sell − spot_buy) + (perp_short_entry − perp_buy_back) − fees
        let spot_pnl = (spot_exit_price as i64 - spot_buy_price as i64) * (qty as i64);
        let perp_pnl = (perp_short_price as i64 - perp_exit_price as i64) * (qty as i64);

        // Fee: 2 legs open + 2 legs close × MAKER_FEE_BPS
        let notional = (spot_buy_price as i128 + perp_short_price as i128) * (qty as i128);
        let fee_raw =
            ((notional * (MAKER_FEE_BPS * 2) as i128 / 10_000i128) / (SCALE as i128)) as i64;

        let trade_pnl_raw = (spot_pnl + perp_pnl) / (SCALE as i64) - fee_raw;

        // min_profit guard semantics:
        // - Full spread inversion: ALWAYS close (position going wrong). No guard.
        // - User-configured exit_spread_raw > 0: ALWAYS close (explicit intent). No guard.
        // - Default convergence to 0 (exit_spread_raw == 0, spread hit zero, not inverted):
        //   apply min_profit guard — don't close if estimated profit is below floor.
        // - max_hold: ALWAYS close regardless. No guard.
        let default_zero_converge = spread_at_exit && self.exit_spread_raw == 0 && !inverted;
        if default_zero_converge && trade_pnl_raw < self.min_profit_raw && !max_hold {
            return;
        }

        let reason = if max_hold {
            "max_hold"
        } else if inverted {
            "spread_inverted"
        } else {
            "spread_converged"
        };

        // Commit close
        let mut cum_pnl = self.cumulative_pnl.write();
        *cum_pnl += trade_pnl_raw;
        let pnl_after = *cum_pnl;
        drop(cum_pnl);

        let mut stats = self.paper_stats.write();
        stats.total_trades += 1;
        if trade_pnl_raw >= 0 {
            stats.wins += 1;
        } else {
            stats.losses += 1;
        }
        stats.pnl_raw = pnl_after;
        drop(stats);

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
            qty: 1_000_000,
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
